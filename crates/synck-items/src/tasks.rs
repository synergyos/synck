//! Retained task fields and their legacy/current normalization rules.
//!
//! These functions interpret stored metadata. They do not evaluate assignee
//! eligibility, notification followers, permissions or platform commands.

pub mod email;
pub mod history;
pub mod migration;
pub mod workflow;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct TaskFieldError(pub String);

/// Invalid persisted task data. Commands and authorization use private errors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TaskError {
    #[error("{0}")]
    Invalid(String),
}

impl From<TaskFieldError> for TaskError {
    fn from(error: TaskFieldError) -> Self {
        Self::Invalid(error.0)
    }
}

/// Inert user/group identity recorded in historical assignment provenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TaskFollowerKind {
    #[serde(rename = "USER")]
    User,
    #[serde(rename = "GROUP")]
    Group,
}

/// A recorded assignment subject; it does not grant access or subscribe anyone.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFollower {
    pub kind: TaskFollowerKind,
    pub id: String,
}

// The original Rust names also determine serde's expected-type diagnostics.
// Assignment provenance uses the same stored identity without subscribing it.
pub use TaskFollower as AssignmentSubject;
pub use TaskFollowerKind as AssignmentSubjectKind;

/// Validate recorded subject identity syntax and uniqueness, not eligibility.
pub fn validate_assignment_subjects(subjects: &[AssignmentSubject]) -> Result<(), TaskError> {
    let mut seen = std::collections::HashSet::new();
    for subject in subjects {
        let kind = match subject.kind {
            AssignmentSubjectKind::User => 0,
            AssignmentSubjectKind::Group => 1,
        };
        if subject.id.is_empty()
            || subject.id.trim() != subject.id
            || subject.id.contains('\0')
            || !seen.insert((kind, subject.id.as_str()))
        {
            return Err(TaskError::Invalid(
                "task followers must be unique valid identities".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct TaskFields {
    pub subject: String,
    pub assigned_users: Vec<String>,
    pub assigned_groups: Vec<String>,
    pub priority: String,
    pub status: String,
    pub due_at: Option<chrono::DateTime<chrono::Utc>>,
    pub due_on: Option<chrono::NaiveDate>,
    pub due_timezone: Option<String>,
    pub column_values: serde_json::Value,
    pub metadata_json: serde_json::Value,
}

pub fn normalize_fields(meta: &serde_json::Value) -> Result<TaskFields, TaskFieldError> {
    normalize_workflow_fields(meta, false)
}

/// Canonical history contains both vocabularies. Named priorities identify the
/// new contract; historical missing fields retain their original defaults.
pub fn normalize_history_fields(meta: &serde_json::Value) -> Result<TaskFields, TaskFieldError> {
    let current = matches!(
        meta.get("priority").and_then(serde_json::Value::as_str),
        Some("URGENT" | "HIGH" | "NORMAL" | "LOW")
    );
    normalize_workflow_fields(meta, current)
}

pub fn normalize_current_fields(meta: &serde_json::Value) -> Result<TaskFields, TaskFieldError> {
    normalize_workflow_fields(meta, true)
}

fn normalize_workflow_fields(
    meta: &serde_json::Value,
    current: bool,
) -> Result<TaskFields, TaskFieldError> {
    use std::collections::HashSet;

    let meta_obj = meta
        .as_object()
        .ok_or_else(|| TaskFieldError("task row meta must be a JSON object".to_string()))?;

    let sort_index = match meta_obj.get("index") {
        None => 0_i64,
        Some(v) => {
            let Some(raw) = v.as_i64() else {
                return Err(TaskFieldError(
                    "task row meta.index must be an integer".to_string(),
                ));
            };
            if raw < 0 {
                return Err(TaskFieldError(
                    "task row meta.index must be >= 0".to_string(),
                ));
            }
            raw
        }
    };

    let subject = meta_obj
        .get("subject")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            TaskFieldError("task row meta.subject must be a non-empty string".to_string())
        })?;

    let description = match meta_obj.get("description") {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(v)) => v.trim().to_string(),
        Some(_) => {
            return Err(TaskFieldError(
                "task row meta.description must be a string or null".to_string(),
            ));
        }
    };

    let mut seen_users: HashSet<String> = HashSet::new();
    let mut assigned_users: Vec<String> = Vec::new();
    if let Some(raw) = meta_obj.get("assigned_users") {
        let users = raw.as_array().ok_or_else(|| {
            TaskFieldError("task row meta.assigned_users must be an array".to_string())
        })?;
        for user in users {
            let Some(user_id) = user.as_str() else {
                continue;
            };
            let user_id = user_id.trim();
            if user_id.is_empty() {
                continue;
            }
            if seen_users.insert(user_id.to_string()) {
                assigned_users.push(user_id.to_string());
            }
        }
    }

    let mut seen_groups: HashSet<String> = HashSet::new();
    let mut assigned_groups: Vec<String> = Vec::new();
    if let Some(raw) = meta_obj.get("assigned_groups") {
        let groups = raw.as_array().ok_or_else(|| {
            TaskFieldError("task row meta.assigned_groups must be an array".to_string())
        })?;
        for group in groups {
            let Some(group_id) = group.as_str() else {
                continue;
            };
            let group_id = group_id.trim();
            if !group_id.is_empty() && seen_groups.insert(group_id.to_string()) {
                assigned_groups.push(group_id.to_string());
            }
        }
    }

    let priority = if current {
        match meta_obj.get("priority") {
            None => "NORMAL".to_owned(),
            Some(value) => {
                serde_json::from_value::<workflow::Priority>(value.clone()).map_err(|_| {
                    TaskFieldError("task priority requires URGENT, HIGH, NORMAL, or LOW".into())
                })?;
                value.as_str().expect("validated priority").to_owned()
            }
        }
    } else {
        match meta_obj.get("priority").and_then(|v| v.as_str()) {
            None => "P1".to_string(),
            Some(v) if matches!(v.trim(), "P0" | "P1" | "P2" | "P3") => v.trim().to_string(),
            Some(v) => {
                return Err(TaskFieldError(format!(
                    "task row meta.priority invalid `{}` (expected P0, P1, P2, or P3)",
                    v.trim()
                )));
            }
        }
    };

    let status = if current {
        match meta_obj.get("status") {
            None => "TODO".to_owned(),
            Some(value) => {
                serde_json::from_value::<workflow::Status>(value.clone())
                    .map_err(|_| TaskFieldError("invalid current task status".into()))?;
                value.as_str().expect("validated status").to_owned()
            }
        }
    } else {
        match meta_obj.get("status").and_then(|v| v.as_str()) {
            None => "TODO".to_string(),
            Some(v)
                if matches!(
                    v.trim(),
                    "TODO" | "IN_PROGRESS" | "BLOCKED" | "DONE" | "CANCELED"
                ) =>
            {
                v.trim().to_string()
            }
            Some(v) => {
                return Err(TaskFieldError(format!(
                    "task row meta.status invalid `{}` (expected TODO, IN_PROGRESS, BLOCKED, DONE, or CANCELED)",
                    v.trim()
                )));
            }
        }
    };

    let parse_due_at = |value: Option<&serde_json::Value>| match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(v)) => {
            let v = v.trim();
            if v.is_empty() {
                Ok(None)
            } else {
                let parsed = chrono::DateTime::parse_from_rfc3339(v).map_err(|e| {
                    TaskFieldError(format!("task row meta.due_at invalid RFC3339 value: {e}"))
                })?;
                Ok(Some(parsed.with_timezone(&chrono::Utc)))
            }
        }
        Some(_) => Err(TaskFieldError(
            "task row meta.due_at must be RFC3339 string or null".to_string(),
        )),
    };
    let (due_at, due_on, due_timezone, normalized_due) = match meta_obj.get("due") {
        None => (parse_due_at(meta_obj.get("due_at"))?, None, None, None),
        Some(serde_json::Value::Null) => (None, None, None, Some(serde_json::Value::Null)),
        Some(serde_json::Value::Object(due)) => match due.get("kind").and_then(|v| v.as_str()) {
            Some("DATE") => {
                let date = due
                    .get("date")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        TaskFieldError("task row meta.due DATE requires date".to_string())
                    })?;
                let date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|e| {
                    TaskFieldError(format!("task row meta.due DATE must use YYYY-MM-DD: {e}"))
                })?;
                (
                    None,
                    Some(date),
                    None,
                    Some(serde_json::json!({ "kind": "DATE", "date": date.to_string() })),
                )
            }
            Some("DATETIME") => {
                let at = due
                    .get("at")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        TaskFieldError("task row meta.due DATETIME requires at".to_string())
                    })?;
                let timezone = due
                    .get("timezone")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty() && v.len() <= 128)
                    .ok_or_else(|| {
                        TaskFieldError("task row meta.due DATETIME requires timezone".to_string())
                    })?;
                if timezone.bytes().any(|b| b == 0 || b == b'\'' || b == b';') {
                    return Err(TaskFieldError(
                        "task row meta.due DATETIME timezone is invalid".to_string(),
                    ));
                }
                let parsed = chrono::DateTime::parse_from_rfc3339(at).map_err(|e| {
                    TaskFieldError(format!(
                        "task row meta.due DATETIME.at must be RFC3339: {e}"
                    ))
                })?;
                (
                    Some(parsed.with_timezone(&chrono::Utc)),
                    None,
                    Some(timezone.to_string()),
                    Some(serde_json::json!({
                        "kind": "DATETIME",
                        "at": parsed.to_rfc3339(),
                        "timezone": timezone,
                    })),
                )
            }
            Some(other) => {
                return Err(TaskFieldError(format!(
                    "task row meta.due kind `{other}` is invalid"
                )));
            }
            None => {
                return Err(TaskFieldError(
                    "task row meta.due requires kind".to_string(),
                ));
            }
        },
        Some(_) => {
            return Err(TaskFieldError(
                "task row meta.due must be an object or null".to_string(),
            ));
        }
    };

    let column_values = match meta_obj.get("column_values") {
        None => serde_json::json!({}),
        Some(v) if v.is_object() => v.clone(),
        Some(_) => {
            return Err(TaskFieldError(
                "task row meta.column_values must be an object".to_string(),
            ));
        }
    };

    let mut normalized_obj = meta_obj.clone();
    normalized_obj.insert(
        "subject".to_string(),
        serde_json::Value::String(subject.clone()),
    );
    normalized_obj.insert(
        "description".to_string(),
        serde_json::Value::String(description),
    );
    normalized_obj.insert(
        "assigned_users".to_string(),
        serde_json::Value::Array(
            assigned_users
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    normalized_obj.insert(
        "assigned_groups".to_string(),
        serde_json::Value::Array(
            assigned_groups
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    normalized_obj.insert(
        "priority".to_string(),
        serde_json::Value::String(priority.clone()),
    );
    normalized_obj.insert(
        "status".to_string(),
        serde_json::Value::String(status.clone()),
    );
    normalized_obj.insert(
        "due_at".to_string(),
        due_at
            .as_ref()
            .map(|v| {
                serde_json::Value::String(v.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            })
            .unwrap_or(serde_json::Value::Null),
    );
    if let Some(due) = normalized_due {
        normalized_obj.insert("due".to_string(), due);
    }
    normalized_obj.insert("column_values".to_string(), column_values.clone());
    normalized_obj.insert(
        "index".to_string(),
        serde_json::Value::Number(serde_json::Number::from(sort_index)),
    );

    Ok(TaskFields {
        subject,
        assigned_users,
        assigned_groups,
        priority,
        status,
        due_at,
        due_on,
        due_timezone,
        column_values,
        metadata_json: serde_json::Value::Object(normalized_obj),
    })
}

#[cfg(test)]
mod tests;
