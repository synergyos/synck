//! Storage backend traits and adapters. Keep thread safety explicit.
//!
//! The storage layer is responsible for:
//! - Maintaining the physical schema for `items`, `events`, and `commits`.
//! - Providing a minimal set of operations used by the core engine.
//! - Hiding backend-specific details.
//!
//! It is *not* responsible for policy or structural invariants; those live in
//! the repo/invariants layers. Storage errors are mapped into `StorageError`
//! and then into `CoreError` at higher layers.

use std::sync::Arc;

use async_trait::async_trait;

use crate::errors::StorageError;
use crate::types::{
    CommitEventRow, CommitId, CommitPk, CommitRow, EventInsertRow, EventRow, ItemCursor, ItemId,
    ItemRow, LifecycleState, RefEventInsertRow, RefEventRow, RepoEventCursor, RepoId, Sha256Digest,
    Timestamp,
};

/// Fully materialized sealed commit row ready for persistence.
///
/// This allows core to batch multiple commit header inserts in one backend call
/// while keeping the canonical commit_id/event digest logic in-core.
#[derive(Clone, Debug)]
pub struct SealedCommitInsert {
    pub commit_pk: CommitPk,
    pub repo_seq: Option<i64>,
    pub operation_id: Option<String>,
    pub format_version: i32,
    pub author_id: String,
    pub committer_id: String,
    pub message: Option<String>,
    pub occurred_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub commit_id: CommitId,
    pub events_digest: Sha256Digest,
    pub event_count: i32,
}

/// A single transactional handle used during commit application.
///
/// Implementations must ensure that all methods on a given instance operate
/// within the same database transaction until `commit` or `rollback` is
/// called.
// Commit insertion mirrors one fixed storage row. Packing its fields into an
// argument object would add indirection without reducing the storage contract.
#[async_trait]
#[allow(clippy::too_many_arguments)]
pub trait StorageTxn: Send {
    /// Return the storage authority's timestamp for a commit accepted in this
    /// transaction.
    ///
    /// Core uses this value for persisted `created_at` fields. Proposal
    /// timestamps such as `occurred_at` are origin metadata and MUST NOT drive
    /// durable acceptance order.
    ///
    /// The value MUST be later than the acceptance time of every accepted,
    /// unpublished commit this transaction could depend on, even when the
    /// wall clock stepped back. Publication orders waiting commits by this
    /// time, so it is what keeps publication causal.
    async fn acceptance_timestamp(&mut self, repo_id: &RepoId) -> Result<Timestamp, StorageError>;

    /// Acquire transactional conflict locks for this operation.
    ///
    /// The lock keys are engine-defined and backend-specific; the reference
    /// Postgres backend uses `pg_advisory_xact_lock(bigint)`.
    ///
    /// Callers MUST provide keys in deterministic sorted order to avoid
    /// deadlocks across concurrent writers.
    async fn lock_conflict_keys(&mut self, keys: &[i64]) -> Result<(), StorageError>;

    /// Fetch multiple items by id in a single backend call.
    ///
    /// Returns only items that exist; callers are responsible for detecting
    /// missing ids.
    async fn get_items(
        &mut self,
        repo_id: &RepoId,
        item_ids: &[ItemId],
    ) -> Result<Vec<ItemRow>, StorageError>;

    /// Return at most `remaining + 1` non-tombstoned descendants of `root_item_id`.
    ///
    /// The extra row is an overflow sentinel. Implementations must cap the
    /// recursive traversal itself and must not sort or materialize the complete
    /// subtree before applying this bound. Result order is unspecified; Core
    /// sorts a complete bounded result before comparison.
    async fn list_non_tombstoned_descendants_bounded(
        &mut self,
        repo_id: &RepoId,
        root_item_id: &ItemId,
        remaining: usize,
    ) -> Result<Vec<(ItemId, i64)>, StorageError>;

    /// True when a folder has a live descendant. A live descendant must have
    /// a live path, so traversal stops at non-live children.
    /// Non-tombstoned records whose link targets this folder.
    async fn list_records_linked_to(
        &mut self,
        repo_id: &RepoId,
        folder_id: &ItemId,
    ) -> Result<Vec<ItemRow>, StorageError>;

    async fn has_live_descendants(
        &mut self,
        repo_id: &RepoId,
        root_item_id: &ItemId,
    ) -> Result<bool, StorageError>;

    async fn get_item(
        &mut self,
        repo_id: &RepoId,
        item_id: &ItemId,
    ) -> Result<Option<ItemRow>, StorageError>;

    async fn get_item_by_federation_stream_id(
        &mut self,
        repo_id: &RepoId,
        federation_stream_id: &str,
    ) -> Result<Option<ItemRow>, StorageError>;

    async fn put_item(&mut self, item: &ItemRow) -> Result<(), StorageError>;

    /// Persist multiple item rows in this transaction.
    ///
    /// Default behavior preserves compatibility by calling `put_item` for each
    /// row in order. Backends may override with a true batch implementation.
    async fn put_items(&mut self, items: &[ItemRow]) -> Result<(), StorageError> {
        for item in items {
            self.put_item(item).await?;
        }
        Ok(())
    }

    /// Update only `head_seq` for existing items.
    ///
    /// This is the minimal write needed for content-only appends where no
    /// structural/federation/lifecycle fields changed.
    async fn update_item_heads(
        &mut self,
        repo_id: &RepoId,
        updates: &[(ItemId, i64)],
    ) -> Result<(), StorageError> {
        for (item_id, head_seq) in updates {
            let mut row =
                self.get_item(repo_id, item_id)
                    .await?
                    .ok_or_else(|| StorageError::Io {
                        detail: format!(
                            "missing item during head update: repo_id={} item_id={}",
                            repo_id, item_id
                        ),
                    })?;
            row.head_seq = *head_seq;
            self.put_item(&row).await?;
        }
        Ok(())
    }

    /// Compare and update attached-ref heads for existing owner rows.
    async fn update_item_ref_heads(
        &mut self,
        repo_id: &RepoId,
        updates: &[(ItemId, i64, i64)],
    ) -> Result<(), StorageError> {
        for (item_id, expected, next) in updates {
            let mut row =
                self.get_item(repo_id, item_id)
                    .await?
                    .ok_or_else(|| StorageError::Io {
                        detail: format!(
                            "missing item during ref head update: repo_id={} item_id={}",
                            repo_id, item_id
                        ),
                    })?;
            if row.ref_head_seq != *expected {
                return Err(StorageError::Io {
                    detail: format!(
                        "ref head changed during update: repo_id={} item_id={} expected={} found={}",
                        repo_id, item_id, expected, row.ref_head_seq
                    ),
                });
            }
            row.ref_head_seq = *next;
            self.put_item(&row).await?;
        }
        Ok(())
    }

    async fn get_commit(
        &mut self,
        repo_id: &RepoId,
        commit_id: &CommitId,
    ) -> Result<Option<CommitRow>, StorageError>;

    /// Fetch a commit by operation id within a repo.
    async fn get_commit_by_operation_id(
        &mut self,
        repo_id: &RepoId,
        operation_id: &str,
    ) -> Result<Option<CommitRow>, StorageError>;

    async fn list_events_for_item(
        &mut self,
        repo_id: &RepoId,
        item_id: &ItemId,
        from_seq: i64,
        limit: usize,
    ) -> Result<Vec<EventRow>, StorageError>;

    /// Assign `repo_seq` to committed unsequenced commits for a repo.
    ///
    /// Returns the number of commits sequenced.
    async fn sequence_repo_activity(
        &mut self,
        repo_id: &RepoId,
        limit: usize,
    ) -> Result<usize, StorageError>;

    /// Persist embedder-owned evidence for a newly authored local commit in this
    /// transaction. Called after its insert succeeds, never for replay or ingested
    /// history. Failure aborts the append; canonical bytes are already fixed.
    async fn record_authored_commit(
        &mut self,
        _repo_id: &RepoId,
        _commit_pk: &CommitPk,
        _created_at: Timestamp,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    /// Insert an unsealed commit header row.
    ///
    /// The commit starts with `commit_id`/`events_digest`/`event_count` unset
    /// and is sealed via `seal_commit()`.
    async fn start_commit(
        &mut self,
        repo_id: &RepoId,
        commit_pk: &CommitPk,
        repo_seq: Option<i64>,
        operation_id: Option<&str>,
        format_version: i32,
        author_id: &str,
        committer_id: &str,
        message: Option<&str>,
        occurred_at: Option<Timestamp>,
        created_at: Timestamp,
    ) -> Result<(), StorageError>;

    /// Insert a sealed commit header row in one logical operation.
    ///
    /// Backends may override this with a single SQL statement to avoid an
    /// extra update round-trip compared to `start_commit + seal_commit`.
    async fn insert_commit_sealed(
        &mut self,
        repo_id: &RepoId,
        commit_pk: &CommitPk,
        repo_seq: Option<i64>,
        operation_id: Option<&str>,
        format_version: i32,
        author_id: &str,
        committer_id: &str,
        message: Option<&str>,
        occurred_at: Option<Timestamp>,
        created_at: Timestamp,
        commit_id: &CommitId,
        events_digest: &Sha256Digest,
        event_count: i32,
    ) -> Result<(), StorageError> {
        self.start_commit(
            repo_id,
            commit_pk,
            repo_seq,
            operation_id,
            format_version,
            author_id,
            committer_id,
            message,
            occurred_at,
            created_at,
        )
        .await?;
        self.seal_commit(repo_id, commit_pk, commit_id, events_digest, event_count)
            .await
    }

    /// Insert multiple sealed commit header rows in one logical operation.
    ///
    /// Default behavior preserves compatibility by calling
    /// `insert_commit_sealed` for each row in order. Backends may override
    /// with a true bulk insert implementation.
    async fn insert_commits_sealed(
        &mut self,
        repo_id: &RepoId,
        commits: &[SealedCommitInsert],
    ) -> Result<(), StorageError> {
        for commit in commits {
            self.insert_commit_sealed(
                repo_id,
                &commit.commit_pk,
                commit.repo_seq,
                commit.operation_id.as_deref(),
                commit.format_version,
                &commit.author_id,
                &commit.committer_id,
                commit.message.as_deref(),
                commit.occurred_at,
                commit.created_at,
                &commit.commit_id,
                &commit.events_digest,
                commit.event_count,
            )
            .await?;
        }
        Ok(())
    }

    /// Append a single event row for a commit (streaming).
    async fn append_event(&mut self, event: &EventInsertRow) -> Result<(), StorageError>;

    /// Append multiple event rows for a commit in this transaction.
    ///
    /// Default behavior preserves compatibility by calling `append_event` for
    /// each row in order. Backends may override with a true batch insert.
    async fn append_events(&mut self, events: &[EventInsertRow]) -> Result<(), StorageError> {
        for event in events {
            self.append_event(event).await?;
        }
        Ok(())
    }

    async fn append_ref_event(&mut self, event: &RefEventInsertRow) -> Result<(), StorageError>;

    async fn append_ref_events(
        &mut self,
        events: &[RefEventInsertRow],
    ) -> Result<(), StorageError> {
        for event in events {
            self.append_ref_event(event).await?;
        }
        Ok(())
    }

    /// Seal a commit by setting its content-addressed id and digest metadata.
    async fn seal_commit(
        &mut self,
        repo_id: &RepoId,
        commit_pk: &CommitPk,
        commit_id: &CommitId,
        events_digest: &Sha256Digest,
        event_count: i32,
    ) -> Result<(), StorageError>;

    /// Fetch (and initialize if missing) the federation domain cursor for update.
    async fn federation_domain_last_seq_for_update(
        &mut self,
        repo_id: &RepoId,
        domain_id: &str,
    ) -> Result<i64, StorageError>;

    /// Update the federation domain cursor to the new last domain seq.
    async fn federation_domain_set_last_seq(
        &mut self,
        repo_id: &RepoId,
        domain_id: &str,
        last_domain_seq: i64,
        updated_at: Timestamp,
    ) -> Result<(), StorageError>;

    async fn commit(self: Box<Self>) -> Result<(), StorageError>;

    async fn rollback(self: Box<Self>) -> Result<(), StorageError>;
}

/// Storage backend trait implemented by concrete backends.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Initialize or migrate the schema as needed.
    async fn init_schema(&self) -> Result<(), StorageError>;

    /// Begin a new transaction for work on a single repository.
    async fn begin_txn(&self) -> Result<Box<dyn StorageTxn>, StorageError>;

    /// Read a single item without an explicit multi-statement transaction.
    async fn get_item_readonly(
        &self,
        repo_id: &RepoId,
        item_id: &ItemId,
    ) -> Result<Option<ItemRow>, StorageError>;

    /// Read multiple items without an explicit caller-owned transaction.
    ///
    /// Backends should override this with one set-oriented read. The default
    /// keeps non-optimized backends correct without forcing every test backend
    /// to duplicate plumbing.
    async fn get_items_readonly(
        &self,
        repo_id: &RepoId,
        item_ids: &[ItemId],
    ) -> Result<Vec<ItemRow>, StorageError> {
        let mut txn = self.begin_txn().await?;
        match txn.get_items(repo_id, item_ids).await {
            Ok(rows) => {
                txn.rollback().await?;
                Ok(rows)
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    /// Read a single commit without an explicit multi-statement transaction.
    async fn get_commit_readonly(
        &self,
        repo_id: &RepoId,
        commit_id: &CommitId,
    ) -> Result<Option<CommitRow>, StorageError>;

    /// Fetch one sealed commit by its repository-scoped operation identity.
    ///
    /// This is the replay lookup. Implementations must use the operation-id
    /// uniqueness key and must not scan repository history.
    async fn get_commit_by_operation_id_readonly(
        &self,
        repo_id: &RepoId,
        operation_id: &str,
    ) -> Result<Option<CommitRow>, StorageError> {
        let mut txn = self.begin_txn().await?;
        match txn.get_commit_by_operation_id(repo_id, operation_id).await {
            Ok(row) => {
                txn.rollback().await?;
                Ok(row)
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    /// Fetch at most `limit` events for one sealed commit in event order.
    ///
    /// Implementations must constrain the producer by the exact repository and
    /// commit key. Callers compare the result with `CommitRow::event_count` to
    /// prove completeness.
    async fn list_events_for_commit_readonly(
        &self,
        repo_id: &RepoId,
        commit_id: &CommitId,
        limit: usize,
    ) -> Result<Vec<EventRow>, StorageError> {
        let Some(commit) = self.get_commit_readonly(repo_id, commit_id).await? else {
            return Ok(Vec::new());
        };
        let Some(repo_seq) = commit.repo_seq else {
            return Ok(Vec::new());
        };
        let after = RepoEventCursor {
            repo_seq: repo_seq.saturating_sub(1),
            commit_event_idx: i32::MAX,
        };
        let rows = self
            .list_repo_events_readonly(repo_id, Some(&after), limit)
            .await?;
        Ok(rows
            .into_iter()
            .take_while(|event| event.commit_id == *commit_id)
            .collect())
    }

    /// List events for an item without an explicit multi-statement transaction.
    async fn list_events_for_item_readonly(
        &self,
        repo_id: &RepoId,
        item_id: &ItemId,
        from_seq: i64,
        limit: usize,
    ) -> Result<Vec<EventRow>, StorageError>;

    async fn list_ref_events_for_owner_readonly(
        &self,
        repo_id: &RepoId,
        owner_item_id: &ItemId,
        from_seq: i64,
        limit: usize,
    ) -> Result<Vec<RefEventRow>, StorageError>;

    /// Fetch complete tagged events for one commit in commit-event order.
    async fn list_commit_events_readonly(
        &self,
        repo_id: &RepoId,
        commit_id: &CommitId,
        limit: usize,
    ) -> Result<Vec<CommitEventRow>, StorageError>;

    /// Stream complete tagged repo activity in `(repo_seq, commit_event_idx)` order.
    async fn list_repo_commit_events_readonly(
        &self,
        repo_id: &RepoId,
        after: Option<&RepoEventCursor>,
        limit: usize,
    ) -> Result<Vec<CommitEventRow>, StorageError>;

    /// Stream events in global repo order by `(repo_seq, commit_event_idx)` with a cursor.
    ///
    /// Implementations MUST return events ordered by `(repo_seq ASC, commit_event_idx ASC)`.
    /// If `after` is provided, returned events MUST be strictly after that cursor.
    async fn list_repo_events_readonly(
        &self,
        repo_id: &RepoId,
        after: Option<&RepoEventCursor>,
        limit: usize,
    ) -> Result<Vec<EventRow>, StorageError>;

    /// List commits ordered by `repo_seq` with optional upper bound.
    ///
    /// Implementations MUST return commits ordered by `repo_seq ASC`.
    async fn list_commits_by_repo_seq_readonly(
        &self,
        repo_id: &RepoId,
        from_repo_seq: i64,
        to_repo_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<CommitRow>, StorageError>;

    /// List items ordered by `(created_at, item_id)` with optional lifecycle filter.
    ///
    /// Implementations MUST return items ordered by `(created_at ASC, item_id ASC)`.
    /// If `after` is provided, returned items MUST be strictly after that cursor.
    async fn list_items_readonly(
        &self,
        repo_id: &RepoId,
        after: Option<&ItemCursor>,
        lifecycle_state: Option<LifecycleState>,
        limit: usize,
    ) -> Result<Vec<ItemRow>, StorageError>;
}

/// Shared handle to a storage backend.
pub struct Storage {
    pub backend: Arc<dyn StorageBackend>,
}

impl Storage {
    pub fn new(backend: Arc<dyn StorageBackend>) -> Self {
        Self { backend }
    }
}
