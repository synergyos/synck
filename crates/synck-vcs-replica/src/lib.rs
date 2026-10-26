//! Bounded, storage-neutral wire contract for exact sealed-history replication.

use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use synck_vcs::config::{
    SYNCKHUB_MAX_AUTHOR_ID_BYTES, SYNCKHUB_MAX_COMMIT_MESSAGE_BYTES,
    SYNCKHUB_MAX_COMMIT_PAYLOAD_BYTES, SYNCKHUB_MAX_EVENT_PAYLOAD_BYTES,
    SYNCKHUB_MAX_EVENTS_PER_COMMIT, SYNCKHUB_MAX_OPERATION_ID_BYTES,
};
use synck_vcs::state::{FORMAT_VERSION as STATE_FORMAT_VERSION, RootHeader, StateNode};
use synck_vcs::types::{
    COMMIT_EVENT_FAMILY_ITEM, COMMIT_EVENT_FAMILY_REF, CommitId, CommitPk, EventProposal, ItemId,
    REF_COMMIT_FORMAT_VERSION, REF_IMPORT_COMMIT_FORMAT_VERSION, RefEventProposal, RepoId,
    Sha256Digest, canonical_event_proposal_bytes, canonical_ref_event_proposal_bytes,
    canonical_stored_commit_bytes_for_hash, compute_commit_id, compute_sha256_digest_bytes,
    sha256_prefixed_hex_from_digest_bytes,
};

pub const PROTOCOL_VERSION: u32 = 2;
pub const CORE_REPLAY_VERSION: u32 = 2;
pub const MAX_PAGE_COMMITS: usize = 64;
pub const MAX_PAGE_UNCOMPRESSED_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaCheckpoint {
    pub protocol_version: u32,
    pub core_replay_version: u32,
    pub state_format_version: u8,
    pub repo_id: RepoId,
    pub terminal_repo_seq: i64,
    pub terminal_commit_id: Option<CommitId>,
    pub state_root: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaCommitPage {
    pub protocol_version: u32,
    pub repo_id: RepoId,
    pub checkpoint_etag: String,
    pub groups: Vec<ReplicaCommitGroup>,
    pub last_scanned_repo_seq: i64,
    pub done: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaCommitGroup {
    pub commit: ReplicaCommit,
    pub events: Vec<ReplicaEvent>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaCommit {
    pub commit_pk: CommitPk,
    pub commit_id: CommitId,
    pub parent_commit_id: Option<CommitId>,
    pub repo_seq: i64,
    pub operation_id: Option<String>,
    pub format_version: i32,
    pub import_job_id: Option<String>,
    pub author_id: String,
    pub committer_id: String,
    pub message: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub event_count: i32,
    pub events_digest: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event_family", rename_all = "snake_case")]
pub enum ReplicaEvent {
    Item {
        commit_event_idx: i32,
        item_id: ItemId,
        item_seq: i64,
        event_type: String,
        payload_base64: String,
        event_payload_digest: Sha256Digest,
        created_at: DateTime<Utc>,
        parent_folder_id: Option<ItemId>,
        parent_folder_id_before: Option<ItemId>,
    },
    Ref {
        commit_event_idx: i32,
        owner_item_id: ItemId,
        ref_seq: i64,
        event_type: String,
        payload_base64: String,
        event_payload_digest: Sha256Digest,
        created_at: DateTime<Utc>,
    },
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ReplicaError {
    #[error("malformed replica data: {0}")]
    Malformed(String),
    #[error("replica data exceeds a fixed bound: {0}")]
    Limit(String),
    #[error("corrupt local replica: {0}")]
    Corrupt(String),
    #[error("replica storage failure: {0}")]
    Storage(String),
}

impl ReplicaCheckpoint {
    pub fn validate(&self) -> Result<(), ReplicaError> {
        if self.protocol_version != PROTOCOL_VERSION
            || self.core_replay_version != CORE_REPLAY_VERSION
            || self.state_format_version != STATE_FORMAT_VERSION
        {
            return Err(malformed("unsupported checkpoint version"));
        }
        if self.terminal_repo_seq < 0 {
            return Err(malformed("terminal_repo_seq must be nonnegative"));
        }
        match (self.terminal_repo_seq, self.terminal_commit_id.as_ref()) {
            (0, None) | (1.., Some(_)) => Ok(()),
            _ => Err(malformed("terminal sequence and commit ID disagree")),
        }
    }

    pub fn etag(&self) -> Result<String, ReplicaError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)
            .map_err(|error| malformed(format!("serialize checkpoint: {error}")))?;
        let mut hasher = Sha256::new();
        hasher.update(b"synck-vcs-replica-checkpoint\0");
        hasher.update(bytes);
        Ok(format!("\"sha256:{}\"", hex::encode(hasher.finalize())))
    }
}

pub fn empty_state_root(repo_id: &RepoId) -> Sha256Digest {
    let item_root = synck_vcs::state::hash_node_bytes(
        &synck_vcs::state::encode_node(&StateNode::ItemLeaf(Vec::new()))
            .expect("empty item node is valid"),
    );
    let namespace_root = synck_vcs::state::hash_node_bytes(
        &synck_vcs::state::encode_node(&StateNode::NamespaceLeaf(Vec::new()))
            .expect("empty namespace node is valid"),
    );
    let root = synck_vcs::state::hash_node_bytes(
        &synck_vcs::state::encode_node(&StateNode::Root(RootHeader {
            repo_id: repo_id.clone(),
            repo_seq: 0,
            commit_id: None,
            item_map_root: item_root,
            namespace_root,
        }))
        .expect("empty root node is valid"),
    );
    Sha256Digest::parse(root.to_string()).expect("state root uses the sha256 ID format")
}

pub fn decode_and_validate_page(
    bytes: &[u8],
    checkpoint: &ReplicaCheckpoint,
    after_repo_seq: i64,
    through_repo_seq: i64,
) -> Result<ReplicaCommitPage, ReplicaError> {
    if bytes.len() > MAX_PAGE_UNCOMPRESSED_BYTES {
        return Err(ReplicaError::Limit(format!(
            "page bytes={} max={MAX_PAGE_UNCOMPRESSED_BYTES}",
            bytes.len()
        )));
    }
    let page: ReplicaCommitPage = serde_json::from_slice(bytes)
        .map_err(|error| malformed(format!("decode page: {error}")))?;
    validate_page(&page, checkpoint, after_repo_seq, through_repo_seq)?;
    Ok(page)
}

pub fn validate_page(
    page: &ReplicaCommitPage,
    checkpoint: &ReplicaCheckpoint,
    after_repo_seq: i64,
    through_repo_seq: i64,
) -> Result<(), ReplicaError> {
    checkpoint.validate()?;
    if after_repo_seq < 0 || after_repo_seq > through_repo_seq {
        return Err(malformed("invalid requested sequence interval"));
    }
    if through_repo_seq != checkpoint.terminal_repo_seq {
        return Err(malformed("page terminal does not match checkpoint"));
    }
    if page.protocol_version != PROTOCOL_VERSION || page.repo_id != checkpoint.repo_id {
        return Err(malformed("page identity or version mismatch"));
    }
    if page.checkpoint_etag != checkpoint.etag()? {
        return Err(malformed("page checkpoint binding mismatch"));
    }
    if page.groups.len() > MAX_PAGE_COMMITS {
        return Err(ReplicaError::Limit(format!(
            "commit groups={} max={MAX_PAGE_COMMITS}",
            page.groups.len()
        )));
    }

    let mut previous = after_repo_seq;
    for group in &page.groups {
        validate_group(group)?;
        let sequence = group.commit.repo_seq;
        if sequence <= previous || sequence > through_repo_seq {
            return Err(malformed(
                "commit is outside the requested ordered interval",
            ));
        }
        previous = sequence;
    }
    if page.last_scanned_repo_seq != previous {
        return Err(malformed("last scanned cursor does not match the page"));
    }
    if page.done != (previous == through_repo_seq) {
        return Err(malformed("done does not match interval exhaustion"));
    }
    if !page.done && page.groups.is_empty() {
        return Err(malformed("an unfinished page must make progress"));
    }
    Ok(())
}

pub fn validate_group(group: &ReplicaCommitGroup) -> Result<(), ReplicaError> {
    let commit = &group.commit;
    if commit.repo_seq < 1 || commit.parent_commit_id.is_some() {
        return Err(malformed("invalid sealed commit header"));
    }
    if commit.author_id.is_empty() || commit.committer_id.is_empty() {
        return Err(malformed("author_id and committer_id must not be empty"));
    }
    validate_len("author_id", &commit.author_id, SYNCKHUB_MAX_AUTHOR_ID_BYTES)?;
    validate_len(
        "committer_id",
        &commit.committer_id,
        SYNCKHUB_MAX_AUTHOR_ID_BYTES,
    )?;
    if let Some(message) = commit.message.as_deref() {
        validate_len("message", message, SYNCKHUB_MAX_COMMIT_MESSAGE_BYTES)?;
    }
    if let Some(operation_id) = commit.operation_id.as_deref() {
        validate_len(
            "operation_id",
            operation_id,
            SYNCKHUB_MAX_OPERATION_ID_BYTES,
        )?;
        if operation_id.trim().is_empty() {
            return Err(malformed("operation_id must not be blank"));
        }
    }
    if commit.event_count < 1
        || usize::try_from(commit.event_count).ok() != Some(group.events.len())
        || group.events.len() > SYNCKHUB_MAX_EVENTS_PER_COMMIT
    {
        return Err(malformed("event count does not match the complete group"));
    }

    let mut events_hasher = Sha256::new();
    let mut total_payload = 0usize;
    let ref_format = matches!(
        commit.format_version,
        REF_COMMIT_FORMAT_VERSION | REF_IMPORT_COMMIT_FORMAT_VERSION
    );
    let mut refs_started = false;
    let mut saw_ref = false;
    for (index, event) in group.events.iter().enumerate() {
        let (commit_event_idx, sequence, event_type, payload_base64, event_payload_digest) =
            match event {
                ReplicaEvent::Item {
                    commit_event_idx,
                    item_seq,
                    event_type,
                    payload_base64,
                    event_payload_digest,
                    ..
                } => (
                    *commit_event_idx,
                    *item_seq,
                    event_type,
                    payload_base64,
                    event_payload_digest,
                ),
                ReplicaEvent::Ref {
                    commit_event_idx,
                    ref_seq,
                    event_type,
                    payload_base64,
                    event_payload_digest,
                    ..
                } => (
                    *commit_event_idx,
                    *ref_seq,
                    event_type,
                    payload_base64,
                    event_payload_digest,
                ),
            };
        if usize::try_from(commit_event_idx).ok() != Some(index) || sequence < 1 {
            return Err(malformed("event order or item sequence is invalid"));
        }
        let payload = base64::engine::general_purpose::STANDARD
            .decode(payload_base64)
            .map_err(|error| malformed(format!("invalid payload_base64: {error}")))?;
        if base64::engine::general_purpose::STANDARD.encode(&payload) != *payload_base64 {
            return Err(malformed("payload_base64 is not canonical padded base64"));
        }
        if payload.len() > SYNCKHUB_MAX_EVENT_PAYLOAD_BYTES {
            return Err(ReplicaError::Limit(
                "event payload is too large".to_string(),
            ));
        }
        total_payload = total_payload
            .checked_add(payload.len())
            .ok_or_else(|| ReplicaError::Limit("commit payload size overflow".to_string()))?;
        if total_payload > SYNCKHUB_MAX_COMMIT_PAYLOAD_BYTES {
            return Err(ReplicaError::Limit(
                "commit payload is too large".to_string(),
            ));
        }
        let digest = compute_sha256_digest_bytes(&payload);
        if sha256_prefixed_hex_from_digest_bytes(digest) != event_payload_digest.as_str() {
            return Err(malformed("event payload digest mismatch"));
        }
        match event {
            ReplicaEvent::Item { item_id, .. } => {
                if refs_started {
                    return Err(malformed("item event follows a ref event"));
                }
                let proposal: EventProposal =
                    serde_json::from_slice(&payload).map_err(|error| {
                        malformed(format!("invalid canonical item-event payload: {error}"))
                    })?;
                if canonical_event_proposal_bytes(&proposal)
                    .map_err(|error| malformed(error.to_string()))?
                    != payload
                {
                    return Err(malformed("item-event payload is not canonical"));
                }
                if proposal.item_id.as_ref() != Some(item_id) || proposal.event_type != *event_type
                {
                    return Err(malformed(
                        "item-event columns disagree with canonical payload",
                    ));
                }
                if matches!(
                    commit.format_version,
                    REF_COMMIT_FORMAT_VERSION | REF_IMPORT_COMMIT_FORMAT_VERSION
                ) {
                    events_hasher.update([COMMIT_EVENT_FAMILY_ITEM]);
                }
            }
            ReplicaEvent::Ref { owner_item_id, .. } => {
                refs_started = true;
                saw_ref = true;
                if !ref_format {
                    return Err(malformed("legacy commit format contains a ref event"));
                }
                let proposal: RefEventProposal =
                    serde_json::from_slice(&payload).map_err(|error| {
                        malformed(format!("invalid canonical ref-event payload: {error}"))
                    })?;
                if canonical_ref_event_proposal_bytes(&proposal)
                    .map_err(|error| malformed(error.to_string()))?
                    != payload
                {
                    return Err(malformed("ref-event payload is not canonical"));
                }
                if proposal.owner_item_id != *owner_item_id || proposal.event_type != *event_type {
                    return Err(malformed(
                        "ref-event columns disagree with canonical payload",
                    ));
                }
                events_hasher.update([COMMIT_EVENT_FAMILY_REF]);
            }
        }
        events_hasher.update(digest);
    }
    if ref_format && !saw_ref {
        return Err(malformed("ref commit contains no ref event"));
    }

    let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
        events_hasher.finalize().into(),
    ))
    .expect("sha256 digest construction is valid");
    if events_digest != commit.events_digest {
        return Err(malformed("commit events digest mismatch"));
    }
    let canonical = canonical_stored_commit_bytes_for_hash(
        commit.format_version,
        &commit.author_id,
        commit.message.as_deref(),
        commit.parent_commit_id.as_ref(),
        commit.import_job_id.as_deref(),
        group.events.len(),
        &events_digest,
    )
    .map_err(|error| malformed(error.to_string()))?;
    if compute_commit_id(&canonical) != commit.commit_id {
        return Err(malformed("commit ID mismatch"));
    }
    Ok(())
}

fn validate_len(field: &str, value: &str, limit: usize) -> Result<(), ReplicaError> {
    if value.len() > limit {
        return Err(ReplicaError::Limit(format!(
            "{field} bytes={} max={limit}",
            value.len()
        )));
    }
    Ok(())
}

fn malformed(message: impl Into<String>) -> ReplicaError {
    ReplicaError::Malformed(message.into())
}

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(test)]
mod tests;
