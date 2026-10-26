//! Product fields derived solely from canonical VCS item events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use synck_vcs::types::{BlobId, CommitId, EventContent, EventProposal, ItemId, LifecycleState};

use crate::metadata::{
    ITEM_SOURCE_MODIFIED_AT_REPAIR_EVENT_TYPE, ITEM_SUBTYPE_STANDARD,
    ITEM_TREE_HIDDEN_METADATA_KEY, item_color_metadata_from_json,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplicaItemProjection {
    pub item_id: ItemId,
    pub item_type: String,
    pub item_subtype: String,
    pub name: String,
    pub parent_folder_id: Option<ItemId>,
    pub lifecycle_state: LifecycleState,
    pub metadata_json: serde_json::Value,
    pub color_metadata: Option<String>,
    pub content_blob_id: Option<String>,
    pub content_item_seq: Option<i64>,
    pub size: Option<i64>,
    pub latest_item_seq: i64,
    pub last_updated_at: DateTime<Utc>,
    pub last_updated_by: String,
    pub item_date_at: DateTime<Utc>,
    pub item_date_kind: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReplicaProjectionError {
    #[error("event for unknown item {0}")]
    MissingItem(String),
    #[error("create event for existing item {0}")]
    DuplicateItem(String),
    #[error("invalid item projection: {0}")]
    Invalid(String),
}

pub fn apply_replica_item_event(
    current: Option<ReplicaItemProjection>,
    item_id: ItemId,
    item_seq: i64,
    proposal: &EventProposal,
    updated_at: DateTime<Utc>,
    updated_by: &str,
) -> Result<ReplicaItemProjection, ReplicaProjectionError> {
    if item_seq < 1 {
        return Err(ReplicaProjectionError::Invalid(
            "item sequence must be positive".to_string(),
        ));
    }

    let is_create = current.is_none();
    let mut item = match (current, proposal.create_item.as_ref()) {
        (Some(_), Some(_)) => {
            return Err(ReplicaProjectionError::DuplicateItem(item_id.to_string()));
        }
        (None, None) => {
            return Err(ReplicaProjectionError::MissingItem(item_id.to_string()));
        }
        (Some(item), None) => item,
        (None, Some(create)) => ReplicaItemProjection {
            item_id: item_id.clone(),
            item_type: create.item_type.clone(),
            item_subtype: ITEM_SUBTYPE_STANDARD.to_string(),
            name: create.name.clone(),
            parent_folder_id: create.parent_folder_id.clone(),
            lifecycle_state: LifecycleState::Live,
            metadata_json: serde_json::json!({}),
            color_metadata: None,
            content_blob_id: None,
            content_item_seq: None,
            size: None,
            latest_item_seq: item_seq,
            last_updated_at: updated_at,
            last_updated_by: updated_by.to_string(),
            item_date_at: updated_at,
            item_date_kind: "LAST_UPDATED".to_string(),
        },
    };

    if item.item_id != item_id || (!is_create && item_seq != item.latest_item_seq + 1) {
        return Err(ReplicaProjectionError::Invalid(format!(
            "non-contiguous item sequence for {item_id}"
        )));
    }
    if let Some(parent) = proposal.set_parent.as_ref() {
        item.parent_folder_id.clone_from(parent);
    }
    if let Some(name) = proposal.set_name.as_ref() {
        item.name.clone_from(name);
    }
    if let Some(tombstoned) = proposal.set_tombstoned {
        item.lifecycle_state = if tombstoned {
            LifecycleState::Tombstoned
        } else {
            LifecycleState::Live
        };
    }
    if let Some(in_trash) = proposal.set_in_trash {
        if in_trash && item.lifecycle_state != LifecycleState::Tombstoned {
            item.lifecycle_state = LifecycleState::Trash;
        } else if !in_trash && item.lifecycle_state == LifecycleState::Trash {
            item.lifecycle_state = LifecycleState::Live;
        }
    }
    let item_level_metadata = proposal.event_type.matches("::").count() == 1
        && matches!(
            proposal.event_type.split_once("::").map(|(_, verb)| verb),
            Some("CREATE" | "SET_METADATA")
        );
    if item_level_metadata && let Some(metadata) = proposal.set_metadata.as_ref() {
        item.metadata_json = metadata.clone();
        item.item_subtype = item_subtype(metadata)?;
        item.color_metadata = item_color_metadata_from_json(metadata);
    }
    if proposal.event_type == "CHAT::SET_PROFILE"
        && proposal
            .set_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("chat_kind"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| kind.eq_ignore_ascii_case("DIRECT"))
    {
        item.metadata_json
            .as_object_mut()
            .ok_or_else(|| {
                ReplicaProjectionError::Invalid("item metadata must be an object".to_string())
            })?
            .insert(
                ITEM_TREE_HIDDEN_METADATA_KEY.to_string(),
                serde_json::Value::Bool(true),
            );
    }
    // Historical imports may embed the initial FILE blob in CREATE. Current
    // writers still emit a separate SET_CONTENT under the FILE/FOLDER profile.
    if matches!(
        proposal.event_type.as_str(),
        "FILE::CREATE" | "FILE::SET_CONTENT"
    ) && let Some(EventContent::Blob { blob_id }) = proposal.set_content.as_ref()
    {
        let previous_blob = item.content_blob_id.as_deref();
        let explicit_size = proposal
            .set_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("content"))
            .and_then(|content| content.get("size"))
            .and_then(serde_json::Value::as_i64)
            .filter(|size| *size >= 0);
        if explicit_size.is_none() && previous_blob != Some(blob_id.as_str()) {
            item.size = None;
        } else if explicit_size.is_some() {
            item.size = explicit_size;
        }
        item.content_blob_id = Some(blob_id.clone());
        item.content_item_seq = Some(item_seq);
    } else if proposal.event_type == "FILE::VERSION::CREATE"
        && let Some(size) = content_size(proposal.set_metadata.as_ref())
    {
        // Legacy version-create events projected the current size but never moved
        // the item's content head. Preserve that cloud-projection contract.
        item.size = Some(size);
    }
    item.latest_item_seq = item_seq;
    item.last_updated_at = projected_updated_at(proposal, updated_at)?;
    item.last_updated_by = updated_by.to_string();
    (item.item_date_at, item.item_date_kind) =
        resolve_item_date(&item.item_type, &item.metadata_json, item.last_updated_at);
    Ok(item)
}

pub fn resolve_item_date(
    item_type: &str,
    metadata: &serde_json::Value,
    last_updated_at: DateTime<Utc>,
) -> (DateTime<Utc>, String) {
    if item_type == "EMAIL" {
        for (field, kind) in [("received_at", "EMAIL_RECEIVED"), ("sent_at", "EMAIL_SENT")] {
            if let Some(timestamp) = metadata
                .get("mailcal")
                .and_then(|mailcal| mailcal.get(field))
                .and_then(serde_json::Value::as_str)
                .and_then(|raw| DateTime::parse_from_rfc3339(raw.trim()).ok())
            {
                return (timestamp.with_timezone(&Utc), kind.to_string());
            }
        }
        return (last_updated_at, "LAST_UPDATED_FALLBACK".to_string());
    }
    (last_updated_at, "LAST_UPDATED".to_string())
}

fn content_size(metadata: Option<&serde_json::Value>) -> Option<i64> {
    metadata
        .and_then(|metadata| metadata.get("content"))
        .and_then(|content| content.get("size"))
        .and_then(serde_json::Value::as_i64)
        .filter(|size| *size >= 0)
}

fn projected_updated_at(
    proposal: &EventProposal,
    default: DateTime<Utc>,
) -> Result<DateTime<Utc>, ReplicaProjectionError> {
    if proposal.event_type != ITEM_SOURCE_MODIFIED_AT_REPAIR_EVENT_TYPE {
        return Ok(default);
    }
    let metadata = proposal
        .set_metadata
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| invalid_repair("set_metadata must be an object"))?;
    const KEYS: [&str; 3] = [
        "source_modified_at",
        "manifest_sha256",
        "expected_prior_commit_id",
    ];
    if metadata.len() != KEYS.len() || metadata.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(invalid_repair(
            "set_metadata must contain exactly source_modified_at, manifest_sha256, and expected_prior_commit_id",
        ));
    }
    let required = |key: &str| {
        metadata
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid_repair(&format!("metadata.{key} must be a non-empty string")))
    };
    let timestamp = DateTime::parse_from_rfc3339(required("source_modified_at")?)
        .map_err(|error| invalid_repair(&format!("source_modified_at is invalid: {error}")))?
        .with_timezone(&Utc);
    BlobId::parse(required("manifest_sha256")?)
        .map_err(|error| invalid_repair(&format!("manifest_sha256 is invalid: {error}")))?;
    CommitId::parse(required("expected_prior_commit_id")?).map_err(|error| {
        invalid_repair(&format!("expected_prior_commit_id is invalid: {error}"))
    })?;
    Ok(timestamp)
}

fn invalid_repair(message: &str) -> ReplicaProjectionError {
    ReplicaProjectionError::Invalid(format!(
        "{ITEM_SOURCE_MODIFIED_AT_REPAIR_EVENT_TYPE} {message}"
    ))
}

fn item_subtype(metadata: &serde_json::Value) -> Result<String, ReplicaProjectionError> {
    let Some(value) = metadata.get("subtype") else {
        return Ok(ITEM_SUBTYPE_STANDARD.to_string());
    };
    if value.is_null() {
        return Ok(ITEM_SUBTYPE_STANDARD.to_string());
    }
    let subtype = value.as_str().ok_or_else(|| {
        ReplicaProjectionError::Invalid("metadata.subtype must be a string".to_string())
    })?;
    let mut chars = subtype.chars();
    if !chars.next().is_some_and(|ch| ch.is_ascii_uppercase())
        || !chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Err(ReplicaProjectionError::Invalid(format!(
            "metadata.subtype {subtype:?} is invalid"
        )));
    }
    Ok(subtype.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use synck_vcs::types::CreateItem;

    #[test]
    fn create_and_content_project_exact_navigation_fields() {
        let item_id = ItemId::new();
        let now = Utc::now();
        let create = EventProposal {
            item_id: Some(item_id.clone()),
            event_type: "FILE::CREATE".to_string(),
            set_metadata: Some(serde_json::json!({"subtype":"STANDARD","color_metadata":"blue"})),
            set_content: None,
            create_item: Some(CreateItem {
                item_type: "FILE".to_string(),
                parent_folder_id: None,
                name: "Report.PDF".to_string(),
            }),
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: None,
        };
        let created = apply_replica_item_event(None, item_id.clone(), 1, &create, now, "actor")
            .expect("project create");
        assert_eq!(created.color_metadata.as_deref(), Some("blue"));

        let mut content = create.clone();
        content.create_item = None;
        content.event_type = "FILE::SET_CONTENT".to_string();
        content.set_metadata = Some(serde_json::json!({"content":{"size":42}}));
        content.set_content = Some(EventContent::Blob {
            blob_id: format!("sha256:{}", "ab".repeat(32)),
        });
        content.base_seq = Some(1);
        let projected = apply_replica_item_event(Some(created), item_id, 2, &content, now, "actor")
            .expect("project content");
        assert_eq!(projected.size, Some(42));
        assert_eq!(projected.content_item_seq, Some(2));
    }

    #[test]
    fn direct_chat_profile_is_tree_hidden_without_create_metadata() {
        let item_id = ItemId::new();
        let now = Utc::now();
        let create = EventProposal {
            item_id: None,
            event_type: "CHAT::CREATE".to_string(),
            set_metadata: None,
            set_content: None,
            create_item: Some(CreateItem {
                item_type: "CHAT".to_string(),
                parent_folder_id: None,
                name: "direct-chat-opaque".to_string(),
            }),
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: None,
        };
        let created = apply_replica_item_event(None, item_id.clone(), 1, &create, now, "actor")
            .expect("project CHAT create");
        assert_eq!(created.metadata_json, serde_json::json!({}));

        let profile = EventProposal {
            item_id: None,
            event_type: "CHAT::SET_PROFILE".to_string(),
            set_metadata: Some(serde_json::json!({"chat_kind": "DIRECT"})),
            set_content: None,
            create_item: None,
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: None,
        };
        let projected = apply_replica_item_event(Some(created), item_id, 2, &profile, now, "actor")
            .expect("project DIRECT profile");
        assert_eq!(
            projected.metadata_json[ITEM_TREE_HIDDEN_METADATA_KEY],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn version_blob_does_not_replace_the_current_content_head() {
        let item_id = ItemId::new();
        let now = Utc::now();
        let current = ReplicaItemProjection {
            item_id: item_id.clone(),
            item_type: "FILE".to_string(),
            item_subtype: ITEM_SUBTYPE_STANDARD.to_string(),
            name: "report.pdf".to_string(),
            parent_folder_id: None,
            lifecycle_state: LifecycleState::Live,
            metadata_json: serde_json::json!({}),
            color_metadata: None,
            content_blob_id: Some(format!("sha256:{}", "ab".repeat(32))),
            content_item_seq: Some(1),
            size: Some(10),
            latest_item_seq: 1,
            last_updated_at: now,
            last_updated_by: "actor".to_string(),
            item_date_at: now,
            item_date_kind: "LAST_UPDATED".to_string(),
        };
        let event = EventProposal {
            item_id: Some(item_id.clone()),
            event_type: "FILE::VERSION::CREATE".to_string(),
            set_metadata: Some(serde_json::json!({"content":{"size":20}})),
            set_content: Some(EventContent::Blob {
                blob_id: format!("sha256:{}", "cd".repeat(32)),
            }),
            create_item: None,
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: Some(1),
        };
        let projected = apply_replica_item_event(Some(current), item_id, 2, &event, now, "actor")
            .expect("project version event");
        assert_eq!(
            projected.content_blob_id.as_deref(),
            Some(format!("sha256:{}", "ab".repeat(32)).as_str())
        );
        assert_eq!(projected.content_item_seq, Some(1));
        assert_eq!(projected.size, Some(20));
    }

    #[test]
    fn item_date_matches_email_then_last_updated_precedence() {
        let updated: DateTime<Utc> = "2026-08-24T10:11:12Z".parse().unwrap();
        let metadata = serde_json::json!({
            "mailcal": {
                "received_at": "2020-01-02T03:04:05+02:00",
                "sent_at": "2019-01-02T03:04:05Z"
            }
        });
        let (date, kind) = resolve_item_date("EMAIL", &metadata, updated);
        assert_eq!(date.to_rfc3339(), "2020-01-02T01:04:05+00:00");
        assert_eq!(kind, "EMAIL_RECEIVED");

        let (date, kind) = resolve_item_date("FILE", &metadata, updated);
        assert_eq!(date, updated);
        assert_eq!(kind, "LAST_UPDATED");
    }
}
