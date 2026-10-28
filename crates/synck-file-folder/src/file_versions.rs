//! FILE named-version refs and label ordering.

use synck_vcs::types::{ItemId, RefEventProposal, RepoId};

pub const VERSION_CREATE_EVENT: &str = "FILE_VERSION::CREATE";
pub const VERSION_SET_DESCRIPTION_EVENT: &str = "FILE_VERSION::SET_DESCRIPTION";
pub const VERSION_SET_KIND_EVENT: &str = "FILE_VERSION::SET_KIND";
pub const VERSION_SET_LABEL_SOURCE_EVENT: &str = "FILE_VERSION::SET_LABEL_SOURCE";
pub const VERSION_SET_ORDER_EVENT: &str = "FILE_VERSION::SET_ORDER";
pub const VERSION_SET_REVISION_EVENT: &str = "FILE_VERSION::SET_REVISION";
pub const VERSION_TOMBSTONE_EVENT: &str = "FILE_VERSION::TOMBSTONE";

pub const ORDER_KEY_GAP: i64 = 1024;
pub const KIND_MAJOR: &str = "MAJOR";
pub const KIND_MINOR: &str = "MINOR";
pub const LABEL_SOURCE_INTERNAL: &str = "INTERNAL";
pub const LABEL_SOURCE_EXTERNAL: &str = "EXTERNAL";

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
) -> Result<RefEventProposal, String> {
    let mut metadata = automatic_initial_metadata(
        repo_id,
        file_item_id,
        source_item_seq,
        source_commit_event_idx,
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
        set_content: None,
        base_ref_seq,
    })
}

fn automatic_initial_metadata(
    repo_id: &RepoId,
    file_item_id: &ItemId,
    source_item_seq: i64,
    source_commit_event_idx: i32,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    if source_item_seq <= 0 || source_commit_event_idx < 0 {
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
    Ok(metadata)
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
    fn revision_pointer_matches_language_neutral_vector() {
        use synck_vcs::{metadata::Metadata, types::canonical_ref_event_proposal_bytes};
        let vector: serde_json::Value = serde_json::from_str(include_str!("../../../docs/spec/vectors/file-version.json")).unwrap();
        let event: RefEventProposal = serde_json::from_value(vector["event"].clone()).unwrap();
        assert!(event.set_content.is_none());
        let id = event.set_metadata.as_ref().unwrap().id();
        let encoded_payload = vector["metadata_payload_hex"].as_str().unwrap();
        let payload = (0..encoded_payload.len()).step_by(2).map(|i| u8::from_str_radix(&encoded_payload[i..i + 2], 16).unwrap()).collect();
        let metadata = Metadata::verify(&id, payload).unwrap();
        assert_eq!(metadata.value(), &vector["metadata"]);
        let encoded = canonical_ref_event_proposal_bytes(&event).unwrap();
        assert_eq!(encoded, vector["canonical_event"].as_str().unwrap().as_bytes());
        assert_eq!(synck_vcs::types::sha256_prefixed_hex_from_digest_bytes(synck_vcs::types::compute_sha256_digest_bytes(&encoded)), vector["event_digest"].as_str().unwrap());
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
