//! Persisted task status and priority vocabulary.

use super::{TaskError, history::TaskData};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Todo,
    InProgress,
    Blocked,
    FollowUp,
    Done,
    Canceled,
}

impl Status {
    pub const fn is_open(self) -> bool {
        matches!(
            self,
            Self::Todo | Self::InProgress | Self::Blocked | Self::FollowUp
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Priority {
    Urgent,
    High,
    Normal,
    Low,
}

impl Priority {
    /// Business order, independent of the spelling or declaration order.
    pub const fn rank(self) -> u8 {
        match self {
            Self::Urgent => 0,
            Self::High => 1,
            Self::Normal => 2,
            Self::Low => 3,
        }
    }
}

pub struct LegacyConversion {
    pub data: TaskData,
    pub ambiguous_follow_up: bool,
}

/// Convert an exact legacy head, never an arbitrary old revision or new task.
///
/// `compose_follow_up` must be established by the maintenance caller from
/// accepted send/creation evidence AND the absence of a later explicit status
/// choice. The reducer cannot infer that fact from metadata or subject text.
/// Lifecycle admission, source-version fencing, and receipts belong to the
/// maintenance writer. Repeated conversion is an error; resume recovers receipts.
pub fn convert_legacy(
    data: &TaskData,
    compose_follow_up: bool,
) -> Result<LegacyConversion, TaskError> {
    let invalid = |field| TaskError::Invalid(format!("invalid legacy task {field}"));
    let priority = data
        .meta
        .get("priority")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("priority"))?;
    let next_priority = match priority {
        "P0" => Priority::Urgent,
        "P1" | "P2" => Priority::Normal,
        "P3" => Priority::Low,
        _ => return Err(invalid("priority")),
    };
    let status = match data.meta.get("status").and_then(serde_json::Value::as_str) {
        Some("TODO") => Status::Todo,
        Some("IN_PROGRESS") => Status::InProgress,
        Some("BLOCKED") => Status::Blocked,
        Some("DONE") => Status::Done,
        Some("CANCELED") => Status::Canceled,
        _ => return Err(invalid("status")),
    };
    let next_status = if status == Status::Todo && (priority == "P2" || compose_follow_up) {
        Status::FollowUp
    } else {
        status
    };
    let mut next = data.clone();
    next.meta["priority"] = serde_json::json!(next_priority);
    next.meta["status"] = serde_json::json!(next_status);
    Ok(LegacyConversion {
        data: next,
        ambiguous_follow_up: priority == "P2"
            && matches!(status, Status::InProgress | Status::Blocked),
    })
}

#[cfg(test)]
mod tests;
