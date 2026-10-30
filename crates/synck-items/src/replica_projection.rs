//! Product fields derived solely from canonical VCS item events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use synck_vcs::metadata::MetadataRef;
use synck_vcs::types::{ObjectId, CommitId, EventProposal, ItemId, LifecycleState, is_record_type};

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
    pub content_roles: std::collections::BTreeMap<String, ObjectId>,
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
            name: create.name.clone().unwrap_or_default(),
            parent_folder_id: create.parent_folder_id.clone(),
            lifecycle_state: LifecycleState::Live,
            metadata_json: serde_json::json!({}),
            color_metadata: None,
            content_blob_id: None,
            content_roles: Default::default(),
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
    if item_level_metadata && let Some(metadata) = proposal.set_metadata.as_ref().and_then(synck_vcs::metadata::MetadataRef::value) {
        item.metadata_json = metadata.clone();
        item.item_subtype = item_subtype(metadata)?;
        item.color_metadata = item_color_metadata_from_json(metadata);
    }
    if proposal.event_type == "CHAT::SET_PROFILE"
        && proposal
            .set_metadata.as_ref().and_then(synck_vcs::metadata::MetadataRef::value)
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
    set_record_title(&mut item, proposal)?;
    if proposal.event_type == format!("{}::CREATE", item.item_type)
        || proposal.event_type == format!("{}::SET_CONTENT", item.item_type)
    {
        if let Some(content) = &proposal.set_content {
            item.content_roles = content.roles().clone();
        }
    }
    if matches!(
        proposal.event_type.as_str(),
        "FILE::CREATE" | "FILE::SET_CONTENT"
    ) && let Some(blob_id) = proposal.set_content.as_ref().and_then(|content| content.only("bytes"))
    {
        let previous_blob = item.content_blob_id.as_deref();
        let explicit_size = proposal
            .set_metadata.as_ref().and_then(synck_vcs::metadata::MetadataRef::value)
            .and_then(|metadata| metadata.get("content"))
            .and_then(|content| content.get("size"))
            .and_then(serde_json::Value::as_i64)
            .filter(|size| *size >= 0);
        if explicit_size.is_none() && previous_blob != Some(blob_id.as_str()) {
            item.size = None;
        } else if explicit_size.is_some() {
            item.size = explicit_size;
        }
        item.content_blob_id = Some(blob_id.to_string());
        item.content_item_seq = Some(item_seq);
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



fn projected_updated_at(
    proposal: &EventProposal,
    default: DateTime<Utc>,
) -> Result<DateTime<Utc>, ReplicaProjectionError> {
    if proposal.event_type != ITEM_SOURCE_MODIFIED_AT_REPAIR_EVENT_TYPE {
        return Ok(default);
    }
    let metadata = proposal
        .set_metadata.as_ref().and_then(synck_vcs::metadata::MetadataRef::value)
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
    ObjectId::parse(required("manifest_sha256")?)
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

const UNAVAILABLE_TITLE: &str = "(unavailable)";

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

/// A record has no name, so `name` carries its title. An email without one shows
/// its subject. That text stays here: it is display, never stored in history.
/// Erased metadata ends the search: an older title must not come back.
fn set_record_title(
    item: &mut ReplicaItemProjection,
    proposal: &EventProposal,
) -> Result<(), ReplicaProjectionError> {
    if !is_record_type(&item.item_type) {
        return Ok(());
    }
    let set_title = proposal.event_type == format!("{}::SET_TITLE", item.item_type);
    if !set_title && proposal.event_type != format!("{}::CREATE", item.item_type) {
        return Ok(());
    }
    if proposal.set_metadata.as_ref().is_some_and(|metadata| metadata.value().is_none()) {
        item.name = UNAVAILABLE_TITLE.to_string();
        return Ok(());
    }
    let metadata = proposal.set_metadata.as_ref().and_then(MetadataRef::value);
    let title = metadata
        .and_then(|metadata| metadata.get("title"))
        .and_then(serde_json::Value::as_str);
    match title {
        Some(title) if !title.is_empty() => item.name = title.to_string(),
        Some(_) => {
            return Err(ReplicaProjectionError::Invalid("a title is nonempty".to_string()));
        }
        None if set_title => {
            return Err(ReplicaProjectionError::Invalid("SET_TITLE has no title".to_string()));
        }
        None if item.item_type == crate::email::ITEM_TYPE => {
            let subject = metadata
                .and_then(|metadata| metadata.get("mailcal"))
                .and_then(|mailcal| mailcal.get("subject"))
                .and_then(serde_json::Value::as_str)
                .filter(|subject| !subject.is_empty());
            item.name = subject.unwrap_or("(no subject)").to_string();
        }
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use synck_vcs::types::EventContent;
    use super::*;
    use synck_vcs::types::CreateItem;

    #[test]
    fn create_and_content_project_exact_navigation_fields() {
        let item_id = ItemId::new();
        let now = Utc::now();
        let create = EventProposal { set_link: None,
            item_id: Some(item_id.clone()),
            event_type: "FILE::CREATE".to_string(),
            set_metadata: Some(synck_vcs::metadata::MetadataRef::new(serde_json::json!({"subtype":"STANDARD","color_metadata":"blue"})).unwrap()),
            set_content: None,
            create_item: Some(CreateItem { key: None, link: None,
                item_type: "FILE".to_string(),
                parent_folder_id: None,
                name: Some("Report.PDF".to_string()),
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
        content.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(serde_json::json!({"content":{"size":42}})).unwrap());
        content.set_content = Some(EventContent::bytes(ObjectId::from_digest([0xab;32])));
        content.base_seq = Some(1);
        let projected = apply_replica_item_event(Some(created), item_id, 2, &content, now, "actor")
            .expect("project content");
        assert_eq!(projected.size, Some(42));
        assert_eq!(projected.content_item_seq, Some(2));
    }

    #[test]
    fn erased_title_never_shows_an_older_one() {
        let item_id = ItemId::new();
        let now = Utc::now();
        let create = EventProposal::from_fields(serde_json::json!({
            "item_id": item_id, "event_type": "EMAIL::CREATE",
            "create_item": {"item_type": "EMAIL"}, "set_metadata": {"title": "first"}
        }))
        .unwrap();
        let created = apply_replica_item_event(None, item_id.clone(), 1, &create, now, "actor")
            .unwrap();
        assert_eq!(created.name, "first");
        let mut erased: EventProposal = EventProposal::from_fields(serde_json::json!({
            "item_id": item_id, "event_type": "EMAIL::SET_TITLE", "base_seq": 1,
            "set_metadata": {"title": "second"}
        }))
        .unwrap();
        erased.set_metadata = Some(MetadataRef::Absent(ObjectId::from_digest([1; 32])));
        let projected =
            apply_replica_item_event(Some(created), item_id, 2, &erased, now, "actor").unwrap();
        assert_eq!(projected.name, UNAVAILABLE_TITLE);
    }

    #[test]
    fn direct_chat_profile_is_tree_hidden_without_create_metadata() {
        let item_id = ItemId::new();
        let now = Utc::now();
        let create = EventProposal { set_link: None,
            item_id: None,
            event_type: "CHAT::CREATE".to_string(),
            set_metadata: None,
            set_content: None,
            create_item: Some(CreateItem { key: None, link: None,
                item_type: "CHAT".to_string(),
                parent_folder_id: None,
                name: Some("direct-chat-opaque".to_string()),
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

        let profile = EventProposal { set_link: None,
            item_id: None,
            event_type: "CHAT::SET_PROFILE".to_string(),
            set_metadata: Some(synck_vcs::metadata::MetadataRef::new(serde_json::json!({"chat_kind": "DIRECT"})).unwrap()),
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
