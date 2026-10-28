//! Shared format-7 synchronization contract. Storage, authentication and atomic
//! Core acceptance belong to the authority; transfer validation is not acceptance.
mod history;
mod policy;
mod proposal;

pub use history::{HistoryVerifier, verify_published_commit};
pub use policy::{PolicyLimit, PolicyLimitName, PublicationViolation, ServerPolicy};
pub use proposal::{ProposalVerifier, ValidatedProposal, validate_submit};

use serde::{Deserialize, Serialize};
use synck_vcs::types::{
    CommitEventRow, CommitId, CommitRow, EventProposal, ItemId, RefEventProposal, RepoId,
    Sha256Digest, Timestamp,
};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;
/// Bounds apply to a transfer, never to an accepted commit. Eight MiB permits
/// any format-valid single event even with numeric-byte JSON history encoding.
pub const MAX_PAGE_RECORDS: usize = 512;
pub const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid sync request: {0}")]
    Invalid(String),
    #[error("sync transfer limit exceeded: {0}")]
    Limit(&'static str),
    #[error("POLICY_LIMIT: {0}")]
    PolicyLimit(#[from] PolicyLimit),
    #[error("unaccepted proposal upload expired; upload its pages again")]
    UploadExpired,
    #[error("repository identity or history generation changed")]
    Generation,
    #[error("checkpoint does not match verified history")]
    Checkpoint,
    #[error(transparent)]
    Core(#[from] synck_vcs::errors::CoreError),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryIdentity {
    pub repo_id: RepoId,
    pub generation: Uuid,
}
impl RepositoryIdentity {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.generation.get_version_num() != 7 {
            return Err(invalid("generation must be UUIDv7"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub repository: RepositoryIdentity,
    pub repo_seq: u64,
    pub checkpoint: Sha256Digest,
}
impl Cursor {
    pub fn genesis(repository: RepositoryIdentity) -> Self {
        Self {
            repository,
            repo_seq: 0,
            checkpoint: digest_id(synck_vcs::checkpoint::GENESIS),
        }
    }
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.repository.validate()?;
        if self.repo_seq > i64::MAX as u64 {
            return Err(invalid("repository sequence overflow"));
        }
        if self.repo_seq == 0 && self.checkpoint != digest_id(synck_vcs::checkpoint::GENESIS) {
            return Err(ProtocolError::Checkpoint);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub protocol_version: u16,
    pub repository: RepositoryIdentity,
    pub latest: Cursor,
    /// None for every field is the reference server's unrestricted policy.
    pub policy: ServerPolicy,
}
impl Capabilities {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.repository.validate()?;
        self.latest.validate()?;
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(invalid("unsupported sync protocol version"));
        }
        if self.latest.repository != self.repository {
            return Err(ProtocolError::Generation);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemHead {
    pub item_id: ItemId,
    pub head_seq: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitMetadata {
    pub author_id: String,
    pub committer_id: String,
    pub message: Option<String>,
    pub occurred_at: Option<Timestamp>,
    pub import_job_id: Option<String>,
}

/// Ordinary events, then ref events, then sorted unique head guards. Guards are
/// paged too: their count is not a hidden cap on the number of affected items.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProposalRecord {
    Item(EventProposal),
    Ref(RefEventProposal),
    Guard(ItemHead),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalPage {
    pub repository: RepositoryIdentity,
    pub operation_id: String,
    pub offset: u64,
    pub records: Vec<ProposalRecord>,
}

/// A small immutable submission referring to already staged pages. The request
/// digest binds metadata, generation, operation ID, event and guard summaries.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub repository: RepositoryIdentity,
    pub operation_id: String,
    pub metadata: CommitMetadata,
    pub event_count: u64,
    pub events_digest: Sha256Digest,
    pub guard_count: u64,
    pub guards_digest: Sha256Digest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub operation_id: String,
    pub request_digest: Sha256Digest,
    pub commit_id: CommitId,
    pub accepted_at: Timestamp,
    pub published: Option<Cursor>,
}
impl Receipt {
    pub fn validate_for(&self, request: &SubmitRequest) -> Result<(), ProtocolError> {
        let validated = validate_submit(request)?;
        if request.operation_id != self.operation_id
            || validated.request_digest != self.request_digest
            || validated.commit_id != self.commit_id
        {
            return Err(invalid("receipt does not match pending request"));
        }
        if self.accepted_at.timestamp_subsec_nanos() >= 1_000_000_000 {
            return Err(invalid("invalid receipt acceptance timestamp"));
        }
        if let Some(cursor) = &self.published {
            cursor.validate()?;
            if cursor.repository != request.repository || cursor.repo_seq == 0 {
                return Err(ProtocolError::Generation);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", deny_unknown_fields)]
pub enum SubmitResult {
    #[serde(rename = "accepted")]
    Accepted { receipt: Receipt },
    #[serde(rename = "conflict")]
    Conflict { items: Vec<ItemHead> },
    /// Non-retryable without changing the proposed work or server policy.
    #[serde(rename = "POLICY_LIMIT")]
    PolicyLimit {
        #[serde(flatten)]
        violation: PolicyLimit,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingCommit {
    pub commit_id: CommitId,
    pub next_event_index: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryPosition {
    /// Advances only when an entire commit has been verified and installed.
    pub completed: Cursor,
    pub pending: Option<PendingCommit>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRequest {
    pub after: HistoryPosition,
    pub limit: usize,
}
impl HistoryRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.after.completed.validate()?;
        if self
            .after
            .pending
            .as_ref()
            .is_some_and(|p| p.next_event_index == 0 || p.next_event_index > i32::MAX as u64)
        {
            return Err(invalid("invalid pending event index"));
        }
        if self.limit == 0 || self.limit > MAX_PAGE_RECORDS {
            return Err(ProtocolError::Limit("history page events"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedHeader {
    pub commit: CommitRow,
    pub checkpoint: Sha256Digest,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryPage {
    pub after: HistoryPosition,
    /// One commit per page; repeat the identical header on every continuation.
    /// None is only valid for an empty terminal page at a complete cursor.
    pub header: Option<PublishedHeader>,
    pub events: Vec<CommitEventRow>,
    pub has_more: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedCommit {
    pub commit: CommitRow,
    pub events: Vec<CommitEventRow>,
    pub checkpoint: Sha256Digest,
}

fn digest_id(bytes: [u8; 32]) -> Sha256Digest {
    Sha256Digest::parse(synck_vcs::types::sha256_prefixed_hex_from_digest_bytes(
        bytes,
    ))
    .expect("computed digest")
}
fn invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError::Invalid(message.into())
}

/// Count wire bytes without allocating a second serialized page. Transports
/// must additionally bound the request/response body before deserialization.
fn check_page_size(value: &impl Serialize) -> Result<(), ProtocolError> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .filter(|n| *n <= MAX_PAGE_BYTES)
                .ok_or_else(|| std::io::Error::other("page byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(0), value).map_err(|_| ProtocolError::Limit("page bytes"))
}
