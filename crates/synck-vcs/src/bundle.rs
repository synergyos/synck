//! Storage-independent canonical NDJSON bundle records and validation.
//!
//! The source schema in a header is provenance, not a target schema instruction.
//! Only the current bundle encoding is accepted.
use crate::config::SYNCKHUB_MAX_EVENT_PAYLOAD_BYTES;
use crate::types::{
    COMMIT_FORMAT_VERSION, EventProposal, RefEventProposal, RepoId, Sha256Digest,
    canonical_event_proposal_bytes, canonical_ref_event_proposal_bytes,
    compute_sha256_digest_bytes, sha256_digest_to_digest_bytes,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::BufRead;

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

pub const BUNDLE_FORMAT_VERSION: u32 = 5;

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
    pub exported_at: String,
    pub schema: BundleSchema,
    pub commit_format_version: i32,
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
    pub import_job_id: Option<String>,
    pub format_version: i32,
    pub author_id: String,
    pub committer_id: String,
    pub message: Option<String>,
    pub occurred_at: Option<String>,
    pub created_at: String,
    pub event_count: i32,
    pub events_digest: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleEventRecord {
    #[serde(rename = "type")]
    pub r#type: String,
    pub commit_id: String,
    pub repo_seq: i64,
    pub commit_event_idx: i32,
    pub item_id: String,
    pub item_seq: i64,
    pub event_type: String,
    pub payload_json: String,
    pub event_payload_digest: String,
    pub created_at: String,
    pub parent_folder_id: Option<String>,
    pub parent_folder_id_before: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleRefEventRecord {
    #[serde(rename = "type")]
    pub r#type: String,
    pub commit_id: String,
    pub repo_seq: i64,
    pub commit_event_idx: i32,
    pub owner_item_id: String,
    pub ref_seq: i64,
    pub event_type: String,
    pub payload_json: String,
    pub event_payload_digest: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleItemRecord {
    #[serde(rename = "type")]
    pub r#type: String,
    pub item_id: String,
    pub item_type: String,
    pub parent_folder_id: Option<String>,
    pub name: String,
    pub name_key: String,
    pub head_seq: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ref_head_seq: i64,
    pub lifecycle_state: String,
    pub created_at: String,
    pub created_by: String,
    pub sequencing_mode: String,
    pub federation_stream_id: Option<String>,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
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

fn decode_commit_record(
    value: serde_json::Value,
    bundle_format_version: u32,
) -> Result<BundleCommitRecord, String> {
    if bundle_format_version != BUNDLE_FORMAT_VERSION {
        return Err(format!(
            "unsupported bundle_format_version: {bundle_format_version}"
        ));
    }
    for field in ["operation_id", "import_job_id"] {
        if value.get(field).is_none() {
            return Err(format!("commit requires {field}"));
        }
    }
    let record: BundleCommitRecord =
        serde_json::from_value(value).map_err(|error| format!("decode commit: {error}"))?;
    if record
        .operation_id
        .as_deref()
        .is_some_and(|operation_id| operation_id.trim().is_empty())
    {
        return Err("operation_id must be null or nonblank".to_string());
    }
    Ok(record)
}

/// A single typed body record. Importers enforce section order and complete commits.
#[derive(Debug)]
pub enum BundleRecord {
    Metadata(BundleMetadataRecord),
    Commit(BundleCommitRecord),
    Event(BundleEventRecord),
    RefEvent(BundleRefEventRecord),
    Item(BundleItemRecord),
    FederationDomain(BundleFederationDomainRecord),
}

/// Read one body record, preserving the existing blank-line and strict-field rules.
pub fn read_record(reader: &mut dyn BufRead, version: u32) -> Result<Option<BundleRecord>, String> {
    if version != BUNDLE_FORMAT_VERSION {
        return Err(format!("unsupported bundle_format_version: {version}"));
    }
    let mut line = String::new();
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .map_err(|error| format!("read: {error}"))?
            == 0
        {
            return Ok(None);
        }
        if !line.trim_end().is_empty() {
            break;
        }
    }
    let value: serde_json::Value =
        serde_json::from_str(line.trim_end()).map_err(|error| format!("parse json: {error}"))?;
    let kind = value
        .get("type")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "record missing string field: type".to_string())?;
    let record = match kind {
        "metadata" => BundleRecord::Metadata(
            serde_json::from_value(value).map_err(|error| format!("decode metadata: {error}"))?,
        ),
        "commit" => BundleRecord::Commit(decode_commit_record(value, version)?),
        "event" => BundleRecord::Event(
            serde_json::from_value(value).map_err(|error| format!("decode event: {error}"))?,
        ),
        "ref_event" => BundleRecord::RefEvent(
            serde_json::from_value(value).map_err(|error| format!("decode ref_event: {error}"))?,
        ),
        "item" => BundleRecord::Item(
            serde_json::from_value(value).map_err(|error| format!("decode item: {error}"))?,
        ),
        "federation_domain" => BundleRecord::FederationDomain(
            serde_json::from_value(value)
                .map_err(|error| format!("decode federation_domain: {error}"))?,
        ),
        other => return Err(format!("unknown record type: {other}")),
    };
    Ok(Some(record))
}

/// A validated bundle header. The reader is positioned at the first body record.
pub struct BundleHeader {
    repo_id: RepoId,
    history_generation: String,
    bundle_format_version: u32,
    source_schema: BundleSchema,
}

impl BundleHeader {
    pub fn history_generation(&self) -> &str {
        &self.history_generation
    }
    pub fn repo_id(&self) -> &RepoId {
        &self.repo_id
    }

    pub fn bundle_format_version(&self) -> u32 {
        self.bundle_format_version
    }

    /// Physical source schema provenance; never applied to a target adapter.
    pub fn source_schema(&self) -> &BundleSchema {
        &self.source_schema
    }
}

/// Read and validate the header before acquiring repository authority.
pub fn read_header(reader: &mut dyn BufRead) -> Result<BundleHeader, String> {
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .map_err(|e| format!("read header: {e}"))?
        == 0
    {
        return Err("empty input".to_string());
    }
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
    if header.commit_format_version != COMMIT_FORMAT_VERSION {
        return Err(format!(
            "unsupported commit_format_version: {}",
            header.commit_format_version
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
        && header.schema.schema_id != "core-v7"
    {
        return Err("unsupported SQLite source schema provenance".into());
    }
    Ok(BundleHeader {
        repo_id: RepoId::parse(&header.repo_id).map_err(|e| e.to_string())?,
        bundle_format_version: header.bundle_format_version,
        source_schema: header.schema,
        history_generation: header.history_generation,
    })
}

pub fn parse_rfc3339_utc(s: &str) -> Result<DateTime<Utc>, String> {
    let dt =
        DateTime::parse_from_rfc3339(s).map_err(|e| format!("invalid rfc3339 timestamp: {e}"))?;
    Ok(dt.with_timezone(&Utc))
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
            if current <= prev {
                return Err(format!(
                    "repo_seq must be strictly increasing (prev {prev}, got {current})"
                ));
            }
        }
    }
    Ok(current)
}

// Apply the same supported canonical limits as fsck before retaining a commit's
// payloads. Aggregate accounting checks integer overflow, not a commit-size cap.
pub fn bounded_commit_payload_bytes(previous: usize, event: usize) -> Result<usize, String> {
    if event > SYNCKHUB_MAX_EVENT_PAYLOAD_BYTES {
        return Err("event payload exceeds supported canonical byte limit".into());
    }
    previous
        .checked_add(event)
        .ok_or_else(|| "commit payload exceeds supported canonical byte limit".into())
}

pub fn validate_bundle_event_payload_json_and_digest(
    payload_json: &str,
    event_payload_digest: &str,
) -> Result<[u8; 32], String> {
    let payload_bytes = payload_json.as_bytes();

    let parsed_ev: EventProposal =
        serde_json::from_str(payload_json).map_err(|e| format!("invalid payload_json: {e}"))?;
    let canonical = canonical_event_proposal_bytes(&parsed_ev)
        .map_err(|e| format!("payload_json does not canonicalize: {e}"))?;
    if canonical != payload_bytes {
        return Err("payload_json is not canonical EventProposal bytes".to_string());
    }

    let event_hash = compute_sha256_digest_bytes(payload_bytes);
    let expected_event_digest =
        Sha256Digest::parse(event_payload_digest).map_err(|e| e.to_string())?;
    if sha256_digest_to_digest_bytes(&expected_event_digest) != event_hash {
        return Err("event_payload_digest mismatch".to_string());
    }

    Ok(event_hash)
}

pub fn validate_bundle_ref_event_payload_json_and_digest(
    payload_json: &str,
    event_payload_digest: &str,
) -> Result<[u8; 32], String> {
    let payload_bytes = payload_json.as_bytes();
    let proposal: RefEventProposal =
        serde_json::from_str(payload_json).map_err(|e| format!("invalid payload_json: {e}"))?;
    let canonical = canonical_ref_event_proposal_bytes(&proposal)
        .map_err(|e| format!("payload_json does not canonicalize: {e}"))?;
    if canonical != payload_bytes {
        return Err("payload_json is not canonical RefEventProposal bytes".to_string());
    }
    let event_hash = compute_sha256_digest_bytes(payload_bytes);
    let expected_event_digest =
        Sha256Digest::parse(event_payload_digest).map_err(|e| e.to_string())?;
    if sha256_digest_to_digest_bytes(&expected_event_digest) != event_hash {
        return Err("event_payload_digest mismatch".to_string());
    }
    Ok(event_hash)
}

#[cfg(test)]
mod bundle_tests {
    use super::*;
    use crate::types::{
        CreateItem, EventContent, EventProposal, ItemId, canonical_event_proposal_bytes,
        compute_sha256_digest_bytes, sha256_digest_from_digest_bytes,
    };
    use uuid::Uuid;

    fn commit_json() -> serde_json::Value {
        serde_json::json!({
            "type": "commit",
            "commit_id": format!("sha256:{}", "a".repeat(64)),
            "checkpoint": format!("sha256:{}", "c".repeat(64)),
            "operation_id": null,
            "import_job_id": null,
            "repo_seq": 1,
            "format_version": COMMIT_FORMAT_VERSION,
            "author_id": "author",
            "committer_id": "author",
            "message": null,
            "occurred_at": null,
            "created_at": "2026-08-22T00:00:00Z",
            "event_count": 1,
            "events_digest": format!("sha256:{}", "b".repeat(64))
        })
    }

    #[test]
    fn bundle_payload_budget_bounds_events_and_checks_integer_overflow() {
        assert_eq!(
            bounded_commit_payload_bytes(0, SYNCKHUB_MAX_EVENT_PAYLOAD_BYTES).unwrap(),
            SYNCKHUB_MAX_EVENT_PAYLOAD_BYTES
        );
        assert!(bounded_commit_payload_bytes(0, SYNCKHUB_MAX_EVENT_PAYLOAD_BYTES + 1).is_err());
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
    fn bundle_commit_requires_explicit_optional_fields_and_current_version() {
        let value = commit_json();
        assert!(decode_commit_record(value.clone(), BUNDLE_FORMAT_VERSION).is_ok());
        for version in 1..BUNDLE_FORMAT_VERSION {
            assert!(decode_commit_record(value.clone(), version).is_err());
        }
        for field in ["operation_id", "import_job_id"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(decode_commit_record(missing, BUNDLE_FORMAT_VERSION).is_err());
        }
        let mut blank = value;
        blank["operation_id"] = serde_json::json!(" ");
        assert!(decode_commit_record(blank, BUNDLE_FORMAT_VERSION).is_err());
    }

    #[test]
    fn non_canonical_payload_json_is_detected_by_roundtrip() {
        let item_id = crate::types::ItemId::new();
        let payload_json = format!(
            r#"{{"event_type":"x","item_id":"{}","base_seq":1}}"#,
            item_id.as_str()
        );

        let parsed_ev: EventProposal = serde_json::from_str(&payload_json).expect("parse payload");
        let canonical = canonical_event_proposal_bytes(&parsed_ev).expect("canonicalize");
        assert_ne!(canonical, payload_json.as_bytes());
    }

    #[test]
    fn canonical_payload_json_digest_matches() {
        let item_id = crate::types::ItemId::new();
        let ev = EventProposal {
            item_id: Some(item_id),
            event_type: "x".to_string(),
            set_metadata: None,
            set_content: None,
            create_item: None,
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: Some(1),
        };

        let bytes = canonical_event_proposal_bytes(&ev).expect("canonical bytes");
        let hash = compute_sha256_digest_bytes(&bytes);
        let digest = Sha256Digest::parse(crate::types::sha256_prefixed_hex_from_digest_bytes(hash))
            .expect("sha256 digest parse");
        assert_eq!(sha256_digest_to_digest_bytes(&digest), hash);
    }

    #[test]
    fn bundle_event_payload_validation_is_strict_and_idempotent() {
        #[derive(Clone)]
        struct XorShift64 {
            state: u64,
        }

        impl XorShift64 {
            fn new(seed: u64) -> Self {
                let seed = if seed == 0 {
                    0x9e37_79b9_7f4a_7c15
                } else {
                    seed
                };
                Self { state: seed }
            }

            fn next_u64(&mut self) -> u64 {
                let mut x = self.state;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.state = x;
                x
            }

            fn next_bool(&mut self) -> bool {
                (self.next_u64() & 1) == 1
            }

            fn gen_usize(&mut self, max_exclusive: usize) -> usize {
                if max_exclusive == 0 {
                    return 0;
                }
                (self.next_u64() as usize) % max_exclusive
            }
        }

        fn gen_uuid_v7(rng: &mut XorShift64) -> Uuid {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&rng.next_u64().to_be_bytes());
            bytes[8..].copy_from_slice(&rng.next_u64().to_be_bytes());
            bytes[6] = (bytes[6] & 0x0f) | 0x70;
            bytes[8] = (bytes[8] & 0x3f) | 0x80;
            Uuid::from_bytes(bytes)
        }

        fn gen_item_id(rng: &mut XorShift64) -> ItemId {
            ItemId::parse(gen_uuid_v7(rng).to_string()).expect("generated ItemId must be UUIDv7")
        }

        fn gen_string(rng: &mut XorShift64, max_len: usize) -> String {
            const UNICODE: &[char] = &['é', 'ß', '中', 'Ω', 'λ', 'Ж', 'א', 'م'];
            let len = rng.gen_usize(max_len + 1);
            let mut s = String::with_capacity(len);
            for _ in 0..len {
                let bucket = rng.gen_usize(6);
                let ch = match bucket {
                    0 => (b'a' + (rng.gen_usize(26) as u8)) as char,
                    1 => (b'A' + (rng.gen_usize(26) as u8)) as char,
                    2 => (b'0' + (rng.gen_usize(10) as u8)) as char,
                    3 => match rng.gen_usize(8) {
                        0 => ' ',
                        1 => '\n',
                        2 => '\t',
                        3 => '\r',
                        4 => '"',
                        5 => '\\',
                        6 => '/',
                        _ => '\u{0000}',
                    },
                    4 => UNICODE[rng.gen_usize(UNICODE.len())],
                    _ => '-',
                };
                s.push(ch);
            }
            s
        }

        fn gen_event_proposal(rng: &mut XorShift64) -> EventProposal {
            EventProposal {
                item_id: Some(gen_item_id(rng)),
                event_type: gen_string(rng, 16),
                set_metadata: if rng.next_bool() {
                    Some(crate::metadata::MetadataRef::new(serde_json::json!({
                        "k": gen_string(rng, 16),
                        "n": (rng.next_u64() % 1_000_000) as i64
                    })).unwrap())
                } else {
                    None
                },
                set_content: if rng.next_bool() {
                    Some(EventContent::bytes(crate::types::ObjectId::from_digest([rng.next_u64() as u8;32])))
                } else {
                    None
                },
                create_item: if rng.next_bool() {
                    Some(CreateItem {
                        item_type: if rng.next_bool() {
                            "FILE".to_string()
                        } else {
                            "FOLDER".to_string()
                        },
                        parent_folder_id: if rng.next_bool() {
                            Some(gen_item_id(rng))
                        } else {
                            None
                        },
                        name: gen_string(rng, 32),
                    })
                } else {
                    None
                },
                set_parent: match rng.gen_usize(3) {
                    0 => None,
                    1 => Some(None),
                    _ => Some(Some(gen_item_id(rng))),
                },
                set_name: if rng.next_bool() {
                    Some(gen_string(rng, 32))
                } else {
                    None
                },
                set_tombstoned: None,
                set_in_trash: None,
                set_is_federated: None,
                set_federation_stream_id: None,
                base_seq: if rng.next_bool() {
                    Some((rng.next_u64() % 10_000) as i64)
                } else {
                    None
                },
            }
        }

        let iters: usize = std::env::var("SYNCK_VCS_FUZZ_ITERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(300);

        let mut rng = XorShift64::new(0x6275_6e64_6c65_2d31);
        for _ in 0..iters {
            let ev = gen_event_proposal(&mut rng);

            let canonical_bytes = canonical_event_proposal_bytes(&ev).expect("canonical bytes");
            let canonical_json = String::from_utf8(canonical_bytes.clone()).expect("utf8");

            let digest_bytes = compute_sha256_digest_bytes(&canonical_bytes);
            let digest = sha256_digest_from_digest_bytes(&digest_bytes)
                .expect("digest must parse from bytes");

            let validated = match validate_bundle_event_payload_json_and_digest(
                &canonical_json,
                digest.as_str(),
            ) {
                Ok(v) => v,
                Err(e) => {
                    let parsed: EventProposal =
                        serde_json::from_str(&canonical_json).expect("parse canonical");
                    let recanon = canonical_event_proposal_bytes(&parsed).expect("re-canonicalize");
                    let recanon_json = String::from_utf8(recanon).expect("utf8");
                    panic!(
                        "canonical payload must validate: {e}\norig={canonical_json}\nrecanon={recanon_json}"
                    );
                }
            };
            assert_eq!(validated, digest_bytes);

            let non_canonical = serde_json::to_string(&ev).expect("serde json");
            assert!(
                validate_bundle_event_payload_json_and_digest(&non_canonical, digest.as_str())
                    .is_err(),
                "non-canonical JSON must be rejected"
            );

            let with_ws = format!("  {canonical_json}\n");
            assert!(
                validate_bundle_event_payload_json_and_digest(&with_ws, digest.as_str()).is_err(),
                "whitespace-variant JSON must be rejected"
            );

            let mut bad = digest.as_str().to_string();
            let last = bad.pop().expect("non-empty digest");
            bad.push(if last == '0' { '1' } else { '0' });
            assert!(
                validate_bundle_event_payload_json_and_digest(&canonical_json, &bad).is_err(),
                "mismatched digest must be rejected"
            );
        }
    }

    #[test]
    fn bundle_records_reject_unknown_fields() {
        let json = r#"{
            "type": "synck_vcs_bundle_header",
            "bundle_format_version": 1,
            "repo_id": "01890c6c-1111-7777-8888-aaaaaaaaaaaa",
            "exported_at": "2025-01-01T00:00:00Z",
            "schema": { "subsystem": "synck_vcs_pg", "schema_id": "v1" },
            "commit_format_version": 2,
            "extra": true
        }"#;

        let err = serde_json::from_str::<BundleHeaderRecord>(json).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn import_repo_seq_is_strictly_increasing() {
        assert!(validate_import_repo_seq(None, 1).is_ok());
        assert!(validate_import_repo_seq(Some(1), 2).is_ok());
        assert!(validate_import_repo_seq(None, 2).is_err());
        assert!(validate_import_repo_seq(Some(2), 2).is_err());
        assert!(validate_import_repo_seq(Some(2), 1).is_err());
    }
}
