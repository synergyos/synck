use serde_json::{Value, json};
use synck_local::{
    Change, ChangeKind, CommitEvent, DeletedEntry, DetailedEventLogEntry, DetailedLogEntry,
    EventHistoryCommit, HistoryCommit, LocalError, LogEntry, NamedVersionEntry, TextDiffEntry,
    TextDiffKind, VersionEntry,
};

pub(crate) const SCHEMA_VERSION: u32 = 5;

pub(crate) fn change(value: &Change) -> Value {
    json!({
        "kind": match value.kind {
            ChangeKind::Add => "ADD",
            ChangeKind::Modify => "MODIFY",
            ChangeKind::Rename => "RENAME",
            ChangeKind::Move => "MOVE",
            ChangeKind::MoveRename => "MOVE_RENAME",
            ChangeKind::Delete => "DELETE",
        },
        "path": value.path,
        "source_path": value.source_path,
        "content_changed": value.content_changed,
    })
}

pub(crate) fn text_diff_entry(value: &TextDiffEntry) -> Value {
    let (kind, patch) = match &value.kind {
        TextDiffKind::Patch(patch) => ("PATCH", Some(patch)),
        TextDiffKind::Binary => ("BINARY", None),
        TextDiffKind::TooLarge => ("TOO_LARGE", None),
    };
    json!({
        "old_path": value.old_path,
        "new_path": value.new_path,
        "kind": kind,
        "patch": patch,
    })
}

pub(crate) fn detailed_log_entry(value: &DetailedLogEntry) -> Value {
    let commit = &value.commit;
    json!({
        "repo_seq": commit.repo_seq,
        "commit_id": commit.commit_id,
        "created_at": commit.created_at.to_rfc3339(),
        "author": commit.author,
        "message": commit.message,
        "changes": value.changes.iter().map(change).collect::<Vec<_>>(),
        "changes_total": value.changes_total,
        "changes_shown": value.changes.len(),
        "changes_truncated": value.changes.len() < value.changes_total,
    })
}

pub(crate) fn commit_event(value: &CommitEvent) -> Value {
    let payload: Value = serde_json::from_str(&value.payload)
        .expect("validated canonical event payload is valid JSON");
    json!({
        "repo_id": value.repo_id,
        "commit_id": value.commit_id,
        "repo_seq": value.repo_seq,
        "commit_event_idx": value.commit_event_idx,
        "item_id": value.item_id,
        "item_seq": value.item_seq,
        "event_type": value.event_type,
        "payload": payload,
        "event_payload_digest": value.event_payload_digest,
        "created_at": value.created_at.to_rfc3339(),
        "parent_folder_id": value.parent_folder_id,
        "parent_folder_id_before": value.parent_folder_id_before,
    })
}

pub(crate) fn detailed_event_log_entry(value: &DetailedEventLogEntry) -> Value {
    let mut output = detailed_log_entry(&value.entry);
    let object = output
        .as_object_mut()
        .expect("detailed log entry renders as an object");
    object.insert("event_count".to_string(), json!(value.events.len()));
    object.insert(
        "events_digest".to_string(),
        json!(value.entry.commit.events_digest),
    );
    object.insert(
        "events".to_string(),
        json!(value.events.iter().map(commit_event).collect::<Vec<_>>()),
    );
    output
}

pub(crate) fn log_metadata(value: &LogEntry) -> Value {
    json!({
        "repo_seq": value.repo_seq,
        "commit_id": value.commit_id,
        "created_at": value.created_at.to_rfc3339(),
        "author": value.author,
        "message": value.message,
    })
}

pub(crate) fn history_commit(value: &HistoryCommit) -> Value {
    json!({
        "repo_seq": value.repo_seq,
        "commit_id": value.commit_id,
        "created_at": value.created_at.to_rfc3339(),
        "author": value.author,
        "message": value.message,
        "changes": value.changes.iter().map(change).collect::<Vec<_>>(),
    })
}

pub(crate) fn event_history_commit(value: &EventHistoryCommit) -> Value {
    let mut output = history_commit(&value.commit);
    let object = output
        .as_object_mut()
        .expect("history commit renders as an object");
    object.insert("event_count".to_string(), json!(value.events.len()));
    object.insert(
        "events_digest".to_string(),
        json!(value.commit.events_digest),
    );
    object.insert(
        "events".to_string(),
        json!(value.events.iter().map(commit_event).collect::<Vec<_>>()),
    );
    output
}

pub(crate) fn deleted_entry(value: &DeletedEntry) -> Value {
    json!({
        "item_id": value.item_id,
        "item_type": value.item_type,
        "path": value.path,
        "repo_seq": value.repo_seq,
        "commit_id": value.commit_id,
        "deleted_at": value.deleted_at.to_rfc3339(),
    })
}

pub(crate) fn version_entry(value: &VersionEntry) -> Value {
    json!({
        "item_seq": value.item_seq,
        "commit_id": value.commit_id,
        "created_at": value.created_at.to_rfc3339(),
        "blob_id": value.blob_id,
        "size": value.size,
    })
}

pub(crate) fn named_version_entry(value: &NamedVersionEntry) -> Value {
    json!({
        "label": value.label,
        "version_id": value.version_id,
        "lifecycle_state": value.lifecycle_state,
        "kind": value.kind,
        "label_source": value.label_source,
        "description": value.description,
        "source_item_seq": value.source_item_seq,
        "version_set_seq": value.version_set_seq,
        "created_at": value.created_at.to_rfc3339(),
        "blob_id": value.blob_id,
        "size": value.size,
    })
}

pub(crate) fn error(value: &LocalError) -> Value {
    let kind = match value {
        LocalError::Busy => "BUSY",
        LocalError::Invalid(_) | LocalError::WorkingTree(_) => "INVALID",
        LocalError::Integrity(_) => "INTEGRITY",
        LocalError::MonitorUnavailable(_) => "MONITOR_UNAVAILABLE",
        LocalError::AcceptedNeedsRecovery { .. } => "ACCEPTED_NEEDS_RECOVERY",
        LocalError::Conflict { .. } => "CONFLICT",
        LocalError::MoveNeedsRecovery { .. } => "MOVE_NEEDS_RECOVERY",
        LocalError::PublishedNeedsDurability { .. } => "PUBLISHED_NEEDS_DURABILITY",
        LocalError::Io { .. } => "IO",
        LocalError::Sqlite(_) => "SQLITE",
        LocalError::Core(_) => "VCS",
        LocalError::FileFolder(_) => "FILE_FOLDER",
        LocalError::ConfigJson(_) => "CONFIG_JSON",
    };
    let mut error = json!({
        "kind": kind,
        "message": value.to_string(),
    });
    if let LocalError::Conflict {
        item_id,
        head_seq,
        blob_id,
        size,
        ..
    } = value
    {
        error["current"] = json!({
            "item_id": item_id,
            "head_seq": head_seq,
            "blob_id": blob_id,
            "size": size,
        });
    }
    json!({
        "schema_version": SCHEMA_VERSION,
        "error": error,
    })
}

#[cfg(test)]
mod tests {
    use synck_file_folder::{SemanticError, SemanticErrorCode};

    use super::*;

    #[test]
    fn file_folder_errors_keep_a_distinct_json_kind() {
        let value = error(&LocalError::FileFolder(SemanticError {
            code: SemanticErrorCode::NameConflict,
            detail: "conflict".to_string(),
        }));

        assert_eq!(value["error"]["kind"], "FILE_FOLDER");
        assert_eq!(
            value["error"]["message"],
            "FILE/FOLDER semantic error: NAME_CONFLICT: conflict"
        );
    }

    #[test]
    fn conflicts_carry_the_current_file_head() {
        let value = error(&LocalError::Conflict {
            item_id: "item".to_string(),
            head_seq: 4,
            blob_id: "sha256:current".to_string(),
            size: 7,
            expected: "sha256:base".to_string(),
        });

        assert_eq!(value["error"]["kind"], "CONFLICT");
        assert_eq!(
            value["error"]["current"],
            json!({"item_id":"item","head_seq":4,"blob_id":"sha256:current","size":7})
        );
    }

    #[test]
    fn published_output_has_a_distinct_json_kind() {
        let value = error(&LocalError::PublishedNeedsDurability {
            operation: "archive",
            path: std::path::PathBuf::from("archive"),
            detail: "injected sync failure".to_string(),
        });

        assert_eq!(value["error"]["kind"], "PUBLISHED_NEEDS_DURABILITY");
    }
}
