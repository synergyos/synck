use async_trait::async_trait;

use serde::{Deserialize, Serialize};

use super::{ItemValue, NamespaceKey, StateError, StateNodeStore, StateRootId};
use crate::envelope::EnvelopeItemState;
use crate::types::{CommitId, ItemId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateProjectionHead {
    pub applied_repo_seq: i64,
    pub state_root: StateRootId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateRootRecord {
    pub repo_seq: i64,
    pub commit_id: CommitId,
    pub state_root: StateRootId,
}

#[derive(Debug)]
pub struct StateVerificationBatch {
    pub nodes_verified: u64,
    pub findings: Vec<StateError>,
    pub complete: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectionItem {
    pub original: Option<ItemValue>,
    pub current: Option<ItemValue>,
    pub envelope: EnvelopeItemState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StagedNamespaceMutation {
    Put(NamespaceKey, ItemId),
    Remove(NamespaceKey),
}

/// Repository-scoped durable state-index session.
///
/// The caller owns exclusive state-index authority for the complete lifetime
/// of this value. Canonical history remains outside that authority.
#[async_trait]
pub trait StateIndexStore: StateNodeStore {
    async fn projection_head(&mut self) -> Result<Option<StateProjectionHead>, StateError>;

    async fn root_at(&mut self, repo_seq: i64) -> Result<Option<StateRootRecord>, StateError>;

    async fn list_roots(
        &mut self,
        after_repo_seq: i64,
        limit: usize,
    ) -> Result<Vec<StateRootRecord>, StateError>;

    /// Install the sequence-zero head after its complete root closure exists.
    async fn install_empty_head(&mut self, state_root: StateRootId) -> Result<(), StateError>;

    /// Publish one complete commit root and advance the head atomically.
    async fn publish_root(
        &mut self,
        expected_applied_repo_seq: i64,
        root: &StateRootRecord,
    ) -> Result<(), StateError>;

    /// Remove the publication head. Every remaining root and node is then
    /// unpublished reset work.
    async fn remove_head(&mut self) -> Result<(), StateError>;

    async fn delete_roots_batch(&mut self, limit: usize) -> Result<usize, StateError>;

    async fn delete_nodes_batch(&mut self, limit: usize) -> Result<usize, StateError>;

    async fn state_row_counts(&mut self) -> Result<(u64, u64, u64), StateError>;

    async fn begin_state_verification(&mut self) -> Result<(), StateError>;

    /// Hash and decode one bounded batch from the referenced node closure.
    /// Nodes examined since `begin_state_verification` are not read again.
    /// Backends bound node count and aggregate payload bytes; a single item
    /// value may exceed the byte budget because opaque strings have no format limit.
    /// Integrity findings are data; `Err` is reserved for storage failure.
    async fn verify_root_closures(
        &mut self,
        roots: &[StateRootId],
    ) -> Result<StateVerificationBatch, StateError>;

    async fn finish_state_verification(&mut self) -> Result<(), StateError>;
}

/// Disk-backed, repository-local scratch storage for one projected commit.
///
/// Scratch rows are never authoritative and may be discarded at any time.
#[async_trait]
pub trait StateProjectorStore: StateIndexStore {
    async fn begin_projection_commit(&mut self) -> Result<(), StateError>;

    async fn projection_item(
        &mut self,
        item_id: &ItemId,
    ) -> Result<Option<ProjectionItem>, StateError>;

    async fn put_projection_item(
        &mut self,
        item_id: &ItemId,
        item: &ProjectionItem,
    ) -> Result<(), StateError>;

    async fn list_projection_items(
        &mut self,
        after: Option<&ItemId>,
        limit: usize,
    ) -> Result<Vec<(ItemId, ProjectionItem)>, StateError>;

    async fn put_namespace_mutation(
        &mut self,
        mutation: &StagedNamespaceMutation,
    ) -> Result<(), StateError>;

    async fn list_namespace_mutations(
        &mut self,
        after_sort_key: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, StagedNamespaceMutation)>, StateError>;

    /// Commit the scratch writes before durable immutable-node mutation starts.
    async fn seal_projection_commit(&mut self) -> Result<(), StateError>;

    async fn finish_projection_commit(&mut self) -> Result<(), StateError>;
}
