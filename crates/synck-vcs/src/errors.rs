//! Core error definitions.
//!
//! This module defines the stable error surface for the Synck VCS core.
//! Variants roughly follow the specification's error categories, with a
//! small number of additional variants for parsing/validation and storage.

use thiserror::Error;

use crate::types::{CommitId, IdParseError, ItemId, RepoId};

/// Errors returned by the core engine.
///
/// Stability notes:
/// - Variants corresponding to spec-defined categories (`EmptyCommit`,
///   `ItemNotFound`, `ItemTombstoned`, `ItemAlreadyExists`, `ParentNotFound`,
///   `ParentNotFolder`, `NameConflict`, `CycleDetected`, `ItemIsFederated`,
///   `ItemNotFederated`, `FederatedIdMismatch`, `BaseSeqMismatch`, `ItemSeqMismatch`,
///   `CommitIdMismatch`, `MultipleItemsInFederatedProposal`, `EventCountMismatch`,
///   `CommitEventIdxMismatch`, `IllegalFederationTransition`, `RepoSeqConflict`,
///   `DuplicateCommit`, `OperationAlreadyApplied`, and `StorageError`) are part
///   of the stable public API.
/// - Additional variants (`InvalidId`, `InvalidTimestamp`, `InvalidProposal`)
///   are provided for robustness and may gain new fields or siblings over time,
///   but will not silently change meaning.
#[derive(Debug, Error)]
pub enum CoreError {
    // --- Spec-defined categories ---
    /// Commit has no events.
    #[error("empty commit")]
    EmptyCommit,

    /// Target item was not found.
    #[error("item not found: repo={repo_id}, item={item_id}")]
    ItemNotFound { repo_id: RepoId, item_id: ItemId },

    /// Target item is tombstoned and cannot accept new events.
    #[error("item tombstoned: repo={repo_id}, item={item_id}")]
    ItemTombstoned { repo_id: RepoId, item_id: ItemId },

    /// Item creation conflicts with an existing item.
    #[error("item already exists: repo={repo_id}, item={item_id}")]
    ItemAlreadyExists { repo_id: RepoId, item_id: ItemId },

    /// Parent folder not found.
    #[error("parent not found: repo={repo_id}, parent_item={parent_item_id}")]
    ParentNotFound {
        repo_id: RepoId,
        parent_item_id: ItemId,
    },

    /// Parent is not a folder.
    #[error("parent not folder: repo={repo_id}, parent_item={parent_item_id}")]
    ParentNotFolder {
        repo_id: RepoId,
        parent_item_id: ItemId,
    },

    /// Parent folder exists but is not LIVE for checked FILE/FOLDER placement.
    #[error("parent not live: repo={repo_id}, parent_item={parent_item_id}")]
    ParentNotLive {
        repo_id: RepoId,
        parent_item_id: ItemId,
    },

    /// Name conflict within a folder.
    #[error("name conflict: repo={repo_id}, parent={parent_folder_id:?}, name={name}")]
    NameConflict {
        repo_id: RepoId,
        parent_folder_id: Option<ItemId>,
        name: String,
    },

    /// Name is syntactically invalid according to core naming rules.
    #[error("invalid name: repo={repo_id}, item={item_id:?}, name={name}, reason={reason}")]
    NameInvalid {
        repo_id: RepoId,
        item_id: Option<ItemId>,
        name: String,
        reason: String,
    },

    /// Name exceeds the configured byte-length limit.
    #[error(
        "name too long: repo={repo_id}, item={item_id:?}, name={name}, bytes={bytes}, max={max}"
    )]
    NameTooLong {
        repo_id: RepoId,
        item_id: Option<ItemId>,
        name: String,
        bytes: usize,
        max: usize,
    },

    /// Folder graph would become cyclic.
    #[error("cycle detected involving item: repo={repo_id}, item={item_id}")]
    CycleDetected { repo_id: RepoId, item_id: ItemId },

    /// Item is federated but a local append was attempted.
    #[error("item is federated: repo={repo_id}, item={item_id}")]
    ItemIsFederated { repo_id: RepoId, item_id: ItemId },

    /// Item is not federated but a federated ingest was attempted.
    #[error("item not federated: repo={repo_id}, item={item_id}")]
    ItemNotFederated { repo_id: RepoId, item_id: ItemId },

    /// Federated ID on the item does not match the commit.
    #[error(
        "federated id mismatch: repo={repo_id}, item={item_id}, expected={expected}, found={found}"
    )]
    FederatedIdMismatch {
        repo_id: RepoId,
        item_id: ItemId,
        expected: String,
        found: String,
    },

    /// Provided base sequence does not match current item head sequence.
    #[error(
        "base sequence mismatch: repo={repo_id}, item={item_id}, expected={expected}, found={found}"
    )]
    BaseSeqMismatch {
        repo_id: RepoId,
        item_id: ItemId,
        expected: i64,
        found: i64,
    },

    /// Provided attached-ref base does not match the owner's current ref head.
    #[error(
        "ref base sequence mismatch: repo={repo_id}, owner={owner_item_id}, expected={expected}, found={found}"
    )]
    RefBaseSeqMismatch {
        repo_id: RepoId,
        owner_item_id: ItemId,
        expected: i64,
        found: i64,
    },

    /// Provided item sequence does not match expected `head_seq + 1`.
    #[error(
        "item sequence mismatch: repo={repo_id}, item={item_id}, expected={expected}, found={found}"
    )]
    ItemSeqMismatch {
        repo_id: RepoId,
        item_id: ItemId,
        expected: i64,
        found: i64,
    },

    /// Provided domain sequence does not match expected `last_domain_seq + 1`.
    #[error(
        "domain sequence mismatch: repo={repo_id}, domain={domain_id}, expected={expected}, found={found}"
    )]
    DomainSeqMismatch {
        repo_id: RepoId,
        domain_id: String,
        expected: i64,
        found: i64,
    },

    /// Proposal overlaps already ingested domain sequence space.
    #[error(
        "domain sequence overlap: repo={repo_id}, domain={domain_id}, last={last}, proposed_end={proposed_end}"
    )]
    DomainSeqOverlap {
        repo_id: RepoId,
        domain_id: String,
        last: i64,
        proposed_end: i64,
    },

    /// Canonical commit ID mismatch.
    #[error("commit id mismatch: repo={repo_id}, expected={expected}, found={found}")]
    CommitIdMismatch {
        repo_id: RepoId,
        expected: CommitId,
        found: CommitId,
    },

    /// Federated proposal contains events for multiple items.
    #[error("multiple items in federated proposal")]
    MultipleItemsInFederatedProposal,

    /// Declared event_count does not match actual number of events.
    #[error("event count mismatch: expected={expected}, found={found}")]
    EventCountMismatch { expected: usize, found: usize },

    /// `commit_event_idx` sequence is not contiguous 0..N-1.
    #[error("commit event index mismatch")]
    CommitEventIdxMismatch,

    /// Illegal transition of sequencing/federation state.
    #[error("illegal federation transition")]
    IllegalFederationTransition,

    /// Commit with this id already exists in the repo.
    #[error("duplicate commit: repo={repo_id}, commit_id={commit_id}")]
    DuplicateCommit {
        repo_id: RepoId,
        commit_id: CommitId,
    },

    /// Operation id was already applied in this repo.
    #[error(
        "operation already applied: repo={repo_id}, operation_id={operation_id}, commit_id={commit_id}"
    )]
    OperationAlreadyApplied {
        repo_id: RepoId,
        operation_id: String,
        commit_id: CommitId,
    },

    /// A guarded FILE/FOLDER subtree changed after preparation.
    #[error("subtree changed: repo={repo_id}, root_item={root_item_id}")]
    SubtreeChanged {
        repo_id: RepoId,
        root_item_id: ItemId,
    },

    /// Conflict allocating `repo_seq` (unique constraint violation).
    #[error("repo sequence conflict for repo={repo_id}")]
    RepoSeqConflict { repo_id: RepoId },

    /// Storage-layer error.
    #[error("storage error: {0}")]
    StorageError(#[from] StorageError),

    // --- Additional categories for robustness ---
    /// ID parsing/validation failed.
    #[error("invalid identifier: {0}")]
    InvalidId(#[from] IdParseError),

    /// Timestamp parsing/validation failed.
    #[error("invalid timestamp: {0}")]
    InvalidTimestamp(String),

    /// Proposal is structurally invalid before hitting invariants.
    #[error("invalid commit proposal: {0}")]
    InvalidProposal(String),
}

/// Storage backend error.
///
/// This is intentionally minimal and backend-agnostic. Backend implementations
/// should map their native error types into one of these variants.
#[derive(Debug, Error)]
pub enum StorageError {
    /// Underlying storage reported a uniqueness violation (for example name or
    /// constraint conflicts) that does not map to a more specific core error.
    #[error("unique constraint violation: {detail}")]
    UniqueViolation {
        detail: String,
        constraint: Option<String>,
    },

    /// Underlying storage reported an I/O-level or connectivity problem.
    #[error("storage I/O error: {detail}")]
    Io { detail: String },

    /// The bounded storage-client queue could not provide capacity before its deadline.
    #[error("storage overloaded: {detail}")]
    Overloaded { detail: String },

    /// Catch-all for other backend-specific failures.
    #[error("backend error: {detail}")]
    Other { detail: String },
}
