//! Storage-free reference discovery from complete accepted item and ref history.
//!
//! Scan every retained event, including deleted items and refs; current heads
//! cannot establish historical object closure. Direct canonical references are
//! preserved for unknown event families, without claiming typed reader support.
//! Only the existing POST, mailcal and TASK compatibility schemas add metadata
//! references. Drafts, derived artifacts and private jobs are separate roots.

use serde_json::{Map, Value};
use synck_vcs::types::{
    BlobId, CommitEventRow, ContentId, EventContent, EventProposal, RefEventProposal,
};

use crate::{body_events, tasks};

#[derive(Debug, thiserror::Error)]
pub enum ReferenceError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Task(#[from] tasks::TaskError),
    #[error(transparent)]
    Body(#[from] body_events::BodyEventError),
}

impl From<String> for ReferenceError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

/// Direct canonical references plus the explicitly versioned metadata exceptions.
/// Blob and logical-body objects occupy separate storage namespaces.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryReferences {
    pub blobs: Vec<BlobId>,
    pub contents: Vec<ContentId>,
}

fn push_optional_blob_id(
    out: &mut Vec<String>,
    value: Option<&Value>,
    path: &str,
) -> Result<(), String> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(blob_id)) => {
            out.push(blob_id.clone());
            Ok(())
        }
        Some(_) => Err(format!("{path} must be a string or null")),
    }
}

fn push_object_blob_id(
    out: &mut Vec<String>,
    object: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<(), String> {
    push_optional_blob_id(out, object.get(field), path)
}

fn push_mailcal_blob_references(
    out: &mut Vec<String>,
    event_type: &str,
    metadata: &Value,
) -> Result<(), String> {
    if !matches!(event_type, "EMAIL::CREATE" | "EMAIL::SET_METADATA") {
        return Ok(());
    }

    let Some(mailcal_value) = metadata.get("mailcal") else {
        return Ok(());
    };
    let mailcal = mailcal_value
        .as_object()
        .ok_or_else(|| "mailcal metadata must be an object".to_string())?;
    let version = mailcal
        .get("v")
        .and_then(Value::as_u64)
        .ok_or_else(|| "mailcal.v must be integer 1 or 2".to_string())?;
    if !matches!(version, 1 | 2) {
        return Err(format!("unsupported mailcal.v {version}"));
    }

    if let Some(body_value) = mailcal.get("body") {
        let body = body_value
            .as_object()
            .ok_or_else(|| "mailcal.body must be an object".to_string())?;
        push_object_blob_id(out, body, "blob_id", "mailcal.body.blob_id")?;
    }
    push_object_blob_id(
        out,
        mailcal,
        "body_html_blob_id",
        "mailcal.body_html_blob_id",
    )?;
    push_object_blob_id(
        out,
        mailcal,
        "raw_rfc822_blob_id",
        "mailcal.raw_rfc822_blob_id",
    )?;
    push_object_blob_id(
        out,
        mailcal,
        "provider_raw_json_blob_id",
        "mailcal.provider_raw_json_blob_id",
    )?;

    if version == 2
        && let Some(source_value) = mailcal.get("source")
    {
        let source = source_value
            .as_object()
            .ok_or_else(|| "mailcal.source must be an object".to_string())?;
        push_object_blob_id(out, source, "blob_id", "mailcal.source.blob_id")?;
    }

    match mailcal.get("inline_images") {
        None | Some(Value::Null) => {}
        Some(Value::Array(images)) => {
            for (index, image_value) in images.iter().enumerate() {
                let image = image_value
                    .as_object()
                    .ok_or_else(|| format!("mailcal.inline_images[{index}] must be an object"))?;
                push_object_blob_id(
                    out,
                    image,
                    "blob_id",
                    &format!("mailcal.inline_images[{index}].blob_id"),
                )?;
            }
        }
        Some(_) => return Err("mailcal.inline_images must be an array or null".to_string()),
    }

    Ok(())
}

/// Enumerate legacy raw blob strings without changing existing validation order.
/// Use `event_references` when validated identifiers and body references are needed.
pub fn event_blob_references(event: &EventProposal) -> Result<Vec<String>, ReferenceError> {
    let mut refs = tasks::email::event_blob_references(event)?;
    if let Some(EventContent::Blob { blob_id }) = event.set_content.as_ref() {
        refs.push(blob_id.clone());
    }

    if let Some(metadata) = event.set_metadata.as_ref() {
        match event.event_type.as_str() {
            "POST::CREATE" | "POST::SET_METADATA" | "POST::SET_CONTENT" => {
                push_optional_blob_id(
                    &mut refs,
                    metadata.get("body_markdown_blob_id"),
                    "body_markdown_blob_id",
                )?;
            }
            "POST::ASSET::CREATE" => {
                push_optional_blob_id(&mut refs, metadata.get("blob_id"), "blob_id")?;
            }
            _ => {}
        }
        push_mailcal_blob_references(&mut refs, event.event_type.as_str(), metadata)?;
    }

    refs.sort();
    refs.dedup();
    Ok(refs)
}

// Item compatibility schemas remain item-only. Attached refs carry first-class
// content directly; their arbitrary metadata is not a blob reference schema.
pub fn commit_event_blob_references(row: &CommitEventRow) -> Result<Vec<String>, ReferenceError> {
    match row {
        CommitEventRow::Item(row) => {
            let event = serde_json::from_slice::<EventProposal>(&row.payload)
                .map_err(|error| format!("invalid item event payload: {error}"))?;
            event_blob_references(&event)
        }
        CommitEventRow::Ref(row) => {
            let event = serde_json::from_slice::<RefEventProposal>(&row.payload)
                .map_err(|error| format!("invalid ref event payload: {error}"))?;
            Ok(match event.set_content {
                Some(EventContent::Blob { blob_id }) => vec![blob_id],
                _ => Vec::new(),
            })
        }
    }
}

fn typed_blobs(raw: Vec<String>) -> Result<Vec<BlobId>, ReferenceError> {
    let mut blobs = raw
        .into_iter()
        .map(|id| {
            BlobId::parse(&id).map_err(|error| {
                ReferenceError::Invalid(format!("invalid blob reference {id}: {error}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    blobs.sort();
    blobs.dedup();
    Ok(blobs)
}

/// Enumerate canonical object references without resolving or rewriting content.
/// Known body families validate the existing inline/reference representation.
pub fn event_references(event: &EventProposal) -> Result<HistoryReferences, ReferenceError> {
    let blobs = typed_blobs(event_blob_references(event)?)?;
    let contents = if body_events::body_key(&event.event_type).is_some() {
        body_events::inline_body(event)?;
        body_events::reference_ids(std::slice::from_ref(event))?
    } else {
        match event.set_content {
            Some(EventContent::Content { content_id }) => vec![content_id],
            _ => Vec::new(),
        }
    };
    Ok(HistoryReferences { blobs, contents })
}

/// Attached refs have direct canonical content; arbitrary metadata adds no roots.
pub fn ref_event_references(event: &RefEventProposal) -> Result<HistoryReferences, ReferenceError> {
    let mut references = HistoryReferences::default();
    match &event.set_content {
        Some(EventContent::Blob { blob_id }) => {
            references.blobs = typed_blobs(vec![blob_id.clone()])?;
        }
        Some(EventContent::Content { content_id }) => references.contents.push(*content_id),
        _ => {}
    }
    Ok(references)
}

pub fn commit_event_references(row: &CommitEventRow) -> Result<HistoryReferences, ReferenceError> {
    match row {
        CommitEventRow::Item(row) => {
            let event = serde_json::from_slice::<EventProposal>(&row.payload)
                .map_err(|error| format!("invalid item event payload: {error}"))?;
            event_references(&event)
        }
        CommitEventRow::Ref(row) => {
            let event = serde_json::from_slice::<RefEventProposal>(&row.payload)
                .map_err(|error| format!("invalid ref event payload: {error}"))?;
            ref_event_references(&event)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synck_vcs::types::ItemId;

    const A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const D: &str = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const E: &str = "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    fn event(event_type: &str, metadata: Value) -> EventProposal {
        EventProposal {
            item_id: Some(ItemId::new()),
            event_type: event_type.to_string(),
            set_metadata: Some(metadata),
            set_content: None,
            create_item: None,
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: None,
        }
    }

    #[test]
    fn task_snapshot_references_include_every_part_and_reject_malformed_descriptors() {
        let blob = |id: &str| serde_json::json!({"blob_id":id,"size_bytes":10});
        let source = serde_json::json!({"state":"saved","generation":1,"identity":{"kind":"personal","owner_id":"alice","account_id":uuid::Uuid::new_v4(),"message_id":"m"},
            "snapshot":{"captured_at":"2026-09-04T12:00:00Z","raw_rfc822":blob(A),"subject":null,"internet_message_id":null,
            "from":null,"sender":null,"to":[],"cc":[],"bcc":[],"reply_to":[],"sent_at":null,"received_at":null,
            "body_markdown":blob(B),"body_html":blob(C),"attachments":[{"attachment_id":"1","filename":"photo.png","mime_type":"image/png","content":blob(D),"is_inline":true,"content_id":"photo"}]}});
        let mut captured = event(
            "TASK::CAPTURE_EMAIL",
            serde_json::json!({"task_list_id":null,"parent_task_id":null,"meta":{"email_source":source}}),
        );
        captured.set_content = Some(EventContent::Blob { blob_id: A.into() });
        captured.base_seq = Some(1);
        assert_eq!(event_blob_references(&captured).unwrap(), vec![A, B, C, D]);
        for family in ["TASK::CREATE", "TASK::IMPORT"] {
            let mut initial = captured.clone();
            initial.event_type = family.into();
            initial.base_seq = None;
            initial.create_item = Some(serde_json::from_value(serde_json::json!({
                "item_type":"TASK", "name":crate::tasks::history::internal_name(initial.item_id.as_ref().unwrap())
            })).unwrap());
            assert_eq!(event_blob_references(&initial).unwrap(), vec![A, B, C, D]);
        }
        captured.set_metadata.as_mut().unwrap()["meta"]["email_source"]["snapshot"]["attachments"]
            [0]["content"]["blob_id"] = serde_json::json!("malformed");
        assert!(event_blob_references(&captured).is_err());
        captured.set_metadata = None;
        assert!(event_blob_references(&captured).is_err());
    }

    #[test]
    fn provider_mailcal_references_are_explicit_and_deduplicated() {
        let event = event(
            "EMAIL::SET_METADATA",
            serde_json::json!({
                "mailcal": {
                    "v": 1,
                    "body": { "blob_id": A },
                    "body_html_blob_id": B,
                    "raw_rfc822_blob_id": C,
                    "provider_raw_json_blob_id": null,
                    "inline_images": [
                        { "blob_id": D },
                        { "blob_id": D },
                        { "content_id": "no-blob" }
                    ],
                    "unrelated": { "blob_id": E }
                }
            }),
        );

        assert_eq!(event_blob_references(&event).unwrap(), vec![A, B, C, D]);
    }

    #[test]
    fn imported_mailcal_references_include_exact_source() {
        let event = event(
            "EMAIL::CREATE",
            serde_json::json!({
                "mailcal": {
                    "v": 2,
                    "source": { "blob_id": A },
                    "body": { "blob_id": B },
                    "body_html_blob_id": C,
                    "raw_rfc822_blob_id": D,
                    "provider_raw_json_blob_id": E
                }
            }),
        );

        assert_eq!(event_blob_references(&event).unwrap(), vec![A, B, C, D, E]);
    }

    #[test]
    fn unknown_or_malformed_mailcal_metadata_fails_closed() {
        let unknown = event(
            "EMAIL::SET_METADATA",
            serde_json::json!({ "mailcal": { "v": 3, "body": { "blob_id": A } } }),
        );
        assert_eq!(
            event_blob_references(&unknown).unwrap_err().to_string(),
            "unsupported mailcal.v 3"
        );

        let malformed = event(
            "EMAIL::SET_METADATA",
            serde_json::json!({ "mailcal": { "v": 1, "inline_images": {} } }),
        );
        assert_eq!(
            event_blob_references(&malformed).unwrap_err().to_string(),
            "mailcal.inline_images must be an array or null"
        );
    }

    #[test]
    fn unrelated_metadata_blob_ids_are_not_durable_references() {
        let event = event(
            "FILE::SET_METADATA",
            serde_json::json!({ "blob_id": A, "mailcal": { "v": 2, "source": { "blob_id": B } } }),
        );
        assert!(event_blob_references(&event).unwrap().is_empty());
    }

    #[test]
    fn canonical_blob_references_cover_content_versions_and_attachment_families() {
        for family in [
            "FILE::CREATE",
            "FILE::SET_CONTENT",
            "FILE::VERSION::CREATE",
            "FILE_VERSION_SET::VERSION_CREATE",
            "CHAT::MESSAGE.ATTACHMENT::CREATE",
            "CHAT::MESSAGE.ATTACHMENT::SET_CONTENT",
            "CHAT::FILE::CREATE",
            "CHAT::FILE::SET_CONTENT",
            "EMAIL::CREATE",
            "EMAIL::ATTACHMENT::CREATE",
            "POST::ASSET::CREATE",
            "TABLE::RECORD::CREATE",
            "CUSTOM::SNAPSHOT",
        ] {
            let mut event = event(
                family,
                serde_json::json!({
                    "draft_blob_id":B, "ocr":{"blob_id":C}, "blob_id":D,
                }),
            );
            // POST assets explicitly retain their legacy blob_id metadata.
            if family == "POST::ASSET::CREATE" {
                event.set_metadata = Some(serde_json::json!({"asset_id":"asset"}));
            }
            event.set_content = Some(EventContent::Blob { blob_id: A.into() });
            assert_eq!(
                event_references(&event).unwrap(),
                HistoryReferences {
                    blobs: vec![BlobId::parse(A).unwrap()],
                    contents: vec![],
                },
                "{family}"
            );
        }
    }

    #[test]
    fn body_references_are_distinct_and_known_body_shapes_are_validated() {
        let id = ContentId::parse(A).unwrap();
        for family in [
            "CHAT::MESSAGE::CREATE",
            "CHAT::MESSAGE::SET_CONTENT",
            "POST::COMMENT::CREATE",
            "TASK_LIST::TASK.COMMENT::CREATE",
            "TASK::COMMENT::CREATE",
            "TABLE::RECORD.COMMENT::CREATE",
            "TABLE::RECORD.COMMENT::SET_CONTENT",
            "CALENDAR_EVENT::SET_DESCRIPTION",
            "CALENDAR_EVENT::SET_OCCURRENCE_DESCRIPTION",
        ] {
            let mut event = event(family, serde_json::json!({"blob_id":B}));
            event.set_content = Some(EventContent::Content { content_id: id });
            assert_eq!(
                event_references(&event).unwrap(),
                HistoryReferences {
                    blobs: vec![],
                    contents: vec![id],
                },
                "{family}"
            );
            event.set_content = Some(EventContent::Json {
                json: serde_json::json!({"body":7}),
            });
            assert!(event_references(&event).is_err(), "{family}");
        }
        let mut unknown = event("FUTURE::BODY", serde_json::json!({"blob_id":B}));
        unknown.set_content = Some(EventContent::Content { content_id: id });
        assert_eq!(event_references(&unknown).unwrap().contents, vec![id]);
        assert!(event_references(&unknown).unwrap().blobs.is_empty());
    }

    fn item_row(event: &EventProposal) -> CommitEventRow {
        CommitEventRow::Item(synck_vcs::types::EventRow {
            repo_id: synck_vcs::types::RepoId::new(),
            item_id: event.item_id.clone().unwrap(),
            item_seq: 1,
            event_type: event.event_type.clone(),
            commit_id: synck_vcs::types::CommitId::parse(A).unwrap(),
            commit_event_idx: 0,
            repo_seq: Some(1),
            payload: serde_json::to_vec(event).unwrap(),
            event_payload_digest: synck_vcs::types::Sha256Digest::parse(B).unwrap(),
            created_at: chrono::Utc::now(),
            parent_folder_id: None,
            parent_folder_id_before: None,
        })
    }

    fn ref_row(event: &RefEventProposal) -> CommitEventRow {
        CommitEventRow::Ref(synck_vcs::types::RefEventRow {
            repo_id: synck_vcs::types::RepoId::new(),
            owner_item_id: event.owner_item_id.clone(),
            ref_seq: 1,
            event_type: event.event_type.clone(),
            commit_id: synck_vcs::types::CommitId::parse(A).unwrap(),
            commit_event_idx: 0,
            repo_seq: Some(1),
            payload: serde_json::to_vec(event).unwrap(),
            event_payload_digest: synck_vcs::types::Sha256Digest::parse(B).unwrap(),
            created_at: chrono::Utc::now(),
        })
    }

    #[test]
    fn deleted_item_and_ref_history_retain_previous_objects() {
        let mut created = event("FILE::CREATE", Value::Null);
        created.set_content = Some(EventContent::Blob { blob_id: A.into() });
        let mut deleted = event("FILE::TOMBSTONE", Value::Null);
        deleted.item_id = created.item_id.clone();
        deleted.set_tombstoned = Some(true);
        let historical = commit_event_references(&item_row(&created)).unwrap();
        assert_eq!(historical.blobs, vec![BlobId::parse(A).unwrap()]);
        assert!(
            commit_event_references(&item_row(&deleted))
                .unwrap()
                .blobs
                .is_empty()
        );

        let mut version = RefEventProposal {
            owner_item_id: created.item_id.unwrap(),
            event_type: "FILE_VERSION::CREATE".into(),
            set_metadata: Some(
                serde_json::json!({"blob_id":B, "mailcal":{"v":2,"source":{"blob_id":C}}}),
            ),
            set_content: Some(EventContent::Blob { blob_id: D.into() }),
            base_ref_seq: 0,
            initial_ref_seq: None,
        };
        assert_eq!(
            commit_event_references(&ref_row(&version)).unwrap().blobs,
            vec![BlobId::parse(D).unwrap()]
        );
        assert_eq!(
            commit_event_blob_references(&ref_row(&version)).unwrap(),
            vec![D]
        );
        version.event_type = "FILE_VERSION::TOMBSTONE".into();
        version.set_content = None;
        assert!(
            commit_event_references(&ref_row(&version))
                .unwrap()
                .blobs
                .is_empty()
        );
        version.event_type = "FUTURE_REF::BODY".into();
        version.set_content = Some(EventContent::Content {
            content_id: ContentId::parse(E).unwrap(),
        });
        let refs = commit_event_references(&ref_row(&version)).unwrap();
        assert_eq!(refs.contents, vec![ContentId::parse(E).unwrap()]);
        assert!(refs.blobs.is_empty());
    }

    #[test]
    fn typed_scanning_rejects_invalid_legacy_ids_and_malformed_rows() {
        let legacy = event(
            "POST::SET_METADATA",
            serde_json::json!({"body_markdown_blob_id":"invalid"}),
        );
        assert_eq!(event_blob_references(&legacy).unwrap(), vec!["invalid"]);
        assert!(event_references(&legacy).is_err());
        let mut row = item_row(&legacy);
        let CommitEventRow::Item(item) = &mut row else {
            unreachable!()
        };
        item.payload = vec![0xff];
        assert!(
            commit_event_references(&row)
                .unwrap_err()
                .to_string()
                .starts_with("invalid item event payload:")
        );
        let mut reference = ref_row(&RefEventProposal {
            owner_item_id: ItemId::new(),
            event_type: "FILE_VERSION::CREATE".into(),
            set_metadata: None,
            set_content: None,
            base_ref_seq: 0,
            initial_ref_seq: None,
        });
        let CommitEventRow::Ref(row) = &mut reference else {
            unreachable!()
        };
        row.payload = vec![0xff];
        assert!(
            commit_event_references(&reference)
                .unwrap_err()
                .to_string()
                .starts_with("invalid ref event payload:")
        );
    }

    #[test]
    fn legacy_post_creation_retains_the_same_body_field_as_later_updates() {
        for family in ["POST::CREATE", "POST::SET_METADATA", "POST::SET_CONTENT"] {
            let mut post = event(family, serde_json::json!({"body_markdown_blob_id":A}));
            assert_eq!(event_blob_references(&post).unwrap(), vec![A], "{family}");
            assert_eq!(
                event_references(&post).unwrap().blobs,
                vec![BlobId::parse(A).unwrap()],
                "{family}"
            );
            post.set_content = Some(EventContent::Blob { blob_id: B.into() });
            assert_eq!(event_blob_references(&post).unwrap(), vec![A, B]);
            post.set_metadata = Some(serde_json::json!({"body_markdown_blob_id":"invalid"}));
            assert_eq!(event_blob_references(&post).unwrap(), vec!["invalid", B]);
            assert!(event_references(&post).is_err());
        }
    }
}
