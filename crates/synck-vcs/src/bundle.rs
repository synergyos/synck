//! Storage-independent NDJSON bundle records and validation.
//!
//! The source schema in a header is provenance, not a target schema instruction.
//! Only the current bundle encoding is accepted.
use crate::errors::CoreError;
use crate::limits::{MAX_BUNDLE_RECORD_BYTES, MAX_EVENT_PAYLOAD_BYTES};
use crate::types::{
    EventType, ItemId, RepoId, Sha256Digest, decode_event_proposal, decode_ref_event_proposal,
    sha256_prefixed_hex_from_digest_bytes,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead, Read};

/// Canonical commit outcome shared by offline bundle command wrappers.
#[derive(Debug)]
pub enum ImportFailure {
    BeforeCommit(String),
    AcceptedStateIncomplete { repo_id: RepoId, detail: String },
    CommitOutcomeUnknown { repo_id: RepoId, detail: String },
}

impl std::fmt::Display for ImportFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeCommit(detail)
            | Self::AcceptedStateIncomplete { detail, .. }
            | Self::CommitOutcomeUnknown { detail, .. } => detail.fmt(formatter),
        }
    }
}

pub const SCHEMA_META_SUBSYSTEM_VCS: &str = "synck_vcs_pg";
pub const SCHEMA_META_SUBSYSTEM_SQLITE: &str = "synck_vcs_sqlite";

pub const BUNDLE_FORMAT_VERSION: u32 = 7;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSchema {
    pub subsystem: String,
    pub schema_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleHeaderRecord {
    #[serde(rename = "type")]
    pub r#type: String,
    pub bundle_format_version: u32,
    pub repo_id: String,
    pub history_generation: String,
    pub encrypted: bool,
    pub exported_at: String,
    pub schema: BundleSchema,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleCommitRecord {
    #[serde(rename = "type")]
    pub r#type: String,
    pub commit_id: String,
    pub checkpoint: Sha256Digest,
    pub repo_seq: i64,
    pub operation_id: Option<String>,
    pub author_id: String,
    pub committer_id: String,
    pub message: Option<String>,
    pub occurred_at: Option<String>,
    pub created_at: String,
    pub event_count: i32,
}

/// An item event of the commit record it follows. Its item ID and event type
/// are those of its envelope.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEventRecord {
    #[serde(rename = "type")]
    pub r#type: String,
    pub commit_event_idx: i32,
    pub item_seq: i64,
    pub payload_hex: String,
}

/// A ref event of the commit record it follows. Its owner and event type are
/// those of its envelope.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleRefEventRecord {
    #[serde(rename = "type")]
    pub r#type: String,
    pub commit_event_idx: i32,
    pub ref_seq: i64,
    pub payload_hex: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleFederationDomainRecord {
    #[serde(rename = "type")]
    pub r#type: String,
    pub domain_id: String,
    pub last_domain_seq: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleMetadataRecord {
    #[serde(rename = "type")]
    pub r#type: String,
    pub object_id: crate::types::ObjectId,
    pub payload_hex: String,
}

/// The last record of a bundle. It covers every byte before it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEndRecord {
    #[serde(rename = "type")]
    pub r#type: String,
    pub record_count: u64,
    pub checkpoint: Sha256Digest,
    pub digest: Sha256Digest,
}

fn require_fields(value: &serde_json::Value, kind: &str, fields: &[&str]) -> Result<(), String> {
    for field in fields {
        if value.get(field).is_none() {
            return Err(format!("{kind} requires {field}"));
        }
    }
    Ok(())
}

fn decode_commit_record(value: serde_json::Value) -> Result<BundleCommitRecord, String> {
    require_fields(
        &value,
        "commit",
        &["operation_id", "message", "occurred_at"],
    )?;
    let record: BundleCommitRecord =
        serde_json::from_value(value).map_err(|error| format!("decode commit: {error}"))?;
    if record
        .operation_id
        .as_deref()
        .is_some_and(|operation_id| operation_id.trim().is_empty())
    {
        return Err("operation_id must be null or nonblank".to_string());
    }
    crate::limits::validate_commit_metadata(
        &record.author_id,
        &record.committer_id,
        record.message.as_deref(),
        record.operation_id.as_deref(),
    )
    .map_err(|e| e.to_string())?;
    Ok(record)
}

/// A single typed body record. Importers enforce section order and complete commits.
#[derive(Debug)]
pub enum BundleRecord {
    Metadata(BundleMetadataRecord),
    Commit(BundleCommitRecord),
    Event(BundleEventRecord),
    RefEvent(BundleRefEventRecord),
    FederationDomain(BundleFederationDomainRecord),
}

/// Reads a bundle in order and checks its end record. Only `read_record` returning
/// `None` proves the reader saw the whole bundle.
pub struct BundleReader<'a> {
    reader: &'a mut dyn BufRead,
    hasher: Sha256,
    record_count: u64,
    checkpoint: Option<Sha256Digest>,
}

impl<'a> BundleReader<'a> {
    pub fn new(reader: &'a mut dyn BufRead) -> Self {
        Self {
            reader,
            hasher: Sha256::new(),
            record_count: 0,
            checkpoint: None,
        }
    }

    /// The end record's checkpoint, once `read_record` has returned `None`.
    pub fn checkpoint(&self) -> Option<&Sha256Digest> {
        self.checkpoint.as_ref()
    }

    /// Read one line, which every record ends with. `None` is end of input.
    fn line(&mut self) -> Result<Option<String>, String> {
        let mut line = String::new();
        if (&mut self.reader)
            .take(MAX_BUNDLE_RECORD_BYTES + 1)
            .read_line(&mut line)
            .map_err(|error| format!("read: {error}"))?
            == 0
        {
            return Ok(None);
        }
        if line.len() as u64 > MAX_BUNDLE_RECORD_BYTES {
            return Err("bundle record exceeds its size bound".to_string());
        }
        if !line.ends_with('\n') {
            return Err("bundle record has no final line feed".to_string());
        }
        Ok(Some(line))
    }

    fn accept(&mut self, line: &str) {
        self.hasher.update(line.as_bytes());
        self.record_count += 1;
    }

    fn finish(&mut self, value: serde_json::Value) -> Result<(), String> {
        let end: BundleEndRecord =
            serde_json::from_value(value).map_err(|error| format!("decode end: {error}"))?;
        if end.record_count != self.record_count {
            return Err("bundle record count mismatch".to_string());
        }
        let digest: [u8; 32] = std::mem::take(&mut self.hasher).finalize().into();
        if end.digest.as_str() != sha256_prefixed_hex_from_digest_bytes(digest) {
            return Err("bundle digest mismatch".to_string());
        }
        if self
            .line()
            .map_err(|_| "bytes after the end record")?
            .is_some()
        {
            return Err("bytes after the end record".to_string());
        }
        self.checkpoint = Some(end.checkpoint);
        Ok(())
    }

    /// Read one body record. Blank lines are skipped, and hashed.
    pub fn read_record(&mut self) -> Result<Option<BundleRecord>, String> {
        let line = loop {
            let Some(line) = self.line()? else {
                return Err("bundle has no end record".to_string());
            };
            if line != "\n" {
                break line;
            }
            self.hasher.update(line.as_bytes());
        };
        let value: serde_json::Value = serde_json::from_str(line.trim_end())
            .map_err(|error| format!("parse json: {error}"))?;
        let kind = value
            .get("type")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "record missing string field: type".to_string())?
            .to_string();
        if kind == "synck_vcs_bundle_end" {
            self.finish(value)?;
            return Ok(None);
        }
        self.accept(&line);
        decode_record(&kind, value).map(Some)
    }

    /// Read and validate the header before acquiring repository authority.
    pub fn read_header(&mut self) -> Result<BundleHeader, String> {
        let Some(line) = self.line()? else {
            return Err("empty input".to_string());
        };
        self.accept(&line);
        decode_header(&line)
    }
}

fn decode_record(kind: &str, value: serde_json::Value) -> Result<BundleRecord, String> {
    Ok(match kind {
        "metadata" => BundleRecord::Metadata(
            serde_json::from_value(value).map_err(|error| format!("decode metadata: {error}"))?,
        ),
        "commit" => BundleRecord::Commit(decode_commit_record(value)?),
        "event" => BundleRecord::Event(
            serde_json::from_value(value).map_err(|error| format!("decode event: {error}"))?,
        ),
        "ref_event" => BundleRecord::RefEvent(
            serde_json::from_value(value).map_err(|error| format!("decode ref_event: {error}"))?,
        ),
        "federation_domain" => BundleRecord::FederationDomain(
            serde_json::from_value(value)
                .map_err(|error| format!("decode federation_domain: {error}"))?,
        ),
        other => return Err(format!("unknown record type: {other}")),
    })
}

/// A validated bundle header. The reader is positioned at the first body record.
pub struct BundleHeader {
    repo_id: RepoId,
    history_generation: String,
    encrypted: bool,
    source_schema: BundleSchema,
}

impl BundleHeader {
    pub fn history_generation(&self) -> &str {
        &self.history_generation
    }
    pub fn repo_id(&self) -> &RepoId {
        &self.repo_id
    }

    pub fn encrypted(&self) -> bool {
        self.encrypted
    }

    /// The checkpoint an empty history ends on, and the first commit extends.
    pub fn genesis(&self) -> Result<[u8; 32], CoreError> {
        crate::checkpoint::genesis(&self.repo_id, &self.history_generation, self.encrypted)
    }

    /// Physical source schema provenance; never applied to a target adapter.
    pub fn source_schema(&self) -> &BundleSchema {
        &self.source_schema
    }
}

fn decode_header(line: &str) -> Result<BundleHeader, String> {
    let value: serde_json::Value =
        serde_json::from_str(line.trim_end()).map_err(|e| format!("parse header json: {e}"))?;
    let kind = value
        .get("type")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "header missing string field: type".to_string())?;
    if kind != "synck_vcs_bundle_header" {
        return Err(format!(
            "first record must be synck_vcs_bundle_header, got {kind}"
        ));
    }
    let header: BundleHeaderRecord =
        serde_json::from_value(value).map_err(|e| format!("decode header: {e}"))?;
    if header.bundle_format_version != BUNDLE_FORMAT_VERSION {
        return Err(format!(
            "unsupported bundle_format_version: {}",
            header.bundle_format_version
        ));
    }
    let generation =
        uuid::Uuid::parse_str(&header.history_generation).map_err(|e| e.to_string())?;
    if generation.get_version_num() != 7 || generation.to_string() != header.history_generation {
        return Err("history_generation must be canonical UUIDv7".into());
    }
    if !matches!(
        header.schema.subsystem.as_str(),
        SCHEMA_META_SUBSYSTEM_VCS | SCHEMA_META_SUBSYSTEM_SQLITE
    ) {
        return Err(format!(
            "unsupported schema subsystem: {}",
            header.schema.subsystem
        ));
    }
    if header.schema.subsystem == SCHEMA_META_SUBSYSTEM_SQLITE
        && header.schema.schema_id != "core-v8"
    {
        return Err("unsupported SQLite source schema provenance".into());
    }
    Ok(BundleHeader {
        repo_id: RepoId::parse(&header.repo_id).map_err(|e| e.to_string())?,
        encrypted: header.encrypted,
        source_schema: header.schema,
        history_generation: header.history_generation,
    })
}

pub fn parse_rfc3339_utc(s: &str) -> Result<DateTime<Utc>, String> {
    let dt =
        DateTime::parse_from_rfc3339(s).map_err(|e| format!("invalid rfc3339 timestamp: {e}"))?;
    let dt = dt.with_timezone(&Utc);
    if !crate::types::timestamp_has_format_year(dt) {
        return Err("timestamp UTC year must be 0000 through 9999".to_string());
    }
    Ok(dt)
}

pub fn validate_import_repo_seq(prev: Option<i64>, current: i64) -> Result<i64, String> {
    if current < 1 {
        return Err("repo_seq must be >= 1".to_string());
    }
    match prev {
        None => {
            if current != 1 {
                return Err(format!(
                    "first commit must have repo_seq = 1 (got {current})"
                ));
            }
        }
        Some(prev) => {
            if prev.checked_add(1) != Some(current) {
                return Err(format!(
                    "repo_seq must be consecutive (prev {prev}, got {current})"
                ));
            }
        }
    }
    Ok(current)
}

// Apply the same supported canonical limits as fsck before retaining a commit's
// payloads. Aggregate accounting checks integer overflow, not a commit-size cap.
pub fn bounded_commit_payload_bytes(previous: usize, event: usize) -> Result<usize, String> {
    if event > MAX_EVENT_PAYLOAD_BYTES {
        return Err("event payload exceeds supported canonical byte limit".into());
    }
    previous
        .checked_add(event)
        .ok_or_else(|| "commit payload exceeds supported canonical byte limit".into())
}

/// The exact envelope a `payload_hex` spells, lowercase hexadecimal of bytes
/// that decode strictly as an item envelope naming its item, and its item ID
/// and event type.
pub fn bundle_event_payload(payload_hex: &str) -> Result<(Vec<u8>, ItemId, EventType), String> {
    let bytes = lower_hex(payload_hex)?;
    let proposal = decode_event_proposal(&bytes).map_err(|error| error.to_string())?;
    let item_id = proposal
        .item_id
        .ok_or("an accepted envelope names its item")?;
    Ok((bytes, item_id, proposal.event_type))
}

/// As [`bundle_event_payload`], for an attached-ref envelope and its owner.
pub fn bundle_ref_event_payload(payload_hex: &str) -> Result<(Vec<u8>, ItemId, EventType), String> {
    let bytes = lower_hex(payload_hex)?;
    let proposal = decode_ref_event_proposal(&bytes).map_err(|error| error.to_string())?;
    Ok((bytes, proposal.owner_item_id, proposal.event_type))
}

fn lower_hex(text: &str) -> Result<Vec<u8>, String> {
    if text.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err("payload_hex must be lowercase".to_string());
    }
    hex::decode(text).map_err(|error| format!("invalid payload_hex: {error}"))
}

#[cfg(test)]
mod bundle_tests {
    use super::*;
    use crate::types::{EventProposal, canonical_event_proposal_bytes};

    fn commit_json() -> serde_json::Value {
        serde_json::json!({
            "type": "commit",
            "commit_id": format!("sha256:{}", "a".repeat(64)),
            "checkpoint": format!("sha256:{}", "c".repeat(64)),
            "operation_id": null,
            "repo_seq": 1,
            "author_id": "author",
            "committer_id": "author",
            "message": null,
            "occurred_at": null,
            "created_at": "2026-08-22T00:00:00Z",
            "event_count": 1
        })
    }

    #[test]
    fn bundle_payload_budget_bounds_events_and_checks_integer_overflow() {
        assert_eq!(
            bounded_commit_payload_bytes(0, MAX_EVENT_PAYLOAD_BYTES).unwrap(),
            MAX_EVENT_PAYLOAD_BYTES
        );
        assert!(bounded_commit_payload_bytes(0, MAX_EVENT_PAYLOAD_BYTES + 1).is_err());
        assert_eq!(
            bounded_commit_payload_bytes((16 * 1024 * 1024) - 1, 1).unwrap(),
            (16 * 1024 * 1024)
        );
        assert_eq!(
            bounded_commit_payload_bytes(16 * 1024 * 1024, 1).unwrap(),
            16 * 1024 * 1024 + 1
        );
        assert!(bounded_commit_payload_bytes(usize::MAX, 1).is_err());
    }

    #[test]
    fn a_record_past_the_size_bound_is_refused_before_it_is_parsed() {
        let mut input = std::io::Cursor::new(vec![b' '; MAX_BUNDLE_RECORD_BYTES as usize + 2]);
        let error = BundleReader::new(&mut input).read_record().unwrap_err();
        assert!(error.contains("size bound"), "{error}");
    }

    #[test]
    fn bundle_commit_requires_explicit_optional_fields() {
        let value = commit_json();
        assert!(decode_commit_record(value.clone()).is_ok());
        for field in ["operation_id", "message", "occurred_at"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(decode_commit_record(missing).is_err(), "{field}");
        }
        let mut blank = value;
        blank["operation_id"] = serde_json::json!(" ");
        assert!(decode_commit_record(blank).is_err());
    }

    #[test]
    fn a_payload_hex_is_lowercase_and_one_whole_envelope() {
        let event = EventProposal::from_fields(serde_json::json!({
            "item_id": ItemId::new(), "event_type": "ITEM::NOTE", "base_seq": 1
        }))
        .unwrap();
        let payload = canonical_event_proposal_bytes(&event).unwrap();
        let (bytes, item_id, event_type) = bundle_event_payload(&hex::encode(&payload)).unwrap();
        assert_eq!(bytes, payload);
        assert_eq!(Some(item_id), event.item_id);
        assert_eq!(event_type, "ITEM::NOTE");
        assert!(bundle_event_payload(&hex::encode_upper(&payload)).is_err());
        assert!(bundle_event_payload(&hex::encode([&payload[..], &[0]].concat())).is_err());
        assert!(bundle_ref_event_payload(&hex::encode(&payload)).is_err());
    }

    #[test]
    fn bundle_records_reject_unknown_fields() {
        let json = r#"{
            "type": "synck_vcs_bundle_header",
            "bundle_format_version": 1,
            "repo_id": "01890c6c-1111-7777-8888-aaaaaaaaaaaa",
            "exported_at": "2025-01-01T00:00:00Z",
            "schema": { "subsystem": "synck_vcs_pg", "schema_id": "v1" },
            "extra": true
        }"#;

        let err = serde_json::from_str::<BundleHeaderRecord>(json).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn import_repo_seq_is_consecutive() {
        assert!(validate_import_repo_seq(None, 1).is_ok());
        assert!(validate_import_repo_seq(Some(1), 2).is_ok());
        assert!(validate_import_repo_seq(None, 2).is_err());
        assert!(validate_import_repo_seq(Some(2), 2).is_err());
        assert!(validate_import_repo_seq(Some(2), 1).is_err());
        assert!(validate_import_repo_seq(Some(2), 4).is_err());
        assert!(validate_import_repo_seq(Some(i64::MAX), 1).is_err());
    }
}
