//! Shared protocol 2 synchronization contract over format 8. Storage, authentication and atomic
//! Core acceptance belong to the authority; transfer validation is not acceptance.

#![forbid(unsafe_code)]

mod history;
mod policy;
mod proposal;
mod wire;

pub use history::HistoryVerifier;
pub use policy::{PolicyLimit, PolicyLimitName, PublicationViolation, ServerPolicy};
pub use proposal::{ProposalVerifier, ValidatedProposal, validate_submit};
pub use wire::{Refusal, Wire, decode, encode, event_len, header_len, submit_header};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use synck_vcs::types::{
    AuthorId, CommitEventRow, CommitId, CommitRow, EventProposal, ItemId, LifecycleState, ObjectId,
    RefEventProposal, RepoId, SequencingMode, Sha256Digest, Timestamp,
};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 2;
/// Bounds apply to a transfer, never to an accepted commit. Eight MiB fits
/// any format-valid single event. Receivers cap a body at `MAX_PAGE_BYTES`
/// before decoding it.
pub const MAX_PAGE_RECORDS: usize = 512;
pub const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;
/// Objects in one object pack, uploaded or requested.
pub const MAX_PACK_OBJECTS: usize = 512;
/// An object pack ends after the object that brings it to this many bytes,
/// so one request stays bounded and a cut-off transfer resumes per pack. It
/// bounds no object.
pub const PACK_BYTES: u64 = 64 * 1024 * 1024;
/// A pack entry's header: the object ID's 32 digest bytes, then its size as
/// eight big-endian bytes. The object's exact bytes follow.
pub const PACK_HEADER_BYTES: usize = 32 + 8;

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
impl From<synck_vcs::codec::DecodeError> for ProtocolError {
    fn from(error: synck_vcs::codec::DecodeError) -> Self {
        Self::Invalid(error.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryIdentity {
    pub repo_id: RepoId,
    pub generation: Uuid,
}
impl RepositoryIdentity {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.generation.get_version_num() != 7
            || self.generation.get_variant() != uuid::Variant::RFC4122
        {
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
    pub encrypted: bool,
    /// The published head; its repository is the server's.
    pub latest: Cursor,
    /// None for every field is the reference server's unrestricted policy.
    pub policy: ServerPolicy,
}
impl Capabilities {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.latest.validate()?;
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(invalid("unsupported sync protocol version"));
        }
        if self.latest.repo_seq == 0
            && self.latest != Cursor::genesis(self.latest.repository.clone(), self.encrypted)?
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedHeader {
    pub commit: CommitRow,
    pub checkpoint: Sha256Digest,
}
#[derive(Clone, Debug)]
pub struct HistoryPage {
    pub after: HistoryPosition,
    /// Ordered commit fragments sharing the page's event and byte budgets.
    /// Only the first may continue a prior page; only the last may be incomplete.
    pub fragments: Vec<HistoryFragment>,
    pub has_more: bool,
}
#[derive(Clone, Debug)]
pub struct HistoryFragment {
    /// Repeat the identical header when continuing a commit.
    pub header: PublishedHeader,
    pub events: Vec<CommitEventRow>,
}
/// A position in the state feed, which is ordered by `(changed_seq, item_id)`.
/// A null `item_id` sorts before every item: bootstrap is `changed_seq` 0 and
/// no item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatePosition {
    pub repository: RepositoryIdentity,
    pub changed_seq: u64,
    pub item_id: Option<ItemId>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateRequest {
    pub after: StatePosition,
    pub limit: usize,
}
impl StateRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.after.repository.validate()?;
        if self.after.changed_seq > i64::MAX as u64 {
            return Err(invalid("repository sequence overflow"));
        }
        if self.limit == 0 || self.limit > MAX_PAGE_RECORDS {
            return Err(ProtocolError::Limit("state page entries"));
        }
        Ok(())
    }
}
/// One item's structural state after its last published event, which the
/// commit at `changed_seq` holds. In an encrypted repository `name` is a tag
/// and the display name is in the object `name_metadata_id` names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateEntry {
    pub item_id: ItemId,
    pub item_type: String,
    pub changed_seq: u64,
    pub head_seq: i64,
    pub ref_head_seq: i64,
    pub parent_folder_id: Option<ItemId>,
    pub name: Option<String>,
    pub record_key: Option<String>,
    pub link_folder_id: Option<ItemId>,
    pub lifecycle_state: LifecycleState,
    pub sequencing_mode: SequencingMode,
    pub federation_stream_id: Option<String>,
    /// The current role map, empty when the item has no content.
    pub content: BTreeMap<String, ObjectId>,
    pub metadata_id: Option<ObjectId>,
    pub name_metadata_id: Option<ObjectId>,
    pub created_at: Timestamp,
    pub created_by: String,
    /// When and by whom the commit at `changed_seq` was made: its
    /// `occurred_at`, or its `created_at` when it has none, and `author_id`.
    pub changed_at: Timestamp,
    pub changed_by: String,
}
/// With `has_more` false the entries read so far are every item's state at
/// `through`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatePage {
    pub after: StatePosition,
    pub entries: Vec<StateEntry>,
    /// Items that left a subtree-limited credential's subtrees, in the
    /// entries' order. Absent for a credential that reads everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub departed: Vec<StateDeparture>,
    pub through: Cursor,
    pub has_more: bool,
}
/// An item, and with it its descendants, that left the credential's subtrees
/// in the commit at `changed_seq`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateDeparture {
    pub item_id: ItemId,
    pub changed_seq: u64,
}
/// One item's published events in `item_seq` order and the published ref
/// events it owns in `ref_seq` order. `commits` holds the commit of every row
/// in the page once, by ascending `repo_seq`.
#[derive(Clone, Debug)]
pub struct ItemHistoryPage {
    pub item_id: ItemId,
    pub events: Vec<CommitEventRow>,
    pub refs: Vec<CommitEventRow>,
    pub commits: Vec<ItemCommit>,
    pub has_more: bool,
}
/// A commit's `occurred_at` is its `created_at` when it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemCommit {
    pub repo_seq: u64,
    pub commit_id: CommitId,
    pub author_id: AuthorId,
    pub occurred_at: Timestamp,
    pub created_at: Timestamp,
}

/// One item of an `ItemHistoriesRequest`: its item events after `after_seq`
/// and its ref events after `after_ref_seq`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemHistoryPosition {
    pub item_id: ItemId,
    pub after_seq: u64,
    #[serde(default)]
    pub after_ref_seq: u64,
}
/// `POST /items/histories`: several items' histories, `limit` rows in all.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemHistoriesRequest {
    pub repository: RepositoryIdentity,
    pub items: Vec<ItemHistoryPosition>,
    pub limit: usize,
}
impl ItemHistoriesRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.repository.validate()?;
        if self.items.is_empty() || self.items.len() > MAX_PAGE_RECORDS {
            return Err(ProtocolError::Limit("item histories items"));
        }
        if self.limit == 0 || self.limit > MAX_PAGE_RECORDS {
            return Err(ProtocolError::Limit("item histories page rows"));
        }
        let mut items = std::collections::BTreeSet::new();
        for item in &self.items {
            if item.after_seq > i64::MAX as u64 || item.after_ref_seq > i64::MAX as u64 {
                return Err(invalid("item sequence overflow"));
            }
            if !items.insert(&item.item_id) {
                return Err(invalid("duplicate item in item histories"));
            }
        }
        Ok(())
    }
}
/// The pages of a prefix of the request's items, in their order. Every page
/// but the last is complete.
#[derive(Clone, Debug)]
pub struct ItemHistoriesPage {
    pub repository: RepositoryIdentity,
    pub pages: Vec<ItemHistoryPage>,
}

/// Item events a `CommitSummary` lists at most.
pub const COMMIT_SUMMARY_EVENTS: usize = 32;
/// `POST /commits`: published commits before `before_seq`, newest first;
/// null reads from the newest.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitLogRequest {
    pub repository: RepositoryIdentity,
    pub before_seq: Option<u64>,
    pub limit: usize,
}
impl CommitLogRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.repository.validate()?;
        if self
            .before_seq
            .is_some_and(|seq| seq == 0 || seq > i64::MAX as u64)
        {
            return Err(invalid("commit log position out of range"));
        }
        if self.limit == 0 || self.limit > MAX_PAGE_RECORDS {
            return Err(ProtocolError::Limit("commit log page commits"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitLogPage {
    pub commits: Vec<CommitSummary>,
    pub has_more: bool,
}
/// A published commit: its author, its `occurred_at` or else `created_at`,
/// its count of item and ref events, and its first `COMMIT_SUMMARY_EVENTS`
/// item events in commit order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitSummary {
    pub repo_seq: u64,
    pub commit_id: CommitId,
    pub author_id: AuthorId,
    pub occurred_at: Timestamp,
    pub event_count: u32,
    pub events: Vec<CommitSummaryEvent>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitSummaryEvent {
    pub item_id: ItemId,
    pub item_seq: i64,
    pub commit_event_idx: i32,
    pub event_type: String,
}

/// `POST /leases`: reserve repository paths, as `synck lock acquire` does.
/// `ttl` is 1 to 86400 seconds.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRequest {
    pub paths: Vec<String>,
    pub owner: String,
    pub ttl: u32,
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
    pub ttl: u32,
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
