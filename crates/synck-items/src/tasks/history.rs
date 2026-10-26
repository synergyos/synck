//! Basic non-tree task event contract. Acceptance still owns authorization,
//! expected-version checks, relation invariants, and source/ACL side effects.
//! This vocabulary is not registered for serving until the cutover is wired.

use super::TaskError;
use super::normalize_history_fields;
use crate::MutationPolicy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use synck_vcs::types::{EventContent, EventProposal, ItemId};

pub const ITEM_TYPE: &str = "TASK";
pub const MAX_COMMENT_BYTES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskData {
    pub task_list_id: Option<ItemId>,
    pub parent_task_id: Option<ItemId>,
    pub meta: Value,
}

impl TaskData {
    pub fn normalized(mut self) -> Result<Self, TaskError> {
        self.meta = normalize_history_fields(&self.meta)?.metadata_json;
        Ok(self)
    }

    pub fn validate(&self, item_id: &ItemId) -> Result<(), TaskError> {
        if self.meta.get("followers").is_some() {
            return Err(invalid("followers are local state, not task metadata"));
        }
        // Import provenance describes the original baseline, not today's
        // assignments. Validate its full assignment match at IMPORT only.
        super::migration::from_metadata(&self.meta)?;
        if let Some(source) = super::email::from_metadata(&self.meta)? {
            if let super::email::Identity::Team { task_list_id, .. } = source.identity() {
                if self.task_list_id.as_ref() != Some(task_list_id) {
                    return Err(invalid("Team email task must retain its registered list"));
                }
            }
        }
        if self.parent_task_id.as_ref() == Some(item_id) {
            return Err(invalid("task cannot be its own parent"));
        }
        if self.task_list_id.as_ref() == Some(item_id) {
            return Err(invalid("task cannot be its own task list"));
        }
        if normalize_history_fields(&self.meta)?.metadata_json != self.meta {
            return Err(invalid(
                "task metadata must be normalized before acceptance",
            ));
        }
        if self
            .meta
            .get("created_by")
            .and_then(Value::as_str)
            .is_none_or(|value| value.is_empty() || value.trim() != value)
        {
            return Err(invalid("task requires an immutable creator"));
        }
        let created_at = self
            .meta
            .get("created_at")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("task requires an immutable creation time"))?;
        chrono::DateTime::parse_from_rfc3339(created_at)
            .map_err(|_| invalid("invalid task creation time"))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Lifecycle {
    Live,
    Trash,
    Tombstoned,
}

/// Reconstructed from accepted events; not another canonical task record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskState {
    pub item_id: ItemId,
    pub data: TaskData,
    pub lifecycle: Lifecycle,
}

pub fn internal_name(item_id: &ItemId) -> String {
    format!("task-{}", item_id.as_str())
}

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    match event_type {
        "TASK::CREATE"
        | "TASK::IMPORT"
        | "TASK::SET_METADATA"
        | "TASK::SET_CONTEXT"
        | "TASK::RETIRE_PERSONAL_LIST"
        | "TASK::MIGRATE_WORKFLOW"
        | "TASK::TRASH"
        | "TASK::RESTORE"
        | "TASK::TOMBSTONE"
        | "TASK::CAPTURE_EMAIL" => Some(MutationPolicy::Optimistic),
        "TASK::COMMENT::CREATE" => Some(MutationPolicy::Append),
        _ => None,
    }
}

fn invalid(message: &str) -> TaskError {
    TaskError::Invalid(message.to_owned())
}

pub fn validate_envelope(event: &EventProposal) -> Result<(), TaskError> {
    if event_policy(&event.event_type).is_none() {
        return Err(invalid("unsupported task event"));
    }
    let item_id = event
        .item_id
        .as_ref()
        .ok_or_else(|| invalid("task requires explicit item identity"))?;
    if event.set_parent.is_some()
        || event.set_name.is_some()
        || event.set_is_federated.is_some()
        || event.set_federation_stream_id.is_some()
    {
        return Err(invalid(
            "tasks forbid generic tree and federation modifiers",
        ));
    }
    let create = matches!(event.event_type.as_str(), "TASK::CREATE" | "TASK::IMPORT");
    if create {
        let item = event
            .create_item
            .as_ref()
            .ok_or_else(|| invalid("task create requires create_item"))?;
        if item.item_type != ITEM_TYPE
            || item.parent_folder_id.is_some()
            || item.name != internal_name(item_id)
            || event.base_seq.is_some()
        {
            return Err(invalid("invalid task create identity or parent"));
        }
    } else if event.create_item.is_some() || event.base_seq.is_none_or(|seq| seq <= 0) {
        return Err(invalid(
            "task mutation requires a positive base sequence and no create_item",
        ));
    }
    let (in_trash, tombstoned) = match event.event_type.as_str() {
        "TASK::TRASH" => (Some(true), None),
        "TASK::RESTORE" => (Some(false), None),
        "TASK::TOMBSTONE" => (None, Some(true)),
        _ => (None, None),
    };
    if event.set_in_trash != in_trash || event.set_tombstoned != tombstoned {
        return Err(invalid("task event has invalid lifecycle modifiers"));
    }
    match event.event_type.as_str() {
        "TASK::CREATE" | "TASK::IMPORT" | "TASK::CAPTURE_EMAIL" => {
            let data = decode_data(event)?;
            let source = super::email::from_metadata(&data.meta)?;
            match source.as_ref().and_then(super::email::Source::snapshot) {
                Some(snapshot) => {
                    if !matches!(&event.set_content, Some(EventContent::Blob { blob_id }) if blob_id == snapshot.raw_rfc822.blob_id.as_str())
                    {
                        return Err(invalid(
                            "saved task email requires its exact RFC822 content blob",
                        ));
                    }
                }
                None if event.event_type == "TASK::CAPTURE_EMAIL"
                    || event.set_content.is_some() =>
                {
                    return Err(invalid("task capture requires a saved email snapshot"));
                }
                None => {}
            }
        }
        "TASK::SET_METADATA"
        | "TASK::SET_CONTEXT"
        | "TASK::RETIRE_PERSONAL_LIST"
        | "TASK::MIGRATE_WORKFLOW" => {
            if event.set_metadata.is_none() || event.set_content.is_some() {
                return Err(invalid("task field/context event requires metadata only"));
            }
        }
        "TASK::COMMENT::CREATE" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Comment {
                comment_id: uuid::Uuid,
            }
            let comment: Comment =
                serde_json::from_value(event.set_metadata.clone().unwrap_or(Value::Null))
                    .map_err(|_| invalid("invalid task comment metadata"))?;
            if comment.comment_id.is_nil() {
                return Err(invalid("task comment id must not be nil"));
            }
            // A stored envelope can retain a reference. Receipt inspection must
            // not depend on fetching the body; domain reduction still requires it.
            if matches!(event.set_content, Some(EventContent::Content { .. })) {
                return Ok(());
            }
            let Some(EventContent::Json { json }) = event.set_content.as_ref() else {
                return Err(invalid("task comment requires hydrated body content"));
            };
            let body = json
                .as_object()
                .filter(|object| object.len() == 1)
                .and_then(|object| object.get("body"))
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("task comment requires body only"))?;
            if body.trim().is_empty() || body.len() > MAX_COMMENT_BYTES {
                return Err(invalid(
                    "task comment body must contain 1..10000 UTF-8 bytes",
                ));
            }
        }
        _ => {
            if event.set_metadata.is_some() || event.set_content.is_some() {
                return Err(invalid("task lifecycle event forbids metadata and content"));
            }
        }
    }
    Ok(())
}

fn decode_data(event: &EventProposal) -> Result<TaskData, TaskError> {
    serde_json::from_value(event.set_metadata.clone().unwrap_or(Value::Null))
        .map_err(|_| invalid("invalid task metadata shape"))
}

pub fn reduce(
    previous: Option<&TaskState>,
    event: &EventProposal,
    actor: &str,
) -> Result<TaskState, TaskError> {
    validate_envelope(event)?;
    if event.event_type == "TASK::COMMENT::CREATE"
        && matches!(event.set_content, Some(EventContent::Content { .. }))
    {
        return Err(invalid("task comment requires hydrated body content"));
    }
    if actor.is_empty() || actor.trim() != actor {
        return Err(invalid("task event requires an actor"));
    }
    let item_id = event.item_id.as_ref().expect("validated identity");
    if matches!(event.event_type.as_str(), "TASK::CREATE" | "TASK::IMPORT") {
        if previous.is_some() {
            return Err(invalid("task already exists"));
        }
        let data = decode_data(event)?;
        data.validate(item_id)?;
        let legacy = super::migration::from_metadata(&data.meta)?;
        if event.event_type == "TASK::IMPORT" {
            legacy
                .ok_or_else(|| invalid("task import requires legacy provenance"))?
                .validate(&data)?;
        } else {
            if legacy.is_some() {
                return Err(invalid(
                    "ordinary task creation cannot claim legacy history",
                ));
            }
            if data.meta["created_by"].as_str() != Some(actor) {
                return Err(invalid("task creator must match the create actor"));
            }
        }
        return Ok(TaskState {
            item_id: item_id.clone(),
            data,
            lifecycle: Lifecycle::Live,
        });
    }
    let previous = previous.ok_or_else(|| invalid("task does not exist"))?;
    if previous.item_id != *item_id || previous.lifecycle == Lifecycle::Tombstoned {
        return Err(invalid("invalid task identity or terminal state"));
    }
    let mut next = previous.clone();
    match event.event_type.as_str() {
        "TASK::MIGRATE_WORKFLOW" => {
            let data = decode_data(event)?;
            data.validate(item_id)?;
            // The maintenance owner proves compose provenance. Replay bounds
            // either proven mapping to these two fields and no other effects.
            let ordinary = super::workflow::convert_legacy(&previous.data, false)?.data;
            if data != ordinary
                && data != super::workflow::convert_legacy(&previous.data, true)?.data
            {
                return Err(invalid(
                    "workflow migration must preserve all fields except the reviewed status/priority mapping",
                ));
            }
            next.data = data;
        }
        "TASK::RETIRE_PERSONAL_LIST" => {
            let data = decode_data(event)?;
            data.validate(item_id)?;
            let expected = retired_personal_data(&previous.data, actor)?;
            if data != expected {
                return Err(invalid(
                    "personal-list retirement may only detach and retain owner discovery",
                ));
            }
            next.data = data;
        }
        "TASK::RESTORE" if previous.lifecycle == Lifecycle::Trash => {
            next.lifecycle = Lifecycle::Live
        }
        "TASK::TOMBSTONE" => next.lifecycle = Lifecycle::Tombstoned,
        _ if previous.lifecycle != Lifecycle::Live
            && !matches!(
                event.event_type.as_str(),
                "TASK::SET_CONTEXT" | "TASK::CAPTURE_EMAIL"
            ) =>
        {
            return Err(invalid("task must be live"));
        }
        "TASK::TRASH" => next.lifecycle = Lifecycle::Trash,
        "TASK::SET_METADATA" | "TASK::SET_CONTEXT" | "TASK::CAPTURE_EMAIL" => {
            let data = decode_data(event)?;
            data.validate(item_id)?;
            let current = |data: &TaskData| {
                matches!(
                    data.meta["priority"].as_str(),
                    Some("URGENT" | "HIGH" | "NORMAL" | "LOW")
                )
            };
            if current(&data) != current(&previous.data) {
                return Err(invalid("task workflow conversion requires maintenance"));
            }
            if data.meta.get("legacy_task") != previous.data.meta.get("legacy_task") {
                return Err(invalid("task import provenance is immutable"));
            }
            let source = super::email::from_metadata(&data.meta)?;
            let old_source = super::email::from_metadata(&previous.data.meta)?;
            if event.event_type == "TASK::CAPTURE_EMAIL" {
                match (&old_source, &source) {
                    (
                        Some(old @ super::email::Source::ProviderOnly { .. }),
                        Some(saved @ super::email::Source::Saved { .. }),
                    ) if old.identity() == saved.identity()
                        && old.generation() == saved.generation() => {}
                    _ => {
                        return Err(invalid(
                            "task capture must save its existing provider-only source exactly once",
                        ));
                    }
                }
            } else if source != old_source {
                return Err(invalid("task email source is immutable outside capture"));
            }
            let captured_placement = event.event_type == "TASK::CAPTURE_EMAIL"
                && data.task_list_id != previous.data.task_list_id;
            if event.event_type == "TASK::SET_CONTEXT" || captured_placement {
                let mut fields = data.meta.clone();
                if captured_placement {
                    fields["email_source"] = previous.data.meta["email_source"].clone();
                    if data.parent_task_id != previous.data.parent_task_id {
                        return Err(invalid("email capture placement cannot reparent a task"));
                    }
                }
                if previous.lifecycle == Lifecycle::Trash
                    && (data.parent_task_id != previous.data.parent_task_id
                        || fields != previous.data.meta)
                {
                    return Err(invalid(
                        "trashed task context may only change list placement",
                    ));
                }
                fields["index"] = previous.data.meta["index"].clone();
                if fields != previous.data.meta {
                    return Err(invalid("task context event cannot edit task fields"));
                }
            } else {
                if previous.lifecycle != Lifecycle::Live {
                    return Err(invalid("task must be live"));
                }
                if data.task_list_id != previous.data.task_list_id
                    || data.parent_task_id != previous.data.parent_task_id
                {
                    return Err(invalid("task field event cannot change context"));
                }
                for key in ["created_by", "created_at"] {
                    if data.meta[key] != previous.data.meta[key] {
                        return Err(invalid("task creator provenance is immutable"));
                    }
                }
            }
            next.data = data;
        }
        "TASK::COMMENT::CREATE" => {}
        _ => return Err(invalid("invalid task lifecycle transition")),
    }
    Ok(next)
}

/// The maintenance caller separately proves that actor is the registered list
/// owner. This reducer bounds its canonical effects for acceptance and replay.
pub fn retired_personal_data(data: &TaskData, owner: &str) -> Result<TaskData, TaskError> {
    if data.task_list_id.is_none() || owner.is_empty() || owner.trim() != owner {
        return Err(invalid(
            "personal-list retirement requires a listed task and owner",
        ));
    }
    let mut next = data.clone();
    next.task_list_id = None;
    if data.meta["assigned_users"]
        .as_array()
        .is_some_and(Vec::is_empty)
        && data.meta["assigned_groups"]
            .as_array()
            .is_some_and(Vec::is_empty)
        && data.meta["created_by"].as_str() != Some(owner)
    {
        next.meta["assigned_users"] = serde_json::json!([owner]);
    }
    Ok(next)
}

#[cfg(test)]
mod tests;
