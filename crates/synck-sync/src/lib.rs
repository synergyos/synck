//! Central synchronization for format-7 repositories.
//!
//! These wire values and validators are shared by private hosted authorities
//! and native clients. They do not authenticate callers, acquire write locks,
//! store proposals, or validate application plaintext. In particular,
//! [`validate_submit`] does not replace Core's transactional acceptance checks.

mod history;
mod proposal;

pub use history::{verify_history_page, verify_published_commit};
pub use proposal::{ValidatedProposal, validate_submit};

use serde::{Deserialize, Serialize};
use synck_vcs::types::{
    CommitEventRow, CommitId, CommitProposal, CommitRow, ItemId, ObjectId, RefEventProposal,
    RepoId, Sha256Digest, Timestamp,
};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_EVENTS: usize = 512;
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;
pub const MAX_PROPOSAL_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_HISTORY_COMMITS: usize = 64;
// Event payload byte vectors use Core's JSON row encoding. The history bound
// must fit one maximum-size proposal even with numeric-byte JSON expansion.
pub const MAX_HISTORY_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_OPERATION_ID_BYTES: usize = 128;
pub const MAX_HEAD_GUARDS: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid sync request: {0}")]
    Invalid(String),
    #[error("sync limit exceeded: {0}")]
    Limit(&'static str),
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
            return Err(ProtocolError::Invalid("generation must be UUIDv7".into()));
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
            return Err(ProtocolError::Invalid(
                "repository sequence overflow".into(),
            ));
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
    pub max_events: usize,
    pub max_proposal_bytes: usize,
    pub max_history_commits: usize,
    pub max_history_bytes: usize,
    pub max_object_bytes: u64,
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
        if self.max_events == 0
            || self.max_events > MAX_EVENTS
            || self.max_proposal_bytes == 0
            || self.max_proposal_bytes > MAX_PROPOSAL_BYTES
            || self.max_history_commits == 0
            || self.max_history_commits > MAX_HISTORY_COMMITS
            || self.max_history_bytes == 0
            || self.max_history_bytes > MAX_HISTORY_BYTES
            || self.max_object_bytes == 0
        {
            return Err(invalid("unsupported sync limits"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemHead {
    pub item_id: ItemId,
    /// A positive number asserts an exact existing item head. Conflicts use
    /// zero to report that a requested item is absent.
    pub head_seq: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub repository: RepositoryIdentity,
    pub proposal: CommitProposal,
    #[serde(default)]
    pub ref_events: Vec<RefEventProposal>,
    #[serde(default)]
    pub head_guards: Vec<ItemHead>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub operation_id: String,
    /// Digest of the entire canonical request, including the history generation
    /// and guards. An operation ID reused with another digest is an error.
    pub request_digest: Sha256Digest,
    pub commit_id: CommitId,
    pub accepted_at: Timestamp,
    /// Acceptance is durable even while activity publication is pending.
    pub published: Option<Cursor>,
}

impl Receipt {
    /// Validate a recovered receipt before marking a durable pending request as
    /// accepted. Publication must still be verified through checkpoint history.
    pub fn validate_for(&self, request: &SubmitRequest) -> Result<(), ProtocolError> {
        let validated = validate_submit(request)?;
        if request.proposal.operation_id.as_deref() != Some(self.operation_id.as_str())
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
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SubmitResult {
    Accepted { receipt: Receipt },
    Conflict { items: Vec<ItemHead> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRequest {
    pub after: Cursor,
    pub limit: usize,
}

impl HistoryRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.after.validate()?;
        if self.limit == 0 || self.limit > MAX_HISTORY_COMMITS {
            return Err(ProtocolError::Limit("history commit count"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedCommit {
    pub commit: CommitRow,
    pub events: Vec<CommitEventRow>,
    pub checkpoint: Sha256Digest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryPage {
    pub after: Cursor,
    pub commits: Vec<PublishedCommit>,
    pub next: Cursor,
    pub has_more: bool,
}

/// Missing is distinct from an empty file. Object bodies use the canonical
/// salted framing, transferred as application/octet-stream on HTTP.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObjectPresence {
    Available {
        object_id: ObjectId,
        stored_bytes: u64,
    },
    Missing {
        object_id: ObjectId,
    },
}

fn digest_id(bytes: [u8; 32]) -> Sha256Digest {
    Sha256Digest::parse(synck_vcs::types::sha256_prefixed_hex_from_digest_bytes(
        bytes,
    ))
    .expect("computed digest is canonical")
}

fn invalid(message: impl Into<String>) -> ProtocolError {
    ProtocolError::Invalid(message.into())
}
