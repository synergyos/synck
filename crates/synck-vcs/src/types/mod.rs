//! Core shared type definitions.
//!
//! This module defines:
//! - Strongly-typed identifiers for repos/items/commits/blobs.
//! - Timestamp representation for domain logic.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use serde::{Deserializer, Serializer};
use uuid::Uuid;

/// Domain timestamp type.
///
/// Internally we use `DateTime<Utc>` everywhere in the engine.
/// At persistence / wire boundaries, timestamps are serialized as
/// canonical RFC 3339 UTC with a `Z` suffix.
pub type Timestamp = DateTime<Utc>;

/// Returns the current time in UTC.
pub fn now() -> Timestamp {
    Utc::now()
}

/// Error type for identifier parsing / validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdParseError {
    pub kind: IdParseErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdParseErrorKind {
    InvalidUuidV7,
    InvalidSha256Id,
}

impl fmt::Display for IdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            IdParseErrorKind::InvalidUuidV7 => write!(f, "invalid UUIDv7 identifier"),
            IdParseErrorKind::InvalidSha256Id => write!(f, "invalid sha256 identifier"),
        }
    }
}

impl std::error::Error for IdParseError {}

/// Helper macro for UUIDv7-backed ID newtypes.
macro_rules! uuid_v7_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Parses and validates a UUIDv7-based identifier from a string.
            pub fn parse<S: AsRef<str>>(s: S) -> Result<Self, IdParseError> {
                let s_ref = s.as_ref();
                let uuid = Uuid::parse_str(s_ref).map_err(|_| IdParseError {
                    kind: IdParseErrorKind::InvalidUuidV7,
                })?;
                if uuid.get_version_num() != 7 {
                    return Err(IdParseError {
                        kind: IdParseErrorKind::InvalidUuidV7,
                    });
                }
                // Canonicalize to lowercase hyphenated UUID string so equality,
                // hashing, and commit hashing are stable across formatting.
                Ok(Self(uuid.to_string()))
            }

            /// Returns the underlying string representation.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let s = String::deserialize(deserializer)?;
                Self::parse(&s).map_err(D::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }
    };
}

/// Helper macro for `"sha256:<hex64>"`-style ID newtypes.
macro_rules! sha256_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Parses and validates a `sha256:<hex64>` identifier from a string.
            pub fn parse<S: AsRef<str>>(s: S) -> Result<Self, IdParseError> {
                let s_ref = s.as_ref();
                const PREFIX: &str = "sha256:";
                if !s_ref.starts_with(PREFIX) {
                    return Err(IdParseError {
                        kind: IdParseErrorKind::InvalidSha256Id,
                    });
                }
                let hex_part = &s_ref[PREFIX.len()..];
                if hex_part.len() != 64
                    || !hex_part.bytes().all(|b| {
                        matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')
                    })
                {
                    return Err(IdParseError {
                        kind: IdParseErrorKind::InvalidSha256Id,
                    });
                }

                // Canonicalize hex to lowercase so commit hashing and equality are stable.
                let mut out = String::with_capacity(PREFIX.len() + 64);
                out.push_str(PREFIX);
                out.push_str(&hex_part.to_ascii_lowercase());
                Ok(Self(out))
            }

            /// Returns the underlying string representation.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let s = String::deserialize(deserializer)?;
                Self::parse(&s).map_err(D::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }
    };
}

uuid_v7_id!(
    /// Repository identifier (UUIDv7, lowercase, hyphenated).
    RepoId
);

uuid_v7_id!(
    /// Item identifier (UUIDv7, lowercase, hyphenated).
    ItemId
);

sha256_id!(
    /// Commit identifier (`sha256:<hex64>` of canonical commit bytes).
    CommitId
);

uuid_v7_id!(
    /// Surrogate commit key (UUIDv7) used to group events before the final
    /// content-addressed `commit_id` is known.
    ///
    /// This is an internal storage detail; APIs should use `CommitId`.
    CommitPk
);

sha256_id!(
    /// Blob identifier (`sha256:<hex64>` of blob content).
    BlobId
);

sha256_id!(
    /// Generic SHA-256 digest encoded as `"sha256:<hex64>"`.
    ///
    /// Used for commit `events_digest` and other digest fields that are not
    /// themselves object ids.
    Sha256Digest
);

// Identity allocation must remain explicit; a random UUID is not a semantic
// default value.
#[allow(clippy::new_without_default)]
impl RepoId {
    /// Generate a new random repository identifier (UUIDv7).
    pub fn new() -> Self {
        let uuid = Uuid::now_v7();
        Self(uuid.to_string())
    }
}

// Identity allocation must remain explicit; a random UUID is not a semantic
// default value.
#[allow(clippy::new_without_default)]
impl ItemId {
    /// Generate a new random item identifier (UUIDv7).
    pub fn new() -> Self {
        let uuid = Uuid::now_v7();
        Self(uuid.to_string())
    }
}

// Identity allocation must remain explicit; a random UUID is not a semantic
// default value.
#[allow(clippy::new_without_default)]
impl CommitPk {
    /// Generate a new random commit surrogate key (UUIDv7).
    pub fn new() -> Self {
        let uuid = Uuid::now_v7();
        Self(uuid.to_string())
    }
}

/// Opaque author identifier.
pub type AuthorId = String;

/// Opaque committer identifier.
///
/// This identifies who *recorded* a commit into a repo. It may differ from
/// `AuthorId` (for example, gateways or background workers committing on
/// behalf of a user).
pub type CommitterId = String;

/// Opaque item type identifier (platform-defined).
pub type ItemType = String;

/// Opaque event type identifier (platform-defined).
pub type EventType = String;

/// Federation stream identifier (platform-defined).
///
/// For ITEM-domain federation, this is the authority-owned stream id that
/// sequences the item's events (and is used for blob namespaces).
pub type FederationStreamId = String;

/// Lifecycle state of an item.
///
/// This is the single source of truth for whether an item is live,
/// in trash, or tombstoned. It replaces the older pair of booleans
/// (`tombstoned`, `in_trash`) to avoid impossible combinations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LifecycleState {
    Live,
    Trash,
    Tombstoned,
}

/// Sequencing mode for an item: local or federated.
///
/// Serialized as `LOCAL` / `FEDERATED` to match the spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SequencingMode {
    Local,
    Federated,
}

/// Logical representation of an `items` table row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemRow {
    pub repo_id: RepoId,
    pub item_id: ItemId,
    pub item_type: ItemType,
    pub parent_folder_id: Option<ItemId>,
    /// User-visible, case-preserving name.
    pub name: String,
    /// Canonicalized name key used for uniqueness and lookup.
    pub name_key: String,
    pub head_seq: i64,
    /// Head of the independent attached-ref ledger. Zero means no ref events.
    #[serde(default)]
    pub ref_head_seq: i64,
    pub lifecycle_state: LifecycleState,
    pub created_at: Timestamp,
    pub created_by: String,
    pub sequencing_mode: SequencingMode,
    pub federation_stream_id: Option<String>,
}

/// An opaque logical-content commitment, distinct from a file's BlobId.
/// Parsing rejects noncanonical spelling instead of normalizing hash inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentId([u8; 32]);

impl ContentId {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, IdParseError> {
        let invalid = || IdParseError {
            kind: IdParseErrorKind::InvalidSha256Id,
        };
        let hex = value.as_ref().strip_prefix("sha256:").ok_or_else(invalid)?;
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(invalid());
        }
        let mut digest = [0; 32];
        hex::decode_to_slice(hex, &mut digest).map_err(|_| invalid())?;
        Ok(Self(digest))
    }

    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", hex::encode(self.0))
    }
}

impl FromStr for ContentId {
    type Err = IdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for ContentId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ContentId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Structured content body for an event.
///
/// Canonical rule: a `set_content` value is *exactly one* of:
///
/// - `{ "text": "..." }`
/// - `{ "json": { ... } }`
/// - `{ "blob_id": "sha256:<hex64>" }`
/// - `{ "content_id": "sha256:<lowercase hex64>" }`
///
/// If an event has no content, `set_content` is `null`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum EventContent {
    Text { text: String },
    Json { json: serde_json::Value },
    Blob { blob_id: String },
    Content { content_id: ContentId },
}

/// Logical representation of an `events` table row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventRow {
    pub repo_id: RepoId,
    pub item_id: ItemId,
    pub item_seq: i64,
    pub event_type: EventType,
    pub commit_id: CommitId,
    pub commit_event_idx: i32,
    /// Optional monotonic commit activity sequence for this repo.
    ///
    /// For per-item reads, events may be unsequenced (`repo_seq = None`).
    /// Repo-wide scans return only sequenced events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_seq: Option<i64>,
    /// Canonical RFC 8785 JSON bytes for the EventProposal payload.
    pub payload: Vec<u8>,
    /// SHA-256 digest of the canonical payload bytes.
    pub event_payload_digest: Sha256Digest,
    pub created_at: Timestamp,
    /// Parent folder after this event is applied.
    pub parent_folder_id: Option<ItemId>,
    /// Parent folder before this event is applied (only populated for moves).
    pub parent_folder_id_before: Option<ItemId>,
}

/// Cursor for streaming events in global repo order.
///
/// The ordering key is `(repo_seq, commit_event_idx)` ascending.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoEventCursor {
    pub repo_seq: i64,
    pub commit_event_idx: i32,
}

/// Cursor for paginating items in a stable insertion order.
///
/// The ordering key is `(created_at, item_id)` ascending.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemCursor {
    pub created_at: Timestamp,
    pub item_id: ItemId,
}

/// Logical representation of an `events` table row for insertion.
///
/// Unlike `EventRow` (which is shaped for reads and includes `commit_id`),
/// this struct uses `commit_pk` to allow streaming event insertion before
/// the final content-addressed `commit_id` is known.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventInsertRow {
    pub repo_id: RepoId,
    pub commit_pk: CommitPk,
    /// 0..n-1 within the commit.
    pub commit_event_idx: i32,
    pub item_id: ItemId,
    pub item_seq: i64,
    pub event_type: EventType,
    /// Canonical RFC 8785 JSON bytes for the EventProposal payload.
    pub payload: Vec<u8>,
    /// Raw 32-byte SHA-256 digest of `payload`.
    pub event_payload_digest: [u8; 32],
    pub created_at: Timestamp,
    pub parent_folder_id: Option<ItemId>,
    pub parent_folder_id_before: Option<ItemId>,
}

/// A narrow event in an item's attached ref ledger.
///
/// Ref events cannot create or structurally mutate their owner. Product layers
/// define event types and payload semantics; Core owns ordering and CAS.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefEventProposal {
    pub owner_item_id: ItemId,
    pub event_type: EventType,
    #[serde(default, skip_serializing_if = "is_none_or_json_null")]
    pub set_metadata: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_content: Option<EventContent>,
    pub base_ref_seq: i64,
}

/// Logical representation of an attached ref-event row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefEventRow {
    pub repo_id: RepoId,
    pub owner_item_id: ItemId,
    pub ref_seq: i64,
    pub event_type: EventType,
    pub commit_id: CommitId,
    pub commit_event_idx: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_seq: Option<i64>,
    pub payload: Vec<u8>,
    pub event_payload_digest: Sha256Digest,
    pub created_at: Timestamp,
}

/// Storage row for an attached ref event before the commit id is sealed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefEventInsertRow {
    pub repo_id: RepoId,
    pub commit_pk: CommitPk,
    pub commit_event_idx: i32,
    pub owner_item_id: ItemId,
    pub ref_seq: i64,
    pub event_type: EventType,
    pub payload: Vec<u8>,
    pub event_payload_digest: [u8; 32],
    pub created_at: Timestamp,
}

/// One event in complete commit/repository order.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event_family", rename_all = "snake_case")]
pub enum CommitEventRow {
    Item(EventRow),
    Ref(RefEventRow),
}

impl CommitEventRow {
    pub fn commit_event_idx(&self) -> i32 {
        match self {
            Self::Item(event) => event.commit_event_idx,
            Self::Ref(event) => event.commit_event_idx,
        }
    }

    pub fn commit_id(&self) -> &CommitId {
        match self {
            Self::Item(event) => &event.commit_id,
            Self::Ref(event) => &event.commit_id,
        }
    }

    pub fn repo_seq(&self) -> Option<i64> {
        match self {
            Self::Item(event) => event.repo_seq,
            Self::Ref(event) => event.repo_seq,
        }
    }
}

/// Logical representation of a `commits` table row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitRow {
    /// Repository membership; not part of canonical commit identity.
    pub repo_id: RepoId,
    /// Hash of the version-selected canonical commit header and ordered events.
    pub commit_id: CommitId,
    /// Repository-local placement assigned after acceptance; not part of
    /// canonical commit identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_seq: Option<i64>,
    pub format_version: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_job_id: Option<String>,
    pub author_id: AuthorId,
    pub committer_id: CommitterId,
    pub message: Option<String>,
    /// Optional origin/author timestamp ("author date") for history views.
    pub occurred_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub event_count: i32,
    pub events_digest: Sha256Digest,
}

// --- Commit proposals and results ---

/// Exact non-tombstoned subtree state prepared by a FILE/FOLDER caller.
///
/// The root's ordinary proposal baseline protects the root itself. Descendant
/// pairs are sorted by `item_id` and participate in the existing append head
/// guards as well as transaction-time subtree membership validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtreeClosureGuard {
    pub root_item_id: ItemId,
    pub expected_non_tombstoned_descendants: Vec<(ItemId, i64)>,
    /// Internal root-only lifecycle transition after all live children were
    /// handled in earlier commits. Checked under the root lock at append time.
    pub require_empty_live_descendants: bool,
}

/// Event proposal used when building commits.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventProposal {
    /// Target item id; required unless `create_item` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<ItemId>,
    pub event_type: EventType,
    /// Non‑structural metadata associated with this event (tags, labels, etc.).
    #[serde(default, skip_serializing_if = "is_none_or_json_null")]
    pub set_metadata: Option<serde_json::Value>,
    /// Structured content for this event (text/JSON/blob ids).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_content: Option<EventContent>,
    /// Create a new item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_item: Option<CreateItem>,

    /// Set parent.
    ///
    /// Encoding is tri-state:
    /// - `None` => no change
    /// - `Some(None)` => move to root
    /// - `Some(Some(id))` => move under `id`
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_set_parent_tristate"
    )]
    pub set_parent: Option<Option<ItemId>>,
    /// Rename item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_name: Option<String>,

    /// Mark item tombstoned (permanent delete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_tombstoned: Option<bool>,
    /// Mark item in/out of trash (soft delete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_in_trash: Option<bool>,

    /// Transition to federated sequencing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_is_federated: Option<bool>,
    /// Set federation stream id for item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_federation_stream_id: Option<FederationStreamId>,

    /// Snapshot of the item's head_seq observed by the caller when this
    /// event was constructed.
    ///
    /// - For events targeting an existing item, this MUST be `Some(seq)`.
    /// - For `create_item` events, this MUST be `None` (no prior history).
    ///
    /// All events in a commit that target the same (repo_id, item_id)
    /// are expected to carry the same `base_seq` value. Core enforces
    /// only shape/consistency; mutation policies are platform concerns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_seq: Option<i64>,
}

fn deserialize_set_parent_tristate<'de, D>(
    deserializer: D,
) -> Result<Option<Option<ItemId>>, D::Error>
where
    D: Deserializer<'de>,
{
    let inner: Option<ItemId> = Option::deserialize(deserializer)?;
    Ok(Some(inner))
}

fn is_none_or_json_null(v: &Option<serde_json::Value>) -> bool {
    matches!(v, None | Some(serde_json::Value::Null))
}

/// Fields for creating a new item structurally.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateItem {
    pub item_type: ItemType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_folder_id: Option<ItemId>,
    pub name: String,
}

/// Commit proposal for local append operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitProposal {
    pub author_id: AuthorId,
    pub committer_id: AuthorId,
    #[serde(default)]
    pub message: Option<String>,
    /// Optional origin/author timestamp ("author date") for history views.
    #[serde(default)]
    pub occurred_at: Option<Timestamp>,
    /// Stable idempotency anchor for local append intent.
    ///
    /// Callers should reuse the same `operation_id` when retrying the same
    /// append intent. Core enforces uniqueness per repo for non-empty values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    pub events: Vec<EventProposal>,
}

/// Federated proposal input for ingest operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederatedProposal {
    pub federation_stream_id: FederationStreamId,
    pub item_seq: i64,
    pub author_id: AuthorId,
    pub committer_id: AuthorId,
    #[serde(default)]
    pub message: Option<String>,
    /// Optional authority/author timestamp ("author date") for history views.
    #[serde(default)]
    pub occurred_at: Option<Timestamp>,
    pub events: Vec<EventProposal>,
}

/// Federated domain proposal input for ingest operations (tree/namespace domains).
///
/// This is used for federated folders (multi-item commits) where the authority
/// allocates a monotonic `domain_seq` for the proposal stream (commit/proposal sequence).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederatedDomainProposal {
    /// Authority-owned identifier for this domain stream (ClearingHouse federation_stream_id).
    pub domain_id: String,
    /// Starting domain sequence for this proposal payload.
    pub domain_seq: i64,
    pub author_id: AuthorId,
    pub committer_id: AuthorId,
    #[serde(default)]
    pub message: Option<String>,
    /// Optional authority/author timestamp ("author date") for history views.
    #[serde(default)]
    pub occurred_at: Option<Timestamp>,
    pub events: Vec<EventProposal>,
    /// Attached FILE ref events sequenced by the same folder-domain proposal.
    #[serde(default)]
    pub ref_events: Vec<RefEventProposal>,
}

/// Information about an item created in a commit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreatedItemInfo {
    /// Index of the event in the proposal that created the item.
    pub event_idx: usize,
    pub item_id: ItemId,
}

/// Result of a successful local append.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AcceptedCommit {
    pub commit_id: CommitId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_seq: Option<i64>,
    pub created_at: Timestamp,
    #[serde(default)]
    pub occurred_at: Option<Timestamp>,
    /// Mapping of event indices to newly created item ids.
    #[serde(default)]
    pub created_items: Vec<CreatedItemInfo>,
    /// Per-event item sequence numbers, indexed by `commit_event_idx`.
    ///
    /// This is returned so callers can avoid re-querying core to discover the
    /// `item_seq` allocated for each event in the accepted commit.
    #[serde(default)]
    pub event_item_seqs: Vec<i64>,
    /// Per-event ref sequence numbers, indexed by the append-with-refs input.
    #[serde(default)]
    pub ref_event_seqs: Vec<i64>,
}

/// Result of a successful federated ingest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IngestedCommit {
    pub commit_id: CommitId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_seq: Option<i64>,
    pub created_at: Timestamp,
    #[serde(default)]
    pub occurred_at: Option<Timestamp>,
    pub item_seq: i64,
    pub federation_stream_id: FederationStreamId,
}

/// Result of a successful federated domain ingest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IngestedDomainCommit {
    /// Whether this ingest actually applied a new commit, or was a replay.
    pub applied: bool,
    #[serde(default)]
    pub commit_id: Option<CommitId>,
    #[serde(default)]
    pub repo_seq: Option<i64>,
    #[serde(default)]
    pub created_at: Option<Timestamp>,
    #[serde(default)]
    pub occurred_at: Option<Timestamp>,
    pub domain_id: String,
    pub domain_seq: i64,
    /// NOTE(v1): for federated folder domains, `domain_seq` is a commit/proposal sequence and
    /// does not advance per-event; `domain_seq_end == domain_seq`.
    pub domain_seq_end: i64,
}

// --- Canonical commit encoding ---

use crate::errors::CoreError;
use num_bigint::BigInt;
use serde_json_canonicalizer::to_vec as canonical_json_to_vec;
use sha2::{Digest, Sha256};

const MAX_SAFE_JSON_INTEGER: i64 = 9_007_199_254_740_991;

// Convert an RFC 8785 number spelling to its exact integer value.
fn decimal_integer(value: &[u8]) -> Option<BigInt> {
    let value = std::str::from_utf8(value).ok()?;
    let (negative, unsigned) = match value.strip_prefix('-') {
        Some(unsigned) => (true, unsigned),
        None => (false, value),
    };
    let (mantissa, exponent) = match unsigned
        .bytes()
        .position(|byte| matches!(byte, b'e' | b'E'))
    {
        Some(index) => (
            &unsigned[..index],
            unsigned[index + 1..].parse::<i64>().ok()?,
        ),
        None => (unsigned, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits = String::with_capacity(whole.len() + fraction.len());
    digits.push_str(whole);
    digits.push_str(fraction);

    let scale = exponent.checked_sub(i64::try_from(fraction.len()).ok()?)?;
    if scale >= 0 {
        digits.push_str(&"0".repeat(usize::try_from(scale).ok()?));
    } else {
        let remove = usize::try_from(scale.checked_neg()?).ok()?;
        let keep = digits.len().checked_sub(remove)?;
        if digits.as_bytes()[keep..].iter().any(|byte| *byte != b'0') {
            return None;
        }
        digits.truncate(keep);
    }

    let integer = digits.parse::<BigInt>().ok()?;
    Some(if negative { -integer } else { integer })
}

fn integer_is_lossless(value: &str, canonical: &[u8]) -> bool {
    let exact = value
        .parse::<BigInt>()
        .expect("serde_json emitted a valid integer");
    decimal_integer(canonical).as_ref() == Some(&exact)
}

fn validate_lossless_json_integers(value: &serde_json::Value) -> Result<(), CoreError> {
    match value {
        serde_json::Value::Number(number) => {
            let exact = if let Some(integer) = number.as_i64() {
                if (-MAX_SAFE_JSON_INTEGER..=MAX_SAFE_JSON_INTEGER).contains(&integer) {
                    return Ok(());
                }
                number.to_string()
            } else if let Some(integer) = number.as_u64() {
                if integer <= MAX_SAFE_JSON_INTEGER as u64 {
                    return Ok(());
                }
                number.to_string()
            } else {
                let exact = number.to_string();
                if exact.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
                    return Ok(());
                }
                exact
            };

            let canonical = canonical_json_to_vec(number)
                .map_err(|error| CoreError::InvalidProposal(error.to_string()))?;
            if !integer_is_lossless(&exact, &canonical) {
                return Err(CoreError::InvalidProposal(format!(
                    "JSON integer {number} cannot be represented by RFC 8785 without changing its value; encode it as a string"
                )));
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                validate_lossless_json_integers(value)?;
            }
        }
        serde_json::Value::Object(fields) => {
            for value in fields.values() {
                validate_lossless_json_integers(value)?;
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::String(_) => {}
    }

    Ok(())
}

/// RFC 8785 bytes with the same lossless-integer admission used by Core events.
/// Typed command digests must reject lossy numbers before receipt lookup too.
pub fn canonical_json_bytes(value: &serde_json::Value) -> Result<Vec<u8>, CoreError> {
    validate_lossless_json_integers(value)?;
    canonical_json_to_vec(value).map_err(|error| CoreError::InvalidProposal(error.to_string()))
}

/// Compute the raw SHA-256 digest bytes of an input.
pub fn compute_sha256_digest_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.into()
}

/// Compute the content-addressed commit id from canonical bytes.
pub fn compute_commit_id(bytes: &[u8]) -> CommitId {
    let digest = compute_sha256_digest_bytes(bytes);
    let hex = hex::encode(digest);
    let full = format!("sha256:{hex}");
    // Safe to unwrap: we just generated a valid sha256:<hex64> string.
    CommitId::parse(full).expect("internal sha256 commit id construction must be valid")
}

/// Convert a raw 32-byte digest into the `"sha256:<hex64>"` string form.
pub fn sha256_prefixed_hex_from_digest_bytes(bytes: [u8; 32]) -> String {
    let hex = hex::encode(bytes);
    format!("sha256:{hex}")
}

/// Convert a `CommitId` into raw 32-byte digest bytes.
pub fn commit_id_to_digest_bytes(id: &CommitId) -> [u8; 32] {
    sha256_id_to_digest_bytes(id.as_str())
}

/// Convert raw digest bytes into a `CommitId`.
pub fn commit_id_from_digest_bytes(bytes: &[u8]) -> Result<CommitId, IdParseError> {
    let digest: [u8; 32] = bytes.try_into().map_err(|_| IdParseError {
        kind: IdParseErrorKind::InvalidSha256Id,
    })?;
    CommitId::parse(sha256_prefixed_hex_from_digest_bytes(digest))
}

/// Convert a `Sha256Digest` into raw 32-byte digest bytes.
pub fn sha256_digest_to_digest_bytes(id: &Sha256Digest) -> [u8; 32] {
    sha256_id_to_digest_bytes(id.as_str())
}

/// Convert raw digest bytes into a `Sha256Digest`.
pub fn sha256_digest_from_digest_bytes(bytes: &[u8]) -> Result<Sha256Digest, IdParseError> {
    let digest: [u8; 32] = bytes.try_into().map_err(|_| IdParseError {
        kind: IdParseErrorKind::InvalidSha256Id,
    })?;
    Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(digest))
}

fn sha256_id_to_digest_bytes(s: &str) -> [u8; 32] {
    const PREFIX: &str = "sha256:";
    let hex_part = s
        .strip_prefix(PREFIX)
        .expect("sha256 id must have sha256: prefix");
    let bytes = hex::decode(hex_part).expect("sha256 id hex must be valid");
    bytes
        .try_into()
        .expect("sha256 id bytes must decode to 32 bytes")
}

/// Canonicalize an `EventProposal` into RFC 8785 canonical JSON bytes.
///
/// Hard rule: the canonical payload is sparse: fields that are conceptually
/// absent are omitted (not emitted as JSON `null`). `null` is only used where
/// it has defined semantics (for example `set_parent: null` meaning “move to root”).
pub fn canonical_event_proposal_bytes(ev: &EventProposal) -> Result<Vec<u8>, CoreError> {
    let value = serde_json::to_value(ev).map_err(|e| {
        CoreError::InvalidProposal(format!(
            "serialize event proposal to json value failed: {e}"
        ))
    })?;
    canonical_json_bytes(&value)
}

/// Canonicalize a narrow attached-ref proposal into RFC 8785 JSON bytes.
pub fn canonical_ref_event_proposal_bytes(ev: &RefEventProposal) -> Result<Vec<u8>, CoreError> {
    let value = serde_json::to_value(ev).map_err(|e| {
        CoreError::InvalidProposal(format!(
            "serialize ref event proposal to json value failed: {e}"
        ))
    })?;
    canonical_json_bytes(&value)
}

/// The sole accepted commit encoding for Synck 0.2.
pub const COMMIT_FORMAT_VERSION: i32 = 6;
pub const COMMIT_EVENT_FAMILY_ITEM: u8 = 0;
pub const COMMIT_EVENT_FAMILY_REF: u8 = 1;

/// Repository placement and acceptance time belong to the checkpoint, not this
/// content identity. Optional values are explicit JSON nulls.
pub fn canonical_commit_bytes_for_hash(
    author_id: &AuthorId,
    committer_id: &CommitterId,
    occurred_at: Option<Timestamp>,
    message: Option<&str>,
    import_job_id: Option<&str>,
    event_count: usize,
    events_digest: &Sha256Digest,
) -> Result<Vec<u8>, CoreError> {
    if event_count == 0 {
        return Err(CoreError::EmptyCommit);
    }
    if let Some(id) = import_job_id {
        validate_import_job_id(id)?;
    }
    if occurred_at.is_some_and(|time| time.timestamp_subsec_nanos() >= 1_000_000_000) {
        return Err(CoreError::InvalidProposal(
            "leap seconds are not supported".into(),
        ));
    }
    let value = serde_json::json!({
        "object_type": "vcs_commit",
        "format_version": COMMIT_FORMAT_VERSION,
        "author_id": author_id,
        "committer_id": committer_id,
        "occurred_at": occurred_at.map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
        "message": message,
        "import_job_id": import_job_id,
        "event_count": event_count,
        "events_digest": events_digest.as_str(),
    });
    canonical_json_bytes(&value)
}

pub fn validate_import_job_id(import_job_id: &str) -> Result<(), CoreError> {
    let parsed = Uuid::parse_str(import_job_id).map_err(|_| {
        CoreError::InvalidProposal("import_job_id must be canonical UUIDv4".to_string())
    })?;
    if parsed.get_version_num() != 4 || parsed.to_string() != import_job_id {
        return Err(CoreError::InvalidProposal(
            "import_job_id must be canonical UUIDv4".to_string(),
        ));
    }
    Ok(())
}

/// Reject every previous format rather than guessing an interpretation.
pub fn canonical_stored_commit_bytes_for_hash(
    format_version: i32,
    author_id: &AuthorId,
    committer_id: &CommitterId,
    occurred_at: Option<Timestamp>,
    message: Option<&str>,
    import_job_id: Option<&str>,
    event_count: usize,
    events_digest: &Sha256Digest,
) -> Result<Vec<u8>, CoreError> {
    if format_version != COMMIT_FORMAT_VERSION {
        return Err(CoreError::InvalidProposal(format!(
            "unsupported commit format version: {format_version}"
        )));
    }
    canonical_commit_bytes_for_hash(
        author_id,
        committer_id,
        occurred_at,
        message,
        import_job_id,
        event_count,
        events_digest,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_ids_deserialize_and_canonicalize() {
        let json = "\"01890C6C-1111-7777-8888-AAAAAAAAAAAA\"";
        let id: ItemId = serde_json::from_str(json).expect("deserialize ItemId");
        assert_eq!(id.as_str(), "01890c6c-1111-7777-8888-aaaaaaaaaaaa");

        let back = serde_json::to_string(&id).expect("serialize ItemId");
        assert_eq!(back, "\"01890c6c-1111-7777-8888-aaaaaaaaaaaa\"");
    }

    #[test]
    fn sha256_ids_deserialize_and_canonicalize_hex() {
        let json = "\"sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"";
        let id: CommitId = serde_json::from_str(json).expect("deserialize CommitId");
        assert_eq!(
            id.as_str(),
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        let back = serde_json::to_string(&id).expect("serialize CommitId");
        assert_eq!(
            back,
            "\"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\""
        );
    }

    #[test]
    fn event_proposal_rejects_unknown_fields() {
        let json = r#"{
            "item_id": "01890c6c-1111-7777-8888-aaaaaaaaaaaa",
            "event_type": "TEST",
            "set_metadata": null,
            "set_content": null,
            "create_item": null,
            "set_parent": null,
            "set_name": null,
            "set_tombstoned": null,
            "set_in_trash": null,
            "set_is_federated": null,
            "set_federation_stream_id": null,
            "base_seq": 1,
            "unexpected_field": "nope"
        }"#;

        let err = serde_json::from_str::<EventProposal>(json).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn commit_proposal_rejects_unknown_fields() {
        let json = r#"{
            "author_id": "author-1",
            "message": null,
            "events": [],
            "unknown": 1
        }"#;
        let err = serde_json::from_str::<CommitProposal>(json).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn create_item_rejects_unknown_fields() {
        let json = r#"{
            "item_type": "FILE",
            "parent_folder_id": null,
            "name": "file.txt",
            "extra": true
        }"#;
        let err = serde_json::from_str::<CreateItem>(json).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn federated_proposal_rejects_unknown_fields() {
        let json = r#"{
            "federation_stream_id": "fid-1",
            "item_seq": 1,
            "author_id": "author-1",
            "message": null,
            "events": [],
            "extra": "x"
        }"#;
        let err = serde_json::from_str::<FederatedProposal>(json).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn canonical_event_proposal_bytes_are_utf8_json() {
        let ev = EventProposal {
            item_id: Some(ItemId::parse("01890c6c-1111-7777-8888-aaaaaaaaaaaa").unwrap()),
            event_type: "ITEM_CREATE".to_string(),
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

        let bytes = canonical_event_proposal_bytes(&ev).unwrap();
        let _s = String::from_utf8(bytes).expect("canonical event bytes must be valid UTF-8");
    }

    #[test]
    fn canonical_event_accepts_lossless_integers_and_floats() {
        let ev = EventProposal {
            item_id: Some(ItemId::parse("01890c6c-1111-7777-8888-aaaaaaaaaaaa").unwrap()),
            event_type: "TEST".to_string(),
            set_metadata: Some(serde_json::json!({
                "integers": [
                    -MAX_SAFE_JSON_INTEGER,
                    MAX_SAFE_JSON_INTEGER,
                    MAX_SAFE_JSON_INTEGER + 1,
                    57_946_832_445_233_864_i64
                ],
                "floating_point": 1e30
            })),
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

        let bytes =
            canonical_event_proposal_bytes(&ev).expect("lossless JSON numbers must be accepted");
        let parsed: EventProposal =
            serde_json::from_slice(&bytes).expect("canonical event must deserialize");
        assert_eq!(
            canonical_event_proposal_bytes(&parsed).unwrap(),
            bytes,
            "accepted large numbers must remain valid after canonical round trip"
        );
    }

    #[test]
    fn canonical_event_rejects_nested_integers_outside_lossless_range() {
        let invalid_integers = [
            serde_json::Number::from(MAX_SAFE_JSON_INTEGER + 2),
            serde_json::Number::from(-MAX_SAFE_JSON_INTEGER - 2),
            serde_json::Number::from(u64::MAX),
        ];

        for integer in invalid_integers {
            let ev = EventProposal {
                item_id: Some(ItemId::parse("01890c6c-1111-7777-8888-aaaaaaaaaaaa").unwrap()),
                event_type: "TEST".to_string(),
                set_metadata: Some(serde_json::json!({
                    "outer": {"inner": [serde_json::Value::Number(integer.clone())]}
                })),
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

            let error = canonical_event_proposal_bytes(&ev).unwrap_err();
            let expected = format!(
                "JSON integer {integer} cannot be represented by RFC 8785 without changing its value; encode it as a string"
            );
            assert!(
                matches!(error, CoreError::InvalidProposal(ref message) if message == &expected),
                "unexpected error for {integer}: {error}"
            );
        }
    }

    #[test]
    fn canonical_event_rejects_raw_integers_beyond_rust_integer_ranges() {
        for integer in ["18446744073709551616", "-9223372036854775809"] {
            let json = format!(
                r#"{{
                    "item_id": "01890c6c-1111-7777-8888-aaaaaaaaaaaa",
                    "event_type": "TEST",
                    "set_metadata": {{"integer": {integer}}},
                    "base_seq": 1
                }}"#
            );
            let ev: EventProposal = serde_json::from_str(&json)
                .expect("large integer proposal must deserialize exactly");

            let error = canonical_event_proposal_bytes(&ev).unwrap_err();
            let expected = format!(
                "JSON integer {integer} cannot be represented by RFC 8785 without changing its value; encode it as a string"
            );
            assert!(
                matches!(error, CoreError::InvalidProposal(ref message) if message == &expected),
                "unexpected error for {integer}: {error}"
            );
        }
    }

    #[test]
    fn canonical_event_accepts_exact_integers_with_exponent_canonical_spelling() {
        for (integer, canonical) in [
            ("1000000000000000000000", "1e+21"),
            ("-1000000000000000000000", "-1e+21"),
        ] {
            let json = format!(
                r#"{{
                    "item_id": "01890c6c-1111-7777-8888-aaaaaaaaaaaa",
                    "event_type": "TEST",
                    "set_metadata": {{"integer": {integer}}},
                    "base_seq": 1
                }}"#
            );
            let event: EventProposal =
                serde_json::from_str(&json).expect("exact large integer proposal must deserialize");

            let bytes = canonical_event_proposal_bytes(&event)
                .expect("canonical notation must not change the integer value");
            assert!(std::str::from_utf8(&bytes).unwrap().contains(canonical));
            let replay: EventProposal = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(canonical_event_proposal_bytes(&replay).unwrap(), bytes);
        }
    }

    #[test]
    fn event_content_rejects_multi_key_or_unknown_fields() {
        // Multi-key objects must be rejected (strict union).
        let multi = r#"{"text":"hi","json":{"k":"v"}}"#;
        assert!(serde_json::from_str::<EventContent>(multi).is_err());

        // Unknown keys must be rejected (no silent drops).
        let unknown = r#"{"text":"hi","extra":1}"#;
        assert!(serde_json::from_str::<EventContent>(unknown).is_err());
    }

    #[test]
    fn content_id_requires_exact_canonical_spelling() {
        let id = ContentId::from_digest([0xab; 32]);
        let canonical = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(id.to_string(), canonical);
        assert_eq!(ContentId::parse(&canonical).unwrap(), id);
        assert_eq!(id.as_bytes(), &[0xab; 32]);
        for invalid in [
            canonical.to_uppercase(),
            canonical.replace("ab", "AB"),
            canonical.replace("sha256:", ""),
            format!(" {canonical}"),
            format!("{canonical}\n"),
            canonical[..70].to_string(),
            format!("{canonical}0"),
            canonical.replace("ab", "gg"),
        ] {
            assert!(ContentId::parse(&invalid).is_err(), "{invalid:?}");
            assert!(serde_json::from_value::<ContentId>(serde_json::json!(invalid)).is_err());
        }
    }

    #[test]
    fn content_reference_is_a_strict_union_and_canonical_roundtrip() {
        let id = ContentId::from_digest([0xab; 32]);
        let content = serde_json::json!({"content_id": id});
        let decoded: EventContent = serde_json::from_value(content.clone()).unwrap();
        assert!(matches!(decoded, EventContent::Content { content_id } if content_id == id));
        assert_eq!(serde_json::to_value(decoded).unwrap(), content);
        for extra in ["text", "json", "blob_id", "extra"] {
            let mut ambiguous = content.clone();
            ambiguous[extra] = serde_json::json!("body");
            assert!(serde_json::from_value::<EventContent>(ambiguous).is_err());
        }
        assert!(
            serde_json::from_value::<EventContent>(serde_json::json!({"content_id": null}))
                .is_err()
        );
    }

    #[test]
    fn set_parent_root_move_is_unambiguous_in_canonical_json() {
        let item_id = ItemId::parse("01890c6c-1111-7777-8888-aaaaaaaaaaaa").unwrap();

        // No change => set_parent is absent.
        let no_change = EventProposal {
            item_id: Some(item_id.clone()),
            event_type: "SET_PARENT".to_string(),
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
        let bytes = canonical_event_proposal_bytes(&no_change).unwrap();
        let json = String::from_utf8(bytes).unwrap();
        assert!(!json.contains("\"set_parent\""));
        let parsed: EventProposal = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.set_parent, None);

        // Move to root => set_parent is explicitly JSON null (field present).
        let to_root = EventProposal {
            set_parent: Some(None),
            ..no_change
        };
        let bytes = canonical_event_proposal_bytes(&to_root).unwrap();
        let json = String::from_utf8(bytes).unwrap();
        assert!(json.contains("\"set_parent\":null"));
        let parsed: EventProposal = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.set_parent, Some(None));
    }
}
