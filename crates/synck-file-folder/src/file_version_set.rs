//! Portable FILE named-version events and frozen legacy sidecar compatibility.

use sha2::{Digest, Sha256};
use synck_vcs::types::{CreateItem, EventContent, EventProposal, ItemId, RefEventProposal, RepoId};
use uuid::Uuid;

pub const ITEM_TYPE: &str = "FILE_VERSION_SET";
pub const CREATE_EVENT: &str = "FILE_VERSION_SET::CREATE";
pub const VERSION_CREATE_EVENT: &str = "FILE_VERSION_SET::VERSION_CREATE";
pub const VERSION_SET_DESCRIPTION_EVENT: &str = "FILE_VERSION_SET::VERSION_SET_DESCRIPTION";
pub const VERSION_SET_KIND_EVENT: &str = "FILE_VERSION_SET::VERSION_SET_KIND";
pub const VERSION_SET_LABEL_SOURCE_EVENT: &str = "FILE_VERSION_SET::VERSION_SET_LABEL_SOURCE";
pub const VERSION_SET_ORDER_EVENT: &str = "FILE_VERSION_SET::VERSION_SET_ORDER";
pub const VERSION_REPLACE_CONTENT_EVENT: &str = "FILE_VERSION_SET::VERSION_REPLACE_CONTENT";
pub const VERSION_TOMBSTONE_EVENT: &str = "FILE_VERSION_SET::VERSION_TOMBSTONE";
pub const ORDER_KEY_GAP: i64 = 1024;
pub const KIND_MAJOR: &str = "MAJOR";
pub const KIND_MINOR: &str = "MINOR";
pub const LABEL_SOURCE_INTERNAL: &str = "INTERNAL";
pub const LABEL_SOURCE_EXTERNAL: &str = "EXTERNAL";

const ITEM_NAME_PREFIX: &str = ".__synck_file_version_set_";
const ITEM_NAME_PREFIX_UPPER_BOUND: &str = ".__synck_file_version_set`";

/// Returns whether the event belongs to the frozen legacy FILE version sidecar vocabulary.
pub fn is_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        CREATE_EVENT
            | VERSION_CREATE_EVENT
            | VERSION_SET_DESCRIPTION_EVENT
            | VERSION_SET_KIND_EVENT
            | VERSION_SET_LABEL_SOURCE_EVENT
            | VERSION_SET_ORDER_EVENT
            | VERSION_REPLACE_CONTENT_EVENT
            | VERSION_TOMBSTONE_EVENT
    )
}

/// Derive the one portable sidecar identity owned by a FILE in a repository.
pub fn item_id(repo_id: &RepoId, file_item_id: &ItemId) -> Result<ItemId, String> {
    let repo_uuid = Uuid::parse_str(repo_id.as_str())
        .map_err(|error| format!("invalid repository UUID: {error}"))?;
    let file_uuid = Uuid::parse_str(file_item_id.as_str())
        .map_err(|error| format!("invalid FILE item UUID: {error}"))?;
    let mut hasher = Sha256::new();
    hasher.update(b"synckhub:file-version-set:item-id:v1");
    hasher.update(repo_uuid.as_bytes());
    hasher.update(file_uuid.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    ItemId::parse(Uuid::from_bytes(bytes).to_string())
        .map_err(|error| format!("generated FILE_VERSION_SET item ID is invalid: {error}"))
}

pub fn item_name(file_item_id: &ItemId) -> String {
    format!("{ITEM_NAME_PREFIX}{}", file_item_id.as_str())
}

/// Exclusive `name_key` bounds reserved for top-level version-set sidecars.
pub fn namespace_name_key_bounds() -> (&'static str, &'static str) {
    (ITEM_NAME_PREFIX, ITEM_NAME_PREFIX_UPPER_BOUND)
}

pub fn is_reserved_namespace_name_key(name_key: &str) -> bool {
    let (start, end) = namespace_name_key_bounds();
    name_key >= start && name_key < end
}

pub fn file_item_id_from_name(name: &str) -> Result<ItemId, String> {
    let value = name
        .strip_prefix(ITEM_NAME_PREFIX)
        .ok_or_else(|| "FILE_VERSION_SET create name must use sidecar prefix".to_string())?;
    ItemId::parse(value).map_err(|error| format!("invalid FILE_VERSION_SET owner ID: {error}"))
}

pub fn create_event(version_set_item_id: ItemId, file_item_id: &ItemId) -> EventProposal {
    EventProposal {
        item_id: Some(version_set_item_id),
        event_type: CREATE_EVENT.to_string(),
        set_metadata: None,
        set_content: None,
        create_item: Some(CreateItem {
            item_type: ITEM_TYPE.to_string(),
            parent_folder_id: None,
            name: item_name(file_item_id),
        }),
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: None,
    }
}

pub fn version_event(
    version_set_item_id: ItemId,
    event_type: &'static str,
    file_item_id: &ItemId,
    version_id: &str,
    mut metadata: serde_json::Map<String, serde_json::Value>,
    content: Option<EventContent>,
) -> Result<EventProposal, String> {
    metadata.insert(
        "file_item_id".to_string(),
        serde_json::Value::String(file_item_id.to_string()),
    );
    metadata.insert(
        "version_id".to_string(),
        serde_json::Value::String(version_id.to_string()),
    );
    Ok(EventProposal {
        item_id: Some(version_set_item_id),
        event_type: event_type.to_string(),
        set_metadata: Some(synck_vcs::metadata::MetadataRef::new(serde_json::Value::Object(metadata)).map_err(|e| e.to_string())?),
        set_content: content,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: None,
    })
}

/// Builds the canonical legacy FILE_VERSION_SET events for historical tests
/// and compatibility validation.
///
/// The producer allocates `version_id`; deterministic readers pass the ID from
/// the event they are verifying and never generate replacement authority.
#[allow(clippy::too_many_arguments)]
pub fn automatic_initial_events(
    repo_id: &RepoId,
    file_item_id: &ItemId,
    version_id: &str,
    include_version_set_create: bool,
    source_item_seq: i64,
    source_commit_event_idx: i32,
    blob_id: String,
    size: Option<i64>,
) -> Result<Vec<EventProposal>, String> {
    let version_set_item_id = item_id(repo_id, file_item_id)?;
    let mut events = Vec::with_capacity(if include_version_set_create { 2 } else { 1 });
    if include_version_set_create {
        events.push(create_event(version_set_item_id.clone(), file_item_id));
    }
    let metadata = automatic_initial_metadata(
        repo_id,
        file_item_id,
        source_item_seq,
        source_commit_event_idx,
        size,
    )?;
    events.push(version_event(
        version_set_item_id,
        VERSION_CREATE_EVENT,
        file_item_id,
        version_id,
        metadata,
        Some(EventContent::bytes(synck_vcs::types::ObjectId::parse(&blob_id).map_err(|error| error.to_string())?)),
    )?);
    Ok(events)
}

/// Builds the canonical automatic initial named-version event directly on the
/// owning FILE's attached ref ledger.
#[allow(clippy::too_many_arguments)]
pub fn automatic_initial_ref_event(
    repo_id: &RepoId,
    file_item_id: &ItemId,
    version_id: &str,
    base_ref_seq: i64,
    source_item_seq: i64,
    source_commit_event_idx: i32,
    blob_id: String,
    size: Option<i64>,
) -> Result<RefEventProposal, String> {
    let mut metadata = automatic_initial_metadata(
        repo_id,
        file_item_id,
        source_item_seq,
        source_commit_event_idx,
        size,
    )?;
    metadata.insert(
        "file_item_id".to_string(),
        serde_json::Value::String(file_item_id.to_string()),
    );
    metadata.insert(
        "version_id".to_string(),
        serde_json::Value::String(version_id.to_string()),
    );
    Ok(RefEventProposal {
        owner_item_id: file_item_id.clone(),
        event_type: "FILE_VERSION::CREATE".to_string(),
        set_metadata: Some(synck_vcs::metadata::MetadataRef::new(serde_json::Value::Object(metadata)).map_err(|e| e.to_string())?),
        set_content: Some(EventContent::bytes(synck_vcs::types::ObjectId::parse(&blob_id).map_err(|error| error.to_string())?)),
        base_ref_seq,
    })
}

fn automatic_initial_metadata(
    repo_id: &RepoId,
    file_item_id: &ItemId,
    source_item_seq: i64,
    source_commit_event_idx: i32,
    size: Option<i64>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    if source_item_seq <= 0 || source_commit_event_idx < 0 || size.is_some_and(|value| value < 0) {
        return Err("automatic initial version has invalid source coordinates or size".to_string());
    }
    let mut metadata = serde_json::Map::new();
    metadata.insert("kind".to_string(), KIND_MINOR.into());
    metadata.insert("label_source".to_string(), LABEL_SOURCE_INTERNAL.into());
    metadata.insert("order_key".to_string(), ORDER_KEY_GAP.into());
    metadata.insert(
        "operation_id".to_string(),
        format!(
            "synckhub:file-version:auto-initial:v1:{}:{}:{source_item_seq}",
            repo_id.as_str(),
            file_item_id.as_str()
        )
        .into(),
    );
    metadata.insert("source_item_seq".to_string(), source_item_seq.into());
    metadata.insert(
        "source_commit_event_idx".to_string(),
        i64::from(source_commit_event_idx).into(),
    );
    if let Some(size) = size {
        metadata.insert("content".to_string(), serde_json::json!({ "size": size }));
    }
    Ok(metadata)
}

pub fn validate_identities(repo_id: &RepoId, events: &[EventProposal]) -> Result<(), String> {
    for event in events {
        if event
            .create_item
            .as_ref()
            .is_some_and(|create| create.item_type == ITEM_TYPE)
            && event.event_type != CREATE_EVENT
        {
            return Err("FILE_VERSION_SET create must use FILE_VERSION_SET::CREATE".to_string());
        }
        if event.event_type == CREATE_EVENT {
            let create = event
                .create_item
                .as_ref()
                .ok_or_else(|| "FILE_VERSION_SET::CREATE has no create_item".to_string())?;
            if create.item_type != ITEM_TYPE || create.parent_folder_id.is_some() {
                return Err("FILE_VERSION_SET::CREATE has an invalid item shape".to_string());
            }
            let file_item_id = file_item_id_from_name(&create.name)?;
            let expected = item_id(repo_id, &file_item_id)?;
            if event.item_id.as_ref() != Some(&expected) {
                return Err(
                    "FILE_VERSION_SET item_id must be deterministic for repo_id and file_item_id"
                        .to_string(),
                );
            }
        } else if event.event_type.starts_with("FILE_VERSION_SET::VERSION_") {
            let metadata = event
                .set_metadata
                .as_ref()
                .and_then(|value| value.as_object())
                .ok_or_else(|| format!("{} has no metadata object", event.event_type))?;
            let file_item_id = metadata
                .get("file_item_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("{} has no file_item_id", event.event_type))
                .and_then(|value| {
                    ItemId::parse(value).map_err(|error| {
                        format!("{} has invalid file_item_id: {error}", event.event_type)
                    })
                })?;
            let expected = item_id(repo_id, &file_item_id)?;
            if event.item_id.as_ref() != Some(&expected) {
                return Err(
                    "FILE_VERSION_SET item_id must be deterministic for repo_id and file_item_id"
                        .to_string(),
                );
            }
        }
    }
    Ok(())
}

pub fn normalize_kind(kind: Option<&str>) -> Result<&'static str, String> {
    match kind.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some(KIND_MAJOR) => Ok(KIND_MAJOR),
        Some(KIND_MINOR) => Ok(KIND_MINOR),
        Some(other) => Err(format!("kind must be MAJOR or MINOR (got {other})")),
    }
}

pub fn normalize_label_source(value: Option<&str>) -> Result<&'static str, String> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some(LABEL_SOURCE_INTERNAL) => Ok(LABEL_SOURCE_INTERNAL),
        Some(LABEL_SOURCE_EXTERNAL) => Ok(LABEL_SOURCE_EXTERNAL),
        Some(other) => Err(format!(
            "label_source must be INTERNAL or EXTERNAL (got {other})"
        )),
    }
}

pub fn advance_label(kind: &str, major: &mut i32, minor: &mut i32) -> Result<(), String> {
    match kind {
        KIND_MAJOR => {
            *major = major
                .checked_add(1)
                .ok_or_else(|| "file version major label overflow".to_string())?;
            *minor = 0;
        }
        KIND_MINOR => {
            *minor = minor
                .checked_add(1)
                .ok_or_else(|| "file version minor label overflow".to_string())?;
        }
        other => return Err(format!("invalid file version kind {other}")),
    }
    Ok(())
}

pub fn next_label<'a>(
    existing_kinds: impl IntoIterator<Item = &'a str>,
    new_kind: &str,
) -> Result<(i32, i32), String> {
    let (mut major, mut minor) = (0, 0);
    for kind in existing_kinds {
        advance_label(kind, &mut major, &mut minor)?;
    }
    advance_label(new_kind, &mut major, &mut minor)?;
    Ok((major, minor))
}

pub fn rebalanced_order_key(position: usize) -> Result<i64, String> {
    let one_based = i64::try_from(position)
        .map_err(|_| "file version ordering overflow".to_string())?
        .checked_add(1)
        .ok_or_else(|| "file version ordering overflow".to_string())?;
    one_based
        .checked_mul(ORDER_KEY_GAP)
        .ok_or_else(|| "file version ordering overflow".to_string())
}

pub fn choose_insert_order_key(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (None, None) => Some(ORDER_KEY_GAP),
        (Some(left), None) => left.checked_add(ORDER_KEY_GAP),
        (None, Some(right)) => right.checked_sub(ORDER_KEY_GAP),
        (Some(left), Some(right)) if left < right => {
            let gap = i128::from(right) - i128::from(left);
            (gap >= 2)
                .then(|| i128::from(left) + gap / 2)
                .and_then(|value| i64::try_from(value).ok())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_identity_is_deterministic_uuidv7() {
        let repo_id = RepoId::new();
        let file_item_id = ItemId::new();
        let first = item_id(&repo_id, &file_item_id).unwrap();
        assert_eq!(first, item_id(&repo_id, &file_item_id).unwrap());
        assert_eq!(
            Uuid::parse_str(first.as_str()).unwrap().get_version_num(),
            7
        );
    }

    #[test]
    fn sidecar_identity_matches_persisted_contract_vector() {
        let repo_id = RepoId::parse("01890f47-6a4b-7c8d-8e9f-0123456789ab").unwrap();
        let file_item_id = ItemId::parse("01890f47-6a4c-7d8e-9f01-123456789abc").unwrap();
        assert_eq!(
            item_id(&repo_id, &file_item_id).unwrap().as_str(),
            "18612109-c1a5-767a-a48f-88a2deb83082"
        );
    }

    #[test]
    fn initial_events_use_the_producer_version_id() {
        let repo_id = RepoId::new();
        let file_item_id = ItemId::new();
        let version_id = "01890f47-6a4d-7e9f-8012-23456789abcd";
        let events = automatic_initial_events(
            &repo_id,
            &file_item_id,
            version_id,
            true,
            2,
            1,
            format!("sha256:{}", "0".repeat(64)),
            Some(7),
        )
        .unwrap();

        assert_eq!(
            events[1]
                .set_metadata
                .as_ref()
                .and_then(|metadata| metadata.get("version_id"))
                .and_then(serde_json::Value::as_str),
            Some(version_id)
        );

        let ref_event = automatic_initial_ref_event(
            &repo_id,
            &file_item_id,
            version_id,
            4,
            2,
            1,
            format!("sha256:{}", "0".repeat(64)),
            Some(7),
        )
        .unwrap();
        assert_eq!(ref_event.owner_item_id, file_item_id);
        assert_eq!(ref_event.event_type, "FILE_VERSION::CREATE");
        assert_eq!(ref_event.base_ref_seq, 4);
        let metadata = ref_event.set_metadata.unwrap();
        assert_eq!(
            metadata
                .get("version_id")
                .and_then(serde_json::Value::as_str),
            Some(version_id)
        );
        assert_eq!(
            metadata
                .get("source_commit_event_idx")
                .and_then(serde_json::Value::as_i64),
            Some(1)
        );
    }

    #[test]
    fn initial_event_bytes_match_persisted_contract_vectors() {
        use synck_vcs::types::{
            canonical_event_proposal_bytes, canonical_ref_event_proposal_bytes,
        };

        let repo_id = RepoId::parse("01890f47-6a4b-7c8d-8e9f-0123456789ab").unwrap();
        let file_item_id = ItemId::parse("01890f47-6a4c-7d8e-9f01-123456789abc").unwrap();
        let version_id = "01890f47-6a4d-7e9f-8012-23456789abcd";
        let blob_id = format!("sha256:{}", "0".repeat(64));
        let events = automatic_initial_events(
            &repo_id,
            &file_item_id,
            version_id,
            true,
            2,
            1,
            blob_id.clone(),
            Some(7),
        )
        .unwrap();
        let ref_event = automatic_initial_ref_event(
            &repo_id,
            &file_item_id,
            version_id,
            4,
            2,
            1,
            blob_id,
            Some(7),
        )
        .unwrap();
        let expected_content = concat!(
            r#"{"blob_id":"sha256:"#,
            "0000000000000000000000000000000000000000000000000000000000000000",
            r#""}"#,
        );
        let expected_metadata = concat!(
            r#"{"content":{"size":7},"file_item_id":"01890f47-6a4c-7d8e-9f01-123456789abc","#,
            r#""kind":"MINOR","label_source":"INTERNAL","#,
            r#""operation_id":"synckhub:file-version:auto-initial:v1:"#,
            r#"01890f47-6a4b-7c8d-8e9f-0123456789ab:01890f47-6a4c-7d8e-9f01-123456789abc:2","#,
            r#""order_key":1024,"source_commit_event_idx":1,"source_item_seq":2,"#,
            r#""version_id":"01890f47-6a4d-7e9f-8012-23456789abcd"}"#,
        );
        assert_eq!(
            canonical_event_proposal_bytes(&events[0]).unwrap(),
            concat!(
                r#"{"create_item":{"item_type":"FILE_VERSION_SET","#,
                r#""name":".__synck_file_version_set_01890f47-6a4c-7d8e-9f01-123456789abc"},"#,
                r#""event_type":"FILE_VERSION_SET::CREATE","#,
                r#""item_id":"18612109-c1a5-767a-a48f-88a2deb83082"}"#,
            )
            .as_bytes(),
        );
        assert_eq!(
            canonical_event_proposal_bytes(&events[1]).unwrap(),
            format!(
                concat!(
                    r#"{{"event_type":"FILE_VERSION_SET::VERSION_CREATE","#,
                    r#""item_id":"18612109-c1a5-767a-a48f-88a2deb83082","#,
                    r#""set_content":{},"set_metadata":{}}}"#,
                ),
                expected_content, expected_metadata,
            )
            .as_bytes(),
        );
        assert_eq!(
            canonical_ref_event_proposal_bytes(&ref_event).unwrap(),
            format!(
                concat!(
                    r#"{{"base_ref_seq":4,"event_type":"FILE_VERSION::CREATE","#,
                    r#""owner_item_id":"01890f47-6a4c-7d8e-9f01-123456789abc","#,
                    r#""set_content":{},"set_metadata":{}}}"#,
                ),
                expected_content, expected_metadata,
            )
            .as_bytes(),
        );
    }

    #[test]
    fn sidecar_namespace_bounds_cover_only_the_reserved_prefix() {
        let (start, end) = namespace_name_key_bounds();
        assert!(start < end);
        assert!(is_reserved_namespace_name_key(&item_name(&ItemId::new())));
        assert!(!is_reserved_namespace_name_key("ordinary-file"));
        assert!(!is_reserved_namespace_name_key(end));
    }

    #[test]
    fn labels_follow_major_minor_contract() {
        assert_eq!(next_label([], KIND_MAJOR).unwrap(), (1, 0));
        assert_eq!(
            next_label([KIND_MAJOR, KIND_MINOR, KIND_MINOR], KIND_MINOR).unwrap(),
            (1, 3)
        );
    }
}
