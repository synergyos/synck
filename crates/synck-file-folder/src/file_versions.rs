//! FILE named-version refs and label ordering.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use synck_vcs::types::{CommitId, ItemId, RefEventProposal, RepoId};
use uuid::Uuid;

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

/// One named version in a FILE's ref ledger (format.md "FILE revision labels").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub version_id: String,
    pub operation_id: String,
    pub live: bool,
    pub kind: String,
    pub label_source: String,
    pub order_key: i64,
    pub description: Option<String>,
    pub source_item_seq: i64,
    pub created_ref_seq: i64,
    pub current_ref_seq: i64,
}

/// A FILE's named versions, rebuilt by applying its ref events in order.
/// Readers that can open label metadata replay it here; the caller checks
/// each commit's shared `base_ref_seq` and resolves source revisions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub head_seq: i64,
    pub labels: BTreeMap<String, Label>,
}

/// A source revision named by `CREATE` or `SET_REVISION`, for the caller to
/// resolve against the FILE's content revisions.
pub struct Source<'a> {
    pub version_id: &'a str,
    pub item_seq: i64,
    pub commit_id: Option<CommitId>,
    pub commit_event_idx: Option<i32>,
}

impl Ledger {
    /// Applies the ref event at `head_seq + 1`. `source` must accept the
    /// revision a label is pointed at: a content revision of this FILE that
    /// precedes the event and agrees with any commit coordinates given.
    pub fn apply(
        &mut self,
        file_item_id: &ItemId,
        event_type: &str,
        metadata: &Map<String, Value>,
        mut source: impl FnMut(&Source<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        let ref_seq = self
            .head_seq
            .checked_add(1)
            .ok_or("FILE ref ledger overflow")?;
        if item_id(metadata, "file_item_id")? != *file_item_id {
            return Err(format!("{event_type} names the wrong FILE"));
        }
        let version_id = uuid(metadata, "version_id")?;
        match event_type {
            VERSION_CREATE_EVENT => {
                if Uuid::parse_str(&version_id).map_err(|e| e.to_string())?.get_version_num() != 7 {
                    return Err("new named-version ID is not UUIDv7".into());
                }
                if self.labels.contains_key(&version_id) {
                    return Err(format!("named version {version_id} is created more than once"));
                }
                let operation_id = string(metadata, "operation_id")?;
                if self.labels.values().any(|label| label.operation_id == operation_id) {
                    return Err(format!("named-version operation {operation_id} is reused"));
                }
                let source_item_seq = self.source(&version_id, metadata, &mut source)?;
                let label = Label {
                    version_id: version_id.clone(),
                    operation_id,
                    live: true,
                    kind: kind(metadata)?,
                    label_source: label_source(metadata)?,
                    order_key: int(metadata, "order_key")?,
                    description: match metadata.contains_key("description") {
                        true => description(metadata)?,
                        false => None,
                    },
                    source_item_seq,
                    created_ref_seq: ref_seq,
                    current_ref_seq: ref_seq,
                };
                self.labels.insert(version_id.clone(), label);
            }
            VERSION_SET_DESCRIPTION_EVENT => {
                let description = description(metadata)?;
                self.live(&version_id)?.description = description;
            }
            VERSION_SET_KIND_EVENT => {
                let kind = kind(metadata)?;
                self.live(&version_id)?.kind = kind;
            }
            VERSION_SET_LABEL_SOURCE_EVENT => {
                let label_source = label_source(metadata)?;
                self.live(&version_id)?.label_source = label_source;
            }
            VERSION_SET_ORDER_EVENT => {
                let order_key = int(metadata, "order_key")?;
                self.live(&version_id)?.order_key = order_key;
            }
            VERSION_SET_REVISION_EVENT => {
                self.live(&version_id)?;
                let source_item_seq = self.source(&version_id, metadata, &mut source)?;
                let description = match metadata.contains_key("description") {
                    true => Some(description(metadata)?),
                    false => None,
                };
                let label = self.live(&version_id)?;
                label.source_item_seq = source_item_seq;
                if let Some(description) = description {
                    label.description = description;
                }
            }
            VERSION_TOMBSTONE_EVENT => self.live(&version_id)?.live = false,
            other => return Err(format!("unsupported FILE_VERSION event {other}")),
        }
        if let Some(label) = self.labels.get_mut(&version_id) {
            label.current_ref_seq = ref_seq;
        }
        self.head_seq = ref_seq;
        Ok(())
    }

    /// Live labels in display order with their computed `major.minor` names.
    pub fn numbered(&self) -> Result<Vec<(&Label, String)>, String> {
        let mut live = self.labels.values().filter(|label| label.live).collect::<Vec<_>>();
        live.sort_by(|left, right| {
            (left.order_key, left.created_ref_seq, &left.version_id)
                .cmp(&(right.order_key, right.created_ref_seq, &right.version_id))
        });
        let (mut major, mut minor) = (0, 0);
        live.into_iter()
            .map(|label| {
                advance_label(&label.kind, &mut major, &mut minor)?;
                Ok((label, format!("{major}.{minor:02}")))
            })
            .collect()
    }

    fn live(&mut self, version_id: &str) -> Result<&mut Label, String> {
        self.labels
            .get_mut(version_id)
            .filter(|label| label.live)
            .ok_or_else(|| format!("mutation references no live named version {version_id}"))
    }

    /// Checks a label's new source; only one live label names a revision.
    fn source(
        &self,
        version_id: &str,
        metadata: &Map<String, Value>,
        check: &mut impl FnMut(&Source<'_>) -> Result<(), String>,
    ) -> Result<i64, String> {
        let item_seq = int(metadata, "source_item_seq")?;
        if item_seq <= 0 {
            return Err("source_item_seq must be greater than zero".into());
        }
        let commit_id = match metadata.get("source_commit_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) => Some(CommitId::parse(value).map_err(|e| e.to_string())?),
            Some(_) => return Err("source_commit_id must be a commit ID or null".into()),
        };
        let commit_event_idx = match metadata.get("source_commit_event_idx") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_i64()
                    .and_then(|value| i32::try_from(value).ok())
                    .filter(|value| *value >= 0)
                    .ok_or("source_commit_event_idx must be a non-negative i32")?,
            ),
        };
        if let Some(other) = self.labels.values().find(|label| {
            label.live && label.source_item_seq == item_seq && label.version_id != version_id
        }) {
            return Err(format!(
                "named versions {} and {version_id} both reference live FILE revision {item_seq}",
                other.version_id
            ));
        }
        check(&Source { version_id, item_seq, commit_id, commit_event_idx })?;
        Ok(item_seq)
    }
}

fn string(metadata: &Map<String, Value>, key: &str) -> Result<String, String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("FILE_VERSION event requires {key}"))
}

fn int(metadata: &Map<String, Value>, key: &str) -> Result<i64, String> {
    metadata
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("FILE_VERSION event requires {key}"))
}

fn uuid(metadata: &Map<String, Value>, key: &str) -> Result<String, String> {
    Uuid::parse_str(&string(metadata, key)?)
        .map(|value| value.to_string())
        .map_err(|error| format!("invalid {key}: {error}"))
}

fn item_id(metadata: &Map<String, Value>, key: &str) -> Result<ItemId, String> {
    ItemId::parse(string(metadata, key)?).map_err(|error| format!("invalid {key}: {error}"))
}

fn kind(metadata: &Map<String, Value>) -> Result<String, String> {
    normalize_kind(Some(&string(metadata, "kind")?)).map(str::to_string)
}

fn label_source(metadata: &Map<String, Value>) -> Result<String, String> {
    normalize_label_source(Some(&string(metadata, "label_source")?)).map(str::to_string)
}

fn description(metadata: &Map<String, Value>) -> Result<Option<String>, String> {
    match metadata.get("description") {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            Ok(Some(value.trim()).filter(|value| !value.is_empty()).map(str::to_string))
        }
        Some(_) => Err("version description must be a string or null".into()),
        None => Err("version description mutation has no description".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revision_pointer_matches_language_neutral_vector() {
        use synck_vcs::{metadata::Metadata, types::canonical_ref_event_proposal_bytes};
        let vector: serde_json::Value = serde_json::from_str(include_str!("../../../docs/spec/vectors/file-version.json")).unwrap();
        for vector in [&vector, &vector["with_source_commit"]] {
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
    }

    #[test]
    fn ledger_replays_labels_and_refuses_invalid_refs() {
        let file = ItemId::parse("01890c6c-1111-7777-8888-aaaaaaaaaaaa").unwrap();
        let (a, b) = (Uuid::now_v7().to_string(), Uuid::now_v7().to_string());
        let meta = |value: serde_json::Value| value.as_object().unwrap().clone();
        let create = |id: &str, seq: i64, op: &str| meta(serde_json::json!({
            "file_item_id": file.to_string(), "version_id": id, "operation_id": op,
            "source_item_seq": seq, "kind": "MINOR", "label_source": "INTERNAL", "order_key": 1024}));
        let ok = |_: &Source<'_>| Ok(());
        let mut ledger = Ledger::default();
        ledger.apply(&file, VERSION_CREATE_EVENT, &create(&a, 1, "one"), ok).unwrap();
        // One live label per revision, and one create per operation.
        assert!(ledger.clone().apply(&file, VERSION_CREATE_EVENT, &create(&b, 1, "two"), ok).is_err());
        assert!(ledger.clone().apply(&file, VERSION_CREATE_EVENT, &create(&b, 2, "one"), ok).is_err());
        // The caller decides whether a source revision exists.
        assert!(ledger.clone().apply(&file, VERSION_CREATE_EVENT, &create(&b, 2, "two"), |_| Err("missing".into())).is_err());
        ledger.apply(&file, VERSION_CREATE_EVENT, &create(&b, 2, "two"), ok).unwrap();
        let set = |id: &str, fields: serde_json::Value| {
            let mut value = meta(fields);
            value.insert("file_item_id".into(), file.to_string().into());
            value.insert("version_id".into(), id.into());
            value
        };
        ledger.apply(&file, VERSION_SET_KIND_EVENT, &set(&b, serde_json::json!({"kind": "MAJOR"})), ok).unwrap();
        ledger.apply(&file, VERSION_SET_DESCRIPTION_EVENT, &set(&a, serde_json::json!({"description": " first "})), ok).unwrap();
        let names = ledger.numbered().unwrap().into_iter().map(|(label, name)| (label.version_id.clone(), name)).collect::<Vec<_>>();
        assert_eq!(names, [(a.clone(), "0.01".into()), (b.clone(), "1.00".into())]);
        assert_eq!(ledger.labels[&a].description.as_deref(), Some("first"));
        ledger.apply(&file, VERSION_TOMBSTONE_EVENT, &set(&a, serde_json::json!({})), ok).unwrap();
        assert!(ledger.clone().apply(&file, VERSION_SET_ORDER_EVENT, &set(&a, serde_json::json!({"order_key": 1})), ok).is_err());
        // A tombstoned label frees its revision.
        ledger.apply(&file, VERSION_SET_REVISION_EVENT, &set(&b, serde_json::json!({"source_item_seq": 1})), ok).unwrap();
        assert_eq!((ledger.head_seq, ledger.labels[&b].source_item_seq, ledger.labels[&b].current_ref_seq), (6, 1, 6));
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
