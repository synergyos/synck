//! Format-7 validity limits. These are not writer or server policy settings.
use crate::errors::CoreError;

pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
pub const MAX_ACTOR_ID_BYTES: usize = 256;
pub const MAX_OPERATION_ID_BYTES: usize = 256;
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const MAX_NAME_BYTES: usize = 255;

pub fn check_bytes(field: &str, length: usize, maximum: usize) -> Result<(), CoreError> {
    if length > maximum {
        return Err(CoreError::InvalidProposal(format!(
            "{field} exceeds format byte limit: bytes={length} max={maximum}"
        )));
    }
    Ok(())
}

pub fn validate_operation_id(value: &str) -> Result<(), CoreError> {
    if value.trim().is_empty() {
        return Err(CoreError::InvalidProposal(
            "operation_id must not be blank".into(),
        ));
    }
    check_bytes("operation_id", value.len(), MAX_OPERATION_ID_BYTES)
}

pub fn validate_commit_metadata(
    author: &str,
    committer: &str,
    message: Option<&str>,
    operation_id: Option<&str>,
) -> Result<(), CoreError> {
    for (field, value) in [("author_id", author), ("committer_id", committer)] {
        if value.is_empty() {
            return Err(CoreError::InvalidProposal(format!(
                "{field} must not be empty"
            )));
        }
        check_bytes(field, value.len(), MAX_ACTOR_ID_BYTES)?;
    }
    if let Some(message) = message {
        check_bytes("message", message.len(), MAX_MESSAGE_BYTES)?;
    }
    if let Some(operation_id) = operation_id {
        validate_operation_id(operation_id)?;
    }
    Ok(())
}
