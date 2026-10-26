//! Exact storage-free subitem vocabulary and decoder helpers.
//!
//! Metadata reconstruction preserves replacement values and raw canonical text.
//! PostgreSQL callers retain their existing projection sanitation separately.

use synck_vcs::types::{BlobId, EventContent};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum SubitemError {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Policy(String),
    #[error("{0}")]
    Schema(String),
}

/// Shared discriminator for supported subitem types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SubitemKind {
    FileVersion,
    ChatMessage,
    ChatMessageReaction,
    ChatAttachment,
    ChatMessageFileRef,
    ChatFile,
    ChatFileFolder,
    EmailAttachment,
    TaskListTask,
    TaskListTaskComment,
    StructuredDataRecord,
    StructuredDataRecordComment,
}

impl SubitemKind {
    pub fn parse(subitem_type: &str) -> Option<Self> {
        match subitem_type {
            "VERSION" => Some(SubitemKind::FileVersion),
            "MESSAGE" => Some(SubitemKind::ChatMessage),
            "MESSAGE.REACTION" => Some(SubitemKind::ChatMessageReaction),
            "MESSAGE.ATTACHMENT" => Some(SubitemKind::ChatAttachment),
            "MESSAGE.FILE_REF" => Some(SubitemKind::ChatMessageFileRef),
            "FILE" => Some(SubitemKind::ChatFile),
            "FILE_FOLDER" => Some(SubitemKind::ChatFileFolder),
            "ATTACHMENT" => Some(SubitemKind::EmailAttachment),
            "TASK" => Some(SubitemKind::TaskListTask),
            "TASK.COMMENT" => Some(SubitemKind::TaskListTaskComment),
            "RECORD" => Some(SubitemKind::StructuredDataRecord),
            "RECORD.COMMENT" => Some(SubitemKind::StructuredDataRecordComment),
            _ => None,
        }
    }

    pub fn owning_item_type(self) -> &'static str {
        match self {
            SubitemKind::FileVersion => "FILE",
            SubitemKind::ChatMessage
            | SubitemKind::ChatMessageReaction
            | SubitemKind::ChatAttachment
            | SubitemKind::ChatMessageFileRef
            | SubitemKind::ChatFile
            | SubitemKind::ChatFileFolder => "CHAT",
            SubitemKind::EmailAttachment => "EMAIL",
            SubitemKind::TaskListTask | SubitemKind::TaskListTaskComment => "TASK_LIST",
            SubitemKind::StructuredDataRecord | SubitemKind::StructuredDataRecordComment => "TABLE",
        }
    }

    pub fn event_type_create(self) -> &'static str {
        match self {
            SubitemKind::FileVersion => "FILE::VERSION::CREATE",
            SubitemKind::ChatMessage => "CHAT::MESSAGE::CREATE",
            SubitemKind::ChatMessageReaction => "CHAT::MESSAGE.REACTION::CREATE",
            SubitemKind::ChatAttachment => "CHAT::MESSAGE.ATTACHMENT::CREATE",
            SubitemKind::ChatMessageFileRef => "CHAT::MESSAGE.FILE_REF::CREATE",
            SubitemKind::ChatFile => "CHAT::FILE::CREATE",
            SubitemKind::ChatFileFolder => "CHAT::FILE_FOLDER::CREATE",
            SubitemKind::EmailAttachment => "EMAIL::ATTACHMENT::CREATE",
            SubitemKind::TaskListTask => "TASK_LIST::TASK::CREATE",
            SubitemKind::TaskListTaskComment => "TASK_LIST::TASK.COMMENT::CREATE",
            SubitemKind::StructuredDataRecord => "TABLE::RECORD::CREATE",
            SubitemKind::StructuredDataRecordComment => "TABLE::RECORD.COMMENT::CREATE",
        }
    }

    pub fn event_type_edit_meta(self) -> &'static str {
        match self {
            SubitemKind::FileVersion => "FILE::VERSION::SET_METADATA",
            SubitemKind::ChatMessage => "CHAT::MESSAGE::SET_CONTENT",
            SubitemKind::ChatMessageReaction => "CHAT::MESSAGE.REACTION::SET_CONTENT",
            SubitemKind::ChatAttachment => "CHAT::MESSAGE.ATTACHMENT::SET_METADATA",
            SubitemKind::ChatMessageFileRef => "CHAT::MESSAGE.FILE_REF::SET_METADATA",
            SubitemKind::ChatFile => "CHAT::FILE::SET_METADATA",
            SubitemKind::ChatFileFolder => "CHAT::FILE_FOLDER::SET_METADATA",
            SubitemKind::EmailAttachment => "EMAIL::ATTACHMENT::SET_METADATA",
            SubitemKind::TaskListTask => "TASK_LIST::TASK::SET_METADATA",
            SubitemKind::TaskListTaskComment => "TASK_LIST::TASK.COMMENT::SET_METADATA",
            SubitemKind::StructuredDataRecord => "TABLE::RECORD::SET_METADATA",
            SubitemKind::StructuredDataRecordComment => "TABLE::RECORD.COMMENT::SET_METADATA",
        }
    }

    pub fn event_type_edit_content(self) -> Option<&'static str> {
        match self {
            SubitemKind::ChatMessage => Some("CHAT::MESSAGE::SET_CONTENT"),
            SubitemKind::ChatMessageReaction => Some("CHAT::MESSAGE.REACTION::SET_CONTENT"),
            SubitemKind::ChatAttachment => Some("CHAT::MESSAGE.ATTACHMENT::SET_CONTENT"),
            SubitemKind::ChatFile => Some("CHAT::FILE::SET_CONTENT"),
            SubitemKind::ChatMessageFileRef => None,
            SubitemKind::ChatFileFolder => None,
            SubitemKind::TaskListTask => None,
            SubitemKind::TaskListTaskComment => None,
            SubitemKind::StructuredDataRecord => Some("TABLE::RECORD::SET_CONTENT"),
            SubitemKind::StructuredDataRecordComment => Some("TABLE::RECORD.COMMENT::SET_CONTENT"),
            _ => None,
        }
    }

    pub fn event_type_trash(self) -> &'static str {
        match self {
            SubitemKind::FileVersion => "FILE::VERSION::TRASH",
            SubitemKind::ChatMessage => "CHAT::MESSAGE::TRASH",
            SubitemKind::ChatMessageReaction => "CHAT::MESSAGE.REACTION::TRASH",
            SubitemKind::ChatAttachment => "CHAT::MESSAGE.ATTACHMENT::TRASH",
            SubitemKind::ChatMessageFileRef => "CHAT::MESSAGE.FILE_REF::TRASH",
            SubitemKind::ChatFile => "CHAT::FILE::TRASH",
            SubitemKind::ChatFileFolder => "CHAT::FILE_FOLDER::TRASH",
            SubitemKind::EmailAttachment => "EMAIL::ATTACHMENT::TRASH",
            SubitemKind::TaskListTask => "TASK_LIST::TASK::TRASH",
            SubitemKind::TaskListTaskComment => "TASK_LIST::TASK.COMMENT::TRASH",
            SubitemKind::StructuredDataRecord => "TABLE::RECORD::TRASH",
            SubitemKind::StructuredDataRecordComment => "TABLE::RECORD.COMMENT::TRASH",
        }
    }

    pub fn event_type_restore(self) -> &'static str {
        match self {
            SubitemKind::FileVersion => "FILE::VERSION::RESTORE",
            SubitemKind::ChatMessage => "CHAT::MESSAGE::RESTORE",
            SubitemKind::ChatMessageReaction => "CHAT::MESSAGE.REACTION::RESTORE",
            SubitemKind::ChatAttachment => "CHAT::MESSAGE.ATTACHMENT::RESTORE",
            SubitemKind::ChatMessageFileRef => "CHAT::MESSAGE.FILE_REF::RESTORE",
            SubitemKind::ChatFile => "CHAT::FILE::RESTORE",
            SubitemKind::ChatFileFolder => "CHAT::FILE_FOLDER::RESTORE",
            SubitemKind::EmailAttachment => "EMAIL::ATTACHMENT::RESTORE",
            SubitemKind::TaskListTask => "TASK_LIST::TASK::RESTORE",
            SubitemKind::TaskListTaskComment => "TASK_LIST::TASK.COMMENT::RESTORE",
            SubitemKind::StructuredDataRecord => "TABLE::RECORD::RESTORE",
            SubitemKind::StructuredDataRecordComment => "TABLE::RECORD.COMMENT::RESTORE",
        }
    }

    pub fn event_type_tombstone(self) -> &'static str {
        match self {
            SubitemKind::FileVersion => "FILE::VERSION::TOMBSTONE",
            SubitemKind::ChatMessage => "CHAT::MESSAGE::TOMBSTONE",
            SubitemKind::ChatMessageReaction => "CHAT::MESSAGE.REACTION::TOMBSTONE",
            SubitemKind::ChatAttachment => "CHAT::MESSAGE.ATTACHMENT::TOMBSTONE",
            SubitemKind::ChatMessageFileRef => "CHAT::MESSAGE.FILE_REF::TOMBSTONE",
            SubitemKind::ChatFile => "CHAT::FILE::TOMBSTONE",
            SubitemKind::ChatFileFolder => "CHAT::FILE_FOLDER::TOMBSTONE",
            SubitemKind::EmailAttachment => "EMAIL::ATTACHMENT::TOMBSTONE",
            SubitemKind::TaskListTask => "TASK_LIST::TASK::TOMBSTONE",
            SubitemKind::TaskListTaskComment => "TASK_LIST::TASK.COMMENT::TOMBSTONE",
            SubitemKind::StructuredDataRecord => "TABLE::RECORD::TOMBSTONE",
            SubitemKind::StructuredDataRecordComment => "TABLE::RECORD.COMMENT::TOMBSTONE",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChatAttachmentFields {
    pub kind: String,
    pub blob_id: Option<String>,
    pub file_item_id: Option<Uuid>,
    pub chat_file_folder_id: Option<Uuid>,
    pub size: Option<i64>,
    pub meta: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ChatFileFolderFields {
    pub folder_id: Uuid,
    pub name: String,
    pub name_key: String,
    pub parent_folder_id: Option<Uuid>,
    pub meta: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ChatFileFields {
    pub file_id: Uuid,
    pub name: String,
    pub name_key: String,
    pub parent_folder_id: Option<Uuid>,
    pub kind: String,
    pub linked_file_item_id: Option<Uuid>,
    pub origin_message_item_seq: Option<i64>,
    pub meta: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ChatFileContentFields {
    pub file_id: Uuid,
    pub expected_head_item_seq: Option<i64>,
    pub previous_head_item_seq: Option<i64>,
    pub blob_id: String,
    pub size: i64,
    pub mime_type: Option<String>,
    pub upload_intent_id: Option<String>,
    pub provenance: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ChatMessageFileRefFields {
    pub parent_message_item_seq: i64,
    pub file_id: Uuid,
    pub snapshot_name: String,
    pub snapshot_head_item_seq: Option<i64>,
    pub meta: serde_json::Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubitemVerb {
    Create,
    EditMeta,
    Trash,
    Restore,
    Tombstone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubitemEventVerb {
    Create,
    SetMetadata,
    SetContent,
    Trash,
    Restore,
    Tombstone,
}

pub fn match_subitem_event(event_type: &str) -> Option<(SubitemKind, SubitemVerb)> {
    const KINDS: [SubitemKind; 12] = [
        SubitemKind::FileVersion,
        SubitemKind::ChatMessage,
        SubitemKind::ChatMessageReaction,
        SubitemKind::ChatAttachment,
        SubitemKind::ChatMessageFileRef,
        SubitemKind::ChatFile,
        SubitemKind::ChatFileFolder,
        SubitemKind::EmailAttachment,
        SubitemKind::TaskListTask,
        SubitemKind::TaskListTaskComment,
        SubitemKind::StructuredDataRecord,
        SubitemKind::StructuredDataRecordComment,
    ];

    for kind in KINDS {
        if event_type == kind.event_type_create() {
            return Some((kind, SubitemVerb::Create));
        }
        if event_type == kind.event_type_edit_meta() {
            return Some((kind, SubitemVerb::EditMeta));
        }
        if let Some(edit_content) = kind.event_type_edit_content()
            && event_type == edit_content
        {
            return Some((kind, SubitemVerb::EditMeta));
        }
        if event_type == kind.event_type_trash() {
            return Some((kind, SubitemVerb::Trash));
        }
        if event_type == kind.event_type_restore() {
            return Some((kind, SubitemVerb::Restore));
        }
        if event_type == kind.event_type_tombstone() {
            return Some((kind, SubitemVerb::Tombstone));
        }
    }
    None
}

pub fn classify_subitem_event(event_type: &str) -> Option<(SubitemKind, SubitemEventVerb)> {
    const KINDS: [SubitemKind; 12] = [
        SubitemKind::FileVersion,
        SubitemKind::ChatMessage,
        SubitemKind::ChatMessageReaction,
        SubitemKind::ChatAttachment,
        SubitemKind::ChatMessageFileRef,
        SubitemKind::ChatFile,
        SubitemKind::ChatFileFolder,
        SubitemKind::EmailAttachment,
        SubitemKind::TaskListTask,
        SubitemKind::TaskListTaskComment,
        SubitemKind::StructuredDataRecord,
        SubitemKind::StructuredDataRecordComment,
    ];

    for kind in KINDS {
        if event_type == kind.event_type_create() {
            return Some((kind, SubitemEventVerb::Create));
        }
        if let Some(edit_content) = kind.event_type_edit_content()
            && event_type == edit_content
        {
            return Some((kind, SubitemEventVerb::SetContent));
        }
        if event_type == kind.event_type_edit_meta() {
            return Some((kind, SubitemEventVerb::SetMetadata));
        }
        if event_type == kind.event_type_trash() {
            return Some((kind, SubitemEventVerb::Trash));
        }
        if event_type == kind.event_type_restore() {
            return Some((kind, SubitemEventVerb::Restore));
        }
        if event_type == kind.event_type_tombstone() {
            return Some((kind, SubitemEventVerb::Tombstone));
        }
    }
    None
}

pub fn meta_obj(meta: &serde_json::Value) -> Option<&serde_json::Map<String, serde_json::Value>> {
    meta.as_object()
}

pub fn meta_get_i64(meta: Option<&serde_json::Value>, key: &str) -> Option<i64> {
    meta.and_then(meta_obj)
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_i64())
}

pub fn meta_get_str(meta: Option<&serde_json::Value>, key: &str) -> Option<String> {
    meta.and_then(meta_obj)
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub fn meta_get_json(meta: Option<&serde_json::Value>, key: &str) -> Option<serde_json::Value> {
    meta.and_then(meta_obj).and_then(|m| m.get(key)).cloned()
}

pub fn meta_get_optional_i64(
    meta: Option<&serde_json::Value>,
    key: &str,
) -> Result<Option<Option<i64>>, SubitemError> {
    let Some(value) = meta.and_then(meta_obj).and_then(|m| m.get(key)) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(Some(None));
    }
    let Some(value) = value.as_i64() else {
        return Err(SubitemError::Config(format!(
            "metadata field `{key}` must be an integer or null"
        )));
    };
    Ok(Some(Some(value)))
}

pub fn reconstruct_subitem_meta(meta: Option<&serde_json::Value>) -> serde_json::Value {
    meta_get_json(meta, "meta").unwrap_or_else(|| serde_json::json!({}))
}

pub fn reconstruct_subitem_content(
    set_metadata: Option<&serde_json::Value>,
    set_content: Option<&EventContent>,
) -> serde_json::Value {
    let mut content = match set_content {
        None => serde_json::Value::Null,
        Some(EventContent::Text { text }) => serde_json::Value::String(text.clone()),
        Some(EventContent::Blob { blob_id }) => serde_json::json!({ "blob_id": blob_id }),
        Some(EventContent::Content { content_id }) => {
            serde_json::json!({ "content_id": content_id })
        }
        Some(EventContent::Json { json }) => json.clone(),
    };

    // For VERSION edits, the platform stores content meta (size/...) in
    // set_metadata["content"] while the blob id lives in set_content.
    // Reconstruct the merged view for projection.
    let Some(extra) = meta_get_json(set_metadata, "content") else {
        return content;
    };
    let Some(extra_obj) = extra.as_object() else {
        return content;
    };

    let base_obj = content.as_object_mut();
    if let Some(base_obj) = base_obj {
        for (k, v) in extra_obj {
            base_obj.insert(k.clone(), v.clone());
        }
        return content;
    }

    if content.is_null() {
        content = serde_json::Value::Object(extra_obj.clone());
    }
    content
}

pub fn extract_chat_message_fields(
    actor: &str,
    committer: &str,
    meta: &serde_json::Value,
    content: &serde_json::Value,
) -> Result<(String, Option<String>, String, Option<i64>), SubitemError> {
    let assistant_id = chat_message_identity_metadata(meta, "assistant_id")?;
    let ai_run_id = chat_message_identity_metadata(meta, "ai_run_id")?;
    let sender_id = match (assistant_id, ai_run_id) {
        (None, None) => actor.to_string(),
        (Some(assistant_id), Some(_)) => {
            let expected_committer = format!("assistant:{assistant_id}");
            if committer != expected_committer {
                return Err(SubitemError::Config(format!(
                    "AI chat message committer must be `{expected_committer}`"
                )));
            }
            expected_committer
        }
        _ => {
            return Err(SubitemError::Config(
                "AI chat message metadata requires both assistant_id and ai_run_id".to_string(),
            ));
        }
    };
    let sender_label = meta
        .get("sender_label")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .filter(|v| !v.eq_ignore_ascii_case("unknown user"))
        .map(|v| v.to_string());
    let body = content
        .get("body")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .unwrap_or_default();
    let reply_to = meta.get("reply_to_item_seq").and_then(|v| v.as_i64());
    Ok((sender_id, sender_label, body, reply_to))
}

pub fn chat_message_identity_metadata<'a>(
    meta: &'a serde_json::Value,
    key: &str,
) -> Result<Option<&'a str>, SubitemError> {
    let Some(value) = meta.get(key) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(Some)
        .ok_or_else(|| {
            SubitemError::Config(format!(
                "chat message metadata {key} must be a non-empty string"
            ))
        })
}

pub fn extract_chat_message_metadata(meta: &serde_json::Value) -> serde_json::Value {
    if meta.as_object().is_some() {
        meta.clone()
    } else {
        serde_json::json!({})
    }
}

pub fn extract_chat_message_reaction_emoji(
    content: &serde_json::Value,
) -> Result<String, SubitemError> {
    let emoji = content
        .get("emoji")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            SubitemError::Config(
                "chat message reaction content must include non-empty emoji string".to_string(),
            )
        })?;
    Ok(emoji)
}

pub fn chat_file_folder_id_from_meta(
    meta: &serde_json::Value,
) -> Result<Option<Uuid>, SubitemError> {
    match meta.get("chat_file_folder_id") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(raw)) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return Ok(None);
            }
            Uuid::parse_str(raw)
                .map(Some)
                .map_err(|e| SubitemError::Config(format!("chat_file_folder_id must be uuid: {e}")))
        }
        Some(_) => Err(SubitemError::Config(
            "chat_file_folder_id must be a uuid string or null".to_string(),
        )),
    }
}

pub fn chat_file_optional_uuid_from_meta(
    meta: &serde_json::Value,
    field: &str,
) -> Result<Option<Uuid>, SubitemError> {
    match meta.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(raw)) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return Ok(None);
            }
            Uuid::parse_str(raw).map(Some).map_err(|e| {
                SubitemError::Config(format!("chat file meta.{field} must be uuid: {e}"))
            })
        }
        Some(_) => Err(SubitemError::Config(format!(
            "chat file meta.{field} must be a uuid string or null"
        ))),
    }
}

pub fn chat_file_required_uuid_from_meta(
    meta: &serde_json::Value,
    field: &str,
) -> Result<Uuid, SubitemError> {
    chat_file_optional_uuid_from_meta(meta, field)?
        .ok_or_else(|| SubitemError::Config(format!("chat file meta.{field} must be a uuid")))
}

pub fn extract_chat_file_folder_fields(
    meta: &serde_json::Value,
) -> Result<ChatFileFolderFields, SubitemError> {
    let obj = meta.as_object().ok_or_else(|| {
        SubitemError::Config("chat file folder meta must be a JSON object".to_string())
    })?;
    let folder_id = obj
        .get("folder_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            SubitemError::Config("chat file folder meta.folder_id must be a uuid".to_string())
        })?;
    let folder_id = Uuid::parse_str(folder_id).map_err(|e| {
        SubitemError::Config(format!("chat file folder meta.folder_id must be uuid: {e}"))
    })?;
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            SubitemError::Config("chat file folder meta.name must be non-empty".to_string())
        })?;
    let name_key = synck_vcs::naming::compute_name_key(name.as_str()).map_err(|e| {
        SubitemError::Config(format!("invalid chat file folder name `{name}`: {e}"))
    })?;
    let parent_folder_id = chat_file_optional_uuid_from_meta(meta, "parent_folder_id")?;
    if parent_folder_id == Some(folder_id) {
        return Err(SubitemError::Policy(
            "chat file folder cannot parent itself".to_string(),
        ));
    }
    let mut normalized = obj.clone();
    normalized.insert(
        "folder_id".to_string(),
        serde_json::Value::String(folder_id.to_string()),
    );
    normalized.insert("name".to_string(), serde_json::Value::String(name.clone()));
    normalized.insert(
        "name_key".to_string(),
        serde_json::Value::String(name_key.clone()),
    );
    normalized.insert(
        "parent_folder_id".to_string(),
        parent_folder_id
            .map(|id| serde_json::Value::String(id.to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    Ok(ChatFileFolderFields {
        folder_id,
        name,
        name_key,
        parent_folder_id,
        meta: serde_json::Value::Object(normalized),
    })
}

pub fn extract_chat_file_fields(meta: &serde_json::Value) -> Result<ChatFileFields, SubitemError> {
    let obj = meta
        .as_object()
        .ok_or_else(|| SubitemError::Config("chat file meta must be a JSON object".to_string()))?;
    let file_id = chat_file_required_uuid_from_meta(meta, "file_id")?;
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| SubitemError::Config("chat file meta.name must be non-empty".to_string()))?;
    let name_key = synck_vcs::naming::compute_name_key(name.as_str())
        .map_err(|e| SubitemError::Config(format!("invalid chat file name `{name}`: {e}")))?;
    let parent_folder_id = chat_file_optional_uuid_from_meta(meta, "parent_folder_id")?;
    let kind = obj
        .get("kind")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("CHAT_FILE")
        .to_string();
    if kind != "CHAT_FILE" && kind != "FILE_LINK" {
        return Err(SubitemError::Config(
            "chat file meta.kind must be CHAT_FILE or FILE_LINK".to_string(),
        ));
    }
    let linked_file_item_id = chat_file_optional_uuid_from_meta(meta, "linked_file_item_id")?;
    if kind == "FILE_LINK" && linked_file_item_id.is_none() {
        return Err(SubitemError::Config(
            "chat file FILE_LINK requires linked_file_item_id".to_string(),
        ));
    }
    if kind == "CHAT_FILE" && linked_file_item_id.is_some() {
        return Err(SubitemError::Config(
            "chat file CHAT_FILE forbids linked_file_item_id".to_string(),
        ));
    }
    let origin_message_item_seq = obj
        .get("origin_message_item_seq")
        .and_then(|v| if v.is_null() { None } else { v.as_i64() });
    let mut normalized = obj.clone();
    normalized.insert(
        "file_id".to_string(),
        serde_json::Value::String(file_id.to_string()),
    );
    normalized.insert("name".to_string(), serde_json::Value::String(name.clone()));
    normalized.insert(
        "name_key".to_string(),
        serde_json::Value::String(name_key.clone()),
    );
    normalized.insert(
        "parent_folder_id".to_string(),
        parent_folder_id
            .map(|id| serde_json::Value::String(id.to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    normalized.insert("kind".to_string(), serde_json::Value::String(kind.clone()));
    normalized.insert(
        "linked_file_item_id".to_string(),
        linked_file_item_id
            .map(|id| serde_json::Value::String(id.to_string()))
            .unwrap_or(serde_json::Value::Null),
    );
    if let Some(seq) = origin_message_item_seq {
        normalized.insert(
            "origin_message_item_seq".to_string(),
            serde_json::Value::Number(seq.into()),
        );
    }
    Ok(ChatFileFields {
        file_id,
        name,
        name_key,
        parent_folder_id,
        kind,
        linked_file_item_id,
        origin_message_item_seq,
        meta: serde_json::Value::Object(normalized),
    })
}

pub fn extract_chat_file_content_fields(
    meta: &serde_json::Value,
    content: &serde_json::Value,
) -> Result<ChatFileContentFields, SubitemError> {
    let file_id = chat_file_required_uuid_from_meta(meta, "file_id")?;
    let expected_head_item_seq = meta
        .get("expected_head_item_seq")
        .and_then(|v| {
            if v.is_null() {
                Some(None)
            } else {
                v.as_i64().map(Some)
            }
        })
        .flatten();
    let previous_head_item_seq = meta
        .get("previous_head_item_seq")
        .and_then(|v| {
            if v.is_null() {
                Some(None)
            } else {
                v.as_i64().map(Some)
            }
        })
        .flatten();
    let blob_id = content
        .get("blob_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            SubitemError::Config("chat file content.blob_id must be non-empty".to_string())
        })?;
    let size = content
        .get("size")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| {
            SubitemError::Config("chat file content.size must be an integer".to_string())
        })?;
    if size < 0 {
        return Err(SubitemError::Config(
            "chat file content.size must be non-negative".to_string(),
        ));
    }
    let mime_type = content
        .get("mime_type")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let upload_intent_id = meta
        .get("upload_intent_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let provenance = meta
        .get("provenance")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(ChatFileContentFields {
        file_id,
        expected_head_item_seq,
        previous_head_item_seq,
        blob_id,
        size,
        mime_type,
        upload_intent_id,
        provenance,
    })
}

pub fn extract_chat_message_file_ref_fields(
    meta: &serde_json::Value,
    parent_item_seq: Option<i64>,
) -> Result<ChatMessageFileRefFields, SubitemError> {
    let obj = meta.as_object().ok_or_else(|| {
        SubitemError::Config("chat message file ref meta must be a JSON object".to_string())
    })?;
    let parent_message_item_seq = parent_item_seq
        .or_else(|| obj.get("parent_item_seq").and_then(|v| v.as_i64()))
        .ok_or_else(|| {
            SubitemError::Config("MESSAGE.FILE_REF requires parent_item_seq".to_string())
        })?;
    let file_id = chat_file_required_uuid_from_meta(meta, "file_id")?;
    let snapshot_name = obj
        .get("snapshot_name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            SubitemError::Config(
                "chat message file ref meta.snapshot_name must be non-empty".to_string(),
            )
        })?;
    let snapshot_head_item_seq = obj
        .get("snapshot_head_item_seq")
        .and_then(|v| {
            if v.is_null() {
                Some(None)
            } else {
                v.as_i64().map(Some)
            }
        })
        .flatten();
    let mut normalized = obj.clone();
    normalized.insert(
        "parent_item_seq".to_string(),
        serde_json::Value::Number(parent_message_item_seq.into()),
    );
    normalized.insert(
        "file_id".to_string(),
        serde_json::Value::String(file_id.to_string()),
    );
    normalized.insert(
        "snapshot_name".to_string(),
        serde_json::Value::String(snapshot_name.clone()),
    );
    normalized.insert(
        "snapshot_head_item_seq".to_string(),
        snapshot_head_item_seq
            .map(|seq| serde_json::Value::Number(seq.into()))
            .unwrap_or(serde_json::Value::Null),
    );
    Ok(ChatMessageFileRefFields {
        parent_message_item_seq,
        file_id,
        snapshot_name,
        snapshot_head_item_seq,
        meta: serde_json::Value::Object(normalized),
    })
}

pub fn extract_chat_attachment_fields(
    meta: &serde_json::Value,
    content: &serde_json::Value,
) -> Result<ChatAttachmentFields, SubitemError> {
    let kind = meta
        .get("kind")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            SubitemError::Config("chat attachment meta must include kind".to_string())
        })?;

    match kind.as_str() {
        "EMBEDDED" => {
            let chat_file_folder_id = chat_file_folder_id_from_meta(meta)?;
            if content.get("file_item_id").is_some() {
                return Err(SubitemError::Config(
                    "chat attachment EMBEDDED forbids file_item_id".to_string(),
                ));
            }
            let blob_id = content
                .get("blob_id")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .ok_or_else(|| {
                    SubitemError::Config(
                        "chat attachment EMBEDDED must include non-empty content.blob_id"
                            .to_string(),
                    )
                })?;
            let size = content.get("size").and_then(|v| v.as_i64());
            Ok(ChatAttachmentFields {
                kind,
                blob_id: Some(blob_id),
                file_item_id: None,
                chat_file_folder_id,
                size,
                meta: meta.clone(),
            })
        }
        "FILE_LINK" => {
            let chat_file_folder_id = chat_file_folder_id_from_meta(meta)?;
            if content.get("blob_id").is_some() {
                return Err(SubitemError::Config(
                    "chat attachment FILE_LINK forbids blob_id".to_string(),
                ));
            }
            if content.get("size").is_some() {
                return Err(SubitemError::Config(
                    "chat attachment FILE_LINK forbids size".to_string(),
                ));
            }
            let file_item_id = content
                .get("file_item_id")
                .and_then(|v| v.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    SubitemError::Config(
                        "chat attachment FILE_LINK must include non-empty content.file_item_id"
                            .to_string(),
                    )
                })?;
            let file_item_id = Uuid::parse_str(file_item_id).map_err(|e| {
                SubitemError::Config(format!("chat attachment file_item_id must be uuid: {e}"))
            })?;
            Ok(ChatAttachmentFields {
                kind,
                blob_id: None,
                file_item_id: Some(file_item_id),
                chat_file_folder_id,
                size: None,
                meta: meta.clone(),
            })
        }
        other => Err(SubitemError::Config(format!(
            "chat attachment meta.kind must be EMBEDDED or FILE_LINK (got `{other}`)"
        ))),
    }
}

pub fn extract_email_attachment_fields(
    meta: &serde_json::Value,
    content: &serde_json::Value,
) -> Result<(String, serde_json::Value), SubitemError> {
    let blob_id = content
        .get("blob_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            SubitemError::Config("email attachment content must include blob_id string".to_string())
        })?
        .to_owned();
    Ok((blob_id, meta.clone()))
}

pub fn task_row_metadata_with_creator(
    meta: serde_json::Value,
    created_at: chrono::DateTime<chrono::Utc>,
    created_by: &str,
) -> Result<serde_json::Value, SubitemError> {
    let created_by = created_by.trim();
    if created_by.is_empty() {
        return Err(SubitemError::Config(
            "task row created_by must be non-empty".to_string(),
        ));
    }
    let mut obj = meta
        .as_object()
        .cloned()
        .ok_or_else(|| SubitemError::Config("task row meta must be a JSON object".to_string()))?;
    obj.insert(
        "created_at".to_string(),
        serde_json::Value::String(created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
    );
    obj.insert(
        "created_by".to_string(),
        serde_json::Value::String(created_by.to_string()),
    );
    Ok(serde_json::Value::Object(obj))
}

pub fn merge_task_row_meta(
    existing_meta: &serde_json::Value,
    patch: &serde_json::Value,
) -> Result<serde_json::Value, SubitemError> {
    let mut merged = existing_meta.as_object().cloned().unwrap_or_default();
    let patch_obj = patch.as_object().ok_or_else(|| {
        SubitemError::Config("task row meta patch must be a JSON object".to_string())
    })?;
    for (key, value) in patch_obj {
        if key == "created_at" || key == "created_by" {
            continue;
        }
        merged.insert(key.clone(), value.clone());
    }
    Ok(serde_json::Value::Object(merged))
}

pub fn structured_data_record_content_object(
    content: &serde_json::Value,
) -> Result<&serde_json::Map<String, serde_json::Value>, SubitemError> {
    content.as_object().ok_or_else(|| {
        SubitemError::Schema("structured-data RECORD content must be object".to_string())
    })
}

pub fn structured_data_record_data_from_content(
    content: &serde_json::Value,
) -> Result<serde_json::Value, SubitemError> {
    structured_data_record_content_object(content)?
        .get("data")
        .cloned()
        .ok_or_else(|| {
            SubitemError::Schema("structured-data RECORD content.data missing".to_string())
        })
}

pub fn structured_data_record_display_value_from_content(
    content: &serde_json::Value,
    data_json: &serde_json::Value,
) -> String {
    if let Some(display_value) = content
        .as_object()
        .and_then(|obj| obj.get("display_value"))
        .and_then(|v| v.as_str())
    {
        return display_value.trim().to_string();
    }
    if let Some(primary_text) = data_json
        .as_object()
        .and_then(|obj| obj.get("name"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return primary_text.to_string();
    }
    match data_json {
        serde_json::Value::String(v) => v.clone(),
        _ => data_json.to_string(),
    }
}

pub fn structured_data_record_lifecycle_from_content(
    content: &serde_json::Value,
    default_state: &str,
) -> Result<String, SubitemError> {
    let lifecycle_state = content
        .as_object()
        .and_then(|obj| obj.get("lifecycle_state"))
        .and_then(|v| v.as_str())
        .unwrap_or(default_state);
    match lifecycle_state {
        "DRAFT" | "LIVE" | "ARCHIVED" => Ok(lifecycle_state.to_string()),
        other => Err(SubitemError::Schema(format!(
            "structured-data RECORD lifecycle_state `{other}` is invalid"
        ))),
    }
}

pub fn structured_data_record_version_from_content(
    content: &serde_json::Value,
    default_version: i64,
) -> Result<i64, SubitemError> {
    let version = content
        .as_object()
        .and_then(|obj| obj.get("version"))
        .and_then(|v| v.as_i64())
        .unwrap_or(default_version);
    if version <= 0 {
        return Err(SubitemError::Schema(
            "structured-data RECORD version must be > 0".to_string(),
        ));
    }
    Ok(version)
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ChatProfileEvent {
    pub chat_kind: String,
    pub human_a_principal_key: Option<String>,
    pub human_b_principal_key: Option<String>,
    pub assistant_id: Option<String>,
    pub display_subject_key: Option<String>,
    pub room_key: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ChatUserParticipantEvent {
    pub tenant_id: String,
    pub user_id: String,
    pub role: String,
}

pub fn optional_nonempty_string(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match map.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
            Ok(Some(s.trim().to_string()))
        }
        Some(serde_json::Value::String(_)) => Err(format!("{key} must not be empty")),
        Some(_) => Err(format!("{key} must be string|null")),
    }
}

pub fn parse_chat_profile_event(meta: &serde_json::Value) -> Result<ChatProfileEvent, String> {
    let map = meta
        .as_object()
        .ok_or_else(|| "CHAT::SET_PROFILE metadata must be an object".to_string())?;
    let chat_kind = map
        .get("chat_kind")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .ok_or_else(|| "chat_kind is required".to_string())?
        .to_ascii_uppercase();
    let human_a_principal_key = optional_nonempty_string(map, "human_a_principal_key")?;
    let human_b_principal_key = optional_nonempty_string(map, "human_b_principal_key")?;
    let assistant_id = optional_nonempty_string(map, "assistant_id")?;
    let display_subject_key = optional_nonempty_string(map, "display_subject_key")?;
    let room_key = optional_nonempty_string(map, "room_key")?;

    match chat_kind.as_str() {
        "ROOM" => {
            if human_a_principal_key.is_some()
                || human_b_principal_key.is_some()
                || assistant_id.is_some()
            {
                return Err(
                    "ROOM profile forbids human principal keys and assistant_id".to_string()
                );
            }
        }
        "DIRECT" => {
            let (Some(a), Some(b), None) = (
                human_a_principal_key.as_ref(),
                human_b_principal_key.as_ref(),
                assistant_id.as_ref(),
            ) else {
                return Err(
                    "DIRECT profile requires two human principal keys and forbids assistant_id"
                        .to_string(),
                );
            };
            if room_key.is_some() {
                return Err("DIRECT profile forbids room_key".to_string());
            }
            if a >= b {
                return Err(
                    "DIRECT profile requires human_a_principal_key < human_b_principal_key"
                        .to_string(),
                );
            }
        }
        "ASSISTANT_DIRECT" => {
            if human_a_principal_key.is_none()
                || human_b_principal_key.is_some()
                || assistant_id.is_none()
            {
                return Err(
                    "ASSISTANT_DIRECT profile requires human_a_principal_key and assistant_id only"
                        .to_string(),
                );
            }
            if room_key.is_some() {
                return Err("ASSISTANT_DIRECT profile forbids room_key".to_string());
            }
        }
        _ => return Err("chat_kind must be ROOM, DIRECT, or ASSISTANT_DIRECT".to_string()),
    }

    Ok(ChatProfileEvent {
        chat_kind,
        human_a_principal_key,
        human_b_principal_key,
        assistant_id,
        display_subject_key,
        room_key,
    })
}

pub fn parse_chat_assistant_participant_event(meta: &serde_json::Value) -> Result<String, String> {
    let map = meta
        .as_object()
        .ok_or_else(|| "CHAT assistant participant metadata must be an object".to_string())?;
    let assistant_id = map
        .get("assistant_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .ok_or_else(|| "assistant_id is required".to_string())?;
    if assistant_id.is_empty() {
        return Err("assistant_id must not be empty".to_string());
    }
    Ok(assistant_id.to_string())
}

pub fn parse_chat_user_participant_event(
    meta: &serde_json::Value,
) -> Result<ChatUserParticipantEvent, String> {
    let map = meta
        .as_object()
        .ok_or_else(|| "CHAT user participant metadata must be an object".to_string())?;
    let tenant_id = map
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "tenant_id is required".to_string())?
        .to_string();
    let user_id = map
        .get("user_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "user_id is required".to_string())?
        .to_string();
    let role = map
        .get("role")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("MEMBER")
        .to_ascii_uppercase();
    if !matches!(role.as_str(), "OWNER" | "MEMBER" | "VIEWER") {
        return Err("role must be OWNER, MEMBER, or VIEWER".to_string());
    }
    Ok(ChatUserParticipantEvent {
        tenant_id,
        user_id,
        role,
    })
}

pub fn file_version_kind_from_meta(meta: &serde_json::Value) -> String {
    meta.get("kind")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "MAJOR".to_string())
}

pub fn file_version_order_key_from_meta(meta: &serde_json::Value) -> Option<i64> {
    meta.get("order_key").and_then(|v| v.as_i64())
}

pub fn file_version_comment_from_meta(meta: &serde_json::Value) -> Option<String> {
    meta.get("description")
        .or_else(|| meta.get("comment"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
}

pub fn file_version_source_item_seq_from_meta(meta: &serde_json::Value) -> Option<i64> {
    meta.get("source_item_seq")
        .and_then(|v| v.as_i64())
        .filter(|seq| *seq > 0)
}

pub fn file_version_source_commit_id_from_meta(meta: &serde_json::Value) -> Option<String> {
    meta.get("source_commit_id")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
}

pub fn file_version_source_commit_event_idx_from_meta(meta: &serde_json::Value) -> Option<i32> {
    let idx = meta.get("source_commit_event_idx")?.as_i64()?;
    if idx < 0 {
        return None;
    }
    i32::try_from(idx).ok()
}

pub fn file_version_blob_id_from_content(content: &serde_json::Value) -> Option<String> {
    content
        .get("blob_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
}

pub fn file_version_size_from_content(content: &serde_json::Value) -> Option<i64> {
    content.get("size").and_then(|v| v.as_i64())
}

pub fn file_version_content_has_size(content: &serde_json::Value) -> bool {
    content.get("size").is_some()
}

#[derive(Clone, Debug)]
pub struct TaskOrderEntry {
    pub item_seq: i64,
    pub sort_index: i64,
    pub metadata_json: serde_json::Value,
}

/// Apply the existing task sibling placement to a list already sorted by index/position.
pub fn reorder_task_siblings(
    mut ordered: Vec<TaskOrderEntry>,
    moving_item_seq: Option<i64>,
    requested_index: Option<i64>,
    before_item_seq: Option<Option<i64>>,
) -> Result<Vec<TaskOrderEntry>, SubitemError> {
    if let Some(moving_item_seq) = moving_item_seq
        && let Some(from_pos) = ordered
            .iter()
            .position(|entry| entry.item_seq == moving_item_seq)
    {
        let moving = ordered.remove(from_pos);
        let insert_pos = match before_item_seq {
            Some(Some(anchor)) => ordered
                .iter()
                .position(|entry| entry.item_seq == anchor)
                .ok_or_else(|| {
                    SubitemError::Policy(format!(
                        "TASK move anchor {anchor} is not a live destination sibling"
                    ))
                })?,
            Some(None) => ordered.len(),
            None => requested_index
                .and_then(|index| usize::try_from(index).ok())
                .map(|index| index.min(ordered.len()))
                .unwrap_or(ordered.len()),
        };
        ordered.insert(insert_pos, moving);
    }

    Ok(ordered)
}

const FILE_VERSION_LEGACY_UUID_NAMESPACE: Uuid =
    Uuid::from_u128(0x33d1c633_9155_42ad_a37f_d26d116a0c32);

pub fn legacy_file_version_id(repo_uuid: &Uuid, item_uuid: &Uuid, legacy_item_seq: i64) -> Uuid {
    Uuid::new_v5(
        &FILE_VERSION_LEGACY_UUID_NAMESPACE,
        format!("synckhub:file-version:legacy:v1:{repo_uuid}:{item_uuid}:{legacy_item_seq}")
            .as_bytes(),
    )
}

pub fn file_version_ref_required_string(
    meta: &serde_json::Value,
    key: &str,
    event_type: &str,
) -> Result<String, SubitemError> {
    meta.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| SubitemError::Schema(format!("{event_type} requires non-empty {key}")))
}

pub fn file_version_ref_required_kind(
    meta: &serde_json::Value,
    event_type: &str,
) -> Result<String, SubitemError> {
    let raw = file_version_ref_required_string(meta, "kind", event_type)?;
    synck_file_folder::file_version_set::normalize_kind(Some(raw.as_str()))
        .map(str::to_string)
        .map_err(|error| SubitemError::Schema(format!("{event_type} has invalid kind: {error}")))
}

pub fn file_version_ref_required_label_source(
    meta: &serde_json::Value,
    event_type: &str,
) -> Result<String, SubitemError> {
    let raw = file_version_ref_required_string(meta, "label_source", event_type)?;
    synck_file_folder::file_version_set::normalize_label_source(Some(raw.as_str()))
        .map(str::to_string)
        .map_err(|error| {
            SubitemError::Schema(format!("{event_type} has invalid label_source: {error}"))
        })
}

pub fn file_version_ref_optional_positive_i64(
    meta: &serde_json::Value,
    key: &str,
    event_type: &str,
) -> Result<Option<i64>, SubitemError> {
    match meta.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| {
                SubitemError::Schema(format!("{event_type} {key} must be a positive integer"))
            }),
    }
}

pub fn file_version_ref_optional_nonnegative_i32(
    meta: &serde_json::Value,
    key: &str,
    event_type: &str,
) -> Result<Option<i32>, SubitemError> {
    match meta.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .filter(|value| *value >= 0)
            .and_then(|value| i32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| {
                SubitemError::Schema(format!("{event_type} {key} must be a non-negative i32"))
            }),
    }
}

pub fn file_version_ref_optional_commit_id(
    meta: &serde_json::Value,
    key: &str,
    event_type: &str,
) -> Result<Option<String>, SubitemError> {
    match meta.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => {
            let parsed = synck_vcs::types::CommitId::parse(value).map_err(|error| {
                SubitemError::Schema(format!("{event_type} has invalid {key}: {error}"))
            })?;
            Ok(Some(parsed.to_string()))
        }
        Some(_) => Err(SubitemError::Schema(format!(
            "{event_type} {key} must be a commit ID or null"
        ))),
    }
}

pub fn validate_file_version_ref_description(
    meta: &serde_json::Value,
    event_type: &str,
    required: bool,
) -> Result<(), SubitemError> {
    let value = meta.get("description").or_else(|| meta.get("comment"));
    match value {
        None if required => Err(SubitemError::Schema(format!(
            "{event_type} requires description"
        ))),
        None | Some(serde_json::Value::Null) | Some(serde_json::Value::String(_)) => Ok(()),
        Some(_) => Err(SubitemError::Schema(format!(
            "{event_type} description must be a string or null"
        ))),
    }
}

pub fn validate_file_version_ref_content(
    content: &serde_json::Value,
    event_type: &str,
) -> Result<(String, Option<i64>), SubitemError> {
    let blob_id = file_version_blob_id_from_content(content).ok_or_else(|| {
        SubitemError::Schema(format!("{event_type} content must include blob_id"))
    })?;
    BlobId::parse(blob_id.as_str()).map_err(|error| {
        SubitemError::Schema(format!("{event_type} has invalid blob_id: {error}"))
    })?;
    let size = match content.get("size") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(value.as_i64().filter(|size| *size >= 0).ok_or_else(|| {
            SubitemError::Schema(format!("{event_type} size must be non-negative"))
        })?),
    };
    Ok((blob_id, size))
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ItemDataLink {
    pub link_key: String,
    pub database_key: String,
    pub table_key: String,
    pub record_id: Uuid,
    pub display_value: String,
}

pub fn structured_data_meta_required_str(
    meta: &serde_json::Value,
    key: &str,
    ctx: &str,
) -> Result<String, SubitemError> {
    meta.as_object()
        .and_then(|map| map.get(key))
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| SubitemError::Schema(format!("{ctx} metadata_json.{key} must be string")))
}

pub fn parse_structured_data_item_link_projection(
    meta: &serde_json::Value,
    ctx: &str,
) -> Result<ItemDataLink, SubitemError> {
    let link_key = structured_data_meta_required_str(meta, "link_key", ctx)?;
    let database_key = structured_data_meta_required_str(meta, "database_key", ctx)?;
    let table_key = structured_data_meta_required_str(meta, "table_key", ctx)?;
    let record_id_raw = structured_data_meta_required_str(meta, "record_id", ctx)?;
    let record_id = Uuid::parse_str(record_id_raw.as_str()).map_err(|e| {
        SubitemError::Schema(format!(
            "{ctx} metadata_json.record_id `{record_id_raw}` invalid: {e}"
        ))
    })?;
    let display_value = structured_data_meta_required_str(meta, "display_value", ctx)?;
    Ok(ItemDataLink {
        link_key,
        database_key,
        table_key,
        record_id,
        display_value,
    })
}
