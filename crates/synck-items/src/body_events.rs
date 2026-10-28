//! Storage-free interpretation of referenced and legacy inline event bodies.
//!
//! These helpers preserve exact source UTF-8, including NUL bytes. Callers may
//! sanitize a separate projection view only after restoring its body content;
//! restored events must never replace canonical stored or hashed history.

use std::collections::BTreeMap;

use synck_vcs::{
    errors::CoreError,
    types::{ObjectId, EventContent, EventProposal},
};

use synck_vcs::object::Object;
#[cfg(test)]
use synck_vcs::object::Codec;

pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_BATCH_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Invalid event input is distinct from missing or invalid content state.
#[derive(Debug, thiserror::Error)]
pub enum BodyEventError {
    #[error("{0}")]
    InvalidEvent(String),
    #[error("{0}")]
    InvalidContent(String),
    #[error(transparent)]
    Core(#[from] CoreError),
}

fn invalid_content(error: impl std::fmt::Display) -> BodyEventError {
    BodyEventError::InvalidContent(error.to_string())
}

pub fn body_key(event_type: &str) -> Option<&'static str> {
    match event_type {
        "CHAT::MESSAGE::CREATE" | "CHAT::MESSAGE::SET_CONTENT" => Some("body"),
        _ => None,
    }
}

pub fn validate_body(event_type: &str, body: &str) -> Result<(), BodyEventError> {
    let valid = body.len() <= MAX_BODY_BYTES && body_key(event_type).is_some();
    if valid {
        Ok(())
    } else {
        Err(BodyEventError::InvalidEvent(format!(
            "invalid body for {event_type}"
        )))
    }
}

pub fn inline_body(event: &EventProposal) -> Result<Option<&str>, BodyEventError> {
    let Some(key) = body_key(&event.event_type) else {
        if matches!(event.set_content, Some(EventContent::Content { .. })) {
            return Err(BodyEventError::InvalidEvent(
                "content reference is not allowed for this event".into(),
            ));
        }
        return Ok(None);
    };
    match event.set_content.as_ref() {
        Some(EventContent::Content { .. }) => Ok(None),
        Some(EventContent::Json { json }) => {
            let body = json
                .as_object()
                .filter(|object| object.len() == 1)
                .and_then(|object| object.get(key))
                .and_then(|value| value.as_str())
                .ok_or_else(|| {
                    BodyEventError::InvalidEvent(format!(
                        "{} requires exactly JSON {key}",
                        event.event_type
                    ))
                })?;
            validate_body(&event.event_type, body)?;
            Ok(Some(body))
        }
        _ => Err(BodyEventError::InvalidEvent(format!(
            "{} requires body content",
            event.event_type
        ))),
    }
}

pub fn reference_ids(events: &[EventProposal]) -> Result<Vec<ObjectId>, BodyEventError> {
    events
        .iter()
        .filter_map(|event| match &event.set_content {
            Some(EventContent::Content { content_id }) => {
                Some(if body_key(&event.event_type).is_some() {
                    Ok(content_id.clone())
                } else {
                    Err(BodyEventError::InvalidEvent(
                        "content reference is not allowed for this event".into(),
                    ))
                })
            }
            _ => None,
        })
        .collect()
}

/// Preserve legacy byte limits when Core adds IDs/base sequences under lock.
/// The delta depends only on the content representation, not those later fields.
pub fn payload_deltas(
    events: &[EventProposal],
    objects: &BTreeMap<ObjectId, Object>,
) -> Result<synck_vcs::repo::ContentPayloadDeltas, BodyEventError> {
    let mut deltas = synck_vcs::repo::ContentPayloadDeltas::new();
    for event in events {
        let Some(EventContent::Content { content_id }) = &event.set_content else {
            continue;
        };
        let key = (event.event_type.clone(), content_id.clone());
        if deltas.contains_key(&key) {
            continue;
        }
        let reference_len = synck_vcs::types::canonical_event_proposal_bytes(event)?.len();
        let mut legacy = event.clone();
        restore(std::slice::from_mut(&mut legacy), objects)?;
        let legacy_len = synck_vcs::types::canonical_event_proposal_bytes(&legacy)?.len();
        deltas.insert(
            key,
            isize::try_from(legacy_len).map_err(invalid_content)?
                - isize::try_from(reference_len).map_err(invalid_content)?,
        );
    }
    Ok(deltas)
}

/// Charge occurrences, not distinct IDs, before any body load.
pub fn charged_bytes(
    ids: &[ObjectId],
    lengths: &BTreeMap<ObjectId, usize>,
) -> Result<usize, BodyEventError> {
    let mut total = 0usize;
    for id in ids {
        total = total
            .checked_add(
                *lengths
                    .get(id)
                    .ok_or_else(|| invalid_content("missing content metadata"))?,
            )
            .filter(|total| *total <= MAX_BATCH_BODY_BYTES)
            .ok_or_else(|| invalid_content("decoded bodies exceed 16 MiB"))?;
    }
    Ok(total)
}

/// Select complete commits only. Every individually valid first commit fits.
pub fn commit_prefix(
    commits: &[Vec<ObjectId>],
    lengths: &BTreeMap<ObjectId, usize>,
) -> Result<usize, BodyEventError> {
    let mut total = 0usize;
    for (index, ids) in commits.iter().enumerate() {
        let bytes = charged_bytes(ids, lengths)?;
        if bytes > MAX_BATCH_BODY_BYTES - total {
            return Ok(index);
        }
        total += bytes;
    }
    Ok(commits.len())
}

/// Restore legacy shapes only in memory. The caller must not hash/store this view.
pub fn restore(
    events: &mut [EventProposal],
    objects: &BTreeMap<ObjectId, Object>,
) -> Result<(), BodyEventError> {
    for event in events {
        let Some(EventContent::Content { content_id }) = &event.set_content else {
            continue;
        };
        let key = body_key(&event.event_type)
            .ok_or_else(|| invalid_content("reference on unsupported event"))?;
        let body = objects
            .get(content_id)
            .ok_or_else(|| invalid_content("missing body"))?
            .utf8().map_err(invalid_content)?;
        validate_body(&event.event_type, body)?;
        event.set_content = Some(EventContent::Json {
            json: serde_json::json!({key: body}),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_policy_preserves_unicode_and_empty_body_rules() {
        for kind in ["CHAT::MESSAGE::CREATE", "CHAT::MESSAGE::SET_CONTENT"] {
            assert!(validate_body(kind, "").is_ok());
            assert!(validate_body(kind, "\0🦀").is_ok());
            assert!(validate_body(kind, &"x".repeat(MAX_BODY_BYTES)).is_ok());
            assert!(validate_body(kind, &"x".repeat(MAX_BODY_BYTES + 1)).is_err());
        }
        assert!(validate_body("FILE::SET_CONTENT", "body").is_err());
    }

    #[test]
    fn referenced_bodies_restore_exact_source_bytes_and_legacy_payload_sizes() {
        use synck_vcs::types::canonical_event_proposal_bytes;

        let body = " \0🦀\r\n";
        let object = Object::new(Codec::Utf8, body.as_bytes()).unwrap();
        let id = object.id();
        let objects = BTreeMap::from([(id.clone(), object)]);
        for (event_type, key) in [
            ("CHAT::MESSAGE::CREATE", "body"),
            ("CHAT::MESSAGE::SET_CONTENT", "body"),
        ] {
            let legacy: EventProposal = serde_json::from_value(serde_json::json!({
                "event_type": event_type,
                "item_id": "01890c6c-1111-7777-8888-aaaaaaaaaaaa",
                "base_seq": 1,
                "set_content": {"json": {key: body}},
            }))
            .unwrap();
            assert_eq!(inline_body(&legacy).unwrap(), Some(body));
            let legacy_bytes = canonical_event_proposal_bytes(&legacy).unwrap();
            let mut reference = legacy.clone();
            reference.set_content = Some(EventContent::Content { content_id: id.clone() });
            let reference_bytes = canonical_event_proposal_bytes(&reference).unwrap();
            assert_eq!(inline_body(&reference).unwrap(), None);
            let events = [legacy, reference.clone(), reference];
            assert_eq!(reference_ids(&events).unwrap(), [id.clone(), id.clone()]);
            let deltas = payload_deltas(&events, &objects).unwrap();
            assert_eq!(deltas.len(), 1);
            assert_eq!(
                deltas[&(event_type.to_string(), id.clone())],
                legacy_bytes.len() as isize - reference_bytes.len() as isize,
            );
            let mut restored = events.to_vec();
            restore(&mut restored, &objects).unwrap();
            for event in &restored {
                assert_eq!(inline_body(event).unwrap(), Some(body));
                assert_eq!(canonical_event_proposal_bytes(event).unwrap(), legacy_bytes);
            }
            assert_eq!(
                canonical_event_proposal_bytes(&events[1]).unwrap(),
                reference_bytes
            );
        }
    }

    #[test]
    fn repeated_ids_are_charged_per_event() {
        let id = ObjectId::from_digest([1; 32]);
        let metadata = BTreeMap::from([(id.clone(), MAX_BODY_BYTES)]);
        assert_eq!(
            charged_bytes(&vec![id.clone(); 8], &metadata).unwrap(),
            MAX_BATCH_BODY_BYTES
        );
        assert!(charged_bytes(&vec![id.clone(); 9], &metadata).is_err());
        assert!(charged_bytes(&[id], &BTreeMap::new()).is_err());
    }

    #[test]
    fn projection_selects_only_whole_commits_and_resumes() {
        let id = ObjectId::from_digest([1; 32]);
        let metadata = BTreeMap::from([(id.clone(), MAX_BODY_BYTES)]);
        let commits = vec![vec![id.clone(); 3]; 128];
        assert_eq!(commit_prefix(&commits, &metadata).unwrap(), 2);
        assert_eq!(commit_prefix(&commits[2..], &metadata).unwrap(), 2);
        assert_eq!(commit_prefix(&[vec![id.clone(); 8]], &metadata).unwrap(), 1);
        assert!(commit_prefix(&[vec![id.clone(); 9]], &metadata).is_err());
        assert_eq!(
            commit_prefix(&vec![Vec::new(); 128], &BTreeMap::new()).unwrap(),
            128
        );
    }
}
