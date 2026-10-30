//! Format-7 validity limits. These are not writer or server policy settings.
use crate::errors::CoreError;

pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
pub const MAX_ACTOR_ID_BYTES: usize = 256;
pub const MAX_OPERATION_ID_BYTES: usize = 256;
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const MAX_NAME_BYTES: usize = 255;
pub const MAX_RECORD_KEY_BYTES: usize = 256;
pub const MAX_ITEM_TYPE_BYTES: usize = 64;
pub const MAX_EVENT_TYPE_BYTES: usize = 128;
/// Arrays and objects in hashed JSON, the outermost being level 1.
pub const MAX_JSON_DEPTH: usize = 64;

/// One name for each core operation; `ITEM::` is never a second spelling of one.
const CORE_EVENTS: &[&str] = &[
    "CREATE",
    "SET_NAME",
    "SET_PARENT",
    "SET_LINK",
    "SET_METADATA",
    "SET_CONTENT",
    "TRASH",
    "RESTORE",
    "TOMBSTONE",
    "FEDERATE",
];

pub fn check_bytes(field: &str, length: usize, maximum: usize) -> Result<(), CoreError> {
    if length > maximum {
        return Err(CoreError::InvalidProposal(format!(
            "{field} exceeds format byte limit: bytes={length} max={maximum}"
        )));
    }
    Ok(())
}

fn valid_word(word: &str, dot: bool) -> bool {
    let mut bytes = word.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_uppercase())
        && bytes.all(|byte| {
            byte.is_ascii_uppercase()
                || byte.is_ascii_digit()
                || byte == b'_'
                || (dot && byte == b'.')
        })
}

pub fn validate_item_type(item_type: &str) -> Result<(), CoreError> {
    check_bytes("item_type", item_type.len(), MAX_ITEM_TYPE_BYTES)?;
    if matches!(item_type, "ITEM" | "FILE_VERSION") || !valid_word(item_type, false) {
        return Err(CoreError::InvalidProposal(format!(
            "invalid item_type: `{item_type}`"
        )));
    }
    Ok(())
}

/// Two or more `::`-joined segments. The first is the family.
pub fn validate_event_type(event_type: &str) -> Result<(), CoreError> {
    check_bytes("event_type", event_type.len(), MAX_EVENT_TYPE_BYTES)?;
    let mut segments = event_type.split("::");
    let family = segments.next().is_some_and(|word| valid_word(word, true));
    let mut count = 1;
    let rest = segments.all(|word| {
        count += 1;
        valid_word(word, true)
    });
    if !family || !rest || count < 2 {
        return Err(CoreError::InvalidProposal(format!(
            "invalid event_type: `{event_type}`"
        )));
    }
    Ok(())
}

/// The grammar, and no second spelling of a core event.
pub fn validate_item_event_type(event_type: &str) -> Result<(), CoreError> {
    validate_event_type(event_type)?;
    if event_type
        .strip_prefix("ITEM::")
        .is_some_and(|name| CORE_EVENTS.contains(&name))
    {
        return Err(CoreError::InvalidProposal(format!(
            "`{event_type}` spells a core event with ITEM"
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
        if message.is_empty() {
            return Err(CoreError::InvalidProposal(
                "commit message must be null or nonempty".into(),
            ));
        }
        check_bytes("message", message.len(), MAX_MESSAGE_BYTES)?;
    }
    if let Some(operation_id) = operation_id {
        validate_operation_id(operation_id)?;
    }
    Ok(())
}
