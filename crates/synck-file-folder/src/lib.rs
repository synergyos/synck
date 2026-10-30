//! Storage-free implementation of the public Synck FILE/FOLDER profile.
//!
//! This crate validates complete typed intents, materializes canonical Core
//! event proposals, and projects complete accepted commits. It deliberately
//! owns no storage, transport, authorization, or filesystem behavior.

mod error;
pub mod file_versions;
mod materialize;
mod model;
mod operation;
mod project;

pub use error::{SemanticError, SemanticErrorCode};
pub use materialize::{
    MaterializedProposal, validate_and_materialize, validate_and_materialize_with_content_revisions,
};
pub use model::{
    BlobRef, ChangeGroup, CurrentItem, FileFolderKind, ImmutableItemKind, Intent, IntentProposal,
    ItemBase, ItemChange, ItemHead, Location, ProfileLifecycleState,
};
pub use operation::{canonical_operation_bytes, compute_operation_id, normalize_intents};
pub use project::{
    validate_projected_proposal, project_commit, project_commit_events, project_proposal_events,
};

/// Public profile identifier.
pub const PROFILE_ID: &str = "synck-file-folder-v1";

pub use synck_vcs::limits::{MAX_MESSAGE_BYTES, MAX_EVENT_PAYLOAD_BYTES};
pub const MAX_SAFE_JSON_INTEGER: u64 = 9_007_199_254_740_991;

/// Parse the durable bytes referenced by one canonical FILE content event.
///
/// Non-content profile events return `Ok(None)`. A malformed content event is
/// rejected; callers must never guess a size from the blob store.
/// Whether this FILE event states the file's bytes. A file is created with them.
pub fn is_file_content_event(event_type: &str) -> bool {
    matches!(event_type, "FILE::CREATE" | "FILE::SET_CONTENT")
}

pub fn file_blob_reference(
    event: &synck_vcs::types::EventProposal,
) -> Result<Option<BlobRef>, SemanticError> {
    if !is_file_content_event(&event.event_type) {
        return Ok(None);
    }
    let blob_id = event.set_content.as_ref().and_then(|content| content.only("bytes")).ok_or_else(|| {
            SemanticError::new(
                SemanticErrorCode::MalformedProfileEvent,
                "FILE content event lacks a blob",
            )
        })?;
    let size = event
        .set_metadata
        .as_ref()
        .and_then(|metadata| metadata.get("content"))
        .and_then(|content| content.get("size"))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            SemanticError::new(
                SemanticErrorCode::MalformedProfileEvent,
                "FILE content event lacks an exact size",
            )
        })?;
    if size > MAX_SAFE_JSON_INTEGER {
        return Err(SemanticError::new(
            SemanticErrorCode::InvalidContent,
            "FILE content size exceeds the exact JSON integer range",
        ));
    }
    Ok(Some(BlobRef {
        blob_id: synck_vcs::types::ObjectId::parse(blob_id.as_str()).map_err(|_| {
            SemanticError::new(
                SemanticErrorCode::MalformedProfileEvent,
                "FILE content event has an invalid blob ID",
            )
        })?,
        size,
    }))
}

/// Returns whether `event_type` is part of the public FILE/FOLDER event profile.
///
/// Product-only events such as metadata, federation and revisions are
/// deliberately excluded. Tombstones are included because
/// authorities may publish them even though v1 clients cannot request them.
pub fn is_profile_event_type(kind: FileFolderKind, event_type: &str) -> bool {
    match kind {
        FileFolderKind::File => matches!(
            event_type,
            "FILE::CREATE"
                | "FILE::SET_NAME"
                | "FILE::SET_PARENT"
                | "FILE::SET_CONTENT"
                | "FILE::TRASH"
                | "FILE::RESTORE"
                | "FILE::TOMBSTONE"
        ),
        FileFolderKind::Folder => matches!(
            event_type,
            "FOLDER::CREATE"
                | "FOLDER::SET_NAME"
                | "FOLDER::SET_PARENT"
                | "FOLDER::TRASH"
                | "FOLDER::RESTORE"
                | "FOLDER::TOMBSTONE"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{FileFolderKind, file_blob_reference, is_profile_event_type};
    use synck_vcs::types::EventProposal;

    #[test]
    fn profile_event_policy_is_exact() {
        for event_type in [
            "FILE::CREATE",
            "FILE::SET_NAME",
            "FILE::SET_PARENT",
            "FILE::SET_CONTENT",
            "FILE::TRASH",
            "FILE::RESTORE",
            "FILE::TOMBSTONE",
        ] {
            assert!(is_profile_event_type(FileFolderKind::File, event_type));
        }
        for event_type in [
            "FOLDER::CREATE",
            "FOLDER::SET_NAME",
            "FOLDER::SET_PARENT",
            "FOLDER::TRASH",
            "FOLDER::RESTORE",
            "FOLDER::TOMBSTONE",
        ] {
            assert!(is_profile_event_type(FileFolderKind::Folder, event_type));
        }
        for (kind, event_type) in [
            (FileFolderKind::File, "FILE::MOVE"),
            (FileFolderKind::File, "FILE::SET_METADATA"),
            (FileFolderKind::Folder, "FOLDER::MOVE"),
            (FileFolderKind::Folder, "FILE::CREATE"),
        ] {
            assert!(!is_profile_event_type(kind, event_type));
        }
    }

    #[test]
    fn historical_file_reference_requires_an_exact_blob_and_size() {
        let event: EventProposal = EventProposal::from_fields(serde_json::json!({
            "item_id": "01900000-0000-7000-8000-000000000001",
            "event_type": "FILE::SET_CONTENT",
            "set_metadata": {"content": {"size": 7}},
            "set_content": {"bytes": format!("sha256:{}", "a".repeat(64))},
            "base_seq": 1
        }))
        .unwrap();
        let reference = file_blob_reference(&event).unwrap().unwrap();
        assert_eq!(reference.size, 7);
        assert_eq!(
            reference.blob_id.as_str(),
            format!("sha256:{}", "a".repeat(64))
        );

        let mut malformed = event.clone();
        malformed.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(serde_json::json!({"content": {"size": -1}})).unwrap());
        assert!(file_blob_reference(&malformed).is_err());

        let mut unrelated = event;
        unrelated.event_type = "FILE::SET_NAME".to_string();
        assert!(file_blob_reference(&unrelated).unwrap().is_none());
    }
}
