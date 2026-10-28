//! Repository orchestration primitives.
//!
//! `Repo` is the main entry point for embedding Synck VCS. It wires together
//! storage, sequencing, and invariants behind a small async API surface.

use crate::config::CoreConfig;
use crate::errors::{CoreError, StorageError};
use crate::limits::validate_commit_metadata;
use crate::invariants::{
    Invariant, InvariantContext, OperationKind, SequencingModeRule, TombstoneFinality,
    check_file_folder_live_parent_closure, check_folder_acyclicity,
    check_parent_existence_and_type,
};
use crate::naming::{NameError, NameErrorKind};
use crate::sequencer::Sequencer;
use crate::storage::{SealedCommitInsert, Storage, StorageTxn};
use crate::transition::{ItemState, apply_event as apply_item_event};
use crate::types::{
    AcceptedCommit, COMMIT_EVENT_FAMILY_ITEM, COMMIT_EVENT_FAMILY_REF,
    COMMIT_FORMAT_VERSION, CommitEventRow, CommitPk, CommitProposal, CommitRow,
    CreatedItemInfo, EventInsertRow, EventProposal, EventRow,
    FederatedDomainProposal, FederatedProposal,
    IngestedCommit, IngestedDomainCommit, ItemCursor, ItemId, ItemRow, LifecycleState,
    RefEventInsertRow, RefEventProposal, RepoEventCursor, RepoId, SequencingMode, Sha256Digest,
    SubtreeClosureGuard, Timestamp,
};
use crate::types::{
    CommitId, canonical_commit_bytes_for_hash, canonical_event_proposal_bytes,
    canonical_ref_event_proposal_bytes, compute_commit_id, compute_sha256_digest_bytes, now,
    sha256_prefixed_hex_from_digest_bytes, validate_import_job_id,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Instant;
use tracing::{debug, info, instrument, trace, warn};

const ITEM_UPSERT_BATCH_MIN_ROWS: usize = 64;

/// Stable local receipt key for one accepted federation-domain proposal.
///
/// The stream name is hashed so an authority-controlled identifier cannot
/// exceed the commit operation-id bound. The domain sequence keeps proposals
/// distinct while retries resolve the exact commit written by ingestion.
pub fn federated_domain_operation_id(domain_id: &str, domain_seq: i64) -> String {
    let domain_digest = Sha256::digest(domain_id.as_bytes());
    format!("federation-domain:v1:{domain_digest:x}:{domain_seq}")
}

fn perf_timings_enabled() -> bool {
    std::env::var("SYNCK_PERF_TIMINGS")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

/// Persist item rows in an order that avoids spurious uniqueness failures from
/// Postgres's immediate unique indexes and partial indexes.
///
/// Two hazards we need to account for:
/// 1. Items leaving the LIVE set should be written first, so later LIVE upserts
///    don't collide with stale LIVE keys.
/// 2. Rename/move swaps between LIVE items can violate `(parent_folder_id, name_key)`
///    uniqueness if rows are updated naively. We avoid this by first "parking"
///    any existing LIVE item whose uniqueness key changes in the commit by
///    temporarily assigning an internal-only `name_key`, then writing final rows.
async fn persist_items_with_live_parking<T: StorageTxn + ?Sized>(
    txn: &mut T,
    repo_id: &RepoId,
    items: &HashMap<ItemId, ItemRow>,
    items_before_modifiers: &HashMap<ItemId, ItemRow>,
    created_item_ids: &HashSet<ItemId>,
) -> Result<(), CoreError> {
    let mut park_live_items: Vec<(ItemId, String)> = Vec::new();
    for (item_id, after) in items {
        if created_item_ids.contains(item_id) {
            continue;
        }
        let Some(before) = items_before_modifiers.get(item_id) else {
            continue;
        };
        if !matches!(before.lifecycle_state, LifecycleState::Live)
            || !matches!(after.lifecycle_state, LifecycleState::Live)
        {
            continue;
        }
        let before_key = (before.parent_folder_id.clone(), before.name_key.clone());
        let after_key = (after.parent_folder_id.clone(), after.name_key.clone());
        if before_key != after_key {
            let parked_key = format!("/__parked__/{}", item_id);
            park_live_items.push((item_id.clone(), parked_key));
        }
    }

    for (item_id, parked_key) in park_live_items {
        let mut parked = items
            .get(&item_id)
            .expect("park target must exist in items map")
            .clone();
        parked.name_key = parked_key;
        if let Err(err) = txn.put_item(&parked).await {
            return Err(CoreError::StorageError(err));
        }
    }

    let mut non_live: Vec<&ItemRow> = Vec::new();
    let mut live: Vec<&ItemRow> = Vec::new();
    for item in items.values() {
        if !matches!(item.lifecycle_state, LifecycleState::Live) {
            non_live.push(item);
        } else {
            live.push(item);
        }
    }

    let ordered_items: Vec<&ItemRow> = non_live.into_iter().chain(live.into_iter()).collect();
    put_items_with_conflict_mapping(txn, repo_id, &ordered_items).await?;
    Ok(())
}

pub fn advisory_lock_key_for_item(repo_id: &RepoId, item_id: &ItemId) -> i64 {
    // Advisory locks are only used to avoid concurrent writers corrupting per-item linearity
    // and structural invariants. Collisions over-lock (hurts concurrency) but remain correct.
    let mut hasher = Sha256::new();
    hasher.update(b"synck:vcs:lock:item:v1:");
    hasher.update(repo_id.as_str().as_bytes());
    hasher.update(b":");
    hasher.update(item_id.as_str().as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let prefix: [u8; 8] = digest[0..8].try_into().expect("slice should be 8 bytes");
    i64::from_be_bytes(prefix)
}

pub fn advisory_lock_key_for_namespace(repo_id: &RepoId, parent_folder_id: Option<&ItemId>) -> i64 {
    match parent_folder_id {
        Some(parent_folder_id) => advisory_lock_key_for_item(repo_id, parent_folder_id),
        None => {
            let mut hasher = Sha256::new();
            hasher.update(b"synck:vcs:lock:root_namespace:v1:");
            hasher.update(repo_id.as_str().as_bytes());
            let digest: [u8; 32] = hasher.finalize().into();
            let prefix: [u8; 8] = digest[0..8].try_into().expect("slice should be 8 bytes");
            i64::from_be_bytes(prefix)
        }
    }
}

pub fn advisory_lock_key_for_federation_stream(
    repo_id: &RepoId,
    federation_stream_id: &str,
) -> i64 {
    let mut hasher = Sha256::new();
    hasher.update(b"synck:vcs:lock:federation_stream:v1:");
    hasher.update(repo_id.as_str().as_bytes());
    hasher.update(b":");
    hasher.update(federation_stream_id.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let prefix: [u8; 8] = digest[0..8].try_into().expect("slice should be 8 bytes");
    i64::from_be_bytes(prefix)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppendBaseSeqMode {
    /// Caller supplies `base_seq` for existing items; stale bases are caller-visible errors.
    RequirePresent,
    /// Caller omits `base_seq` for existing items; core materializes it under the item lockset.
    MaterializeUnderItemLocks,
}

#[derive(Clone, Copy)]
enum AppendProfile<'a> {
    Generic,
    FileFolder {
        subtree_guards: &'a [SubtreeClosureGuard],
        import_job_id: Option<&'a str>,
    },
}

#[derive(Debug)]
struct AppendApplied {
    accepted: AcceptedCommit,
    author_id: String,
    message: Option<String>,
    event_count: i32,
}

/// Repository handle backed by a storage implementation.
pub struct Repo {
    pub(crate) storage: Storage,
    #[allow(dead_code)]
    config: CoreConfig,
}

impl Repo {
    /// Construct a new repository handle.
    pub fn new(storage: Storage, config: CoreConfig) -> Self {
        Self {
            storage,
            config,
        }
    }

    fn validate_payload_limits(
        &self,
        events: &[EventProposal],
        ref_events: &[RefEventProposal],
    ) -> Result<Vec<Vec<u8>>, CoreError> {
        validate_payload_limits(events, ref_events)
    }

    /// Fetch a single item by id.
    pub async fn get_item(
        &self,
        repo_id: RepoId,
        item_id: ItemId,
    ) -> Result<Option<ItemRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .get_item_readonly(&repo_id, &item_id)
            .await?)
    }

    /// Fetch multiple items by id in one backend read.
    ///
    /// Missing ids are omitted; callers that require a complete set must
    /// validate the returned identities.
    pub async fn get_items(
        &self,
        repo_id: RepoId,
        item_ids: Vec<ItemId>,
    ) -> Result<Vec<ItemRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .get_items_readonly(&repo_id, &item_ids)
            .await?)
    }

    /// Fetch a single item by its federation stream id, if present.
    ///
    /// This is a convenience helper for platform layers that ingest by
    /// `federation_stream_id` and need to resolve the corresponding local `item_id`.
    pub async fn get_item_by_federation_stream_id(
        &self,
        repo_id: RepoId,
        federation_stream_id: String,
    ) -> Result<Option<ItemRow>, CoreError> {
        let mut txn = self.storage.backend.begin_txn().await?;
        let row = txn
            .get_item_by_federation_stream_id(&repo_id, &federation_stream_id)
            .await?;
        // Read-only: roll back the txn.
        txn.rollback().await?;
        Ok(row)
    }

    /// Fetch a single commit by id.
    pub async fn get_commit(
        &self,
        repo_id: RepoId,
        commit_id: CommitId,
    ) -> Result<Option<CommitRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .get_commit_readonly(&repo_id, &commit_id)
            .await?)
    }

    /// Fetch one commit by its repository-scoped idempotency identity.
    pub async fn get_commit_by_operation_id(
        &self,
        repo_id: RepoId,
        operation_id: &str,
    ) -> Result<Option<CommitRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .get_commit_by_operation_id_readonly(&repo_id, operation_id)
            .await?)
    }

    /// Read one commit's complete event list in canonical commit order.
    pub async fn list_events_for_commit(
        &self,
        repo_id: RepoId,
        commit_id: CommitId,
        limit: usize,
    ) -> Result<Vec<EventRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .list_events_for_commit_readonly(&repo_id, &commit_id, limit)
            .await?)
    }

    /// Fetch all event families for one commit in canonical commit order.
    pub async fn list_commit_events(
        &self,
        repo_id: RepoId,
        commit_id: CommitId,
        limit: usize,
    ) -> Result<Vec<CommitEventRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .list_commit_events_readonly(&repo_id, &commit_id, limit)
            .await?)
    }

    /// List events for an item from `from_seq`, up to `limit` entries.
    pub async fn list_events(
        &self,
        repo_id: RepoId,
        item_id: ItemId,
        from_seq: i64,
        limit: usize,
    ) -> Result<Vec<EventRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .list_events_for_item_readonly(&repo_id, &item_id, from_seq, limit)
            .await?)
    }

    /// List attached ref events for an owner from `from_seq`.
    pub async fn list_ref_events(
        &self,
        repo_id: RepoId,
        owner_item_id: ItemId,
        from_seq: i64,
        limit: usize,
    ) -> Result<Vec<crate::types::RefEventRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .list_ref_events_for_owner_readonly(&repo_id, &owner_item_id, from_seq, limit)
            .await?)
    }

    /// Stream events for a repo in global order `(repo_seq, commit_event_idx)` after `after`.
    ///
    /// Cursor semantics: `after` is exclusive. `None` starts from the beginning of history.
    pub async fn list_repo_events(
        &self,
        repo_id: RepoId,
        after: Option<RepoEventCursor>,
        limit: usize,
    ) -> Result<Vec<EventRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .list_repo_events_readonly(&repo_id, after.as_ref(), limit)
            .await?)
    }

    /// Stream all event families in global repository order.
    pub async fn list_repo_commit_events(
        &self,
        repo_id: RepoId,
        after: Option<RepoEventCursor>,
        limit: usize,
    ) -> Result<Vec<CommitEventRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .list_repo_commit_events_readonly(&repo_id, after.as_ref(), limit)
            .await?)
    }

    /// List commits by `repo_seq` order, optionally bounded by `to_repo_seq`.
    pub async fn list_commits_by_repo_seq(
        &self,
        repo_id: RepoId,
        from_repo_seq: i64,
        to_repo_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<CommitRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .list_commits_by_repo_seq_readonly(&repo_id, from_repo_seq, to_repo_seq, limit)
            .await?)
    }

    /// List items ordered by `(created_at, item_id)`, optionally filtered by lifecycle state.
    ///
    /// Cursor semantics: `after` is exclusive. `None` starts from the beginning.
    pub async fn list_items(
        &self,
        repo_id: RepoId,
        after: Option<ItemCursor>,
        lifecycle_state: Option<LifecycleState>,
        limit: usize,
    ) -> Result<Vec<ItemRow>, CoreError> {
        Ok(self
            .storage
            .backend
            .list_items_readonly(&repo_id, after.as_ref(), lifecycle_state, limit)
            .await?)
    }

    /// List a folder's non-tombstoned descendants, bounded by `remaining + 1`.
    ///
    /// Returning one row beyond the caller's remaining budget lets profile
    /// validators reject oversized subtree work without materializing it.
    pub async fn list_non_tombstoned_descendants_bounded(
        &self,
        repo_id: RepoId,
        root_item_id: ItemId,
        remaining: usize,
    ) -> Result<Vec<(ItemId, i64)>, CoreError> {
        let mut txn = self.storage.backend.begin_txn().await?;
        let rows = txn
            .list_non_tombstoned_descendants_bounded(&repo_id, &root_item_id, remaining)
            .await?;
        txn.rollback().await?;
        Ok(rows)
    }

    /// Assign `repo_seq` to committed unsequenced commits for a repo.
    ///
    /// This is a projection step used to provide a stable repo-wide activity cursor.
    /// It does not affect per-item linearity (`item_seq`).
    pub async fn sequence_repo_activity(
        &self,
        repo_id: RepoId,
        limit: usize,
    ) -> Result<usize, CoreError> {
        let mut txn = self.storage.backend.begin_txn().await?;
        let res = txn.sequence_repo_activity(&repo_id, limit).await;
        match res {
            Ok(sequenced) => {
                txn.commit().await?;
                Ok(sequenced)
            }
            Err(err) => {
                if let Err(rb) = txn.rollback().await {
                    warn!(repo_id = %repo_id, error = %rb, "sequence_repo_activity rollback failed");
                }
                Err(CoreError::StorageError(err))
            }
        }
    }

    /// Append a local commit proposal to the given repo.
    #[instrument(
        skip(self, proposal),
        fields(
            repo_id = %repo_id,
            events = %proposal.events.len()
        )
    )]
    pub async fn append(
        &self,
        repo_id: RepoId,
        proposal: CommitProposal,
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_with_base_seq_mode(
            repo_id,
            proposal,
            Vec::new(),
            AppendBaseSeqMode::RequirePresent,
            &[],
            AppendProfile::Generic,
        )
        .await
    }

    /// Append one atomic commit containing ordinary item events followed by
    /// attached ref events. The two sequence domains remain independent.
    pub async fn append_with_refs(
        &self,
        repo_id: RepoId,
        proposal: CommitProposal,
        ref_events: Vec<RefEventProposal>,
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_with_base_seq_mode(
            repo_id,
            proposal,
            ref_events,
            AppendBaseSeqMode::RequirePresent,
            &[],
            AppendProfile::Generic,
        )
        .await
    }

    /// Append while requiring unrelated item heads to remain unchanged.
    ///
    /// Guards participate in the same ordered item lock set and transaction as
    /// the commit. They are preconditions only and never become commit events.
    pub async fn append_guarded(
        &self,
        repo_id: RepoId,
        proposal: CommitProposal,
        guards: &[(ItemId, i64)],
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_with_base_seq_mode(
            repo_id,
            proposal,
            Vec::new(),
            AppendBaseSeqMode::RequirePresent,
            guards,
            AppendProfile::Generic,
        )
        .await
    }

    /// Apply a guarded append inside a caller-owned storage transaction.
    ///
    /// Embedders use this to atomically persist an item and its initial access
    /// policy. All normal append checks apply. The platform must resolve an
    /// already accepted command by operation id before attempting a new append.
    /// The returned receipt is provisional: the caller MUST commit this same
    /// transaction before publishing it or starting projection/delivery. On any
    /// error, the caller MUST roll back the entire transaction, not commit a
    /// partially applied operation. Acquire platform authority locks before
    /// calling this method; Core then acquires its ordinary conflict locks.
    pub async fn append_guarded_in_transaction(
        &self,
        txn: &mut dyn StorageTxn,
        repo_id: RepoId,
        proposal: CommitProposal,
        guards: &[(ItemId, i64)],
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_one_locked_txn(
            txn,
            repo_id,
            proposal,
            Vec::new(),
            AppendBaseSeqMode::RequirePresent,
            guards,
            AppendProfile::Generic,
            OperationKind::Append,
        )
        .await
        .map(|applied| applied.accepted)
    }

    /// Append attached refs inside a caller-owned transaction. Acquire embedder
    /// authority locks first; commit the transaction before publishing the receipt.
    /// Roll back the entire transaction on any error.
    pub async fn append_with_refs_in_transaction(
        &self,
        txn: &mut dyn StorageTxn,
        repo_id: RepoId,
        proposal: CommitProposal,
        ref_events: Vec<RefEventProposal>,
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_one_locked_txn(
            txn,
            repo_id,
            proposal,
            ref_events,
            AppendBaseSeqMode::RequirePresent,
            &[],
            AppendProfile::Generic,
            OperationKind::Append,
        )
        .await
        .map(|applied| applied.accepted)
    }

    /// Append item/ref events with explicit unrelated-head guards in one
    /// caller-owned transaction. Acquire embedder authority locks first; commit
    /// this transaction before publishing a receipt. Roll back on any error.
    pub async fn append_guarded_with_refs_in_transaction(
        &self,
        txn: &mut dyn StorageTxn,
        repo_id: RepoId,
        proposal: CommitProposal,
        ref_events: Vec<RefEventProposal>,
        guards: &[(ItemId, i64)],
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_one_locked_txn(
            txn,
            repo_id,
            proposal,
            ref_events,
            AppendBaseSeqMode::RequirePresent,
            guards,
            AppendProfile::Generic,
            OperationKind::Append,
        )
        .await
        .map(|applied| applied.accepted)
    }

    /// Apply a content-only append with server-materialized baselines in a
    /// caller-owned transaction. Structural modifiers remain forbidden.
    /// Acquire embedder authority locks first. Commit before publishing the
    /// receipt, and roll back the entire transaction on any error.
    pub async fn append_materializing_base_seq_in_transaction(
        &self,
        txn: &mut dyn StorageTxn,
        repo_id: RepoId,
        proposal: CommitProposal,
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_one_locked_txn(
            txn,
            repo_id,
            proposal,
            Vec::new(),
            AppendBaseSeqMode::MaterializeUnderItemLocks,
            &[],
            AppendProfile::Generic,
            OperationKind::Append,
        )
        .await
        .map(|applied| applied.accepted)
    }

    /// Append a checked FILE/FOLDER proposal with exact subtree closure guards.
    ///
    /// This is the only Core append path that enforces parent-closed LIVE state
    /// and transaction-time subtree membership. Event vocabulary and product
    /// policy remain outside Core.
    pub async fn append_file_folder_checked(
        &self,
        repo_id: RepoId,
        proposal: CommitProposal,
        guards: &[(ItemId, i64)],
        subtree_guards: &[SubtreeClosureGuard],
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_with_base_seq_mode(
            repo_id,
            proposal,
            Vec::new(),
            AppendBaseSeqMode::RequirePresent,
            guards,
            AppendProfile::FileFolder {
                subtree_guards,
                import_job_id: None,
            },
        )
        .await
    }

    /// Checked FILE/FOLDER append with attached ref events in the same commit.
    pub async fn append_file_folder_checked_with_refs(
        &self,
        repo_id: RepoId,
        proposal: CommitProposal,
        ref_events: Vec<RefEventProposal>,
        guards: &[(ItemId, i64)],
        subtree_guards: &[SubtreeClosureGuard],
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_with_base_seq_mode(
            repo_id,
            proposal,
            ref_events,
            AppendBaseSeqMode::RequirePresent,
            guards,
            AppendProfile::FileFolder {
                subtree_guards,
                import_job_id: None,
            },
        )
        .await
    }

    /// Apply the same checked FILE/FOLDER append in a caller-owned transaction.
    /// Acquire platform authority locks first. The receipt is provisional until
    /// the caller commits this transaction; roll back the entire transaction on
    /// any error, as with `append_guarded_in_transaction`.
    pub async fn append_file_folder_checked_with_refs_in_transaction(
        &self,
        txn: &mut dyn StorageTxn,
        repo_id: RepoId,
        proposal: CommitProposal,
        ref_events: Vec<RefEventProposal>,
        guards: &[(ItemId, i64)],
        subtree_guards: &[SubtreeClosureGuard],
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_one_locked_txn(
            txn,
            repo_id,
            proposal,
            ref_events,
            AppendBaseSeqMode::RequirePresent,
            guards,
            AppendProfile::FileFolder {
                subtree_guards,
                import_job_id: None,
            },
            OperationKind::Append,
        )
        .await
        .map(|applied| applied.accepted)
    }

    /// Ingest authenticated attached-ref events for an existing federated FILE.
    ///
    /// The authority stream cursor is owned by the caller. Core owns only the
    /// FILE ref ledger CAS and the local idempotent commit identified by
    /// `proposal.operation_id`.
    pub async fn ingest_federated_item_refs(
        &self,
        repo_id: RepoId,
        federation_stream_id: &str,
        proposal: CommitProposal,
        ref_events: Vec<RefEventProposal>,
    ) -> Result<AcceptedCommit, CoreError> {
        if federation_stream_id.trim().is_empty() {
            return Err(CoreError::InvalidProposal(
                "federation_stream_id is required".to_string(),
            ));
        }
        if !proposal.events.is_empty() || ref_events.is_empty() {
            return Err(CoreError::InvalidProposal(
                "federated item ref ingest requires ref events only".to_string(),
            ));
        }
        let owner = self
            .get_item_by_federation_stream_id(repo_id.clone(), federation_stream_id.to_string())
            .await?
            .ok_or_else(|| {
                CoreError::InvalidProposal("unknown federation_stream_id".to_string())
            })?;
        if owner.item_type != "FILE"
            || ref_events.iter().any(|event| {
                event.owner_item_id != owner.item_id
                    || !event.event_type.starts_with("FILE_VERSION::")
            })
        {
            return Err(CoreError::InvalidProposal(
                "federated item refs must target the stream's FILE owner".to_string(),
            ));
        }
        let operation_id = proposal
            .operation_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                CoreError::InvalidProposal(
                    "operation_id is required for federated item ref ingest".to_string(),
                )
            })?
            .to_string();
        if let Some(replayed) = self
            .replay_federated_item_refs(&repo_id, &operation_id, &proposal, &ref_events)
            .await?
        {
            return Ok(replayed);
        }
        let result = self
            .append_with_base_seq_mode_for_operation(
                repo_id.clone(),
                proposal.clone(),
                ref_events.clone(),
                AppendBaseSeqMode::RequirePresent,
                &[],
                AppendProfile::Generic,
                OperationKind::IngestFederated,
            )
            .await;
        match result {
            Ok(accepted) => Ok(accepted),
            Err(error) => match self
                .replay_federated_item_refs(&repo_id, &operation_id, &proposal, &ref_events)
                .await?
            {
                Some(replayed) => Ok(replayed),
                None => Err(error),
            },
        }
    }

    async fn replay_federated_item_refs(
        &self,
        repo_id: &RepoId,
        operation_id: &str,
        proposal: &CommitProposal,
        ref_events: &[RefEventProposal],
    ) -> Result<Option<AcceptedCommit>, CoreError> {
        let Some(commit) = self
            .get_commit_by_operation_id(repo_id.clone(), operation_id)
            .await?
        else {
            return Ok(None);
        };
        let rows = self
            .list_commit_events(
                repo_id.clone(),
                commit.commit_id.clone(),
                ref_events.len() + 1,
            )
            .await?;
        let exact = commit.author_id == proposal.author_id
            && commit.committer_id == proposal.committer_id
            && commit.message == proposal.message
            && commit.occurred_at == proposal.occurred_at
            && rows.len() == ref_events.len()
            && rows
                .iter()
                .zip(ref_events)
                .all(|(row, expected)| match row {
                    CommitEventRow::Ref(row) => canonical_ref_event_proposal_bytes(expected)
                        .is_ok_and(|payload| payload == row.payload),
                    CommitEventRow::Item(_) => false,
                });
        if !exact {
            return Err(CoreError::InvalidProposal(
                "operation_id already used with different federated ref payload".to_string(),
            ));
        }
        let ref_event_seqs = rows
            .into_iter()
            .filter_map(|row| match row {
                CommitEventRow::Ref(row) => Some(row.ref_seq),
                CommitEventRow::Item(_) => None,
            })
            .collect();
        Ok(Some(AcceptedCommit {
            commit_id: commit.commit_id,
            repo_seq: commit.repo_seq,
            created_at: commit.created_at,
            occurred_at: commit.occurred_at,
            created_items: Vec::new(),
            event_item_seqs: Vec::new(),
            ref_event_seqs,
        }))
    }

    /// Append one checked FILE/FOLDER commit belonging to a bulk import job.
    ///
    /// This is the ordinary checked append transaction with a required
    /// hash-bound UUIDv4 provenance field. No validation or limit is relaxed.
    pub async fn append_file_folder_import_checked(
        &self,
        repo_id: RepoId,
        proposal: CommitProposal,
        ref_events: Vec<RefEventProposal>,
        guards: &[(ItemId, i64)],
        subtree_guards: &[SubtreeClosureGuard],
        import_job_id: String,
    ) -> Result<AcceptedCommit, CoreError> {
        validate_import_job_id(&import_job_id)?;
        self.append_with_base_seq_mode(
            repo_id,
            proposal,
            ref_events,
            AppendBaseSeqMode::RequirePresent,
            guards,
            AppendProfile::FileFolder {
                subtree_guards,
                import_job_id: Some(&import_job_id),
            },
        )
        .await
    }

    /// Append a local commit proposal without requiring caller-supplied `base_seq` for existing items.
    ///
    /// This still uses CAS under the hood, but `base_seq` is materialized under the repo lock
    /// so append-mode callers do not devolve into retry loops under multi-writer contention.
    #[instrument(
        skip(self, proposal),
        fields(
            repo_id = %repo_id,
            events = %proposal.events.len()
        )
    )]
    pub async fn append_materializing_base_seq(
        &self,
        repo_id: RepoId,
        proposal: CommitProposal,
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_with_base_seq_mode(
            repo_id,
            proposal,
            Vec::new(),
            AppendBaseSeqMode::MaterializeUnderItemLocks,
            &[],
            AppendProfile::Generic,
        )
        .await
    }

    /// Append multiple local commit proposals to the given repo sequentially.
    ///
    /// NOTE: despite the name, this does not currently batch proposals into a single
    /// storage transaction. It is a convenience helper that returns per-proposal
    /// results.
    pub async fn append_batch(
        &self,
        repo_id: RepoId,
        proposals: Vec<CommitProposal>,
    ) -> Result<Vec<Result<AcceptedCommit, CoreError>>, CoreError> {
        self.append_batch_with_base_seq_modes(
            repo_id,
            proposals
                .into_iter()
                .map(|proposal| (proposal, AppendBaseSeqMode::RequirePresent))
                .collect(),
        )
        .await
    }

    /// Batch variant of [`Repo::append_materializing_base_seq`].
    pub async fn append_batch_materializing_base_seq(
        &self,
        repo_id: RepoId,
        proposals: Vec<CommitProposal>,
    ) -> Result<Vec<Result<AcceptedCommit, CoreError>>, CoreError> {
        self.append_batch_with_base_seq_modes(
            repo_id,
            proposals
                .into_iter()
                .map(|proposal| (proposal, AppendBaseSeqMode::MaterializeUnderItemLocks))
                .collect(),
        )
        .await
    }

    /// Append a mixed batch of proposals sequentially, allowing each proposal to
    /// choose whether `base_seq` is caller-supplied (`false`) or materialized
    /// under item locks (`true`).
    pub async fn append_batch_mixed_base_seq_mode(
        &self,
        repo_id: RepoId,
        proposals: Vec<(CommitProposal, bool)>,
    ) -> Result<Vec<Result<AcceptedCommit, CoreError>>, CoreError> {
        self.append_batch_with_base_seq_modes(
            repo_id,
            proposals
                .into_iter()
                .map(|(proposal, materialize_base_seq)| {
                    let mode = if materialize_base_seq {
                        AppendBaseSeqMode::MaterializeUnderItemLocks
                    } else {
                        AppendBaseSeqMode::RequirePresent
                    };
                    (proposal, mode)
                })
                .collect(),
        )
        .await
    }

    fn log_append_applied(&self, repo_id: &RepoId, applied: &AppendApplied) {
        let accepted = &applied.accepted;
        info!(
            repo_id = %repo_id,
            commit_id = %accepted.commit_id,
            author_id = %applied.author_id,
            repo_seq = accepted.repo_seq,
            event_count = applied.event_count,
            message = applied.message.as_deref().unwrap_or(""),
            "local commit appended"
        );

        debug!(
            repo_id = %repo_id,
            commit_id = %accepted.commit_id,
            author_id = %applied.author_id,
            repo_seq = accepted.repo_seq,
            event_count = applied.event_count,
            message = ?applied.message,
            created_items = ?accepted.created_items,
            "local commit append details"
        );

        trace!(
            repo_id = %repo_id,
            commit_id = %accepted.commit_id,
            "local commit append trace"
        );
    }

    async fn append_with_base_seq_mode(
        &self,
        repo_id: RepoId,
        proposal: CommitProposal,
        ref_events: Vec<RefEventProposal>,
        base_seq_mode: AppendBaseSeqMode,
        guards: &[(ItemId, i64)],
        profile: AppendProfile<'_>,
    ) -> Result<AcceptedCommit, CoreError> {
        self.append_with_base_seq_mode_for_operation(
            repo_id,
            proposal,
            ref_events,
            base_seq_mode,
            guards,
            profile,
            OperationKind::Append,
        )
        .await
    }

    async fn append_with_base_seq_mode_for_operation(
        &self,
        repo_id: RepoId,
        proposal: CommitProposal,
        ref_events: Vec<RefEventProposal>,
        base_seq_mode: AppendBaseSeqMode,
        guards: &[(ItemId, i64)],
        profile: AppendProfile<'_>,
        operation_kind: OperationKind,
    ) -> Result<AcceptedCommit, CoreError> {
        let mut txn = self.storage.backend.begin_txn().await?;
        let res = self
            .append_one_locked_txn(
                &mut *txn,
                repo_id.clone(),
                proposal,
                ref_events,
                base_seq_mode,
                guards,
                profile,
                operation_kind,
            )
            .await;
        match res {
            Ok(applied) => {
                txn.commit().await?;
                self.log_append_applied(&repo_id, &applied);
                Ok(applied.accepted)
            }
            Err(err) => {
                if let Err(rb) = txn.rollback().await {
                    warn!(repo_id = %repo_id, error = %rb, "append rollback failed");
                }
                Err(err)
            }
        }
    }

    async fn append_batch_with_base_seq_modes(
        &self,
        repo_id: RepoId,
        proposals: Vec<(CommitProposal, AppendBaseSeqMode)>,
    ) -> Result<Vec<Result<AcceptedCommit, CoreError>>, CoreError> {
        let mut out: Vec<Result<AcceptedCommit, CoreError>> = Vec::with_capacity(proposals.len());
        for (proposal, mode) in proposals {
            out.push(
                self.append_with_base_seq_mode(
                    repo_id.clone(),
                    proposal,
                    Vec::new(),
                    mode,
                    &[],
                    AppendProfile::Generic,
                )
                .await,
            );
        }
        Ok(out)
    }

    async fn append_one_locked_txn<T: StorageTxn + ?Sized>(
        &self,
        txn: &mut T,
        repo_id: RepoId,
        proposal: CommitProposal,
        ref_events: Vec<RefEventProposal>,
        base_seq_mode: AppendBaseSeqMode,
        guards: &[(ItemId, i64)],
        profile: AppendProfile<'_>,
        operation_kind: OperationKind,
    ) -> Result<AppendApplied, CoreError> {
        let mut proposal = proposal;
        let ref_events = ref_events;
        let (subtree_guards, check_file_folder_parent_closure, import_job_id) = match profile {
            AppendProfile::Generic => (&[][..], false, None),
            AppendProfile::FileFolder {
                subtree_guards,
                import_job_id,
            } => (subtree_guards, true, import_job_id),
        };
        let repo_seq: Option<i64> = None;
        let operation_id = proposal
            .operation_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                CoreError::InvalidProposal(
                    "operation_id is required for append commits".to_string(),
                )
            })?
            .to_string();
        validate_commit_metadata(
            &proposal.author_id,
            &proposal.committer_id,
            proposal.message.as_deref(),
            Some(&operation_id),
        )?;

        let combined_event_count = proposal
            .events
            .len()
            .checked_add(ref_events.len())
            .ok_or_else(|| CoreError::InvalidProposal("commit event count overflow".to_string()))?;
        if combined_event_count == 0 {
            return Err(CoreError::EmptyCommit);
        }

        let mut observed_ref_bases: HashMap<ItemId, i64> = HashMap::new();
        for event in &ref_events {
            if event.event_type.trim().is_empty() {
                return Err(CoreError::InvalidProposal(
                    "ref event_type must not be empty".to_string(),
                ));
            }
            if event.base_ref_seq < 0 {
                return Err(CoreError::InvalidProposal(
                    "base_ref_seq must be non-negative".to_string(),
                ));
            }
            match observed_ref_bases.entry(event.owner_item_id.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(event.base_ref_seq);
                }
                std::collections::hash_map::Entry::Occupied(entry)
                    if *entry.get() != event.base_ref_seq =>
                {
                    return Err(CoreError::InvalidProposal(
                        "inconsistent base_ref_seq for owner within a single commit".to_string(),
                    ));
                }
                std::collections::hash_map::Entry::Occupied(_) => {}
            }
        }

        if base_seq_mode == AppendBaseSeqMode::MaterializeUnderItemLocks
            && proposal.events.iter().any(|ev| {
                ev.create_item.is_some()
                    || ev.set_parent.is_some()
                    || ev.set_name.is_some()
                    || ev.set_tombstoned.is_some()
                    || ev.set_in_trash.is_some()
                    || ev.set_is_federated.is_some()
                    || ev.set_federation_stream_id.is_some()
            })
        {
            return Err(CoreError::InvalidProposal(
                "server-materialized base_seq is restricted to content-only commits".to_string(),
            ));
        }

        // Pre-resolve target item ids for all events, assigning new ids for
        // create_item events that omit item_id and validating that non-create
        // events specify an item_id.
        let mut event_item_ids: Vec<ItemId> = Vec::with_capacity(proposal.events.len());
        let mut created_items: Vec<CreatedItemInfo> = Vec::new();
        let mut create_item_ids: HashSet<ItemId> = HashSet::new();
        for (idx, ev) in proposal.events.iter().enumerate() {
            // Validate structural/federation modifier combinations at the event level.
            if ev.create_item.is_some()
                && (ev.set_parent.is_some()
                    || ev.set_name.is_some()
                    || ev.set_tombstoned.is_some()
                    || ev.set_in_trash.is_some()
                    || ev.set_is_federated.is_some()
                    || ev.set_federation_stream_id.is_some())
            {
                return Err(CoreError::InvalidProposal(
                    "create_item events must not also use structural or federation modifiers"
                        .to_string(),
                ));
            }

            if ev.set_is_federated.is_some() || ev.set_federation_stream_id.is_some() {
                // Federation transitions in append() may only set is_federated = true
                // with a non-empty federation_stream_id, and must be structurally pure.
                match ev.set_is_federated {
                    Some(true) => {
                        let Some(fid) = ev.set_federation_stream_id.as_deref() else {
                            return Err(CoreError::IllegalFederationTransition);
                        };
                        if fid.is_empty() {
                            return Err(CoreError::IllegalFederationTransition);
                        }
                    }
                    Some(false) => {
                        // Unfederate must be ingested, not appended.
                        return Err(CoreError::IllegalFederationTransition);
                    }
                    None => {
                        // set_federation_stream_id without an explicit mode toggle is illegal.
                        return Err(CoreError::IllegalFederationTransition);
                    }
                }

                if ev.set_parent.is_some()
                    || ev.set_name.is_some()
                    || ev.set_tombstoned.is_some()
                    || ev.set_in_trash.is_some()
                {
                    return Err(CoreError::IllegalFederationTransition);
                }
            }

            if ev.create_item.is_some() {
                let item_id = match &ev.item_id {
                    Some(id) => id.clone(),
                    None => ItemId::new(),
                };

                // Multiple create_item events for the same item_id are illegal.
                if !create_item_ids.insert(item_id.clone()) {
                    return Err(CoreError::InvalidProposal(
                        "duplicate create_item for item_id within a single commit".to_string(),
                    ));
                }

                event_item_ids.push(item_id.clone());
                created_items.push(CreatedItemInfo {
                    event_idx: idx,
                    item_id,
                });
            } else {
                let item_id = ev.item_id.clone().ok_or_else(|| {
                    CoreError::InvalidProposal("event missing item_id".to_string())
                })?;
                event_item_ids.push(item_id);
            }
        }

        // Enforce base_seq shape rules from the spec:
        // - For new items created in this commit: base_seq MUST be null for all events.
        // - For existing items:
        //   - strict append: base_seq MUST be present (integer) for all events.
        //   - append-mode (server-materialized): base_seq MUST be null for all events.
        for (idx, item_id) in event_item_ids.iter().enumerate() {
            let ev = &proposal.events[idx];
            if create_item_ids.contains(item_id) {
                if ev.base_seq.is_some() {
                    return Err(CoreError::InvalidProposal(
                        "base_seq must be null for events targeting a newly created item"
                            .to_string(),
                    ));
                }
            } else {
                match base_seq_mode {
                    AppendBaseSeqMode::RequirePresent => {
                        if ev.base_seq.is_none() {
                            return Err(CoreError::InvalidProposal(
                                "base_seq must be present for events targeting an existing item"
                                    .to_string(),
                            ));
                        }
                    }
                    AppendBaseSeqMode::MaterializeUnderItemLocks => {
                        if ev.base_seq.is_some() {
                            return Err(CoreError::InvalidProposal(
                                "base_seq must be null for append-mode commits targeting an existing item"
                                    .to_string(),
                            ));
                        }
                    }
                }
            }
        }

        // If an item is created in this commit and has additional events,
        // require that the first event for that item is the create_item event.
        for created in &created_items {
            let first_idx = event_item_ids
                .iter()
                .position(|id| *id == created.item_id)
                .expect("created item id must be present in event_item_ids");
            if first_idx != created.event_idx {
                return Err(CoreError::InvalidProposal(
                    "create_item event must be the first event for that item within the commit"
                        .to_string(),
                ));
            }
        }

        // Enforce at most one federation mode change per item within a commit.
        let mut mode_change_seen: HashMap<ItemId, ()> = HashMap::new();
        for (idx, ev) in proposal.events.iter().enumerate() {
            if ev.set_is_federated.is_some() {
                let item_id = &event_item_ids[idx];
                if mode_change_seen.insert(item_id.clone(), ()).is_some() {
                    return Err(CoreError::IllegalFederationTransition);
                }
            }
        }

        let touched_items: BTreeSet<ItemId> = event_item_ids.iter().cloned().collect();
        let ref_owner_items: BTreeSet<ItemId> = ref_events
            .iter()
            .map(|event| event.owner_item_id.clone())
            .collect();
        let state_item_ids: BTreeSet<ItemId> =
            touched_items.union(&ref_owner_items).cloned().collect();

        let mut combined_guards: Vec<(ItemId, i64)> = guards.to_vec();
        let mut closure_items: BTreeSet<ItemId> = BTreeSet::new();
        for guard in subtree_guards {
            if !touched_items.contains(&guard.root_item_id) {
                return Err(CoreError::InvalidProposal(
                    "subtree closure guard root must be touched by the proposal".to_string(),
                ));
            }
            let descendant_count = guard.expected_non_tombstoned_descendants.len();
            if guard.require_empty_live_descendants && descendant_count != 0 {
                return Err(CoreError::InvalidProposal(
                    "empty-live subtree guard cannot list descendants".to_string(),
                ));
            }
            if !closure_items.insert(guard.root_item_id.clone()) {
                return Err(CoreError::InvalidProposal(
                    "subtree closure guards overlap".to_string(),
                ));
            }

            let mut previous_item_id: Option<&ItemId> = None;
            for (item_id, head_seq) in &guard.expected_non_tombstoned_descendants {
                if previous_item_id.is_some_and(|previous| previous >= item_id) {
                    return Err(CoreError::InvalidProposal(
                        "subtree closure descendants must be strictly sorted by item_id"
                            .to_string(),
                    ));
                }
                previous_item_id = Some(item_id);
                if !closure_items.insert(item_id.clone()) {
                    return Err(CoreError::InvalidProposal(
                        "subtree closure guards overlap".to_string(),
                    ));
                }
                combined_guards.push((item_id.clone(), *head_seq));
            }
        }

        // Lock all conflict keys up-front to:
        // - preserve strict per-item linearity under multi-writer load, and
        // - make structural checks deterministic enough to be meaningful.
        //
        // We lock:
        // - all touched item ids, and
        // - all referenced parent folder ids (create/move targets),
        // to prevent concurrent structural writes from racing into cycles.
        let mut lock_item_ids: BTreeSet<ItemId> = state_item_ids.clone();
        for (item_id, _) in &combined_guards {
            lock_item_ids.insert(item_id.clone());
        }
        for ev in &proposal.events {
            if let Some(ci) = ev.create_item.as_ref()
                && let Some(parent) = ci.parent_folder_id.as_ref()
            {
                lock_item_ids.insert(parent.clone());
            }
            if let Some(Some(parent)) = ev.set_parent.as_ref() {
                lock_item_ids.insert(parent.clone());
            }
        }

        // Read the current namespace parent for every existing rename or
        // relocation before locking. SET_NAME has no parent in its event, and
        // SET_PARENT names only the destination; both must also lock the source
        // namespace so a concurrent writer cannot reuse the vacated name while
        // this commit is accepting.
        // Existing-item proposals carry base_seq; if the item changes between
        // this read and the post-lock reload, the normal base_seq check rejects
        // the stale proposal.
        let existing_structural_item_ids: Vec<ItemId> = proposal
            .events
            .iter()
            .zip(event_item_ids.iter())
            .filter(|(event, item_id)| {
                (event.set_name.is_some()
                    || event.set_parent.is_some()
                    || (check_file_folder_parent_closure && event.set_in_trash == Some(false)))
                    && !create_item_ids.contains(*item_id)
            })
            .map(|(_, item_id)| item_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let source_namespace_parents: Vec<Option<ItemId>> =
            if existing_structural_item_ids.is_empty() {
                Vec::new()
            } else {
                txn.get_items(&repo_id, &existing_structural_item_ids)
                    .await?
                    .into_iter()
                    .map(|item| item.parent_folder_id)
                    .collect()
            };

        let mut lock_keys: Vec<i64> = lock_item_ids
            .iter()
            .map(|item_id| advisory_lock_key_for_item(&repo_id, item_id))
            .collect();
        for ev in &proposal.events {
            if let Some(ci) = ev.create_item.as_ref() {
                lock_keys.push(advisory_lock_key_for_namespace(
                    &repo_id,
                    ci.parent_folder_id.as_ref(),
                ));
            }
            if let Some(parent) = ev.set_parent.as_ref() {
                lock_keys.push(advisory_lock_key_for_namespace(&repo_id, parent.as_ref()));
            }
        }
        for parent in &source_namespace_parents {
            lock_keys.push(advisory_lock_key_for_namespace(&repo_id, parent.as_ref()));
        }
        for guard in subtree_guards {
            if guard.require_empty_live_descendants {
                lock_keys.push(advisory_lock_key_for_namespace(
                    &repo_id,
                    Some(&guard.root_item_id),
                ));
            }
        }
        lock_keys.sort_unstable();
        lock_keys.dedup();
        txn.lock_conflict_keys(&lock_keys).await?;

        // Load current item state for all touched items in one call to avoid
        // per-item round-trips on large commits.
        let touched_item_ids: Vec<ItemId> = touched_items.iter().cloned().collect();
        let guarded_and_touched_item_ids: Vec<ItemId> = lock_item_ids.iter().cloned().collect();
        let mut existing_rows_by_id: HashMap<ItemId, ItemRow> = txn
            .get_items(&repo_id, &guarded_and_touched_item_ids)
            .await?
            .into_iter()
            .map(|row| (row.item_id.clone(), row))
            .collect();

        for (guard_item_id, expected_head_seq) in &combined_guards {
            let row =
                existing_rows_by_id
                    .get(guard_item_id)
                    .ok_or_else(|| CoreError::ItemNotFound {
                        repo_id: repo_id.clone(),
                        item_id: guard_item_id.clone(),
                    })?;
            if row.head_seq != *expected_head_seq {
                return Err(CoreError::BaseSeqMismatch {
                    repo_id: repo_id.clone(),
                    item_id: guard_item_id.clone(),
                    expected: row.head_seq,
                    found: *expected_head_seq,
                });
            }
        }

        for guard in subtree_guards {
            let root = existing_rows_by_id
                .get(&guard.root_item_id)
                .ok_or_else(|| CoreError::ItemNotFound {
                    repo_id: repo_id.clone(),
                    item_id: guard.root_item_id.clone(),
                })?;
            if root.item_type != "FOLDER" {
                return Err(CoreError::InvalidProposal(
                    "subtree closure guard root must be a FOLDER".to_string(),
                ));
            }

            if guard.require_empty_live_descendants {
                if txn
                    .has_live_descendants(&repo_id, &guard.root_item_id)
                    .await?
                {
                    return Err(CoreError::SubtreeChanged {
                        repo_id: repo_id.clone(),
                        root_item_id: guard.root_item_id.clone(),
                    });
                }
                continue;
            }

            let mut actual = txn
                .list_non_tombstoned_descendants_bounded(
                    &repo_id,
                    &guard.root_item_id,
                    guard.expected_non_tombstoned_descendants.len(),
                )
                .await?;
            actual.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            let membership_matches = actual.len()
                == guard.expected_non_tombstoned_descendants.len()
                && actual
                    .iter()
                    .zip(&guard.expected_non_tombstoned_descendants)
                    .all(|(found, expected)| found.0 == expected.0);
            if !membership_matches {
                return Err(CoreError::SubtreeChanged {
                    repo_id: repo_id.clone(),
                    root_item_id: guard.root_item_id.clone(),
                });
            }
        }

        // Load current item state for all touched items and apply basic invariants.
        let mut items: HashMap<ItemId, ItemRow> = HashMap::new();
        let tombstone = TombstoneFinality;
        let mode_rule = SequencingModeRule;
        let ctx = InvariantContext {
            repo_id: &repo_id,
            op: operation_kind,
        };

        let commit_created_at = txn.acceptance_timestamp().await?;

        for item_id in &state_item_ids {
            // If this item is created in this commit, synthesize an ItemRow
            // instead of loading from storage.
            let is_created = create_item_ids.contains(item_id);

            if is_created {
                // Prevent accidental overwrite of an existing item. This cannot
                // happen with core-assigned ids, but becomes possible when
                // create_item proposals specify an explicit item_id.
                if existing_rows_by_id.contains_key(item_id) {
                    return Err(CoreError::ItemAlreadyExists {
                        repo_id: repo_id.clone(),
                        item_id: item_id.clone(),
                    });
                }

                // Use the first create_item event for this item to seed fields.
                let (idx, ev) = proposal
                    .events
                    .iter()
                    .enumerate()
                    .find(|(i, ev)| ev.create_item.is_some() && event_item_ids[*i] == *item_id)
                    .expect("created item must have a corresponding event");

                let ci = ev.create_item.as_ref().expect("checked above");
                let state = ItemState::from_create(ci, self.config.default_sequencing_mode, None)
                    .map_err(|e| map_name_error(&repo_id, Some(item_id), &ci.name, e))?;
                let row = ItemRow {
                    repo_id: repo_id.clone(),
                    item_id: item_id.clone(),
                    item_type: state.item_type,
                    parent_folder_id: state.parent_folder_id,
                    name: state.name,
                    name_key: state.name_key,
                    head_seq: 0,
                    ref_head_seq: 0,
                    lifecycle_state: state.lifecycle_state,
                    created_at: commit_created_at,
                    created_by: proposal.author_id.clone(),
                    sequencing_mode: state.sequencing_mode,
                    federation_stream_id: state.federation_stream_id,
                };

                tombstone.check(&ctx, item_id, &row, &proposal.events[idx..=idx])?;
                mode_rule.check(&ctx, item_id, &row, &proposal.events[idx..=idx])?;

                items.insert(item_id.clone(), row);
            } else {
                let row =
                    existing_rows_by_id
                        .remove(item_id)
                        .ok_or_else(|| CoreError::ItemNotFound {
                            repo_id: repo_id.clone(),
                            item_id: item_id.clone(),
                        })?;

                tombstone.check(&ctx, item_id, &row, &[])?;
                mode_rule.check(&ctx, item_id, &row, &[])?;

                items.insert(item_id.clone(), row);
            }
        }

        // Snapshot pre-commit item state before applying in-commit structural modifiers.
        // This is used to avoid spurious uniqueness failures on rename/move swaps.
        let items_before_modifiers = items.clone();

        // Append-mode callers do not supply `base_seq`. Materialize it under the repo lock so
        // multi-instance writers don't devolve into repeated `BaseSeqMismatch` retries.
        if base_seq_mode == AppendBaseSeqMode::MaterializeUnderItemLocks {
            for (idx, item_id) in event_item_ids.iter().enumerate() {
                if create_item_ids.contains(item_id) {
                    continue;
                }
                let Some(row) = items.get(item_id) else {
                    return Err(CoreError::ItemNotFound {
                        repo_id: repo_id.clone(),
                        item_id: item_id.clone(),
                    });
                };
                proposal.events[idx].base_seq = Some(row.head_seq);
            }
        }

        // Enforce per-item base_seq consistency and CAS preconditions:
        // for existing items, all events in this commit must agree on base_seq
        // and that value must match the current head_seq under the repo lock.
        let mut observed_base_seq: HashMap<ItemId, Option<i64>> = HashMap::new();
        for (idx, item_id) in event_item_ids.iter().enumerate() {
            let ev = &proposal.events[idx];
            let current = observed_base_seq
                .entry(item_id.clone())
                .or_insert(ev.base_seq);
            if *current != ev.base_seq {
                return Err(CoreError::InvalidProposal(
                    "inconsistent base_seq for item within a single commit".to_string(),
                ));
            }
        }
        for (item_id, base_seq) in &observed_base_seq {
            if create_item_ids.contains(item_id) {
                continue;
            }
            let Some(row) = items.get(item_id) else {
                return Err(CoreError::ItemNotFound {
                    repo_id: repo_id.clone(),
                    item_id: item_id.clone(),
                });
            };
            let found = base_seq.expect("existing-item base_seq presence was validated");
            if found != row.head_seq {
                return Err(CoreError::BaseSeqMismatch {
                    repo_id: repo_id.clone(),
                    item_id: item_id.clone(),
                    expected: row.head_seq,
                    found,
                });
            }
        }

        let mut ref_head_before: HashMap<ItemId, i64> = HashMap::new();
        for (owner_item_id, base_ref_seq) in &observed_ref_bases {
            let row = items
                .get(owner_item_id)
                .ok_or_else(|| CoreError::ItemNotFound {
                    repo_id: repo_id.clone(),
                    item_id: owner_item_id.clone(),
                })?;
            if create_item_ids.contains(owner_item_id) && *base_ref_seq != 0 {
                return Err(CoreError::RefBaseSeqMismatch {
                    repo_id: repo_id.clone(),
                    owner_item_id: owner_item_id.clone(),
                    expected: 0,
                    found: *base_ref_seq,
                });
            }
            if row.ref_head_seq != *base_ref_seq {
                return Err(CoreError::RefBaseSeqMismatch {
                    repo_id: repo_id.clone(),
                    owner_item_id: owner_item_id.clone(),
                    expected: row.ref_head_seq,
                    found: *base_ref_seq,
                });
            }
            ref_head_before.insert(owner_item_id.clone(), row.ref_head_seq);
        }

        // Apply in-commit structural modifiers (set_parent, set_name,
        // set_tombstoned, set_in_trash, set_is_federated, set_federation_stream_id)
        // in event order to derive the final per-item state for this commit.
        //
        // While doing so, capture parent_folder_id before/after each event so
        // we can persist denormalized folder information alongside events.
        let mut event_parent_after: Vec<Option<ItemId>> = Vec::with_capacity(proposal.events.len());
        let mut event_parent_before: Vec<Option<ItemId>> =
            Vec::with_capacity(proposal.events.len());

        // Fast-path: content-only commits (chat messages, file content, etc.) should not pay for
        // structural tree checks. The pre-commit graph is assumed valid; if we don't change parent
        // edges, we cannot introduce cycles or missing parents here.
        let file_folder_parent_closure_item_ids: BTreeSet<ItemId> =
            if check_file_folder_parent_closure {
                proposal
                    .events
                    .iter()
                    .zip(event_item_ids.iter())
                    .filter(|(event, _)| {
                        event.create_item.is_some()
                            || event.set_parent.is_some()
                            || event.set_in_trash == Some(false)
                    })
                    .map(|(_, item_id)| item_id.clone())
                    .collect()
            } else {
                BTreeSet::new()
            };
        let needs_parent_invariants = proposal
            .events
            .iter()
            .any(|ev| ev.create_item.is_some() || ev.set_parent.is_some())
            || !file_folder_parent_closure_item_ids.is_empty();
        let needs_name_uniqueness = proposal.events.iter().any(|ev| {
            ev.create_item.is_some()
                || ev.set_parent.is_some()
                || ev.set_name.is_some()
                || ev.set_tombstoned.is_some()
                || ev.set_in_trash.is_some()
        });

        for (idx, ev) in proposal.events.iter().enumerate() {
            let item_id = &event_item_ids[idx];
            let row = items.get_mut(item_id).expect("item state should exist");

            // Once an item is tombstoned within a commit, no further events
            // targeting it are allowed.
            if matches!(row.lifecycle_state, LifecycleState::Tombstoned) {
                return Err(CoreError::ItemTombstoned {
                    repo_id: repo_id.clone(),
                    item_id: item_id.clone(),
                });
            }

            let mut state = ItemState::from(&*row);
            let transition = apply_item_event(&mut state, ev).map_err(|error| {
                map_name_error(
                    &repo_id,
                    Some(item_id),
                    ev.set_name.as_deref().unwrap_or(&state.name),
                    error,
                )
            })?;
            state.write_to(row);
            event_parent_after.push(transition.parent_folder_id_after);
            event_parent_before.push(transition.parent_folder_id_before);
        }

        let mut ref_head_after = ref_head_before.clone();
        let mut ref_event_seqs = Vec::with_capacity(ref_events.len());
        for event in &ref_events {
            let row = items
                .get(&event.owner_item_id)
                .expect("validated ref owner must be loaded");
            if matches!(row.lifecycle_state, LifecycleState::Tombstoned) {
                return Err(CoreError::ItemTombstoned {
                    repo_id: repo_id.clone(),
                    item_id: event.owner_item_id.clone(),
                });
            }
            let head = ref_head_after
                .get_mut(&event.owner_item_id)
                .expect("validated ref owner head must be loaded");
            *head = head.checked_add(1).ok_or_else(|| {
                CoreError::InvalidProposal("ref sequence overflow".to_string())
            })?;
            ref_event_seqs.push(*head);
        }

        if needs_parent_invariants {
            // Enforce structural invariants on the final in-commit view.
            check_parent_existence_and_type(&ctx, txn, &items).await?;
            let ancestor_items = check_folder_acyclicity(&ctx, txn, &items).await?;
            if check_file_folder_parent_closure {
                check_file_folder_live_parent_closure(
                    &ctx,
                    &items,
                    &ancestor_items,
                    &file_folder_parent_closure_item_ids,
                )?;
            }
        }

        if needs_name_uniqueness {
            // Enforce name uniqueness on the in-commit view using canonical name_key.
            // This mirrors the database unique index over (repo_id, parent_folder_id, name_key)
            // for live items (lifecycle_state = LIVE).
            let mut seen: BTreeSet<(Option<ItemId>, String)> = BTreeSet::new();
            for item in items.values() {
                if !matches!(item.lifecycle_state, LifecycleState::Live) {
                    continue;
                }
                let key = (item.parent_folder_id.clone(), item.name_key.clone());
                if !seen.insert(key.clone()) {
                    return Err(CoreError::NameConflict {
                        repo_id: repo_id.clone(),
                        parent_folder_id: key.0,
                        name: item.name.clone(),
                    });
                }
            }
        }

        let can_use_head_only_item_updates = created_items.is_empty()
            && proposal.events.iter().all(|ev| {
                ev.create_item.is_none()
                    && ev.set_parent.is_none()
                    && ev.set_name.is_none()
                    && ev.set_tombstoned.is_none()
                    && ev.set_in_trash.is_none()
                    && ev.set_is_federated.is_none()
                    && ev.set_federation_stream_id.is_none()
            });

        let occurred_at = proposal.occurred_at;

        // Materialize item ids into the stored event payloads: create_item may
        // omit item_id in the input proposal, but persistence must be fully
        // deterministic and replayable from the event log.
        for (idx, ev) in proposal.events.iter_mut().enumerate() {
            ev.item_id = Some(event_item_ids[idx].clone());
        }

        let event_item_seqs = Sequencer::assign_local_item_seqs(&mut items, &event_item_ids);

        let commit_pk = CommitPk::new();
        let mut events_digest_hasher = Sha256::new();
        let mut payloads = self
            .validate_payload_limits(&proposal.events, &ref_events)?
            .into_iter();
        let mut event_rows: Vec<EventInsertRow> = Vec::with_capacity(proposal.events.len());
        for ((idx, ev), payload) in proposal.events.iter().enumerate().zip(payloads.by_ref()) {
            let item_id = event_item_ids[idx].clone();
            let item_seq = event_item_seqs[idx];
            let item_row = items
                .get(&item_id)
                .expect("event item id must exist in items map");
            validate_event_type_matches_item(
                &item_id,
                item_row.item_type.as_str(),
                ev.event_type.as_str(),
            )?;
            let parent_after = event_parent_after[idx].clone();
            let parent_before = event_parent_before[idx].clone();

            let event_hash = compute_sha256_digest_bytes(&payload);
            events_digest_hasher.update([COMMIT_EVENT_FAMILY_ITEM]);
            events_digest_hasher.update(event_hash);

            let event_row = EventInsertRow {
                repo_id: repo_id.clone(),
                commit_pk: commit_pk.clone(),
                commit_event_idx: idx as i32,
                item_id,
                item_seq,
                event_type: ev.event_type.clone(),
                metadata: ev.set_metadata.as_ref().and_then(|value| value.payload()).cloned(),
                payload,
                event_payload_digest: event_hash,
                created_at: commit_created_at,
                parent_folder_id: parent_after,
                parent_folder_id_before: parent_before,
            };

            event_rows.push(event_row);
        }

        let mut ref_event_rows: Vec<RefEventInsertRow> = Vec::with_capacity(ref_events.len());
        let item_event_count = proposal.events.len();
        for ((idx, event), payload) in ref_events.iter().enumerate().zip(payloads) {
            let event_hash = compute_sha256_digest_bytes(&payload);
            events_digest_hasher.update([COMMIT_EVENT_FAMILY_REF]);
            events_digest_hasher.update(event_hash);
            ref_event_rows.push(RefEventInsertRow {
                repo_id: repo_id.clone(),
                commit_pk: commit_pk.clone(),
                commit_event_idx: i32::try_from(item_event_count + idx).map_err(|_| {
                    CoreError::InvalidProposal("commit event index overflow".to_string())
                })?,
                owner_item_id: event.owner_item_id.clone(),
                ref_seq: ref_event_seqs[idx],
                event_type: event.event_type.clone(),
                metadata: event.set_metadata.as_ref().and_then(|value| value.payload()).cloned(),
                payload,
                event_payload_digest: event_hash,
                created_at: commit_created_at,
            });
        }

        let events_digest_bytes: [u8; 32] = events_digest_hasher.finalize().into();
        let events_digest =
            Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(events_digest_bytes))
                .expect("internal sha256 digest construction must be valid");

        let format_version = COMMIT_FORMAT_VERSION;
        let canonical_commit_bytes = canonical_commit_bytes_for_hash(
            &proposal.author_id,
            &proposal.committer_id,
            proposal.occurred_at,
            proposal.message.as_deref(),
            import_job_id,
            combined_event_count,
            &events_digest,
        )?;
        let commit_id = compute_commit_id(&canonical_commit_bytes);

        if let Err(err) = txn
            .insert_commit_sealed(
                &repo_id,
                &commit_pk,
                repo_seq,
                Some(operation_id.as_str()),
                format_version,
                import_job_id,
                &proposal.author_id,
                &proposal.committer_id,
                proposal.message.as_deref(),
                occurred_at,
                commit_created_at,
                &commit_id,
                &events_digest,
                i32::try_from(combined_event_count).map_err(|_| {
                    CoreError::InvalidProposal("commit event count overflow".to_string())
                })?,
            )
            .await
        {
            match err {
                StorageError::UniqueViolation { .. } => {
                    return Err(map_insert_commit_unique_violation(
                        txn,
                        &repo_id,
                        Some(operation_id.as_str()),
                        &commit_id,
                    )
                    .await?);
                }
                other => return Err(CoreError::StorageError(other)),
            }
        }
        txn.record_authored_commit(&repo_id, &commit_pk, commit_created_at)
            .await?;
        txn.append_events(&event_rows).await?;
        if can_use_head_only_item_updates {
            let mut head_updates: Vec<(ItemId, i64)> = Vec::with_capacity(touched_item_ids.len());
            for item_id in &touched_item_ids {
                let row = items
                    .get(item_id)
                    .expect("touched item must exist in items map");
                head_updates.push((item_id.clone(), row.head_seq));
            }
            txn.update_item_heads(&repo_id, &head_updates).await?;
        } else {
            let created_item_set: HashSet<ItemId> =
                created_items.iter().map(|ci| ci.item_id.clone()).collect();
            persist_items_with_live_parking(
                txn,
                &repo_id,
                &items,
                &items_before_modifiers,
                &created_item_set,
            )
            .await?;
        }

        if !ref_head_before.is_empty() {
            let mut updates = Vec::with_capacity(ref_head_before.len());
            for (owner_item_id, expected) in ref_head_before {
                let next = *ref_head_after
                    .get(&owner_item_id)
                    .expect("ref owner must retain an allocated head");
                updates.push((owner_item_id, expected, next));
            }
            updates.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            txn.update_item_ref_heads(&repo_id, &updates).await?;
        }
        // Ref rows carry an owner foreign key. Persist newly created owners
        // and their head before inserting the rows; the transaction remains
        // the atomic publication boundary.
        txn.append_ref_events(&ref_event_rows).await?;

        let accepted = AcceptedCommit {
            commit_id,
            repo_seq,
            created_at: commit_created_at,
            occurred_at,
            created_items,
            event_item_seqs,
            ref_event_seqs,
        };

        Ok(AppendApplied {
            accepted,
            author_id: proposal.author_id,
            message: proposal.message,
            event_count: i32::try_from(combined_event_count).map_err(|_| {
                CoreError::InvalidProposal("commit event count overflow".to_string())
            })?,
        })
    }

    /// Ingest a federated proposal into the given repo.
    #[instrument(
        skip(self, proposal),
        fields(
            repo_id = %repo_id,
            federation_stream_id = %proposal.federation_stream_id,
            item_seq = proposal.item_seq,
            events = %proposal.events.len()
        )
    )]
    pub async fn ingest(
        &self,
        repo_id: RepoId,
        proposal: FederatedProposal,
    ) -> Result<IngestedCommit, CoreError> {
        let mut txn = self.storage.backend.begin_txn().await?;
        match self
            .ingest_in_transaction(&mut *txn, repo_id.clone(), proposal)
            .await
        {
            Ok(ingested) => {
                txn.commit().await?;
                Ok(ingested)
            }
            Err(error) => {
                if let Err(rollback) = txn.rollback().await {
                    warn!(repo_id = %repo_id, error = %rollback, "ingest rollback failed");
                }
                Err(error)
            }
        }
    }

    /// Ingest accepted item history inside the caller's transaction.
    /// The caller must roll back the whole transaction on error. The result
    /// is provisional until commit. This permits local placement policy and
    /// its durable outcome to share the same conflict locks as Core writes.
    pub async fn ingest_in_transaction(
        &self,
        txn: &mut dyn StorageTxn,
        repo_id: RepoId,
        mut proposal: FederatedProposal,
    ) -> Result<IngestedCommit, CoreError> {
        if proposal.federation_stream_id.trim().is_empty() {
            return Err(CoreError::InvalidProposal(
                "federation_stream_id is required".to_string(),
            ));
        }
        validate_commit_metadata(
            &proposal.author_id,
            &proposal.committer_id,
            proposal.message.as_deref(),
            None,
        )?;
        if proposal.events.is_empty() {
            return Err(CoreError::EmptyCommit);
        }

        // All events must target the same item id if any item ids are supplied.
        // For federated proposals, the caller should typically omit item_id and
        // let core materialize the local item id based on federation_stream_id.
        let mut provided_item_id: Option<ItemId> = None;
        for ev in &proposal.events {
            if let Some(ev_item_id) = &ev.item_id {
                match &provided_item_id {
                    None => provided_item_id = Some(ev_item_id.clone()),
                    Some(existing) if *existing == *ev_item_id => {}
                    Some(_) => return Err(CoreError::MultipleItemsInFederatedProposal),
                }
            }
        }

        // Validate federation modifiers on events for this ingest.
        let mut mode_change_count = 0usize;
        let mut bootstrap_rebind_seen = false;
        for (idx, ev) in proposal.events.iter().enumerate() {
            // If present, create_item must be the first event.
            if ev.create_item.is_some() && idx != 0 {
                return Err(CoreError::InvalidProposal(
                    "create_item must be the first event in a federated proposal".to_string(),
                ));
            }
            if ev.create_item.is_some()
                && (ev.set_parent.is_some()
                    || ev.set_name.is_some()
                    || ev.set_tombstoned.is_some()
                    || ev.set_in_trash.is_some())
            {
                return Err(CoreError::InvalidProposal(
                    "create_item events must not also use structural modifiers".to_string(),
                ));
            }

            if ev.set_is_federated.is_some() || ev.set_federation_stream_id.is_some() {
                if ev.create_item.is_some() {
                    // Bootstrap federation identity may be materialized into the stored
                    // create_item payload. If the caller supplies it, it must match
                    // the proposal's federation_stream_id.
                    match (ev.set_is_federated, ev.set_federation_stream_id.as_deref()) {
                        (Some(true), Some(fid)) => {
                            if fid.is_empty() || fid != proposal.federation_stream_id.as_str() {
                                return Err(CoreError::IllegalFederationTransition);
                            }
                        }
                        (Some(true), None) => {
                            // Core will materialize set_federation_stream_id deterministically.
                        }
                        (None, None) => {}
                        _ => return Err(CoreError::IllegalFederationTransition),
                    }
                    continue;
                }

                match ev.set_is_federated {
                    Some(false) => {
                        // Unfederate is allowed via ingest, but must be structurally pure.
                        if ev.set_federation_stream_id.is_some()
                            || ev.set_parent.is_some()
                            || ev.set_name.is_some()
                            || ev.set_tombstoned.is_some()
                            || ev.set_in_trash.is_some()
                        {
                            return Err(CoreError::IllegalFederationTransition);
                        }
                        mode_change_count += 1;
                    }
                    Some(true) => {
                        // A source bootstrap replays its local bind event after the create
                        // event. The create already materializes the replica as federated,
                        // so accept only that redundant same-stream transition to preserve
                        // source item_seq alignment.
                        let bootstrap_create = proposal
                            .events
                            .first()
                            .is_some_and(|first| first.create_item.is_some());
                        let same_stream = ev
                            .set_federation_stream_id
                            .as_deref()
                            .is_some_and(|fid| fid == proposal.federation_stream_id);
                        let structurally_pure = ev.set_parent.is_none()
                            && ev.set_name.is_none()
                            && ev.set_tombstoned.is_none()
                            && ev.set_in_trash.is_none();
                        if !bootstrap_create
                            || !same_stream
                            || !structurally_pure
                            || bootstrap_rebind_seen
                        {
                            return Err(CoreError::IllegalFederationTransition);
                        }
                        bootstrap_rebind_seen = true;
                    }
                    None => {
                        // set_federation_stream_id without explicit mode toggle, or other malformed combo.
                        return Err(CoreError::IllegalFederationTransition);
                    }
                }
            }
        }

        if mode_change_count > 1 {
            return Err(CoreError::IllegalFederationTransition);
        }

        let perf_enabled = perf_timings_enabled();
        let total_start = Instant::now();
        let mut stream_lock_ms = 0u64;
        let mut item_lookup_ms = 0u64;
        let mut item_lock_ms = 0u64;
        let mut load_item_ms = 0u64;
        let mut build_rows_ms = 0u64;
        let mut insert_commit_ms = 0u64;
        let mut append_events_ms = 0u64;
        let mut update_item_ms = 0u64;
        let res: Result<(IngestedCommit, CommitRow, Vec<EventProposal>, bool), CoreError> = async {
            let repo_seq: Option<i64> = None;

            // Serialize ingest writes per federation stream id. This gives "single writer per
            // federated item" semantics without imposing a repo-wide writer lane.
            let stream_key = advisory_lock_key_for_federation_stream(
                &repo_id,
                proposal.federation_stream_id.as_str(),
            );
            let stream_lock_start = Instant::now();
            txn.lock_conflict_keys(&[stream_key]).await?;
            stream_lock_ms = stream_lock_start.elapsed().as_millis() as u64;

            // Resolve whether this is an existing federated item or a bootstrap create.
            let item_lookup_start = Instant::now();
            let existing_item = txn
                .get_item_by_federation_stream_id(&repo_id, &proposal.federation_stream_id)
                .await?;
            item_lookup_ms = item_lookup_start.elapsed().as_millis() as u64;

            // Lock the resolved local item_id plus any referenced parent folders (create/move
            // targets). This makes structural checks meaningful under concurrency and prevents
            // ingest from racing with local appends on the same item.
            let mut bootstrap_created = false;
            let commit_created_at: Timestamp;
            let mut item_row = if let Some(item_row) = existing_item {
                let item_id = item_row.item_id.clone();
                let mut lock_item_ids: BTreeSet<ItemId> = BTreeSet::new();
                lock_item_ids.insert(item_id.clone());
                if let Some(first) = proposal.events.first().and_then(|ev| ev.create_item.as_ref())
                    && let Some(parent) = first.parent_folder_id.as_ref() {
                        lock_item_ids.insert(parent.clone());
                    }
                for ev in &proposal.events {
                    if let Some(Some(parent)) = ev.set_parent.as_ref() {
                        lock_item_ids.insert(parent.clone());
                    }
                }

                let mut lock_keys: Vec<i64> = lock_item_ids
                    .iter()
                    .map(|id| advisory_lock_key_for_item(&repo_id, id))
                    .collect();
                lock_keys.sort_unstable();
                let item_lock_start = Instant::now();
                txn.lock_conflict_keys(&lock_keys).await?;
                item_lock_ms = item_lock_start.elapsed().as_millis() as u64;
                load_item_ms = 0;
                commit_created_at = txn.acceptance_timestamp().await?;
                item_row
            } else {
                bootstrap_created = true;
                if provided_item_id.is_some() {
                    return Err(CoreError::InvalidProposal(
                        "federated proposals must not supply repo-local item_id for item creation"
                            .to_string(),
                    ));
                }
                if mode_change_count != 0 {
                    return Err(CoreError::IllegalFederationTransition);
                }

                let first = proposal
                    .events
                    .first()
                    .expect("non-empty events validated above");
                let ci = first.create_item.as_ref().ok_or_else(|| {
                    CoreError::InvalidProposal(
                        "unknown federation_stream_id; first event must be create_item".to_string(),
                    )
                })?;

                let item_id = ItemId::new();
                let state = ItemState::from_create(
                    ci,
                    SequencingMode::Federated,
                    Some(proposal.federation_stream_id.clone()),
                )
                .map_err(|e| map_name_error(&repo_id, Some(&item_id), &ci.name, e))?;

                let mut lock_item_ids: BTreeSet<ItemId> = BTreeSet::new();
                lock_item_ids.insert(item_id.clone());
                if let Some(parent) = ci.parent_folder_id.as_ref() {
                    lock_item_ids.insert(parent.clone());
                }
                for ev in &proposal.events {
                    if let Some(Some(parent)) = ev.set_parent.as_ref() {
                        lock_item_ids.insert(parent.clone());
                    }
                }

                let mut lock_keys: Vec<i64> = lock_item_ids
                    .iter()
                    .map(|id| advisory_lock_key_for_item(&repo_id, id))
                    .collect();
                lock_keys.sort_unstable();
                let item_lock_start = Instant::now();
                txn.lock_conflict_keys(&lock_keys).await?;
                item_lock_ms = item_lock_start.elapsed().as_millis() as u64;
                commit_created_at = txn.acceptance_timestamp().await?;

                ItemRow {
                    repo_id: repo_id.clone(),
                    item_id,
                    item_type: state.item_type,
                    parent_folder_id: state.parent_folder_id,
                    name: state.name,
                    name_key: state.name_key,
                    head_seq: 0,
                    ref_head_seq: 0,
                    lifecycle_state: state.lifecycle_state,
                    created_at: commit_created_at,
                    created_by: proposal.author_id.clone(),
                    sequencing_mode: state.sequencing_mode,
                    federation_stream_id: state.federation_stream_id,
                }
            };

            let item_id = item_row.item_id.clone();
            if let Some(provided) = &provided_item_id
                && *provided != item_id {
                    return Err(CoreError::InvalidProposal(
                        "EventProposal.item_id does not match the local item resolved for federation_stream_id"
                            .to_string(),
                    ));
                }
            if !bootstrap_created && proposal.events.iter().any(|ev| ev.create_item.is_some()) {
                return Err(CoreError::InvalidProposal(
                    "create_item is only allowed when bootstrapping a new federated item"
                        .to_string(),
                ));
            }

            let ctx = InvariantContext {
                repo_id: &repo_id,
                op: OperationKind::IngestFederated,
            };
            if bootstrap_created {
                for ev in &mut proposal.events {
                    ev.base_seq = None;
                }
            } else {
                prepare_existing_federated_ingest_events(
                    &ctx,
                    &item_row,
                    proposal.federation_stream_id.as_str(),
                    &mut proposal.events,
                )?;
            }

            // Persist bootstrap federation identity inside the stored create_item event payload
            // so `vcs_items` is rebuildable from the event log.
            if bootstrap_created {
                let first = proposal
                    .events
                    .first_mut()
                    .expect("non-empty events validated above");
                if first.create_item.is_none() {
                    return Err(CoreError::InvalidProposal(
                        "unknown federation_stream_id; first event must be create_item".to_string(),
                    ));
                }

                first.set_is_federated = Some(true);
                first.set_federation_stream_id = Some(proposal.federation_stream_id.clone());
            }

            // Validate sequencing: provided item_seq must be head_seq + 1.
            let expected_seq = item_row.head_seq + 1;
            if proposal.item_seq != expected_seq {
                return Err(CoreError::ItemSeqMismatch {
                    repo_id: repo_id.clone(),
                    item_id,
                    expected: expected_seq,
                    found: proposal.item_seq,
                });
            }

            // Build a local CommitProposal using the federated events. We treat
            // the federated input as a proposal; the local repo owns commit_id,
            let mut local_proposal = CommitProposal {
                author_id: proposal.author_id.clone(),
                committer_id: proposal.committer_id.clone(),
                message: proposal.message.clone(),
                occurred_at: proposal.occurred_at,
                operation_id: None,
                events: proposal.events.clone(),
            };

            if bootstrap_created {
                for ev in &mut local_proposal.events {
                    ev.item_id = Some(item_id.clone());
                }
            }

            // Apply in-commit structural modifiers for this item, capturing parent before/after.
            let mut event_parent_after: Vec<Option<ItemId>> =
                Vec::with_capacity(local_proposal.events.len());
            let mut event_parent_before: Vec<Option<ItemId>> =
                Vec::with_capacity(local_proposal.events.len());

            let needs_parent_invariants = local_proposal
                .events
                .iter()
                .any(|ev| ev.create_item.is_some() || ev.set_parent.is_some());
            let needs_name_uniqueness = local_proposal.events.iter().any(|ev| {
                ev.set_parent.is_some()
                    || ev.set_name.is_some()
                    || ev.set_tombstoned.is_some()
                    || ev.set_in_trash.is_some()
            });
            let head_only_item_update = !bootstrap_created
                && local_proposal.events.iter().all(|ev| {
                    ev.create_item.is_none()
                        && ev.set_parent.is_none()
                        && ev.set_name.is_none()
                        && ev.set_tombstoned.is_none()
                        && ev.set_in_trash.is_none()
                        && ev.set_is_federated.is_none()
                        && ev.set_federation_stream_id.is_none()
                });

            for ev in &local_proposal.events {
                // Once an item is tombstoned within a proposal, no further events
                // targeting it are allowed.
                if matches!(item_row.lifecycle_state, LifecycleState::Tombstoned) {
                    return Err(CoreError::ItemTombstoned {
                        repo_id: repo_id.clone(),
                        item_id: item_id.clone(),
                    });
                }

                let mut state = ItemState::from(&item_row);
                let transition = apply_item_event(&mut state, ev).map_err(|error| {
                    map_name_error(
                        &repo_id,
                        Some(&item_id),
                        ev.set_name.as_deref().unwrap_or(&state.name),
                        error,
                    )
                })?;
                state.write_to(&mut item_row);
                event_parent_before.push(transition.parent_folder_id_before);
                event_parent_after.push(transition.parent_folder_id_after);
            }

            let mut items_map = HashMap::new();
            items_map.insert(item_id.clone(), item_row.clone());

            if needs_parent_invariants {
                check_parent_existence_and_type(&ctx, txn, &items_map).await?;
                check_folder_acyclicity(&ctx, txn, &items_map).await?;
            }

            if needs_name_uniqueness {
                let mut seen: BTreeSet<(Option<ItemId>, String)> = BTreeSet::new();
                for item in items_map.values() {
                    if !matches!(item.lifecycle_state, LifecycleState::Live) {
                        continue;
                    }
                    let key = (item.parent_folder_id.clone(), item.name_key.clone());
                    if !seen.insert(key.clone()) {
                        return Err(CoreError::NameConflict {
                            repo_id: repo_id.clone(),
                            parent_folder_id: key.0,
                            name: item.name.clone(),
                        });
                    }
                }
            }

            // Assign item_seq for each event starting from proposal.item_seq.
            let build_rows_start = Instant::now();
            let commit_pk = CommitPk::new();

            let mut events_digest_hasher = Sha256::new();
            let payloads = self.validate_payload_limits(&local_proposal.events, &[])?;
            let mut seq = proposal.item_seq;
            let mut event_rows: Vec<EventInsertRow> =
                Vec::with_capacity(local_proposal.events.len());
            for ((idx, ev), payload) in local_proposal.events.iter().enumerate().zip(payloads) {
                let ev_item_id = ev.item_id.clone().expect("validated above");
                validate_event_type_matches_item(
                    &ev_item_id,
                    item_row.item_type.as_str(),
                    ev.event_type.as_str(),
                )?;
                let parent_after = event_parent_after[idx].clone();
                let parent_before = event_parent_before[idx].clone();

                let event_hash = compute_sha256_digest_bytes(&payload);
                events_digest_hasher.update([COMMIT_EVENT_FAMILY_ITEM]);
                events_digest_hasher.update(event_hash);

                let event_row = EventInsertRow {
                    repo_id: repo_id.clone(),
                    commit_pk: commit_pk.clone(),
                    commit_event_idx: idx as i32,
                    item_id: ev_item_id,
                    item_seq: seq,
                    event_type: ev.event_type.clone(),
                    metadata: ev.set_metadata.as_ref().and_then(|value| value.payload()).cloned(),
                    payload,
                    event_payload_digest: event_hash,
                    created_at: commit_created_at,
                    parent_folder_id: parent_after,
                    parent_folder_id_before: parent_before,
                };

                event_rows.push(event_row);
                seq += 1;
            }

            let events_digest_bytes: [u8; 32] = events_digest_hasher.finalize().into();
            let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
                events_digest_bytes,
            ))
            .expect("internal sha256 digest construction must be valid");

            let canonical_commit_bytes = canonical_commit_bytes_for_hash(&local_proposal.author_id, &local_proposal.committer_id, local_proposal.occurred_at, local_proposal.message.as_deref(), None, local_proposal.events.len(), &events_digest)?;
            let commit_id = compute_commit_id(&canonical_commit_bytes);
            build_rows_ms = build_rows_start.elapsed().as_millis() as u64;

            let insert_commit_start = Instant::now();
            if let Err(err) = txn
                .insert_commit_sealed(
                    &repo_id,
                    &commit_pk,
                    repo_seq,
                    None,
                    COMMIT_FORMAT_VERSION,
                    None,
                    &local_proposal.author_id,
                    &local_proposal.committer_id,
                    local_proposal.message.as_deref(),
                    local_proposal.occurred_at,
                    commit_created_at,
                    &commit_id,
                    &events_digest,
                    local_proposal.events.len() as i32,
                )
                .await
            {
                match err {
                    StorageError::UniqueViolation { .. } => {
                        return Err(map_insert_commit_unique_violation(
                            txn, &repo_id, None, &commit_id,
                        )
                        .await?);
                    }
                    other => return Err(CoreError::StorageError(other)),
                }
            }
            insert_commit_ms = insert_commit_start.elapsed().as_millis() as u64;
            let append_events_start = Instant::now();
            txn.append_events(&event_rows).await?;
            append_events_ms = append_events_start.elapsed().as_millis() as u64;

            let commit_row = CommitRow {
                repo_id: repo_id.clone(),
                commit_id: commit_id.clone(),
                repo_seq,
                format_version: COMMIT_FORMAT_VERSION,
                import_job_id: None,
                author_id: proposal.author_id.clone(),
                committer_id: proposal.committer_id.clone(),
                message: proposal.message.clone(),
                occurred_at: proposal.occurred_at,
                created_at: commit_created_at,
                event_count: proposal.events.len() as i32,
                events_digest: events_digest.clone(),
            };

            let update_item_start = Instant::now();
            if head_only_item_update {
                txn.update_item_heads(&repo_id, &[(item_id.clone(), seq - 1)])
                    .await?;
            } else {
                item_row.head_seq = seq - 1;

                let parent_folder_id = item_row.parent_folder_id.clone();
                let name = item_row.name.clone();
                if let Err(err) = txn.put_item(&item_row).await {
                    match err {
                        StorageError::UniqueViolation { detail, constraint } => {
                            return Err(map_item_unique_violation(
                                txn,
                                &repo_id,
                                &item_row,
                                parent_folder_id,
                                name,
                                detail,
                                constraint,
                            )
                            .await?);
                        }
                        other => return Err(CoreError::StorageError(other)),
                    }
                }
            }
            update_item_ms = update_item_start.elapsed().as_millis() as u64;

            let ingested = IngestedCommit {
                commit_id,
                repo_seq,
                created_at: commit_row.created_at,
                occurred_at: commit_row.occurred_at,
                item_seq: proposal.item_seq,
                federation_stream_id: proposal.federation_stream_id.clone(),
            };

            Ok((ingested, commit_row, local_proposal.events, head_only_item_update))
        }
        .await;

        match res {
            Ok((ingested, commit_row, events, head_only_item_update)) => {
                if perf_enabled {
                    info!(
                        repo_id = %commit_row.repo_id,
                        federation_stream_id = %ingested.federation_stream_id,
                        item_seq = ingested.item_seq,
                        event_count = commit_row.event_count,
                        head_only_item_update,
                        stream_lock_ms,
                        item_lookup_ms,
                        item_lock_ms,
                        load_item_ms,
                        build_rows_ms,
                        insert_commit_ms,
                        append_events_ms,
                        update_item_ms,
                        total_ms = total_start.elapsed().as_millis() as u64,
                        "perf: federation core ingest transaction"
                    );
                }

                info!(
                    repo_id = %commit_row.repo_id,
                    commit_id = %commit_row.commit_id,
                    author_id = %commit_row.author_id,
                    repo_seq = commit_row.repo_seq,
                    event_count = commit_row.event_count,
                    item_seq = ingested.item_seq,
                    federation_stream_id = %ingested.federation_stream_id,
                    message = commit_row.message.as_deref().unwrap_or(""),
                    "federated proposal prepared in transaction"
                );

                debug!(
                    repo_id = %commit_row.repo_id,
                    commit_id = %commit_row.commit_id,
                    author_id = %commit_row.author_id,
                    repo_seq = commit_row.repo_seq,
                    event_count = commit_row.event_count,
                    item_seq = ingested.item_seq,
                    federation_stream_id = %ingested.federation_stream_id,
                    message = ?commit_row.message,
                    events = ?events,
                    "federated proposal ingest details"
                );

                trace!(
                    repo_id = %commit_row.repo_id,
                    commit_id = %commit_row.commit_id,
                    "federated proposal ingest trace"
                );

                Ok(ingested)
            }
            Err(err) => Err(err),
        }
    }

    pub async fn ingest_content_only_existing_item(
        &self,
        repo_id: RepoId,
        proposal: FederatedProposal,
    ) -> Result<IngestedCommit, CoreError> {
        let mut ingested = self
            .ingest_content_only_existing_item_batch(repo_id, vec![proposal])
            .await?;
        ingested.pop().ok_or_else(|| {
            CoreError::InvalidProposal(
                "content-only existing-item ingest must return exactly one commit".to_string(),
            )
        })
    }

    pub async fn ingest_content_only_existing_item_batch(
        &self,
        repo_id: RepoId,
        mut proposals: Vec<FederatedProposal>,
    ) -> Result<Vec<IngestedCommit>, CoreError> {
        if proposals.is_empty() {
            return Ok(Vec::new());
        }

        let federation_stream_id = proposals[0].federation_stream_id.clone();
        if federation_stream_id.trim().is_empty() {
            return Err(CoreError::InvalidProposal(
                "federation_stream_id is required".to_string(),
            ));
        }
        for proposal in &proposals {
            if proposal.federation_stream_id != federation_stream_id {
                return Err(CoreError::InvalidProposal(
                    "content-only federated batch requires one federation_stream_id".to_string(),
                ));
            }
            validate_commit_metadata(
                &proposal.author_id,
                &proposal.committer_id,
                proposal.message.as_deref(),
                None,
            )?;
        }
        let mut provided_batch_item_id: Option<ItemId> = None;
        for proposal in &proposals {
            for ev in &proposal.events {
                let Some(ev_item_id) = ev.item_id.as_ref() else {
                    continue;
                };
                match &provided_batch_item_id {
                    None => provided_batch_item_id = Some(ev_item_id.clone()),
                    Some(existing) if existing == ev_item_id => {}
                    Some(_) => {
                        return Err(CoreError::MultipleItemsInFederatedProposal);
                    }
                }
            }
        }

        let perf_enabled = perf_timings_enabled();
        let total_start = Instant::now();
        let begin_txn_start = Instant::now();
        let mut txn = self.storage.backend.begin_txn().await?;
        let begin_txn_ms = begin_txn_start.elapsed().as_millis() as u64;
        let mut stream_lock_ms = 0u64;
        let mut item_lookup_ms = 0u64;
        let mut item_lock_ms = 0u64;
        let mut build_rows_ms = 0u64;
        let mut insert_commit_ms = 0u64;
        let mut append_events_ms = 0u64;
        let mut update_item_ms = 0u64;
        let mut batch_event_count = 0usize;
        let total_event_capacity = proposals.iter().map(|proposal| proposal.events.len()).sum();
        let mut batch_event_rows: Vec<EventInsertRow> = Vec::with_capacity(total_event_capacity);
        let mut ingested_commits = Vec::with_capacity(proposals.len());
        let mut sealed_commits: Vec<SealedCommitInsert> = Vec::with_capacity(proposals.len());

        let res: Result<(), CoreError> = async {
            let txn = &mut *txn;

            let stream_key =
                advisory_lock_key_for_federation_stream(&repo_id, federation_stream_id.as_str());
            let stream_lock_start = Instant::now();
            txn.lock_conflict_keys(&[stream_key]).await?;
            stream_lock_ms = stream_lock_start.elapsed().as_millis() as u64;

            let item_lookup_start = Instant::now();
            let mut item_row = if let Some(item_id) = provided_batch_item_id.as_ref() {
                txn.get_item(&repo_id, item_id).await?.ok_or_else(|| {
                    CoreError::InvalidProposal(
                        "content-only federated batch requires an existing item".to_string(),
                    )
                })?
            } else {
                txn.get_item_by_federation_stream_id(&repo_id, &federation_stream_id)
                    .await?
                    .ok_or_else(|| {
                        CoreError::InvalidProposal(
                            "content-only federated batch requires an existing item".to_string(),
                        )
                    })?
            };
            item_lookup_ms = item_lookup_start.elapsed().as_millis() as u64;

            let item_lock_start = Instant::now();
            txn.lock_conflict_keys(&[advisory_lock_key_for_item(&repo_id, &item_row.item_id)])
                .await?;
            item_lock_ms = item_lock_start.elapsed().as_millis() as u64;

            let ctx = InvariantContext {
                repo_id: &repo_id,
                op: OperationKind::IngestFederated,
            };
            for proposal in &mut proposals {
                if proposal.events.is_empty() {
                    return Err(CoreError::EmptyCommit);
                }
                prepare_existing_federated_ingest_events(
                    &ctx,
                    &item_row,
                    federation_stream_id.as_str(),
                    &mut proposal.events,
                )?;
                if !federated_proposal_is_content_only_existing_item(proposal) {
                    return Err(CoreError::InvalidProposal(
                        "content-only federated batch only accepts existing-item content updates"
                            .to_string(),
                    ));
                }

                let expected_seq = item_row.head_seq + 1;
                if proposal.item_seq != expected_seq {
                    return Err(CoreError::ItemSeqMismatch {
                        repo_id: repo_id.clone(),
                        item_id: item_row.item_id.clone(),
                        expected: expected_seq,
                        found: proposal.item_seq,
                    });
                }

                let build_rows_start = Instant::now();
                let proposal_created_at = txn.acceptance_timestamp().await?;
                let repo_seq: Option<i64> = None;
                let item_id = item_row.item_id.clone();

                let commit_pk = CommitPk::new();
                let mut events_digest_hasher = Sha256::new();
                let payloads = self.validate_payload_limits(&proposal.events, &[])?;
                let mut seq = proposal.item_seq;
                let mut event_rows: Vec<EventInsertRow> = Vec::with_capacity(proposal.events.len());
                for ((idx, ev), payload) in proposal.events.iter().enumerate().zip(payloads) {
                    validate_event_type_matches_item(
                        &item_id,
                        item_row.item_type.as_str(),
                        ev.event_type.as_str(),
                    )?;
                    let event_hash = compute_sha256_digest_bytes(&payload);
                    events_digest_hasher.update([COMMIT_EVENT_FAMILY_ITEM]);
                    events_digest_hasher.update(event_hash);
                    event_rows.push(EventInsertRow {
                        repo_id: repo_id.clone(),
                        commit_pk: commit_pk.clone(),
                        commit_event_idx: idx as i32,
                        item_id: item_id.clone(),
                        item_seq: seq,
                        event_type: ev.event_type.clone(),
                        metadata: ev.set_metadata.as_ref().and_then(|value| value.payload()).cloned(),
                        payload,
                        event_payload_digest: event_hash,
                        created_at: proposal_created_at,
                        parent_folder_id: item_row.parent_folder_id.clone(),
                        parent_folder_id_before: None,
                    });
                    seq += 1;
                }

                let events_digest_bytes: [u8; 32] = events_digest_hasher.finalize().into();
                let events_digest =
                    Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(events_digest_bytes))
                        .expect("internal sha256 digest construction must be valid");
                let canonical_commit_bytes = canonical_commit_bytes_for_hash(
                    &proposal.author_id,
                    &proposal.committer_id,
                    proposal.occurred_at,
                    proposal.message.as_deref(),
                    None,
                    proposal.events.len(),
                    &events_digest,
                )?;
                let commit_id = compute_commit_id(&canonical_commit_bytes);
                build_rows_ms += build_rows_start.elapsed().as_millis() as u64;

                sealed_commits.push(SealedCommitInsert {
                    commit_pk: commit_pk.clone(),
                    repo_seq,
                    operation_id: None,
                    format_version: COMMIT_FORMAT_VERSION,
                    import_job_id: None,
                    author_id: proposal.author_id.clone(),
                    committer_id: proposal.committer_id.clone(),
                    message: proposal.message.clone(),
                    occurred_at: proposal.occurred_at,
                    created_at: proposal_created_at,
                    commit_id: commit_id.clone(),
                    events_digest: events_digest.clone(),
                    event_count: proposal.events.len() as i32,
                });

                item_row.head_seq = seq - 1;
                batch_event_count += proposal.events.len();
                batch_event_rows.extend(event_rows);
                ingested_commits.push(IngestedCommit {
                    commit_id,
                    repo_seq,
                    created_at: proposal_created_at,
                    occurred_at: proposal.occurred_at,
                    item_seq: proposal.item_seq,
                    federation_stream_id: proposal.federation_stream_id.clone(),
                });
            }

            let insert_commit_start = Instant::now();
            if let Err(err) = txn.insert_commits_sealed(&repo_id, &sealed_commits).await {
                match err {
                    StorageError::UniqueViolation { .. } => {
                        for commit in &sealed_commits {
                            if let Err(err) = txn
                                .insert_commit_sealed(
                                    &repo_id,
                                    &commit.commit_pk,
                                    commit.repo_seq,
                                    commit.operation_id.as_deref(),
                                    commit.format_version,
                                    commit.import_job_id.as_deref(),
                                    &commit.author_id,
                                    &commit.committer_id,
                                    commit.message.as_deref(),
                                    commit.occurred_at,
                                    commit.created_at,
                                    &commit.commit_id,
                                    &commit.events_digest,
                                    commit.event_count,
                                )
                                .await
                            {
                                match err {
                                    StorageError::UniqueViolation { .. } => {
                                        return Err(map_insert_commit_unique_violation(
                                            txn,
                                            &repo_id,
                                            None,
                                            &commit.commit_id,
                                        )
                                        .await?);
                                    }
                                    other => return Err(CoreError::StorageError(other)),
                                }
                            }
                        }
                    }
                    other => return Err(CoreError::StorageError(other)),
                }
            }
            insert_commit_ms = insert_commit_start.elapsed().as_millis() as u64;

            let append_events_start = Instant::now();
            txn.append_events(&batch_event_rows).await?;
            append_events_ms = append_events_start.elapsed().as_millis() as u64;

            let update_item_start = Instant::now();
            txn.update_item_heads(&repo_id, &[(item_row.item_id.clone(), item_row.head_seq)])
                .await?;
            update_item_ms = update_item_start.elapsed().as_millis() as u64;
            Ok(())
        }
        .await;

        match res {
            Ok(()) => {
                let commit_start = Instant::now();
                txn.commit().await?;
                let commit_ms = commit_start.elapsed().as_millis() as u64;

                if perf_enabled {
                    info!(
                        repo_id = %repo_id,
                        federation_stream_id = %federation_stream_id,
                        proposal_count = ingested_commits.len(),
                        event_count = batch_event_count,
                        begin_txn_ms,
                        stream_lock_ms,
                        item_lookup_ms,
                        item_lock_ms,
                        build_rows_ms,
                        insert_commit_ms,
                        append_events_ms,
                        update_item_ms,
                        commit_ms,
                        total_ms = total_start.elapsed().as_millis() as u64,
                        "perf: federation core ingest content-only batch"
                    );
                }
                Ok(ingested_commits)
            }
            Err(err) => {
                if let Err(rb) = txn.rollback().await {
                    warn!(repo_id = %repo_id, error = %rb, "content-only ingest rollback failed");
                }
                Err(err)
            }
        }
    }

    /// Ingest a federated domain proposal (multi-item) into the given repo.
    ///
    /// This is used for federated folder replicas: proposals are applied as
    /// ordinary commits, but ingestion is serialized and deduplicated by a
    /// monotonic `domain_seq` cursor per `(repo_id, domain_id)` (commit/proposal sequence).
    #[instrument(
        skip(self, proposal),
        fields(
            repo_id = %repo_id,
            domain_id = %proposal.domain_id,
            domain_seq = proposal.domain_seq,
            events = %proposal.events.len(),
            ref_events = %proposal.ref_events.len()
        )
    )]
    pub async fn ingest_domain(
        &self,
        repo_id: RepoId,
        proposal: FederatedDomainProposal,
    ) -> Result<IngestedDomainCommit, CoreError> {
        let mut txn = self.storage.backend.begin_txn().await?;
        match self
            .ingest_domain_in_transaction(&mut *txn, repo_id.clone(), proposal)
            .await
        {
            Ok(out) => {
                txn.commit().await?;
                Ok(out)
            }
            Err(err) => {
                if let Err(rb) = txn.rollback().await {
                    warn!(repo_id = %repo_id, error = %rb, "ingest_domain rollback failed");
                }
                Err(err)
            }
        }
    }

    /// Ingest inside the caller's transaction, including the domain cursor.
    ///
    /// This lets a task migration bind historical identities atomically with
    /// accepted history. The caller owns protocol/receipt verification and must
    /// acquire its platform authority locks before entering Core. The result is
    /// provisional until commit; on any error the whole transaction must roll
    /// back. Head replay retains ordinary ingest semantics: it does not verify
    /// payload equivalence, which remains the caller's responsibility.
    pub async fn ingest_domain_in_transaction(
        &self,
        txn: &mut dyn StorageTxn,
        repo_id: RepoId,
        proposal: FederatedDomainProposal,
    ) -> Result<IngestedDomainCommit, CoreError> {
        if proposal.domain_id.trim().is_empty() {
            return Err(CoreError::InvalidProposal(
                "domain_id is required".to_string(),
            ));
        }
        let operation_id = federated_domain_operation_id(&proposal.domain_id, proposal.domain_seq);
        validate_commit_metadata(
            &proposal.author_id,
            &proposal.committer_id,
            proposal.message.as_deref(),
            Some(&operation_id),
        )?;
        if proposal.domain_seq < 1 {
            return Err(CoreError::InvalidProposal(
                "domain_seq must be >= 1".to_string(),
            ));
        }
        if proposal.events.is_empty() && proposal.ref_events.is_empty() {
            return Err(CoreError::EmptyCommit);
        }
        let combined_event_count = proposal
            .events
            .len()
            .checked_add(proposal.ref_events.len())
            .ok_or_else(|| CoreError::InvalidProposal("commit event count overflow".to_string()))?;

        // Reject per-item federation transitions in domain ingests. A federated folder
        // replica is itself the boundary; individual items do not independently
        // participate in item-federation.
        for ev in &proposal.events {
            if ev.set_is_federated.is_some() || ev.set_federation_stream_id.is_some() {
                return Err(CoreError::IllegalFederationTransition);
            }
        }
        for event in &proposal.ref_events {
            if !event.event_type.starts_with("FILE_VERSION::") {
                return Err(CoreError::InvalidProposal(format!(
                    "unsupported federated ref event type `{}`",
                    event.event_type
                )));
            }
        }

        // Pre-resolve target item ids for all events, requiring explicit ids.
        let mut event_item_ids: Vec<ItemId> = Vec::with_capacity(proposal.events.len());
        let mut created_items: Vec<CreatedItemInfo> = Vec::new();
        let mut create_item_ids: HashSet<ItemId> = HashSet::new();
        for (idx, ev) in proposal.events.iter().enumerate() {
            if ev.create_item.is_some() {
                let Some(item_id) = &ev.item_id else {
                    return Err(CoreError::InvalidProposal(
                        "create_item requires explicit item_id for federated domains".to_string(),
                    ));
                };

                if !create_item_ids.insert(item_id.clone()) {
                    return Err(CoreError::InvalidProposal(
                        "duplicate create_item for item_id within a single commit".to_string(),
                    ));
                }
                event_item_ids.push(item_id.clone());
                created_items.push(CreatedItemInfo {
                    event_idx: idx,
                    item_id: item_id.clone(),
                });
            } else {
                let item_id = ev.item_id.clone().ok_or_else(|| {
                    CoreError::InvalidProposal("event missing item_id".to_string())
                })?;
                event_item_ids.push(item_id);
            }
        }

        // Enforce base_seq shape rules from the spec:
        // - For new items created in this commit: base_seq MUST be null for all events.
        // - For existing items: base_seq MUST be present (integer) for all events.
        for (idx, item_id) in event_item_ids.iter().enumerate() {
            let ev = &proposal.events[idx];
            if create_item_ids.contains(item_id) {
                if ev.base_seq.is_some() {
                    return Err(CoreError::InvalidProposal(
                        "base_seq must be null for events targeting a newly created item"
                            .to_string(),
                    ));
                }
            } else if ev.base_seq.is_none() {
                return Err(CoreError::InvalidProposal(
                    "base_seq must be present for events targeting an existing item".to_string(),
                ));
            }
        }

        // Ensure per-item base_seq consistency within the commit.
        let mut observed_base_seq: HashMap<ItemId, Option<i64>> = HashMap::new();
        for (idx, item_id) in event_item_ids.iter().enumerate() {
            let ev = &proposal.events[idx];
            let current = observed_base_seq
                .entry(item_id.clone())
                .or_insert(ev.base_seq);
            if *current != ev.base_seq {
                return Err(CoreError::InvalidProposal(
                    "inconsistent base_seq for item within a single commit".to_string(),
                ));
            }
        }

        let domain_seq_end = proposal.domain_seq;

        async {
            // Acquire deterministic item-level locks for all conflict keys touched by this domain
            // commit. This preserves per-item linearity and prevents concurrent structural writes
            // from racing into folder cycles or other invariants.
            let mut lock_item_ids: BTreeSet<ItemId> = event_item_ids.iter().cloned().collect();
            lock_item_ids.extend(
                proposal
                    .ref_events
                    .iter()
                    .map(|event| event.owner_item_id.clone()),
            );
            for ev in &proposal.events {
                if let Some(ci) = ev.create_item.as_ref()
                    && let Some(parent) = ci.parent_folder_id.as_ref()
                {
                    lock_item_ids.insert(parent.clone());
                }
                if let Some(Some(parent)) = ev.set_parent.as_ref() {
                    lock_item_ids.insert(parent.clone());
                }
            }
            let mut lock_keys: Vec<i64> = lock_item_ids
                .iter()
                .map(|item_id| advisory_lock_key_for_item(&repo_id, item_id))
                .collect();
            lock_keys.sort_unstable();
            txn.lock_conflict_keys(&lock_keys).await?;

            let last_domain_seq = txn
                .federation_domain_last_seq_for_update(&repo_id, proposal.domain_id.as_str())
                .await?;

            if proposal.domain_seq <= last_domain_seq {
                // Strict dedupe: only the current head can be replayed. Anything older is
                // an out-of-order stream fragment and should not be silently accepted.
                if proposal.domain_seq == last_domain_seq {
                    return Ok(IngestedDomainCommit {
                        applied: false,
                        commit_id: None,
                        repo_seq: None,
                        created_at: None,
                        occurred_at: None,
                        domain_id: proposal.domain_id.clone(),
                        domain_seq: proposal.domain_seq,
                        domain_seq_end,
                    });
                }
                return Err(CoreError::DomainSeqOverlap {
                    repo_id: repo_id.clone(),
                    domain_id: proposal.domain_id.clone(),
                    last: last_domain_seq,
                    proposed_end: domain_seq_end,
                });
            }

            let expected_domain_seq = last_domain_seq + 1;
            if proposal.domain_seq != expected_domain_seq {
                return Err(CoreError::DomainSeqMismatch {
                    repo_id: repo_id.clone(),
                    domain_id: proposal.domain_id.clone(),
                    expected: expected_domain_seq,
                    found: proposal.domain_seq,
                });
            }

            let repo_seq: Option<i64> = None;
            let commit_created_at = txn.acceptance_timestamp().await?;

            // Before canonicalization, validate blob ids referenced by content.
            let mut local_proposal = CommitProposal {
                author_id: proposal.author_id.clone(),
                committer_id: proposal.committer_id.clone(),
                message: proposal.message.clone(),
                occurred_at: proposal.occurred_at,
                operation_id: Some(operation_id),
                events: proposal.events.clone(),
            };
            // Materialize item ids into the stored event payloads.
            for (idx, ev) in local_proposal.events.iter_mut().enumerate() {
                ev.item_id = Some(event_item_ids[idx].clone());
            }

            let mut touched_items: BTreeSet<ItemId> = event_item_ids.iter().cloned().collect();
            touched_items.extend(
                proposal
                    .ref_events
                    .iter()
                    .map(|event| event.owner_item_id.clone()),
            );
            let touched_item_ids: Vec<ItemId> = touched_items.iter().cloned().collect();
            let mut existing_rows_by_id: HashMap<ItemId, ItemRow> = txn
                .get_items(&repo_id, &touched_item_ids)
                .await?
                .into_iter()
                .map(|row| (row.item_id.clone(), row))
                .collect();

            // Load current item state for all touched items and apply basic invariants.
            let mut items: HashMap<ItemId, ItemRow> = HashMap::new();
            let tombstone = TombstoneFinality;
            let mode_rule = SequencingModeRule;
            let ctx = InvariantContext {
                repo_id: &repo_id,
                op: OperationKind::Append,
            };

            for item_id in &touched_items {
                let is_created = create_item_ids.contains(item_id);
                if is_created {
                    if existing_rows_by_id.contains_key(item_id) {
                        return Err(CoreError::ItemAlreadyExists {
                            repo_id: repo_id.clone(),
                            item_id: item_id.clone(),
                        });
                    }

                    let (idx, ev) = local_proposal
                        .events
                        .iter()
                        .enumerate()
                        .find(|(i, ev)| ev.create_item.is_some() && event_item_ids[*i] == *item_id)
                        .expect("created item must have a corresponding event");
                    let ci = ev.create_item.as_ref().expect("checked above");
                    let state = ItemState::from_create(ci, SequencingMode::Local, None)
                        .map_err(|e| map_name_error(&repo_id, Some(item_id), &ci.name, e))?;
                    let row = ItemRow {
                        repo_id: repo_id.clone(),
                        item_id: item_id.clone(),
                        item_type: state.item_type,
                        parent_folder_id: state.parent_folder_id,
                        name: state.name,
                        name_key: state.name_key,
                        head_seq: 0,
                        ref_head_seq: 0,
                        lifecycle_state: state.lifecycle_state,
                        created_at: commit_created_at,
                        created_by: local_proposal.author_id.clone(),
                        sequencing_mode: state.sequencing_mode,
                        federation_stream_id: state.federation_stream_id,
                    };
                    tombstone.check(&ctx, item_id, &row, &local_proposal.events[idx..=idx])?;
                    mode_rule.check(&ctx, item_id, &row, &local_proposal.events[idx..=idx])?;
                    items.insert(item_id.clone(), row);
                } else {
                    let row = existing_rows_by_id.remove(item_id).ok_or_else(|| {
                        CoreError::ItemNotFound {
                            repo_id: repo_id.clone(),
                            item_id: item_id.clone(),
                        }
                    })?;
                    tombstone.check(&ctx, item_id, &row, &[])?;
                    mode_rule.check(&ctx, item_id, &row, &[])?;
                    items.insert(item_id.clone(), row);
                }
            }

            for (item_id, base_seq) in &observed_base_seq {
                if create_item_ids.contains(item_id) {
                    continue;
                }
                let Some(row) = items.get(item_id) else {
                    return Err(CoreError::ItemNotFound {
                        repo_id: repo_id.clone(),
                        item_id: item_id.clone(),
                    });
                };
                let found = base_seq.expect("existing-item base_seq presence was validated");
                if found != row.head_seq {
                    return Err(CoreError::BaseSeqMismatch {
                        repo_id: repo_id.clone(),
                        item_id: item_id.clone(),
                        expected: row.head_seq,
                        found,
                    });
                }
            }

            let mut ref_bases = HashMap::<ItemId, i64>::new();
            for event in &proposal.ref_events {
                let owner =
                    items
                        .get(&event.owner_item_id)
                        .ok_or_else(|| CoreError::ItemNotFound {
                            repo_id: repo_id.clone(),
                            item_id: event.owner_item_id.clone(),
                        })?;
                if owner.item_type != "FILE" {
                    return Err(CoreError::InvalidProposal(format!(
                        "ref event owner {} is not a FILE",
                        event.owner_item_id
                    )));
                }
                if owner.lifecycle_state == LifecycleState::Tombstoned {
                    return Err(CoreError::ItemTombstoned {
                        repo_id: repo_id.clone(),
                        item_id: event.owner_item_id.clone(),
                    });
                }
                let expected = ref_bases
                    .entry(event.owner_item_id.clone())
                    .or_insert(owner.ref_head_seq);
                if event.base_ref_seq != *expected {
                    return Err(CoreError::RefBaseSeqMismatch {
                        repo_id: repo_id.clone(),
                        owner_item_id: event.owner_item_id.clone(),
                        expected: *expected,
                        found: event.base_ref_seq,
                    });
                }
            }

            // Snapshot pre-modifier item state so we can safely handle rename/move swaps
            // when persisting final rows (Postgres unique indexes are immediate).
            let items_before_modifiers = items.clone();

            let mut event_parent_after: Vec<Option<ItemId>> =
                Vec::with_capacity(local_proposal.events.len());
            let mut event_parent_before: Vec<Option<ItemId>> =
                Vec::with_capacity(local_proposal.events.len());

            let needs_parent_invariants = local_proposal
                .events
                .iter()
                .any(|ev| ev.create_item.is_some() || ev.set_parent.is_some());
            let needs_name_uniqueness = local_proposal.events.iter().any(|ev| {
                ev.create_item.is_some()
                    || ev.set_parent.is_some()
                    || ev.set_name.is_some()
                    || ev.set_tombstoned.is_some()
                    || ev.set_in_trash.is_some()
            });

            for (idx, ev) in local_proposal.events.iter().enumerate() {
                let item_id = &event_item_ids[idx];
                let row = items.get_mut(item_id).expect("item state should exist");

                if matches!(row.lifecycle_state, LifecycleState::Tombstoned) {
                    return Err(CoreError::ItemTombstoned {
                        repo_id: repo_id.clone(),
                        item_id: item_id.clone(),
                    });
                }

                let mut state = ItemState::from(&*row);
                let transition = apply_item_event(&mut state, ev).map_err(|error| {
                    map_name_error(
                        &repo_id,
                        Some(item_id),
                        ev.set_name.as_deref().unwrap_or(&state.name),
                        error,
                    )
                })?;
                state.write_to(row);
                event_parent_before.push(transition.parent_folder_id_before);
                event_parent_after.push(transition.parent_folder_id_after);
            }

            if needs_parent_invariants {
                check_parent_existence_and_type(&ctx, txn, &items).await?;
                check_folder_acyclicity(&ctx, txn, &items).await?;
            }

            if needs_name_uniqueness {
                let mut seen: BTreeSet<(Option<ItemId>, String)> = BTreeSet::new();
                for item in items.values() {
                    if !matches!(item.lifecycle_state, LifecycleState::Live) {
                        continue;
                    }
                    let key = (item.parent_folder_id.clone(), item.name_key.clone());
                    if !seen.insert(key.clone()) {
                        return Err(CoreError::NameConflict {
                            repo_id: repo_id.clone(),
                            parent_folder_id: key.0,
                            name: item.name.clone(),
                        });
                    }
                }
            }

            let item_seqs = Sequencer::assign_local_item_seqs(&mut items, &event_item_ids);
            let mut next_ref_seqs = ref_bases.clone();
            let mut ref_seqs = Vec::with_capacity(proposal.ref_events.len());
            for event in &proposal.ref_events {
                let head = next_ref_seqs
                    .get_mut(&event.owner_item_id)
                    .expect("ref owner base was validated");
                *head = head.checked_add(1).ok_or_else(|| {
                    CoreError::InvalidProposal("ref sequence overflow".to_string())
                })?;
                ref_seqs.push(*head);
                items
                    .get_mut(&event.owner_item_id)
                    .expect("ref owner was loaded")
                    .ref_head_seq = *head;
            }

            let commit_pk = CommitPk::new();

            let mut events_digest_hasher = Sha256::new();
            let mut payloads = self
                .validate_payload_limits(&local_proposal.events, &proposal.ref_events)?
                .into_iter();
            let mut event_rows: Vec<EventInsertRow> =
                Vec::with_capacity(local_proposal.events.len());
            for ((idx, ev), payload) in local_proposal
                .events
                .iter()
                .enumerate()
                .zip(payloads.by_ref())
            {
                let item_id = event_item_ids[idx].clone();
                let item_seq = item_seqs[idx];
                let item_row = items
                    .get(&item_id)
                    .expect("event item id must exist in items map");
                validate_event_type_matches_item(
                    &item_id,
                    item_row.item_type.as_str(),
                    ev.event_type.as_str(),
                )?;
                let parent_after = event_parent_after[idx].clone();
                let parent_before = event_parent_before[idx].clone();

                let event_hash = compute_sha256_digest_bytes(&payload);
                events_digest_hasher.update([COMMIT_EVENT_FAMILY_ITEM]);
                events_digest_hasher.update(event_hash);

                let event_row = EventInsertRow {
                    repo_id: repo_id.clone(),
                    commit_pk: commit_pk.clone(),
                    commit_event_idx: idx as i32,
                    item_id,
                    item_seq,
                    event_type: ev.event_type.clone(),
                    metadata: ev.set_metadata.as_ref().and_then(|value| value.payload()).cloned(),
                    payload,
                    event_payload_digest: event_hash,
                    created_at: commit_created_at,
                    parent_folder_id: parent_after,
                    parent_folder_id_before: parent_before,
                };

                event_rows.push(event_row);
            }

            let mut ref_rows = Vec::with_capacity(proposal.ref_events.len());
            for ((offset, (event, ref_seq)), payload) in proposal
                .ref_events
                .iter()
                .zip(ref_seqs.iter().copied())
                .enumerate()
                .zip(payloads)
            {
                let event_hash = compute_sha256_digest_bytes(&payload);
                events_digest_hasher.update([COMMIT_EVENT_FAMILY_REF]);
                events_digest_hasher.update(event_hash);
                ref_rows.push(RefEventInsertRow {
                    repo_id: repo_id.clone(),
                    commit_pk: commit_pk.clone(),
                    commit_event_idx: i32::try_from(local_proposal.events.len() + offset).map_err(
                        |_| CoreError::InvalidProposal("commit event index overflow".to_string()),
                    )?,
                    owner_item_id: event.owner_item_id.clone(),
                    ref_seq,
                    event_type: event.event_type.clone(),
                    metadata: event.set_metadata.as_ref().and_then(|value| value.payload()).cloned(),
                    payload,
                    event_payload_digest: event_hash,
                    created_at: commit_created_at,
                });
            }

            let events_digest_bytes: [u8; 32] = events_digest_hasher.finalize().into();
            let events_digest =
                Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(events_digest_bytes))
                    .expect("internal sha256 digest construction must be valid");

            let format_version = COMMIT_FORMAT_VERSION;
            let canonical_commit_bytes = canonical_commit_bytes_for_hash(
                &local_proposal.author_id,
                &local_proposal.committer_id,
                local_proposal.occurred_at,
                local_proposal.message.as_deref(),
                None,
                combined_event_count,
                &events_digest,
            )?;
            let commit_id = compute_commit_id(&canonical_commit_bytes);

            if let Err(err) = txn
                .insert_commit_sealed(
                    &repo_id,
                    &commit_pk,
                    repo_seq,
                    local_proposal.operation_id.as_deref(),
                    format_version,
                    None,
                    &local_proposal.author_id,
                    &local_proposal.committer_id,
                    local_proposal.message.as_deref(),
                    local_proposal.occurred_at,
                    commit_created_at,
                    &commit_id,
                    &events_digest,
                    i32::try_from(combined_event_count).map_err(|_| {
                        CoreError::InvalidProposal("commit event count overflow".to_string())
                    })?,
                )
                .await
            {
                match err {
                    StorageError::UniqueViolation { .. } => {
                        return Err(map_insert_commit_unique_violation(
                            txn,
                            &repo_id,
                            local_proposal.operation_id.as_deref(),
                            &commit_id,
                        )
                        .await?);
                    }
                    other => return Err(CoreError::StorageError(other)),
                }
            }
            txn.append_events(&event_rows).await?;
            txn.append_ref_events(&ref_rows).await?;

            // Persist updated item rows using the same parking/ordering strategy as append().
            persist_items_with_live_parking(
                txn,
                &repo_id,
                &items,
                &items_before_modifiers,
                &create_item_ids,
            )
            .await?;

            let updated_at = now();
            txn.federation_domain_set_last_seq(
                &repo_id,
                proposal.domain_id.as_str(),
                domain_seq_end,
                updated_at,
            )
            .await?;

            Ok(IngestedDomainCommit {
                applied: true,
                commit_id: Some(commit_id),
                repo_seq,
                created_at: Some(commit_created_at),
                occurred_at: proposal.occurred_at,
                domain_id: proposal.domain_id.clone(),
                domain_seq: proposal.domain_seq,
                domain_seq_end,
            })
        }
        .await
    }
}

fn map_name_error(
    repo_id: &RepoId,
    item_id: Option<&ItemId>,
    name: &str,
    err: NameError,
) -> CoreError {
    match err.kind {
        NameErrorKind::TooLong { bytes, max } => CoreError::NameTooLong {
            repo_id: repo_id.clone(),
            item_id: item_id.cloned(),
            name: name.to_string(),
            bytes,
            max,
        },
        other => CoreError::NameInvalid {
            repo_id: repo_id.clone(),
            item_id: item_id.cloned(),
            name: name.to_string(),
            reason: NameError { kind: other }.to_string(),
        },
    }
}

fn item_type_prefix_from_event_type(event_type: &str) -> Option<&str> {
    event_type.split_once("::").map(|(item_type, _)| item_type)
}

fn event_type_matches_item_type(event_type: &str, item_type: &str) -> bool {
    match item_type_prefix_from_event_type(event_type) {
        Some("ITEM") => true,
        Some(prefix) => prefix == item_type,
        None => false,
    }
}

fn validate_event_type_matches_item(
    item_id: &ItemId,
    item_type: &str,
    event_type: &str,
) -> Result<(), CoreError> {
    let Some(_) = item_type_prefix_from_event_type(event_type) else {
        return Err(CoreError::InvalidProposal(format!(
            "event_type must include item type prefix: `{event_type}`"
        )));
    };
    if !event_type_matches_item_type(event_type, item_type) {
        return Err(CoreError::InvalidProposal(format!(
            "event_type `{event_type}` does not match item_type `{item_type}` (item_id={item_id})"
        )));
    }
    Ok(())
}

fn validate_event_types_match_item(
    item_id: &ItemId,
    item_type: &str,
    events: &[EventProposal],
) -> Result<(), CoreError> {
    for ev in events {
        validate_event_type_matches_item(item_id, item_type, ev.event_type.as_str())?;
    }
    Ok(())
}

async fn map_insert_commit_unique_violation<T: StorageTxn + ?Sized>(
    txn: &mut T,
    repo_id: &RepoId,
    operation_id: Option<&str>,
    commit_id: &CommitId,
) -> Result<CoreError, CoreError> {
    if let Some(operation_id) = operation_id {
        match txn.get_commit_by_operation_id(repo_id, operation_id).await {
            Ok(Some(commit)) => {
                return Ok(CoreError::OperationAlreadyApplied {
                    repo_id: repo_id.clone(),
                    operation_id: operation_id.to_string(),
                    commit_id: commit.commit_id,
                });
            }
            Ok(None) => {}
            Err(err) => return Err(CoreError::StorageError(err)),
        }
    }

    match txn.get_commit(repo_id, commit_id).await {
        Ok(Some(_)) => Ok(CoreError::DuplicateCommit {
            repo_id: repo_id.clone(),
            commit_id: commit_id.clone(),
        }),
        Ok(None) => Ok(CoreError::RepoSeqConflict {
            repo_id: repo_id.clone(),
        }),
        Err(err) => Err(CoreError::StorageError(err)),
    }
}

/// Write item rows in batch, then replay per-row only when we must recover
/// precise domain errors from a uniqueness violation.
async fn put_items_with_conflict_mapping<T: StorageTxn + ?Sized>(
    txn: &mut T,
    repo_id: &RepoId,
    rows: &[&ItemRow],
) -> Result<(), CoreError> {
    if rows.is_empty() {
        return Ok(());
    }

    if rows.len() < ITEM_UPSERT_BATCH_MIN_ROWS {
        for row in rows {
            let parent_folder_id = row.parent_folder_id.clone();
            let name = row.name.clone();
            if let Err(err) = txn.put_item(row).await {
                match err {
                    StorageError::UniqueViolation { detail, constraint } => {
                        return Err(map_item_unique_violation(
                            txn,
                            repo_id,
                            row,
                            parent_folder_id,
                            name,
                            detail,
                            constraint,
                        )
                        .await?);
                    }
                    other => return Err(CoreError::StorageError(other)),
                }
            }
        }
        return Ok(());
    }

    let batch_rows: Vec<ItemRow> = rows.iter().map(|row| (*row).clone()).collect();
    match txn.put_items(&batch_rows).await {
        Ok(()) => Ok(()),
        Err(StorageError::UniqueViolation { detail, constraint }) => {
            for row in rows {
                let parent_folder_id = row.parent_folder_id.clone();
                let name = row.name.clone();
                if let Err(err) = txn.put_item(row).await {
                    match err {
                        StorageError::UniqueViolation { detail, constraint } => {
                            return Err(map_item_unique_violation(
                                txn,
                                repo_id,
                                row,
                                parent_folder_id,
                                name,
                                detail,
                                constraint,
                            )
                            .await?);
                        }
                        other => return Err(CoreError::StorageError(other)),
                    }
                }
            }

            Err(CoreError::StorageError(StorageError::UniqueViolation {
                detail,
                constraint,
            }))
        }
        Err(other) => Err(CoreError::StorageError(other)),
    }
}

async fn map_item_unique_violation<T: StorageTxn + ?Sized>(
    txn: &mut T,
    repo_id: &RepoId,
    row: &ItemRow,
    parent_folder_id: Option<ItemId>,
    name: String,
    detail: String,
    constraint: Option<String>,
) -> Result<CoreError, CoreError> {
    if let Some(fid) = row.federation_stream_id.as_deref() {
        match txn.get_item_by_federation_stream_id(repo_id, fid).await {
            Ok(Some(existing)) if existing.item_id != row.item_id => {
                return Ok(CoreError::InvalidProposal(
                    "federation_stream_id must be unique within a repo".to_string(),
                ));
            }
            Ok(_) => {}
            Err(err) => return Err(CoreError::StorageError(err)),
        }
    }

    if matches!(row.lifecycle_state, LifecycleState::Live) {
        return Ok(CoreError::NameConflict {
            repo_id: repo_id.clone(),
            parent_folder_id,
            name,
        });
    }

    Ok(CoreError::StorageError(StorageError::UniqueViolation {
        detail,
        constraint,
    }))
}


#[cfg(test)]
mod commit_metadata_limit_tests {
    use super::*;

    #[test]
    fn federation_domain_receipt_keys_are_stable_distinct_and_bounded() {
        let long_domain = "d".repeat(16 * 1024);
        let first = federated_domain_operation_id(&long_domain, 41);
        assert_eq!(first, federated_domain_operation_id(&long_domain, 41));
        assert_ne!(first, federated_domain_operation_id(&long_domain, 42));
        assert_ne!(first, federated_domain_operation_id("other", 41));
        assert!(first.len() <= crate::limits::MAX_OPERATION_ID_BYTES);
    }
}

fn prepare_existing_federated_ingest_events(
    ctx: &InvariantContext<'_>,
    item_row: &ItemRow,
    federation_stream_id: &str,
    item_events: &mut [EventProposal],
) -> Result<(), CoreError> {
    let item_id = &item_row.item_id;
    let tombstone = TombstoneFinality;
    let mode_rule = SequencingModeRule;
    tombstone.check(ctx, item_id, item_row, item_events)?;
    mode_rule.check(ctx, item_id, item_row, item_events)?;

    match item_row.federation_stream_id.as_ref() {
        Some(stored) if stored == federation_stream_id => {}
        Some(stored) => {
            return Err(CoreError::FederatedIdMismatch {
                repo_id: ctx.repo_id.clone(),
                item_id: item_id.clone(),
                expected: stored.clone(),
                found: federation_stream_id.to_string(),
            });
        }
        None => {
            return Err(CoreError::FederatedIdMismatch {
                repo_id: ctx.repo_id.clone(),
                item_id: item_id.clone(),
                expected: String::new(),
                found: federation_stream_id.to_string(),
            });
        }
    }

    for ev in item_events.iter() {
        if let Some(provided) = &ev.item_id
            && provided != item_id
        {
            return Err(CoreError::InvalidProposal(
                    "EventProposal.item_id does not match the local item resolved for federation_stream_id"
                        .to_string(),
                ));
        }
    }

    for ev in item_events.iter_mut() {
        ev.base_seq = Some(item_row.head_seq);
        ev.item_id = Some(item_id.clone());
    }

    validate_event_types_match_item(item_id, item_row.item_type.as_str(), item_events)
}

fn federated_proposal_is_content_only_existing_item(proposal: &FederatedProposal) -> bool {
    proposal.events.iter().all(|ev| {
        ev.create_item.is_none()
            && ev.set_parent.is_none()
            && ev.set_name.is_none()
            && ev.set_tombstoned.is_none()
            && ev.set_in_trash.is_none()
            && ev.set_is_federated.is_none()
            && ev.set_federation_stream_id.is_none()
    })
}

/// Return canonical item events followed by refs, enforcing fixed format limits.
/// There is no aggregate commit size or event-count admission limit.
pub fn validate_payload_limits(
    events: &[EventProposal],
    ref_events: &[RefEventProposal],
) -> Result<Vec<Vec<u8>>, CoreError> {
    events
        .iter()
        .map(canonical_event_proposal_bytes)
        .chain(ref_events.iter().map(canonical_ref_event_proposal_bytes))
        .collect()
}
