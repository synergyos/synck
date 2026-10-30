use async_trait::async_trait;
use std::sync::Arc;

use crate::envelope::{
    CommitEnvelopeValidator, EnvelopeItemState, EnvelopeRefState, VerifyCommit, VerifyEvent,
    VerifyRefEvent,
};
use crate::repo::Repo;
use crate::transition::{ItemState, apply_event};
use crate::types::{
    CommitEventRow, CommitRow, EventRow, LifecycleState, RefEventRow, RepoEventCursor, RepoId,
    SequencingMode, sha256_digest_to_digest_bytes,
};

use super::{
    ContentPointer, ItemMutation, ItemValue, NamespaceKey, NamespaceMutation, ProjectionItem,
    RootHeader, StagedNamespaceMutation, StateError, StateNodeStore, StateProjectorStore,
    StateRootRecord, StateTree,
};

#[async_trait]
pub trait StateProjectionSource: Send + Sync {
    async fn commits(
        &self,
        repo_id: RepoId,
        from_repo_seq: i64,
        to_repo_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<CommitRow>, StateError>;

    async fn events(
        &self,
        repo_id: RepoId,
        after: RepoEventCursor,
        limit: usize,
    ) -> Result<Vec<CommitEventRow>, StateError>;
}

#[async_trait]
impl StateProjectionSource for Repo {
    async fn commits(
        &self,
        repo_id: RepoId,
        from_repo_seq: i64,
        to_repo_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<CommitRow>, StateError> {
        self.list_commits_by_repo_seq(repo_id, from_repo_seq, to_repo_seq, limit)
            .await
            .map_err(core_error)
    }

    async fn events(
        &self,
        repo_id: RepoId,
        after: RepoEventCursor,
        limit: usize,
    ) -> Result<Vec<CommitEventRow>, StateError> {
        self.list_repo_commit_events(repo_id, Some(after), limit)
            .await
            .map_err(core_error)
    }
}

const HISTORY_PAGE: usize = 256;
// One page is a fixed memory bound. A larger page also avoids turning a valid
// large commit into hundreds of durability barriers on local filesystems.
const MUTATION_PAGE: usize = 4_096;
const RESET_PAGE: usize = 1_024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StateProjectionReport {
    pub start_repo_seq: i64,
    pub applied_repo_seq: i64,
    pub commits_processed: u64,
}

/// Ensure the sequence-zero state closure exists.
///
/// A missing head makes every existing state row unpublished reset work. The
/// cleanup order preserves the immediate root-to-node foreign keys.
pub async fn initialize_state_index<S: StateProjectorStore>(
    store: &mut S,
    repo_id: &RepoId,
) -> Result<(), StateError> {
    if store.projection_head().await?.is_some() {
        return Ok(());
    }
    while store.delete_roots_batch(RESET_PAGE).await? != 0 {}
    while store.delete_nodes_batch(RESET_PAGE).await? != 0 {}

    store.begin_batch().await?;
    let root = async {
        let mut tree = StateTree::new(&mut *store);
        let item_map_root = tree.empty_item_root().await?;
        let namespace_root = tree.empty_namespace_root().await?;
        tree.store_root(RootHeader {
            repo_id: repo_id.clone(),
            repo_seq: 0,
            commit_id: None,
            item_map_root,
            namespace_root,
        })
        .await
    }
    .await;
    let root = match root {
        Ok(root) => root,
        Err(error) => {
            store.abort_batch().await?;
            return Err(error);
        }
    };
    if let Err(error) = store.finish_batch(&[root.node_id()]).await {
        store.abort_batch().await?;
        return Err(error);
    }
    store.install_empty_head(root).await
}

/// Project complete accepted commits through `target_repo_seq`, or through the
/// current sequenced tail when no target is supplied.
pub async fn catch_up_state_index<'a, S: StateProjectorStore>(
    repo: &'a dyn StateProjectionSource,
    repo_id: &'a RepoId,
    store: &'a mut S,
    target_repo_seq: Option<i64>,
) -> Result<StateProjectionReport, StateError> {
    catch_up_state_index_bounded(repo, repo_id, store, target_repo_seq, None).await
}

pub async fn catch_up_state_index_bounded<'a, S: StateProjectorStore>(
    repo: &'a dyn StateProjectionSource,
    repo_id: &'a RepoId,
    store: &'a mut S,
    target_repo_seq: Option<i64>,
    max_commits: Option<u64>,
) -> Result<StateProjectionReport, StateError> {
    if max_commits == Some(0) {
        return Err(StateError::Storage(
            "state projection commit limit must be positive".to_string(),
        ));
    }
    let repo_id = Arc::new(repo_id.clone());
    initialize_state_index(store, &repo_id).await?;
    let mut head = store
        .projection_head()
        .await?
        .ok_or_else(|| StateError::Storage("state projection head disappeared".to_string()))?;
    validate_projection_head(store, &repo_id, &head).await?;
    let start_repo_seq = head.applied_repo_seq;
    let mut report = StateProjectionReport {
        start_repo_seq,
        applied_repo_seq: start_repo_seq,
        commits_processed: 0,
    };

    loop {
        let from = head.applied_repo_seq.saturating_add(1);
        let commits = repo
            .commits((*repo_id).clone(), from, target_repo_seq, HISTORY_PAGE)
            .await?;
        if commits.is_empty() {
            break;
        }
        for commit in commits {
            if max_commits.is_some_and(|limit| report.commits_processed >= limit) {
                return Ok(report);
            }
            let repo_seq = commit.repo_seq.ok_or_else(|| {
                StateError::InvalidTree("sequenced commit has no repository sequence".to_string())
            })?;
            if let Some(target) = target_repo_seq
                && repo_seq > target
            {
                return Ok(report);
            }
            project_commit(repo, repo_id.clone(), store, head.clone(), Arc::new(commit)).await?;
            head = store.projection_head().await?.ok_or_else(|| {
                StateError::Storage("state projection head disappeared".to_string())
            })?;
            report.applied_repo_seq = head.applied_repo_seq;
            report.commits_processed = report.commits_processed.saturating_add(1);
        }
    }
    Ok(report)
}

async fn project_commit<S: StateProjectorStore>(
    repo: &dyn StateProjectionSource,
    repo_id: Arc<RepoId>,
    store: &mut S,
    head: super::StateProjectionHead,
    commit: Arc<CommitRow>,
) -> Result<(), StateError> {
    if commit.repo_id != *repo_id {
        return Err(StateError::InvalidTree(
            "projected commit belongs to another repository".to_string(),
        ));
    }
    let repo_seq = commit.repo_seq.ok_or_else(|| {
        StateError::InvalidTree("sequenced commit has no repository sequence".to_string())
    })?;
    if head.applied_repo_seq.checked_add(1) != Some(repo_seq) {
        return Err(StateError::InvalidTree(format!(
            "repository sequence {repo_seq} does not follow projection head {}",
            head.applied_repo_seq
        )));
    }
    let previous = validate_projection_head(store, &repo_id, &head).await?;

    store.begin_projection_commit().await?;
    let result = project_commit_inner(repo, repo_id, store, previous, commit).await;
    let cleanup = store.finish_projection_commit().await;
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn validate_projection_head<S: StateProjectorStore>(
    store: &mut S,
    repo_id: &RepoId,
    head: &super::StateProjectionHead,
) -> Result<RootHeader, StateError> {
    let root = StateTree::new(&mut *store)
        .read_root(head.state_root)
        .await?;
    if root.repo_id != *repo_id
        || root.repo_seq != head.applied_repo_seq
        || (root.repo_seq == 0 && root.commit_id.is_some())
    {
        return Err(StateError::InvalidTree(
            "projection head does not match its root header".to_string(),
        ));
    }
    if root.repo_seq == 0 {
        if store.root_at(0).await?.is_some() {
            return Err(StateError::InvalidTree(
                "sequence-zero projection has a commit root mapping".to_string(),
            ));
        }
    } else {
        let published = store.root_at(root.repo_seq).await?.ok_or_else(|| {
            StateError::InvalidTree(format!(
                "projection head has no root mapping at repository sequence {}",
                root.repo_seq
            ))
        })?;
        if published.state_root != head.state_root
            || root.commit_id.as_ref() != Some(&published.commit_id)
        {
            return Err(StateError::InvalidTree(
                "projection head does not match its published commit root".to_string(),
            ));
        }
    }
    Ok(root)
}

async fn project_commit_inner<S: StateProjectorStore>(
    repo: &dyn StateProjectionSource,
    repo_id: Arc<RepoId>,
    store: &mut S,
    previous: RootHeader,
    commit: Arc<CommitRow>,
) -> Result<(), StateError> {
    let repo_seq = commit.repo_seq.expect("caller checked repository sequence");
    let verify_commit = VerifyCommit {
        commit_id: Some(commit.commit_id.clone()),
        repo_seq: commit.repo_seq,
        format_version: commit.format_version,
        committer_id: commit.committer_id.clone(),
        occurred_at: commit.occurred_at,
        author_id: commit.author_id.clone(),
        message: commit.message.clone(),
        event_count: Some(commit.event_count),
        events_digest: Some(commit.events_digest.clone()),
    };
    let mut prior_seq = (previous.repo_seq != 0).then_some(previous.repo_seq);
    let mut validator = CommitEnvelopeValidator::begin(&verify_commit, &mut prior_seq);
    let mut findings = Vec::new();
    let mut cursor = RepoEventCursor {
        repo_seq,
        commit_event_idx: -1,
    };
    let mut reached_next_commit = false;
    let mut ref_states = std::collections::HashMap::<crate::types::ItemId, EnvelopeRefState>::new();

    while !reached_next_commit {
        let events = repo
            .events((*repo_id).clone(), cursor.clone(), HISTORY_PAGE)
            .await?;
        if events.is_empty() {
            break;
        }
        for event in events {
            let event_repo_seq = event.repo_seq().ok_or_else(|| {
                StateError::InvalidTree("repository event has no sequence".to_string())
            })?;
            if event_repo_seq != repo_seq {
                reached_next_commit = true;
                break;
            }
            cursor.commit_event_idx = event.commit_event_idx();
            match event {
                CommitEventRow::Item(event) => {
                    apply_projected_event(
                        store,
                        previous.item_map_root,
                        repo_id.clone(),
                        commit.clone(),
                        event,
                        &mut validator,
                        &mut findings,
                    )
                    .await?;
                }
                CommitEventRow::Ref(event) => {
                    apply_projected_ref_event(
                        store,
                        previous.item_map_root,
                        repo_id.clone(),
                        commit.clone(),
                        event,
                        &mut validator,
                        &mut ref_states,
                        &mut findings,
                    )
                    .await?;
                }
            }
        }
    }
    findings.extend(validator.finish());
    if !findings.is_empty() {
        return Err(StateError::InvalidTree(findings.join("; ")));
    }

    let touched_item_count = stage_namespace_changes(store).await?;
    store.seal_projection_commit().await?;
    let namespace_root = apply_namespace_changes(store, previous.namespace_root).await?;
    let item_map_root = apply_item_changes(store, previous.item_map_root).await?;
    validate_candidate_changes(store, item_map_root, namespace_root, touched_item_count).await?;

    store.begin_batch().await?;
    let state_root = {
        let mut tree = StateTree::new(&mut *store);
        tree.store_root(RootHeader {
            repo_id: (*repo_id).clone(),
            repo_seq,
            commit_id: Some(commit.commit_id.clone()),
            item_map_root,
            namespace_root,
        })
        .await
    };
    let state_root = match state_root {
        Ok(root) => root,
        Err(error) => {
            store.abort_batch().await?;
            return Err(error);
        }
    };
    if let Err(error) = store.finish_batch(&[state_root.node_id()]).await {
        store.abort_batch().await?;
        return Err(error);
    }
    store
        .publish_root(
            previous.repo_seq,
            &StateRootRecord {
                repo_seq,
                commit_id: commit.commit_id.clone(),
                state_root,
            },
        )
        .await
}

async fn apply_projected_event<S: StateProjectorStore>(
    store: &mut S,
    item_root: super::StateNodeId,
    repo_id: Arc<RepoId>,
    commit: Arc<CommitRow>,
    event: EventRow,
    validator: &mut CommitEnvelopeValidator,
    findings: &mut Vec<String>,
) -> Result<(), StateError> {
    if event.repo_id != *repo_id {
        findings.push(format!(
            "repo sequence {:?} event {}: repository ID mismatch",
            commit.repo_seq, event.commit_event_idx
        ));
    }
    if event.commit_id != commit.commit_id {
        findings.push(format!(
            "repo sequence {:?} event {}: commit ID mismatch",
            commit.repo_seq, event.commit_event_idx
        ));
    }

    let mut staged = match store.projection_item(&event.item_id).await? {
        Some(staged) => staged,
        None => {
            let original = {
                let mut tree = StateTree::new(&mut *store);
                tree.get_item(item_root, &event.item_id).await?
            };
            let envelope = match original.as_ref() {
                Some(item) => EnvelopeItemState::from_head(
                    Some(item.head_seq),
                    item.lifecycle_state == LifecycleState::Tombstoned,
                    Some(item.sequencing_mode),
                    item.federation_stream_id.clone(),
                ),
                None => EnvelopeItemState::default(),
            };
            ProjectionItem {
                original: original.clone(),
                current: original,
                envelope,
            }
        }
    };
    let verify_event = VerifyEvent {
        commit_event_idx: event.commit_event_idx,
        item_id: event.item_id.clone(),
        item_seq: event.item_seq,
        event_type: event.event_type.clone(),
        payload: event.payload.clone(),
        event_payload_digest: sha256_digest_to_digest_bytes(&event.event_payload_digest).to_vec(),
        parent_folder_id: event.parent_folder_id.clone(),
        parent_folder_id_before: event.parent_folder_id_before.clone(),
    };
    if let Some(proposal) = validator.validate_event(&verify_event, &mut staged.envelope) {
        let (mut state, parent_before) = match staged.current.as_ref() {
            Some(current) => (ItemState::from(current), None),
            None => match proposal.create_item.as_ref() {
                Some(create) => (
                    ItemState::from_create(
                        create,
                        staged
                            .envelope
                            .sequencing_mode
                            .unwrap_or(SequencingMode::Local),
                        staged.envelope.federation_stream_id.clone(),
                    )
                    .map_err(|error| StateError::InvalidTree(error.to_string()))?,
                    None,
                ),
                None => {
                    store.put_projection_item(&event.item_id, &staged).await?;
                    return Ok(());
                }
            },
        };
        let applied = if staged.current.is_some() {
            apply_event(&mut state, &proposal)
                .map_err(|error| StateError::InvalidTree(error.to_string()))?
        } else {
            crate::transition::AppliedTransition {
                parent_folder_id_before: parent_before,
                parent_folder_id_after: state.parent_folder_id.clone(),
            }
        };
        if event.parent_folder_id_before != applied.parent_folder_id_before {
            findings.push(format!(
                "repo sequence {:?} event {}: parent-before mismatch",
                commit.repo_seq, event.commit_event_idx
            ));
        }
        if event.parent_folder_id != applied.parent_folder_id_after {
            findings.push(format!(
                "repo sequence {:?} event {}: parent-after mismatch",
                commit.repo_seq, event.commit_event_idx
            ));
        }
        let content = if proposal.replaces_content() {
            Some(ContentPointer {
                item_seq: event.item_seq,
                event_digest: sha256_digest_to_digest_bytes(&event.event_payload_digest),
            })
        } else {
            staged
                .current
                .as_ref()
                .and_then(|item| item.content.clone())
        };
        staged.current = Some(ItemValue {
            item_id: event.item_id.clone(),
            item_type: state.item_type,
            parent_folder_id: state.parent_folder_id,
            name: state.name,
            name_key: state.name_key,
            record_key: state.record_key,
            link_folder_id: state.link_folder_id,
            head_seq: event.item_seq,
            ref_head_seq: staged
                .current
                .as_ref()
                .map(|item| item.ref_head_seq)
                .unwrap_or(0),
            lifecycle_state: state.lifecycle_state,
            created_at: staged
                .current
                .as_ref()
                .map(|item| item.created_at)
                .unwrap_or(commit.created_at),
            created_by: staged
                .current
                .as_ref()
                .map(|item| item.created_by.clone())
                .unwrap_or_else(|| commit.author_id.clone()),
            sequencing_mode: state.sequencing_mode,
            federation_stream_id: state.federation_stream_id,
            content,
        });
    }
    store.put_projection_item(&event.item_id, &staged).await
}

#[allow(clippy::too_many_arguments)]
async fn apply_projected_ref_event<S: StateProjectorStore>(
    store: &mut S,
    item_root: super::StateNodeId,
    repo_id: Arc<RepoId>,
    commit: Arc<CommitRow>,
    event: RefEventRow,
    validator: &mut CommitEnvelopeValidator,
    ref_states: &mut std::collections::HashMap<crate::types::ItemId, EnvelopeRefState>,
    findings: &mut Vec<String>,
) -> Result<(), StateError> {
    if event.repo_id != *repo_id {
        findings.push(format!(
            "repo sequence {:?} ref event {}: repository ID mismatch",
            commit.repo_seq, event.commit_event_idx
        ));
    }
    if event.commit_id != commit.commit_id {
        findings.push(format!(
            "repo sequence {:?} ref event {}: commit ID mismatch",
            commit.repo_seq, event.commit_event_idx
        ));
    }

    let mut staged = match store.projection_item(&event.owner_item_id).await? {
        Some(staged) => staged,
        None => {
            let original = {
                let mut tree = StateTree::new(&mut *store);
                tree.get_item(item_root, &event.owner_item_id).await?
            };
            ProjectionItem {
                original: original.clone(),
                current: original,
                envelope: EnvelopeItemState::default(),
            }
        }
    };
    let Some(current) = staged.current.as_mut() else {
        findings.push(format!(
            "repo sequence {:?} ref event {}: owner item is absent",
            commit.repo_seq, event.commit_event_idx
        ));
        return Ok(());
    };
    if current.lifecycle_state == LifecycleState::Tombstoned {
        findings.push(format!(
            "repo sequence {:?} ref event {}: owner item is tombstoned",
            commit.repo_seq, event.commit_event_idx
        ));
    }
    let owner = ref_states
        .entry(event.owner_item_id.clone())
        .or_insert_with(|| EnvelopeRefState {
            head_seq: current.ref_head_seq,
            commit_base_seq: None,
            commit_started: false,
        });
    let verify_event = VerifyRefEvent {
        commit_event_idx: event.commit_event_idx,
        owner_item_id: event.owner_item_id.clone(),
        ref_seq: event.ref_seq,
        event_type: event.event_type,
        payload: event.payload,
        event_payload_digest: sha256_digest_to_digest_bytes(&event.event_payload_digest).to_vec(),
    };
    if validator.validate_ref_event(&verify_event, owner).is_some() {
        current.ref_head_seq = event.ref_seq;
    }
    store
        .put_projection_item(&event.owner_item_id, &staged)
        .await
}

async fn stage_namespace_changes<S: StateProjectorStore>(store: &mut S) -> Result<u64, StateError> {
    let mut after = None;
    let mut touched_item_count = 0_u64;
    loop {
        let page = store
            .list_projection_items(after.as_ref(), MUTATION_PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        for (item_id, staged) in &page {
            touched_item_count = touched_item_count.checked_add(1).ok_or_else(|| {
                StateError::Storage("projected item count overflowed u64".to_string())
            })?;
            let original = live_namespace(staged.original.as_ref());
            let current = live_namespace(staged.current.as_ref());
            if original != current
                && let Some(key) = original
            {
                store
                    .put_namespace_mutation(&StagedNamespaceMutation::Remove(key))
                    .await?;
            }
            after = Some(item_id.clone());
        }
    }
    after = None;
    loop {
        let page = store
            .list_projection_items(after.as_ref(), MUTATION_PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        for (item_id, staged) in &page {
            let original = live_namespace(staged.original.as_ref());
            let current = live_namespace(staged.current.as_ref());
            if original != current
                && let Some(key) = current
            {
                store
                    .put_namespace_mutation(&StagedNamespaceMutation::Put(key, item_id.clone()))
                    .await?;
            }
            after = Some(item_id.clone());
        }
    }
    Ok(touched_item_count)
}

fn live_namespace(item: Option<&ItemValue>) -> Option<NamespaceKey> {
    item.filter(|item| item.lifecycle_state == LifecycleState::Live)
        .and_then(item_namespace)
}

/// A record has no name, so the namespace holds only tree items.
pub fn item_namespace(item: &ItemValue) -> Option<NamespaceKey> {
    let name_key = item.name_key.clone()?;
    Some(NamespaceKey::new(item.parent_folder_id.clone(), name_key))
}

async fn validate_candidate_item<S: StateNodeStore>(
    store: &mut S,
    item_root: super::StateNodeId,
    namespace_root: super::StateNodeId,
    item_id: &crate::types::ItemId,
    staged: &ProjectionItem,
) -> Result<(), StateError> {
    let current = staged.current.as_ref().ok_or_else(|| {
        StateError::InvalidTree(format!("projected item {item_id} has no final item state"))
    })?;
    let stored = StateTree::new(&mut *store)
        .get_item(item_root, item_id)
        .await?;
    if stored.as_ref() != Some(current) {
        return Err(StateError::InvalidTree(format!(
            "candidate item tree does not contain the final state for {item_id}"
        )));
    }

    let Some(current_key) = item_namespace(current) else {
        return Ok(());
    };
    let current_owner = StateTree::new(&mut *store)
        .get_namespace(namespace_root, &current_key)
        .await?;
    if current.lifecycle_state == LifecycleState::Live {
        if current_owner.as_ref() != Some(item_id) {
            return Err(StateError::InvalidTree(format!(
                "candidate namespace does not map live item {item_id} to its final key"
            )));
        }
    } else if current_owner.as_ref() == Some(item_id) {
        return Err(StateError::InvalidTree(format!(
            "candidate namespace still maps non-live item {item_id}"
        )));
    }

    if let Some(original_key) = live_namespace(staged.original.as_ref())
        && original_key != current_key
    {
        let old_owner = StateTree::new(&mut *store)
            .get_namespace(namespace_root, &original_key)
            .await?;
        if old_owner.as_ref() == Some(item_id) {
            return Err(StateError::InvalidTree(format!(
                "candidate namespace still maps item {item_id} at its old key"
            )));
        }
    }
    Ok(())
}

async fn validate_candidate_changes<S: StateProjectorStore>(
    store: &mut S,
    item_root: super::StateNodeId,
    namespace_root: super::StateNodeId,
    expected_item_count: u64,
) -> Result<(), StateError> {
    let mut after = None;
    let mut actual_item_count = 0_u64;
    loop {
        let page = store
            .list_projection_items(after.as_ref(), MUTATION_PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        for (item_id, staged) in &page {
            actual_item_count = actual_item_count.checked_add(1).ok_or_else(|| {
                StateError::Storage("candidate item count overflowed u64".to_string())
            })?;
            validate_candidate_item(store, item_root, namespace_root, item_id, staged).await?;
            after = Some(item_id.clone());
        }
    }
    if actual_item_count != expected_item_count {
        return Err(StateError::InvalidTree(format!(
            "candidate validation observed {actual_item_count} touched items, expected {expected_item_count}"
        )));
    }
    Ok(())
}

async fn apply_namespace_changes<S: StateProjectorStore>(
    store: &mut S,
    mut root: super::StateNodeId,
) -> Result<super::StateNodeId, StateError> {
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = store
            .list_namespace_mutations(after.as_deref(), MUTATION_PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        let mutations: Vec<_> = page
            .iter()
            .map(|(_, mutation)| match mutation {
                StagedNamespaceMutation::Put(key, item_id) => {
                    NamespaceMutation::Put(key.clone(), item_id.clone())
                }
                StagedNamespaceMutation::Remove(key) => NamespaceMutation::Remove(key.clone()),
            })
            .collect();
        root = StateTree::new(&mut *store)
            .apply_namespace_mutations(root, mutations.into_iter())
            .await?;
        after = page.last().map(|entry| entry.0.clone());
    }
    Ok(root)
}

async fn apply_item_changes<S: StateProjectorStore>(
    store: &mut S,
    mut root: super::StateNodeId,
) -> Result<super::StateNodeId, StateError> {
    let mut after = None;
    loop {
        let page = store
            .list_projection_items(after.as_ref(), MUTATION_PAGE)
            .await?;
        if page.is_empty() {
            break;
        }
        let mutations: Vec<_> = page
            .iter()
            .filter_map(|(_, item)| item.current.as_ref().cloned().map(ItemMutation::Put))
            .collect();
        root = StateTree::new(&mut *store)
            .apply_item_mutations(root, mutations.into_iter())
            .await?;
        after = page.last().map(|entry| entry.0.clone());
    }
    Ok(root)
}

fn core_error(error: crate::errors::CoreError) -> StateError {
    StateError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::state::{
        MemoryNodeStore, NamespaceMutation, StateIndexStore, StateNodeId, StateProjectionHead,
        StateRootId, StateVerificationBatch,
    };
    use crate::types::{
        COMMIT_FORMAT_VERSION, CreateItem, EventProposal, ItemId, RepoId, SequencingMode,
        Sha256Digest, canonical_commit_bytes_for_hash, canonical_event_proposal_bytes,
        compute_commit_id, compute_sha256_digest_bytes, sha256_prefixed_hex_from_digest_bytes,
    };

    fn item(head_seq: i64) -> ItemValue {
        ItemValue { link_folder_id: None, record_key: None,
            item_id: ItemId::parse("01890f2a-7c00-7000-8000-000000000002").unwrap(),
            item_type: "FILE".to_string(),
            parent_folder_id: None,
            name: Some("report.txt".to_string()),
            name_key: Some("report.txt".to_string()),
            head_seq,
            ref_head_seq: 0,
            lifecycle_state: LifecycleState::Live,
            created_at: Utc.timestamp_opt(1_688_169_600, 0).unwrap(),
            created_by: "alice".to_string(),
            sequencing_mode: SequencingMode::Local,
            federation_stream_id: None,
            content: None,
        }
    }

    #[tokio::test]
    async fn candidate_fence_rejects_half_applied_item_and_namespace_roots() {
        let mut store = MemoryNodeStore::default();
        let value = item(1);
        let item_id = value.item_id.clone();
        let item_root = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let item_root = StateTree::new(&mut store)
            .apply_item_mutations(item_root, [ItemMutation::Put(value.clone())])
            .await
            .unwrap();
        let namespace_root = StateTree::new(&mut store)
            .empty_namespace_root()
            .await
            .unwrap();
        let staged = ProjectionItem {
            original: None,
            current: Some(value.clone()),
            envelope: EnvelopeItemState::default(),
        };

        let error =
            validate_candidate_item(&mut store, item_root, namespace_root, &item_id, &staged)
                .await
                .expect_err("missing namespace half must fail");
        assert!(error.to_string().contains("candidate namespace"));

        let namespace_root = StateTree::new(&mut store)
            .apply_namespace_mutations(
                namespace_root,
                [NamespaceMutation::Put(
                    item_namespace(&value).unwrap(),
                    item_id.clone(),
                )],
            )
            .await
            .unwrap();
        let wrong_item = ProjectionItem {
            original: None,
            current: Some(item(2)),
            envelope: EnvelopeItemState::default(),
        };
        let error =
            validate_candidate_item(&mut store, item_root, namespace_root, &item_id, &wrong_item)
                .await
                .expect_err("missing item half must fail");
        assert!(error.to_string().contains("candidate item tree"));

        validate_candidate_item(&mut store, item_root, namespace_root, &item_id, &staged)
            .await
            .unwrap();
    }

    #[derive(Clone, Copy, Debug)]
    enum ProjectionFault {
        MissingItemHalf,
        MissingNamespaceHalf,
        TouchedItemCountMismatch,
    }

    struct FaultingProjectorStore {
        nodes: MemoryNodeStore,
        head: Option<StateProjectionHead>,
        roots: BTreeMap<i64, StateRootRecord>,
        projection_items: BTreeMap<ItemId, ProjectionItem>,
        namespace_mutations: BTreeMap<Vec<u8>, StagedNamespaceMutation>,
        projection_active: bool,
        sealed: bool,
        post_seal_item_lists: usize,
        fault: ProjectionFault,
    }

    impl FaultingProjectorStore {
        fn new(fault: ProjectionFault) -> Self {
            Self {
                nodes: MemoryNodeStore::default(),
                head: None,
                roots: BTreeMap::new(),
                projection_items: BTreeMap::new(),
                namespace_mutations: BTreeMap::new(),
                projection_active: false,
                sealed: false,
                post_seal_item_lists: 0,
                fault,
            }
        }
    }

    #[async_trait]
    impl StateNodeStore for FaultingProjectorStore {
        async fn load(&mut self, id: StateNodeId) -> Result<Vec<u8>, StateError> {
            self.nodes.load(id).await
        }

        async fn store(&mut self, id: StateNodeId, bytes: Vec<u8>) -> Result<(), StateError> {
            self.nodes.store(id, bytes).await
        }

        async fn begin_batch(&mut self) -> Result<(), StateError> {
            self.nodes.begin_batch().await
        }

        async fn finish_batch(&mut self, roots: &[StateNodeId]) -> Result<(), StateError> {
            self.nodes.finish_batch(roots).await
        }

        async fn abort_batch(&mut self) -> Result<(), StateError> {
            self.nodes.abort_batch().await
        }
    }

    #[async_trait]
    impl StateIndexStore for FaultingProjectorStore {
        async fn projection_head(&mut self) -> Result<Option<StateProjectionHead>, StateError> {
            Ok(self.head.clone())
        }

        async fn root_at(&mut self, repo_seq: i64) -> Result<Option<StateRootRecord>, StateError> {
            Ok(self.roots.get(&repo_seq).cloned())
        }

        async fn list_roots(
            &mut self,
            after_repo_seq: i64,
            limit: usize,
        ) -> Result<Vec<StateRootRecord>, StateError> {
            Ok(self
                .roots
                .range((after_repo_seq + 1)..)
                .take(limit)
                .map(|(_, root)| root.clone())
                .collect())
        }

        async fn install_empty_head(&mut self, state_root: StateRootId) -> Result<(), StateError> {
            self.head = Some(StateProjectionHead {
                applied_repo_seq: 0,
                state_root,
            });
            Ok(())
        }

        async fn publish_root(
            &mut self,
            expected_applied_repo_seq: i64,
            root: &StateRootRecord,
        ) -> Result<(), StateError> {
            let head = self
                .head
                .as_mut()
                .ok_or_else(|| StateError::Storage("missing test projection head".to_string()))?;
            if head.applied_repo_seq != expected_applied_repo_seq {
                return Err(StateError::Storage(
                    "stale test projection head".to_string(),
                ));
            }
            self.roots.insert(root.repo_seq, root.clone());
            *head = StateProjectionHead {
                applied_repo_seq: root.repo_seq,
                state_root: root.state_root,
            };
            Ok(())
        }

        async fn remove_head(&mut self) -> Result<(), StateError> {
            self.head = None;
            Ok(())
        }

        async fn delete_roots_batch(&mut self, limit: usize) -> Result<usize, StateError> {
            let keys: Vec<_> = self.roots.keys().take(limit).copied().collect();
            for key in &keys {
                self.roots.remove(key);
            }
            Ok(keys.len())
        }

        async fn delete_nodes_batch(&mut self, _limit: usize) -> Result<usize, StateError> {
            assert!(
                self.nodes.is_empty(),
                "test reset must start without state nodes"
            );
            Ok(0)
        }

        async fn state_row_counts(&mut self) -> Result<(u64, u64, u64), StateError> {
            Ok((
                self.nodes.len() as u64,
                self.roots.len() as u64,
                u64::from(self.head.is_some()),
            ))
        }

        async fn begin_state_verification(&mut self) -> Result<(), StateError> {
            Err(StateError::Storage(
                "verification is outside this projector test".to_string(),
            ))
        }

        async fn verify_root_closures(
            &mut self,
            _roots: &[StateRootId],
        ) -> Result<StateVerificationBatch, StateError> {
            Err(StateError::Storage(
                "verification is outside this projector test".to_string(),
            ))
        }

        async fn finish_state_verification(&mut self) -> Result<(), StateError> {
            Err(StateError::Storage(
                "verification is outside this projector test".to_string(),
            ))
        }
    }

    #[async_trait]
    impl StateProjectorStore for FaultingProjectorStore {
        async fn begin_projection_commit(&mut self) -> Result<(), StateError> {
            if self.projection_active {
                return Err(StateError::Storage(
                    "test projection scratch is already active".to_string(),
                ));
            }
            self.projection_items.clear();
            self.namespace_mutations.clear();
            self.projection_active = true;
            self.sealed = false;
            self.post_seal_item_lists = 0;
            Ok(())
        }

        async fn projection_item(
            &mut self,
            item_id: &ItemId,
        ) -> Result<Option<ProjectionItem>, StateError> {
            Ok(self.projection_items.get(item_id).cloned())
        }

        async fn put_projection_item(
            &mut self,
            item_id: &ItemId,
            item: &ProjectionItem,
        ) -> Result<(), StateError> {
            self.projection_items.insert(item_id.clone(), item.clone());
            Ok(())
        }

        async fn list_projection_items(
            &mut self,
            after: Option<&ItemId>,
            limit: usize,
        ) -> Result<Vec<(ItemId, ProjectionItem)>, StateError> {
            if self.sealed {
                self.post_seal_item_lists += 1;
                let suppress = matches!(
                    (self.fault, self.post_seal_item_lists),
                    (ProjectionFault::MissingItemHalf, 1)
                        | (ProjectionFault::TouchedItemCountMismatch, 3)
                );
                if suppress {
                    return Ok(Vec::new());
                }
            }
            Ok(self
                .projection_items
                .iter()
                .filter(|(item_id, _)| after.is_none_or(|after| *item_id > after))
                .take(limit)
                .map(|(item_id, item)| (item_id.clone(), item.clone()))
                .collect())
        }

        async fn put_namespace_mutation(
            &mut self,
            mutation: &StagedNamespaceMutation,
        ) -> Result<(), StateError> {
            let key = match mutation {
                StagedNamespaceMutation::Put(key, _) | StagedNamespaceMutation::Remove(key) => {
                    key.canonical_sort_key()
                }
            };
            self.namespace_mutations.insert(key, mutation.clone());
            Ok(())
        }

        async fn list_namespace_mutations(
            &mut self,
            after_sort_key: Option<&[u8]>,
            limit: usize,
        ) -> Result<Vec<(Vec<u8>, StagedNamespaceMutation)>, StateError> {
            if self.sealed
                && matches!(self.fault, ProjectionFault::MissingNamespaceHalf)
                && after_sort_key.is_none()
            {
                return Ok(Vec::new());
            }
            Ok(self
                .namespace_mutations
                .iter()
                .filter(|(key, _)| after_sort_key.is_none_or(|after| key.as_slice() > after))
                .take(limit)
                .map(|(key, mutation)| (key.clone(), mutation.clone()))
                .collect())
        }

        async fn seal_projection_commit(&mut self) -> Result<(), StateError> {
            self.sealed = true;
            Ok(())
        }

        async fn finish_projection_commit(&mut self) -> Result<(), StateError> {
            self.projection_items.clear();
            self.namespace_mutations.clear();
            self.projection_active = false;
            self.sealed = false;
            Ok(())
        }
    }

    struct OneCommitSource {
        commit: CommitRow,
        event: EventRow,
    }

    #[async_trait]
    impl StateProjectionSource for OneCommitSource {
        async fn commits(
            &self,
            repo_id: RepoId,
            from_repo_seq: i64,
            to_repo_seq: Option<i64>,
            _limit: usize,
        ) -> Result<Vec<CommitRow>, StateError> {
            if repo_id == self.commit.repo_id
                && from_repo_seq <= 1
                && to_repo_seq.is_none_or(|target| target >= 1)
            {
                Ok(vec![self.commit.clone()])
            } else {
                Ok(Vec::new())
            }
        }

        async fn events(
            &self,
            repo_id: RepoId,
            after: RepoEventCursor,
            _limit: usize,
        ) -> Result<Vec<CommitEventRow>, StateError> {
            if repo_id == self.event.repo_id
                && after.repo_seq == 1
                && after.commit_event_idx < self.event.commit_event_idx
            {
                Ok(vec![CommitEventRow::Item(self.event.clone())])
            } else {
                Ok(Vec::new())
            }
        }
    }

    fn one_create_commit() -> (RepoId, OneCommitSource) {
        let repo_id = RepoId::new();
        let item_id = ItemId::new();
        let proposal = EventProposal { set_link: None, merged_from: None,
            item_id: Some(item_id.clone()),
            event_type: "FILE::CREATE".to_string(),
            set_metadata: None,
            set_content: Some(crate::content::EventContent::bytes(crate::types::ObjectId::from_digest([7; 32]))),
            create_item: Some(CreateItem { key: None, link: None, derived_from: None,
                item_type: "FILE".to_string(),
                parent_folder_id: None,
                name: Some("report.txt".to_string()),
            }),
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: None,
        };
        let payload = canonical_event_proposal_bytes(&proposal).unwrap();
        let payload_digest = compute_sha256_digest_bytes(&payload);
        let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
            compute_sha256_digest_bytes(&[&[0u8][..], &payload_digest[..]].concat()),
        ))
        .unwrap();
        let author = "alice".to_string();
        let commit_bytes =
            canonical_commit_bytes_for_hash(&author, &author, None, None, 1, &events_digest)
                .unwrap();
        let commit_id = compute_commit_id(&commit_bytes);
        let created_at = Utc.timestamp_opt(1_688_169_600, 0).unwrap();
        let commit = CommitRow {
            repo_id: repo_id.clone(),
            commit_id: commit_id.clone(),
            repo_seq: Some(1),
            format_version: COMMIT_FORMAT_VERSION,
            author_id: author.clone(),
            committer_id: author,
            message: None,
            occurred_at: None,
            created_at,
            event_count: 1,
            events_digest,
        };
        let event = EventRow {
                metadata: proposal.set_metadata.as_ref().and_then(|value| value.payload()).cloned(),
            repo_id: repo_id.clone(),
            item_id,
            item_seq: 1,
            event_type: proposal.event_type,
            commit_id,
            commit_event_idx: 0,
            repo_seq: Some(1),
            payload,
            event_payload_digest: Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
                payload_digest,
            ))
            .unwrap(),
            created_at,
            parent_folder_id: None,
            parent_folder_id_before: None,
        };
        (repo_id, OneCommitSource { commit, event })
    }

    #[tokio::test]
    async fn projector_faults_never_publish_a_partial_commit_root() {
        for fault in [
            ProjectionFault::MissingItemHalf,
            ProjectionFault::MissingNamespaceHalf,
            ProjectionFault::TouchedItemCountMismatch,
        ] {
            let (repo_id, source) = one_create_commit();
            let mut store = FaultingProjectorStore::new(fault);
            let error = catch_up_state_index(&source, &repo_id, &mut store, None)
                .await
                .expect_err("faulted candidate must not publish");
            assert!(
                error.to_string().contains("candidate"),
                "unexpected error for {fault:?}: {error}"
            );
            assert_eq!(
                store
                    .projection_head()
                    .await
                    .unwrap()
                    .unwrap()
                    .applied_repo_seq,
                0,
                "{fault:?} advanced the projection head"
            );
            assert!(
                store.root_at(1).await.unwrap().is_none(),
                "{fault:?} published a commit root"
            );
        }
    }
}
