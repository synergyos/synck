//! Exact UTF8 body views without changing canonical content references.
use std::collections::BTreeMap;
use synck_vcs::{
    object::Object,
    types::{EventProposal, ObjectId},
};

pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BodyEventError {
    #[error("{0}")]
    InvalidEvent(String),
    #[error("{0}")]
    InvalidContent(String),
}

pub fn body_key(event_type: &str) -> Option<&'static str> {
    match event_type {
        "CHAT::MESSAGE::CREATE" | "CHAT::MESSAGE::SET_CONTENT" => Some("body"),
        _ => None,
    }
}

pub fn validate_body(event_type: &str, body: &str) -> Result<(), BodyEventError> {
    if body.len() <= MAX_BODY_BYTES && body_key(event_type).is_some() {
        Ok(())
    } else {
        Err(BodyEventError::InvalidEvent(format!(
            "invalid body for {event_type}"
        )))
    }
}

pub fn restore(
    events: &mut [EventProposal],
    objects: &BTreeMap<ObjectId, Object>,
) -> Result<(), BodyEventError> {
    for event in events {
        let Some(content) = &mut event.set_content else {
            continue;
        };
        let Some(id) = content.get("body") else {
            continue;
        };
        let object = objects
            .get(id)
            .ok_or_else(|| BodyEventError::InvalidContent("missing body".into()))?;
        validate_body(&event.event_type, object.utf8())?;
        content
            .resolve(object.clone())
            .map_err(|error| BodyEventError::InvalidContent(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use synck_vcs::types::{EventContent, canonical_event_proposal_bytes};
    #[test]
    fn resolving_a_body_preserves_envelope_and_exact_bytes() {
        let body = " \0🦀\r\n";
        let object = Object::new(body.as_bytes()).unwrap();
        let mut event = EventProposal::from_fields(serde_json::json!({"event_type":"CHAT::MESSAGE::CREATE", "set_content":EventContent::body(object.id())})).unwrap();
        let before = canonical_event_proposal_bytes(&event).unwrap();
        restore(
            std::slice::from_mut(&mut event),
            &BTreeMap::from([(object.id(), object)]),
        )
        .unwrap();
        assert_eq!(
            event
                .set_content
                .as_ref()
                .unwrap()
                .object("body")
                .unwrap()
                .utf8(),
            body
        );
        assert_eq!(canonical_event_proposal_bytes(&event).unwrap(), before);
        assert!(validate_body("CHAT::MESSAGE::CREATE", &"a".repeat(MAX_BODY_BYTES + 1)).is_err());
    }
}
