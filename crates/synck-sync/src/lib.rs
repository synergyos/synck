//! Shared format-7 synchronization contract. Storage, authentication and atomic
//! Core acceptance belong to the authority; transfer validation is not acceptance.

#![forbid(unsafe_code)]

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
/// Objects in one object pack, uploaded or requested.
pub const MAX_PACK_OBJECTS: usize = 512;
/// An object pack ends after the object that brings it to this many bytes,
/// so one request stays bounded and a cut-off transfer resumes per pack. It
/// bounds no object.
pub const PACK_BYTES: u64 = 64 * 1024 * 1024;
/// A pack entry's header: the object ID (`sha256:` and 64 hex digits), then
/// its size as eight big-endian bytes. The object's exact bytes follow.
pub const PACK_HEADER_BYTES: usize = 71 + 8;

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
    pub fn genesis(repository: RepositoryIdentity, encrypted: bool) -> Result<Self, ProtocolError> {
        let genesis = synck_vcs::checkpoint::genesis(
            &repository.repo_id,
            &repository.generation.to_string(),
            encrypted,
        )?;
        Ok(Self {
            repository,
            repo_seq: 0,
            checkpoint: digest_id(genesis),
        })
    }
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.repository.validate()?;
        if self.repo_seq > i64::MAX as u64 {
            return Err(invalid("repository sequence overflow"));
        }
        // A cursor alone cannot say whether its repository is encrypted. The first
        // commit's checkpoint binds that choice, and `Capabilities` states it.
        if self.repo_seq == 0
            && Self::genesis(self.repository.clone(), false)?.checkpoint != self.checkpoint
            && Self::genesis(self.repository.clone(), true)?.checkpoint != self.checkpoint
        {
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
    pub encrypted: bool,
    pub latest: Cursor,
    /// None for every field is the reference server's unrestricted policy.
    pub policy: ServerPolicy,
    /// The server takes and serves object packs (`PUT /objects`, `POST
    /// /objects`). Absent from servers that predate them.
    #[serde(default)]
    pub object_packs: bool,
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
        if self.latest.repo_seq == 0
            && self.latest != Cursor::genesis(self.repository.clone(), self.encrypted)?
        {
            return Err(ProtocolError::Checkpoint);
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
    /// The commit ID hashes one spelling, so the request digest uses it too.
    #[serde(default, with = "synck_vcs::types::format_time")]
    pub occurred_at: Option<Timestamp>,
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
#[allow(clippy::large_enum_variant)] // Records are read and applied one at a time.
pub enum ProposalRecord {
    Item(EventProposal),
    Ref(RefEventProposal),
    Guard(ItemHead),
    /// Exact subtree membership, authenticated in the guard stream.
    SubtreeGuard(synck_vcs::types::SubtreeClosureGuard),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalPage {
    pub repository: RepositoryIdentity,
    pub operation_id: String,
    pub offset: u64,
    pub records: Vec<ProposalRecord>,
}

/// The reply to a durably installed page.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageAck {
    pub next_offset: u64,
    pub expires_at: Timestamp,
}

/// A small immutable submission referring to already staged pages. The request
/// digest binds metadata, generation, operation ID, event and guard summaries.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    /// Optional authority fence over the complete published repository state.
    /// Acceptance also requires that no accepted commit remains unpublished.
    /// Omitted requests retain their existing bytes and request digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_head: Option<u64>,
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
    /// Ordered commit fragments sharing the page's event and byte budgets.
    /// Only the first may continue a prior page; only the last may be incomplete.
    pub fragments: Vec<HistoryFragment>,
    pub has_more: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryFragment {
    /// Repeat the identical header when continuing a commit.
    pub header: PublishedHeader,
    pub events: Vec<CommitEventRow>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedCommit {
    pub commit: CommitRow,
    pub events: Vec<CommitEventRow>,
    pub checkpoint: Sha256Digest,
}

/// `POST /leases`: reserve repository paths, as `synck lock acquire` does.
/// `ttl` is 1 to 86400 seconds.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRequest {
    pub paths: Vec<String>,
    pub owner: String,
    pub ttl: u64,
}
/// The token is the only proof of ownership; it is never listed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub token: String,
    pub locks: Vec<LeasedPath>,
}
/// `expires_at` is UTC Unix seconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeasedPath {
    pub path: String,
    pub owner: String,
    pub expires_at: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Leases {
    pub locks: Vec<LeasedPath>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRenewal {
    pub token: String,
    pub ttl: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseToken {
    pub token: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeasePaths {
    pub paths: Vec<String>,
}

/// The objects a `POST /objects` asks for, at most `MAX_PACK_OBJECTS`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectRequest {
    pub objects: Vec<synck_vcs::types::ObjectId>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Released {
    pub released: u64,
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
