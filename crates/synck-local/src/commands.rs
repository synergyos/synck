use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions as StdOpenOptions;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::sync::{LazyLock, Mutex};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params, params_from_iter, types::Value as SqlValue};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use synck_file_folder::{
    BlobRef, CurrentItem, Intent, IntentProposal, ItemBase, Location, ProfileLifecycleState,
    SemanticError, SemanticErrorCode, validate_and_materialize, validate_projected_proposal,
};
use synck_vcs::naming::compute_name_key;
use synck_vcs::state::{
    ItemValue, NamespaceKey, RootHeader, StateIndexStore, StateRootId, StateTree,
};
use synck_vcs::types::{
    BlobId, COMMIT_EVENT_FAMILY_ITEM, COMMIT_EVENT_FAMILY_REF, CommitEventRow, CommitId,
    CommitProposal, CommitRow, EventContent, EventProposal, EventRow, ItemId, LifecycleState,
    RefEventProposal, RepoEventCursor, RepoId, Sha256Digest, SubtreeClosureGuard,
    canonical_commit_bytes_for_hash, canonical_event_proposal_bytes,
    canonical_ref_event_proposal_bytes, compute_commit_id, compute_sha256_digest_bytes,
    sha256_digest_to_digest_bytes, sha256_prefixed_hex_from_digest_bytes,
};
use uuid::Uuid;

use crate::comparison::compare_trees;
use crate::error::{IoContext, LocalError, Result};
use crate::event_profile::{
    LocalEvent, classify_complete_commit, classify_proposal_commit, commit_item_kind_requirements,
    is_reserved_root_name, proposal_item_kind_requirements,
};
use crate::objects::verify_object_file;
#[cfg(windows)]
use crate::repository::windows_extended_length_path_units;
use crate::repository::{
    Repository, is_cap_link_or_reparse, is_link_or_reparse, remove_tree_nofollow, rename_noreplace,
    sync_directory,
};
use crate::tree_materialization::{materialize_tree, validate_materialized_tree};
use crate::working_tree_edit::{WORKING_TREE_EDIT_INTENT_NAME, WORKING_TREE_EDIT_STAGE_NAME};
use crate::worktree::{Change, ChangeKind, CommittedNode, NodeKind, WorkingNode};

const LOG_DEFAULT_LIMIT: usize = 20;
const LOG_MAX_LIMIT: usize = 1_000;
const LOG_CHANGE_LIMIT: usize = 100;
const PAGE_SIZE: usize = 1_024;
const MAX_EVENT_PAGE_SIZE: usize = 10_000;

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum HistoricalWorkKind {
    StateStore,
    CommitDelta,
    ItemEventPage,
    ItemEventRow,
    PathItemRead,
    PathNamespaceRead,
}

#[cfg(test)]
static HISTORICAL_WORK: LazyLock<Mutex<Vec<(String, HistoricalWorkKind, usize)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

#[cfg(test)]
fn record_historical_work(repo_id: &RepoId, kind: HistoricalWorkKind, count: usize) {
    HISTORICAL_WORK
        .lock()
        .unwrap()
        .push((repo_id.to_string(), kind, count));
}
const MOVE_INTENT_VERSION: u32 = 2;
const MOVE_INTENT_NAME: &str = "move.json";
const RECOVER_INTENT_VERSION: u32 = 1;
const RECOVER_INTENT_NAME: &str = "recover.json";
const REVERT_INTENT_VERSION: u32 = 3;
const REVERT_INTENT_NAME: &str = "revert.json";
const REVERT_STAGE_NAME: &str = "revert-stage";
const WORKTREE_RESTORE_INTENT_VERSION: u32 = 1;
const WORKTREE_RESTORE_INTENT_NAME: &str = "worktree-restore.json";
const WORKTREE_RESTORE_STAGE_NAME: &str = "worktree-restore-stage";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MoveIntent {
    version: u32,
    operation_id: String,
    source: String,
    destination: String,
    item_id: ItemId,
    item_type: String,
    base_seq: i64,
    result_head_seq: i64,
    new_parent_id: Option<ItemId>,
    new_name: String,
    message: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecoverIntent {
    version: u32,
    operation_id: String,
    root_item_id: ItemId,
    deletion_commit_id: CommitId,
    message: String,
    items: Vec<RecoverIntentItem>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecoverIntentItem {
    item_id: ItemId,
    item_type: String,
    parent_id: Option<ItemId>,
    name: String,
    path: String,
    base_seq: i64,
    blob_id: Option<BlobId>,
    size: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RevertIntent {
    version: u32,
    operation_id: String,
    base_commit_id: CommitId,
    base_repo_seq: i64,
    publish_base_commit_id: CommitId,
    publish_base_repo_seq: i64,
    message: String,
    items: Vec<RevertIntentItem>,
    events: Vec<EventProposal>,
    subtree_guards: Vec<DurableSubtreeGuard>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DurableSubtreeGuard {
    root_item_id: ItemId,
    expected_non_tombstoned_descendants: Vec<(ItemId, i64)>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RevertIntentItem {
    item_id: ItemId,
    item_type: String,
    before: Option<RevertItemState>,
    after: RevertItemState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RevertItemState {
    parent_id: Option<ItemId>,
    name: String,
    path: String,
    live: bool,
    head_seq: i64,
    blob_id: Option<BlobId>,
    size: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorktreeRestoreIntent {
    version: u32,
    base_commit_id: Option<CommitId>,
    base_repo_seq: i64,
    source_tree: Vec<WorktreeRestoreEntry>,
    items: Vec<RevertIntentItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorktreeRestoreEntry {
    path: String,
    item_id: Option<ItemId>,
    item_type: String,
    blob_id: Option<BlobId>,
    size: Option<i64>,
}

#[derive(Debug)]
pub enum CommitOutcome {
    Clean,
    Committed(String),
}

#[derive(Debug)]
pub struct LogEntry {
    pub repo_seq: i64,
    pub commit_id: String,
    pub created_at: DateTime<Utc>,
    pub author: String,
    pub message: Option<String>,
    pub event_count: usize,
    pub events_digest: String,
}

#[derive(Debug)]
pub struct DetailedLogEntry {
    pub commit: LogEntry,
    pub changes: Vec<Change>,
    pub changes_total: usize,
}

#[derive(Debug)]
pub struct CommitEvent {
    pub repo_id: String,
    pub commit_id: String,
    pub repo_seq: i64,
    pub commit_event_idx: i32,
    pub item_id: String,
    pub display_item_id: String,
    pub item_type: String,
    pub item_name: String,
    pub item_name_before: Option<String>,
    pub item_seq: i64,
    pub event_type: String,
    pub payload: String,
    pub event_payload_digest: String,
    pub created_at: DateTime<Utc>,
    pub parent_folder_id: Option<String>,
    pub parent_folder_name: Option<String>,
    pub parent_folder_id_before: Option<String>,
    pub parent_folder_name_before: Option<String>,
    pub display_parent_folder_id: Option<String>,
    pub display_parent_folder_id_before: Option<String>,
}

#[derive(Debug)]
pub struct CommitEventPage {
    pub events: Vec<CommitEvent>,
    pub total_event_count: usize,
    pub next_after_commit_event_idx: Option<i32>,
}

#[derive(Debug)]
pub struct DetailedEventLogEntry {
    pub entry: DetailedLogEntry,
    pub events: Vec<CommitEvent>,
}

#[derive(Debug)]
pub struct VersionEntry {
    pub item_seq: i64,
    pub commit_id: String,
    pub created_at: DateTime<Utc>,
    pub blob_id: String,
    pub size: i64,
}

#[derive(Debug)]
pub struct HistoryCommit {
    pub repo_seq: i64,
    pub commit_id: String,
    pub created_at: DateTime<Utc>,
    pub author: String,
    pub message: Option<String>,
    pub changes: Vec<Change>,
    pub event_count: usize,
    pub events_digest: String,
}

#[derive(Debug)]
pub struct EventHistoryCommit {
    pub commit: HistoryCommit,
    pub events: Vec<CommitEvent>,
}

pub(crate) struct HistoricalTreeDelta {
    pub from_commit: CommitRow,
    pub from_tree: BTreeMap<String, CommittedNode>,
    pub to_commit: CommitRow,
    pub to_tree: BTreeMap<String, CommittedNode>,
    pub changed_item_ids: BTreeSet<ItemId>,
}

#[derive(Debug)]
pub struct DeletedEntry {
    pub item_id: String,
    pub item_type: String,
    pub path: String,
    pub repo_seq: i64,
    pub commit_id: String,
    pub deleted_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
struct DeletionInfo {
    repo_seq: i64,
    commit_id: CommitId,
    deleted_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
struct HistoryNode {
    parent_id: Option<ItemId>,
    name: String,
    kind: NodeKind,
    live: bool,
    head_seq: i64,
    blob_id: Option<BlobId>,
    size: Option<i64>,
    deletion: Option<DeletionInfo>,
}

#[derive(Debug)]
struct ReplayAccumulator {
    commit: CommitRow,
    events: Vec<CommitEventRow>,
}

struct ReplayResult {
    nodes: HashMap<ItemId, HistoryNode>,
}

struct PreparedWorkingCommit {
    objects: HashMap<String, BlobId>,
    created_ids: HashMap<String, ItemId>,
    events: Vec<EventProposal>,
    ref_events: Vec<RefEventProposal>,
    subtree_guards: Vec<SubtreeClosureGuard>,
    email_objects: Vec<(String, Vec<u8>)>,
    email_sources: Vec<(String, ItemId, String)>,
}

impl Repository {
    pub async fn status(&self) -> Result<Vec<Change>> {
        let (_, _, changes, _) = self.changes().await?;
        Ok(changes)
    }

    /// Status plus Windows interoperability warnings, from the same observation.
    pub async fn status_with_path_warnings(&self) -> Result<(Vec<Change>, Vec<PathBuf>)> {
        let (_, working, changes, _) = self.changes().await?;
        let warnings = crate::path_warnings::long_paths(
            self.root_path(), working.keys().map(String::as_str),
        );
        Ok((changes, warnings))
    }

    pub async fn commit(&self, message: &str) -> Result<CommitOutcome> {
        self.commit_selected(self.root_path(), &[], Some(message))
            .await
    }

    pub async fn commit_selected(
        &self,
        caller: &Path,
        inputs: &[PathBuf],
        message: Option<&str>,
    ) -> Result<CommitOutcome> {
        let options = crate::portable::WriteOptions {
            message: message.map(str::to_string),
            ..Default::default()
        };
        self.commit_selected_with(caller, inputs, &options).await
    }

    /// Selected commit with direct-writer attribution; the configured author
    /// remains the committer.
    pub(crate) async fn commit_selected_with(
        &self,
        caller: &Path,
        inputs: &[PathBuf],
        options: &crate::portable::WriteOptions,
    ) -> Result<CommitOutcome> {
        let message = options.message.as_deref();
        validate_commit_message(message)?;
        let (committed, working, all_changes, working_ids) = self.changes().await?;
        let changes = select_changes(
            self.root_path(),
            caller,
            inputs,
            &committed,
            &working,
            &working_ids,
            &all_changes,
        )?;
        if changes.is_empty() {
            return Ok(CommitOutcome::Clean);
        }
        let prepared =
            self.prepare_working_commit(&committed, &working, &changes, &working_ids, message)?;
        let captured = self.capture_changed_files(&working, &changes)?;
        if captured != prepared.objects {
            return Err(LocalError::Integrity(
                "captured FILE objects differ from the validated commit".to_string(),
            ));
        }
        self.store_imported_email_objects(&prepared.email_objects)?;
        self.record_imported_email_sources(&prepared.email_sources)?;
        let accepted = self
            .core()
            .append_file_folder_checked_with_refs(
                self.config().repo_id().clone(),
                CommitProposal {
                    author_id: options
                        .author
                        .clone()
                        .unwrap_or_else(|| self.config().author().to_string()),
                    committer_id: self.config().author().to_string(),
                    message: message.map(str::to_string),
                    occurred_at: options.occurred_at,
                    operation_id: Some(
                        options
                            .operation_id
                            .as_ref()
                            .map(crate::portable::stored_operation_id)
                            .unwrap_or_else(|| Uuid::new_v4().to_string()),
                    ),
                    events: prepared.events,
                },
                prepared.ref_events,
                &[],
                &prepared.subtree_guards,
            )
            .await?;
        let commit_id = accepted.commit_id.to_string();
        if let Err(error) = self.recover_current().await {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: error.to_string(),
            });
        }
        if let Err(error) = self.publish_committed_identities(
            &committed,
            &working,
            &changes,
            &working_ids,
            &prepared.created_ids,
        ) {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: error.to_string(),
            });
        }
        Ok(CommitOutcome::Committed(commit_id))
    }

    fn prepare_working_commit(
        &self,
        committed: &BTreeMap<String, CommittedNode>,
        working: &BTreeMap<String, WorkingNode>,
        changes: &[Change],
        working_ids: &HashMap<String, ItemId>,
        message: Option<&str>,
    ) -> Result<PreparedWorkingCommit> {
        for change in changes {
            if change.kind != ChangeKind::Add
                && change.kind != ChangeKind::Delete
                && working[&change.path].kind == NodeKind::File
            {
                crate::email_import::reject_file_email_update(path_name(&change.path))?;
                if change.content_changed {
                    crate::email_import::reject_file_email_update(path_name(
                        change.source_path.as_deref().unwrap_or(&change.path),
                    ))?;
                }
            }
        }
        let objects = expected_file_objects(working, changes)?;
        let built_intents =
            build_intents(committed, working, changes, working_ids, &objects, None)?;
        let mut intents = built_intents.intents;
        let current = self.current_file_folder_context(&mut intents)?;
        let built = materialize_events(self.config().repo_id(), message, intents, &current)?;
        let mut prepared = PreparedWorkingCommit {
            objects,
            created_ids: built_intents.created_ids,
            events: built.events,
            ref_events: built.ref_events,
            subtree_guards: built.subtree_guards,
            email_objects: Vec::new(),
            email_sources: Vec::new(),
        };
        let mut emails = HashMap::new();
        for change in changes
            .iter()
            .filter(|change| change.kind == ChangeKind::Add)
        {
            let filename = path_name(&change.path);
            if working[&change.path].kind != NodeKind::File
                || synck_items::imported_email::ImportedEmailFormat::from_name(filename).is_none()
            {
                continue;
            }
            let id = &prepared.created_ids[&change.path];
            let create = prepared
                .events
                .iter()
                .find(|event| event.item_id.as_ref() == Some(id) && event.create_item.is_some())
                .and_then(|event| event.create_item.as_ref())
                .expect("materialized FILE creation");
            let email = crate::email_import::prepare(
                id,
                create.parent_folder_id.clone(),
                filename,
                filename,
                &self.root_path().join(display_to_path(&change.path)),
            )?;
            self.reject_recorded_email_name(&change.path, &email.events[0])?;
            let source_blob = email.events[0]
                .set_metadata
                .as_ref()
                .expect("EMAIL metadata")["mailcal"]["source"]["blob_id"]
                .as_str()
                .expect("EMAIL source blob");
            if source_blob != prepared.objects[&change.path].as_str() {
                return Err(LocalError::Invalid(format!(
                    "email source changed while parsing: {}",
                    change.path
                )));
            }
            prepared
                .email_sources
                .push((change.path.clone(), id.clone(), source_blob.to_string()));
            prepared.email_objects.extend(email.objects);
            emails.insert(id.clone(), email.events);
        }
        if !emails.is_empty() {
            let mut events = Vec::new();
            for event in prepared.events {
                if let Some(id) = event.item_id.as_ref() {
                    if let Some(email_events) = emails.get_mut(id) {
                        if event.create_item.is_some() {
                            events.append(email_events);
                        }
                        continue;
                    }
                }
                events.push(event);
            }
            prepared.events = events;
            prepared
                .ref_events
                .retain(|event| !emails.contains_key(&event.owner_item_id));
        }
        Ok(prepared)
    }

    /// A source edited after it was recorded parses as a new email with the
    /// same derived name. Say what to do instead of reporting a raw conflict.
    fn reject_recorded_email_name(&self, path: &str, create: &EventProposal) -> Result<()> {
        let Some(item) = create.create_item.as_ref() else {
            return Ok(());
        };
        let name_key = synck_vcs::naming::compute_name_key(&item.name)
            .map_err(|error| LocalError::Invalid(error.to_string()))?;
        let parent = item
            .parent_folder_id
            .as_ref()
            .map(|id| id.as_str().to_string());
        let connection = self
            .storage()
            .open_readonly()
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        let existing: Option<String> = connection
            .query_row(
                "SELECT item_type FROM vcs_items WHERE repo_id = ?1 AND parent_folder_id IS ?2 \
                 AND name_key = ?3 AND lifecycle_state = 'LIVE'",
                params![self.config().repo_id().as_str(), parent, name_key],
                |row| row.get(0),
            )
            .optional()?;
        match existing.as_deref() {
            Some("EMAIL") => Err(LocalError::Invalid(format!(
                "{path}: an email named \"{}\" is already recorded in this folder; a recorded email never changes, so save the changed message under a new file name or put the original file back",
                item.name
            ))),
            Some(_) => Err(LocalError::Invalid(format!(
                "{path}: this folder already has an item named \"{}\"; rename the message file before committing it",
                item.name
            ))),
            None => Ok(()),
        }
    }

    pub async fn revert(&self, revision: &str, message: &str) -> Result<String> {
        if message.is_empty() {
            return Err(LocalError::Invalid(
                "revert message must not be empty".to_string(),
            ));
        }
        let (_, _, changes, _) = self.changes().await?;
        if !changes.is_empty() {
            return Err(LocalError::Invalid(
                "synck revert requires a clean working tree".to_string(),
            ));
        }
        let head =
            self.log(Some(1)).await?.into_iter().next().ok_or_else(|| {
                LocalError::Invalid("repository has no commit to revert".to_string())
            })?;
        let publish_base_commit_id = CommitId::parse(&head.commit_id).map_err(|error| {
            LocalError::Integrity(format!("repository HEAD has invalid ID: {error}"))
        })?;
        let selected = self.resolve_revision(revision).await?;
        let base_commit_id = selected.commit_id.clone();
        let base_repo_seq = selected.repo_seq.ok_or_else(|| {
            LocalError::Integrity(format!(
                "commit {base_commit_id} has no repository sequence"
            ))
        })?;
        self.catch_up_state_index().await?;
        let before = self.historical_nodes_before(base_repo_seq).await?;
        let after = self
            .historical_nodes_at_commit(base_repo_seq, &base_commit_id)
            .await?;
        if self.checkout_paths().is_some() {
            let events = self
                .core()
                .list_commit_events(
                    self.repository_id().clone(),
                    base_commit_id.clone(),
                    usize::try_from(selected.event_count)
                        .map_err(|_| LocalError::Integrity("negative event count".into()))?
                        + 1,
                )
                .await?;
            if events.iter().any(|event| match event {
                CommitEventRow::Item(event) => {
                    !before.contains_key(&event.item_id) && !after.contains_key(&event.item_id)
                }
                CommitEventRow::Ref(event) => {
                    !(event.event_type == "FILE_VERSION::CREATE"
                        && after.contains_key(&event.owner_item_id)
                        && !before.contains_key(&event.owner_item_id))
                }
            }) {
                return Err(LocalError::Invalid("revert requires a filesystem-only commit; the selected commit also changes non-filesystem history".into()));
            }
        }
        let current = self.replay_history(Some(head.repo_seq)).await?.nodes;
        let intent = build_revert_intent(
            self.config().repo_id(),
            (&base_commit_id, base_repo_seq),
            (&publish_base_commit_id, head.repo_seq),
            message,
            &before,
            &after,
            &current,
        )?;
        if self.config().reserves_file_version_namespace() {
            reject_reserved_revert_items(&intent.items)?;
        }
        for item in &intent.items {
            if !self.checkout_observes(&item.after.path)
                || item
                    .before
                    .as_ref()
                    .is_some_and(|before| !self.checkout_observes(&before.path))
            {
                return Err(LocalError::Invalid(
                    "revert crosses the checkout selection; widen it first".into(),
                ));
            }
        }
        self.validate_inverse_objects(&intent.items)?;
        self.validate_inverse_destinations(&intent.items)?;
        self.write_revert_intent(&intent)?;
        self.complete_revert_intent(&intent, true).await
    }

    pub(crate) async fn recover_pending_revert(&self) -> Result<()> {
        let Some(intent) = self.read_revert_intent()? else {
            return Ok(());
        };
        self.complete_revert_intent(&intent, false)
            .await
            .map(|_| ())
    }

    async fn complete_revert_intent(
        &self,
        intent: &RevertIntent,
        objects_validated: bool,
    ) -> Result<String> {
        validate_revert_fixed_fields(intent)?;
        if self.config().reserves_file_version_namespace() {
            reject_reserved_revert_items(&intent.items)
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
        }
        self.recover().await?;
        let before = self.historical_nodes_before(intent.base_repo_seq).await?;
        let after = self
            .historical_nodes_at_commit(intent.base_repo_seq, &intent.base_commit_id)
            .await?;
        let publish_base = self
            .replay_history(Some(intent.publish_base_repo_seq))
            .await?
            .nodes;
        validate_revert_intent(
            self.config().repo_id(),
            intent,
            &before,
            &after,
            &publish_base,
        )?;
        let expected_commit_id = expected_revert_commit_id(intent, self.config().author())?;

        let operation_commit = self.operation_commit_id(&intent.operation_id)?;
        let latest =
            self.log(Some(1)).await?.into_iter().next().ok_or_else(|| {
                LocalError::Integrity("revert base commit is missing".to_string())
            })?;
        let commit_id = match operation_commit {
            None => {
                if latest.repo_seq != intent.publish_base_repo_seq
                    || latest.commit_id != intent.publish_base_commit_id.to_string()
                {
                    return Err(LocalError::Integrity(
                        "revert publication base is no longer repository HEAD".to_string(),
                    ));
                }
                if path_exists_nofollow(&self.metadata_path().join(REVERT_STAGE_NAME))? {
                    return Err(LocalError::Integrity(
                        "unaccepted revert intent has staging state".to_string(),
                    ));
                }
                let (_, _, changes, _) = self.changes().await?;
                if !changes.is_empty() {
                    return Err(LocalError::Integrity(
                        "working tree changed after the revert intent was written".to_string(),
                    ));
                }
                if !objects_validated {
                    self.validate_inverse_objects(&intent.items)?;
                }
                self.validate_inverse_destinations(&intent.items)?;
                self.append_revert_intent(intent)
                    .await?
                    .commit_id
                    .to_string()
            }
            Some(commit_id) => {
                if commit_id != expected_commit_id.to_string() {
                    return Err(LocalError::Integrity(
                        "accepted revert commit differs from its durable intent".to_string(),
                    ));
                }
                if latest.commit_id != commit_id {
                    return Err(LocalError::Integrity(
                        "accepted revert is not repository HEAD".to_string(),
                    ));
                }
                commit_id
            }
        };

        if let Err(error) = self.recover().await {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: error.to_string(),
            });
        }
        let current_commit =
            CommitId::parse(&commit_id).map_err(|error| LocalError::AcceptedNeedsRecovery {
                commit_id: commit_id.clone(),
                detail: format!("accepted revert has invalid commit ID: {error}"),
            })?;
        let current_repo_seq = self
            .core()
            .get_commit(self.config().repo_id().clone(), current_commit.clone())
            .await
            .map_err(|error| LocalError::AcceptedNeedsRecovery {
                commit_id: commit_id.clone(),
                detail: error.to_string(),
            })?
            .and_then(|commit| commit.repo_seq)
            .ok_or_else(|| LocalError::AcceptedNeedsRecovery {
                commit_id: commit_id.clone(),
                detail: "accepted revert has no repository sequence".to_string(),
            })?;
        let current = self
            .historical_nodes_at_commit(current_repo_seq, &current_commit)
            .await
            .map_err(|error| LocalError::AcceptedNeedsRecovery {
                commit_id: commit_id.clone(),
                detail: error.to_string(),
            })?;
        validate_reverted_nodes(intent, &publish_base, &current).map_err(|error| {
            LocalError::AcceptedNeedsRecovery {
                commit_id: commit_id.clone(),
                detail: error.to_string(),
            }
        })?;

        let materialized = async {
            self.materialize_inverse_items(&intent.items, REVERT_STAGE_NAME)?;
            let (_, _, changes, _) = self.changes().await?;
            if !changes.is_empty() {
                return Err(LocalError::Integrity(
                    "reverted working tree does not match accepted history".to_string(),
                ));
            }
            self.refresh_worktree_identities().await?;
            self.remove_inverse_stage(REVERT_STAGE_NAME)?;
            self.clear_revert_intent()
        }
        .await;
        if let Err(error) = materialized {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: error.to_string(),
            });
        }
        Ok(commit_id)
    }

    async fn append_revert_intent(
        &self,
        intent: &RevertIntent,
    ) -> Result<synck_vcs::types::AcceptedCommit> {
        let subtree_guards = intent
            .subtree_guards
            .iter()
            .map(|guard| SubtreeClosureGuard {
                root_item_id: guard.root_item_id.clone(),
                expected_non_tombstoned_descendants: guard
                    .expected_non_tombstoned_descendants
                    .clone(),
                require_empty_live_descendants: false,
            })
            .collect::<Vec<_>>();
        self.core()
            .append_file_folder_checked(
                self.config().repo_id().clone(),
                CommitProposal {
                    author_id: self.config().author().to_string(),
                    committer_id: self.config().author().to_string(),
                    message: Some(intent.message.clone()),
                    occurred_at: None,
                    operation_id: Some(intent.operation_id.clone()),
                    events: intent.events.clone(),
                },
                &[],
                &subtree_guards,
            )
            .await
            .map_err(Into::into)
    }

    fn validate_inverse_objects(&self, items: &[RevertIntentItem]) -> Result<()> {
        for item in items {
            let Some(before) = item.before.as_ref().filter(|state| state.live) else {
                continue;
            };
            if item.item_type != "FILE" {
                continue;
            }
            let blob_id = before
                .blob_id
                .as_ref()
                .expect("validated reverted FILE has a blob");
            let size = u64::try_from(before.size.expect("validated reverted FILE has a size"))
                .map_err(|_| {
                    LocalError::Integrity(format!("negative object size for {blob_id}"))
                })?;
            self.verify_blob(blob_id, size)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn validate_revert_objects(&self, intent: &RevertIntent) -> Result<()> {
        self.validate_inverse_objects(&intent.items)
    }

    fn validate_inverse_destinations(&self, items: &[RevertIntentItem]) -> Result<()> {
        let sources: HashSet<&str> = items
            .iter()
            .filter(|item| item.after.live)
            .map(|item| item.after.path.as_str())
            .collect();
        let mut targets: BTreeMap<Option<String>, Vec<(&str, &str)>> = BTreeMap::new();
        for item in items {
            let Some(before) = item.before.as_ref().filter(|state| state.live) else {
                continue;
            };
            let (parent, name) = split_display_path(&before.path);
            targets
                .entry(parent.map(str::to_string))
                .or_default()
                .push((name, &before.path));
        }
        for (parent, desired) in targets {
            let parent_path = parent.as_deref().map_or_else(
                || self.root_path().to_path_buf(),
                |parent| self.root_path().join(display_to_path(parent)),
            );
            let metadata = match std::fs::symlink_metadata(&parent_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(LocalError::Io {
                        action: "inspect revert target parent",
                        path: parent_path,
                        source,
                    });
                }
            };
            if !metadata.file_type().is_dir() || is_link_or_reparse(&metadata) {
                return Err(LocalError::Invalid(format!(
                    "revert target parent is not a directory: {}",
                    desired[0].1
                )));
            }
            let mut target_keys = HashSet::new();
            for (name, path) in desired {
                let key = compute_name_key(name).map_err(|error| {
                    LocalError::Integrity(format!("invalid revert target name {name:?}: {error}"))
                })?;
                if !target_keys.insert(key) {
                    return Err(LocalError::Integrity(format!(
                        "revert target {path} duplicates a portable name below {}",
                        parent.as_deref().unwrap_or("."),
                    )));
                }
            }
            for entry in
                std::fs::read_dir(&parent_path).at("read revert target parent", &parent_path)?
            {
                let entry = entry.at("read revert target parent", &parent_path)?;
                let existing_name = entry.file_name();
                let existing_name = existing_name.to_str().ok_or_else(|| {
                    LocalError::Invalid(format!(
                        "non-Unicode entry conflicts with revert target parent: {}",
                        parent.as_deref().unwrap_or(".")
                    ))
                })?;
                let existing_key = compute_name_key(existing_name).map_err(|error| {
                    LocalError::Invalid(format!(
                        "invalid filesystem name {existing_name:?} beside revert target: {error}"
                    ))
                })?;
                if !target_keys.contains(&existing_key) {
                    continue;
                }
                let existing = parent.as_deref().map_or_else(
                    || existing_name.to_string(),
                    |parent| format!("{parent}/{existing_name}"),
                );
                if !sources.contains(existing.as_str()) {
                    return Err(LocalError::Invalid(format!(
                        "revert target conflicts with filesystem entry: {existing}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn materialize_inverse_items(
        &self,
        items: &[RevertIntentItem],
        stage_name: &str,
    ) -> Result<()> {
        let stage = self.ensure_inverse_stage(stage_name)?;
        validate_inverse_stage_entries(&stage, items)?;

        let mut current = items
            .iter()
            .filter(|item| item.after.live)
            .collect::<Vec<_>>();
        current.sort_by(|left, right| {
            path_depth(&right.after.path)
                .cmp(&path_depth(&left.after.path))
                .then_with(|| left.after.path.cmp(&right.after.path))
                .then_with(|| left.item_id.cmp(&right.item_id))
        });
        for item in current {
            self.stage_revert_item(&stage, item)?;
        }

        let mut folders = items
            .iter()
            .filter(|item| {
                item.item_type == "FOLDER" && item.before.as_ref().is_some_and(|state| state.live)
            })
            .collect::<Vec<_>>();
        folders.sort_by(|left, right| {
            let left = left.before.as_ref().expect("filtered revert folder");
            let right = right.before.as_ref().expect("filtered revert folder");
            path_depth(&left.path)
                .cmp(&path_depth(&right.path))
                .then_with(|| left.path.cmp(&right.path))
        });
        for item in folders {
            self.publish_revert_folder(&stage, item)?;
        }

        let mut files = items
            .iter()
            .filter(|item| {
                item.item_type == "FILE" && item.before.as_ref().is_some_and(|state| state.live)
            })
            .collect::<Vec<_>>();
        files.sort_by(|left, right| {
            left.before
                .as_ref()
                .expect("filtered revert file")
                .path
                .cmp(&right.before.as_ref().expect("filtered revert file").path)
        });
        for item in files {
            self.publish_revert_file(&stage, item)?;
        }
        Ok(())
    }

    fn ensure_inverse_stage(&self, stage_name: &str) -> Result<PathBuf> {
        let stage = self.metadata_path().join(stage_name);
        match std::fs::symlink_metadata(&stage) {
            Ok(metadata) if metadata.file_type().is_dir() && !is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(LocalError::Integrity(
                    "inverse staging is not a real directory".to_string(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&stage).at("create inverse staging", &stage)?;
                sync_directory(self.metadata_path())?;
            }
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect inverse staging",
                    path: stage,
                    source,
                });
            }
        }
        Ok(stage)
    }

    #[cfg(test)]
    fn ensure_revert_stage(&self) -> Result<PathBuf> {
        self.ensure_inverse_stage(REVERT_STAGE_NAME)
    }

    fn stage_revert_item(&self, stage: &Path, item: &RevertIntentItem) -> Result<()> {
        let staged = stage.join(item.item_id.as_str());
        if entry_matches_revert_state(&staged, &item.item_type, &item.after)? {
            return Ok(());
        }
        if path_exists_nofollow(&staged)? {
            return Err(LocalError::Integrity(format!(
                "revert staging entry contradicts item {}",
                item.item_id
            )));
        }

        let source = self.root_path().join(display_to_path(&item.after.path));
        if entry_matches_revert_state(&source, &item.item_type, &item.after)? {
            rename_noreplace(&source, &staged).at("stage reverted item", &source)?;
            sync_display_parent(self.root_path(), &item.after.path)?;
            sync_directory(stage)?;
            return Ok(());
        }
        if path_exists_nofollow(&source)? {
            if let Some(before) = item
                .before
                .as_ref()
                .filter(|before| before.live && before.path == item.after.path)
                && entry_matches_revert_state(&source, &item.item_type, before)?
            {
                return Ok(());
            }
            return Err(LocalError::Integrity(format!(
                "revert source contradicts item {} at {}",
                item.item_id, item.after.path
            )));
        }

        if let Some(before) = item.before.as_ref().filter(|state| state.live) {
            let target = self.root_path().join(display_to_path(&before.path));
            if entry_matches_revert_state(&target, &item.item_type, before)? {
                return Ok(());
            }
            return Err(LocalError::Integrity(format!(
                "revert item {} is absent from source, staging, and target",
                item.item_id
            )));
        }
        Ok(())
    }

    fn publish_revert_folder(&self, stage: &Path, item: &RevertIntentItem) -> Result<()> {
        let before = item.before.as_ref().expect("filtered revert folder");
        let target = self.root_path().join(display_to_path(&before.path));
        if entry_matches_revert_state(&target, "FOLDER", before)? {
            return Ok(());
        }
        if path_exists_nofollow(&target)? {
            return Err(LocalError::Integrity(format!(
                "revert folder target contradicts {}",
                before.path
            )));
        }
        let staged = stage.join(item.item_id.as_str());
        if entry_matches_revert_state(&staged, "FOLDER", &item.after)? {
            rename_noreplace(&staged, &target).at("publish reverted folder", &target)?;
            sync_directory(stage)?;
            sync_display_parent(self.root_path(), &before.path)
        } else {
            self.materialize_recovered_folder(&before.path)
        }
    }

    fn publish_revert_file(&self, stage: &Path, item: &RevertIntentItem) -> Result<()> {
        let before = item.before.as_ref().expect("filtered revert file");
        let target = self.root_path().join(display_to_path(&before.path));
        if entry_matches_revert_state(&target, "FILE", before)? {
            return Ok(());
        }
        if path_exists_nofollow(&target)? {
            return Err(LocalError::Integrity(format!(
                "revert file target contradicts {}",
                before.path
            )));
        }
        let staged = stage.join(item.item_id.as_str());
        if entry_matches_revert_state(&staged, "FILE", before)? {
            rename_noreplace(&staged, &target).at("publish reverted file", &target)?;
            sync_directory(stage)?;
            sync_display_parent(self.root_path(), &before.path)
        } else {
            self.write_working_file(
                &before.path,
                before
                    .blob_id
                    .as_ref()
                    .expect("validated reverted FILE has a blob"),
                before.size.expect("validated reverted FILE has a size"),
                false,
            )
        }
    }

    fn remove_inverse_stage(&self, stage_name: &str) -> Result<()> {
        let stage = self.metadata_path().join(stage_name);
        match std::fs::symlink_metadata(&stage) {
            Ok(_) => {
                remove_tree_nofollow(&stage)?;
                sync_directory(self.metadata_path())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(LocalError::Io {
                action: "inspect inverse staging",
                path: stage,
                source,
            }),
        }
    }

    pub async fn move_path(
        &self,
        caller: &Path,
        source_input: &Path,
        destination_input: &Path,
        message: &str,
    ) -> Result<String> {
        if message.is_empty() {
            return Err(LocalError::Invalid(
                "move message must not be empty".to_string(),
            ));
        }
        let (committed, working, changes, _) = self.changes().await?;
        if !changes.is_empty() {
            return Err(LocalError::Invalid(
                "synck mv requires a clean working tree".to_string(),
            ));
        }
        let source = normalize_input_path(self.root_path(), caller, source_input)?;
        let destination = normalize_input_path(self.root_path(), caller, destination_input)?;
        if self.config().reserves_file_version_namespace() {
            reject_reserved_root_path(&destination)?;
        }
        if source == destination {
            return Err(LocalError::Invalid(
                "move source and destination are the same".to_string(),
            ));
        }
        if let Some(paths) = self.checkout_paths() {
            if !crate::checkout::selected(paths, &destination)
                || (!paths.is_empty()
                    && !paths.iter().any(|root| {
                        source == *root
                            || source
                                .strip_prefix(root)
                                .is_some_and(|p| p.starts_with('/'))
                    }))
            {
                return Err(LocalError::Invalid(
                    "move crosses a sparse checkout boundary; widen selection first".into(),
                ));
            }
        }
        let node = committed.get(&source).ok_or_else(|| {
            LocalError::Invalid(format!("move source is not a tracked live item: {source}"))
        })?;
        if !working.contains_key(&source) {
            return Err(LocalError::Invalid(format!(
                "move source is missing from the working tree: {source}"
            )));
        }
        let destination_path = self.root_path().join(display_to_path(&destination));
        match std::fs::symlink_metadata(&destination_path) {
            Ok(_) => {
                return Err(LocalError::Invalid(format!(
                    "move destination already exists: {destination}"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect move destination",
                    path: destination_path,
                    source,
                });
            }
        }
        let (destination_parent, new_name) = split_display_path(&destination);
        if node.kind == NodeKind::File {
            crate::email_import::reject_file_email_update(new_name)?;
        }
        if node.kind == NodeKind::Folder
            && destination_parent
                .is_some_and(|parent| parent == source || parent.starts_with(&format!("{source}/")))
        {
            return Err(LocalError::Invalid(
                "cannot move a folder below itself".to_string(),
            ));
        }
        let new_parent_id = destination_parent
            .map(|parent| {
                let parent_node = committed.get(parent).ok_or_else(|| {
                    LocalError::Invalid(format!(
                        "move destination parent is not a tracked live folder: {parent}"
                    ))
                })?;
                if parent_node.kind != NodeKind::Folder {
                    return Err(LocalError::Invalid(format!(
                        "move destination parent is not a folder: {parent}"
                    )));
                }
                Ok(parent_node.item_id.clone())
            })
            .transpose()?;
        reject_destination_name_conflict(&committed, &node.item_id, destination_parent, new_name)?;
        let old_parent_id = split_display_path(&source)
            .0
            .map(|parent| committed[parent].item_id.clone());
        let event_count = i64::from(old_parent_id != new_parent_id)
            + i64::from(split_display_path(&source).1 != new_name);
        let result_head_seq = node
            .head_seq
            .checked_add(event_count)
            .ok_or_else(|| LocalError::Invalid("move item sequence would overflow".to_string()))?;

        let intent = MoveIntent {
            version: MOVE_INTENT_VERSION,
            operation_id: Uuid::new_v4().to_string(),
            source: source.clone(),
            destination: destination.clone(),
            item_id: node.item_id.clone(),
            item_type: node_kind_name(node.kind).to_string(),
            base_seq: node.head_seq,
            result_head_seq,
            new_parent_id,
            new_name: new_name.to_string(),
            message: message.to_string(),
        };
        self.write_move_intent(&intent)?;

        let source_path = self.root_path().join(display_to_path(&source));
        if let Err(error) =
            rename_noreplace(&source_path, &destination_path).at("move", &destination_path)
        {
            let _ = self.clear_move_intent();
            return Err(error);
        }
        if let Err(error) = sync_move_parents(self.root_path(), &source, &destination) {
            return Err(move_pending_error(&intent, error));
        }

        self.complete_move_intent(&intent).await
    }

    pub(crate) async fn recover_pending_move(&self) -> Result<()> {
        let Some(intent) = self.read_move_intent()? else {
            return Ok(());
        };
        if self.config().reserves_file_version_namespace() {
            reject_reserved_root_path(&intent.destination)
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
        }
        self.recover().await?;
        let committed = self.committed_tree().await?;
        validate_move_intent(self.root_path(), &committed, &intent)?;

        let source_node = committed
            .get(&intent.source)
            .filter(|node| node.item_id == intent.item_id);
        let destination_node = committed
            .get(&intent.destination)
            .filter(|node| node.item_id == intent.item_id);
        let source_kind = filesystem_node_kind(self.root_path(), &intent.source)?;
        let destination_kind = filesystem_node_kind(self.root_path(), &intent.destination)?;
        let expected_kind = parse_node_kind(&intent.item_type)?;

        match (source_node, destination_node, source_kind, destination_kind) {
            (Some(node), None, Some(source_kind), None)
                if source_kind == expected_kind
                    && node.kind == expected_kind
                    && node.head_seq == intent.base_seq =>
            {
                self.clear_move_intent()
            }
            (Some(node), None, None, Some(destination_kind))
                if destination_kind == expected_kind
                    && node.kind == expected_kind
                    && node.head_seq == intent.base_seq =>
            {
                sync_move_parents(self.root_path(), &intent.source, &intent.destination)
                    .map_err(|error| move_pending_error(&intent, error))?;
                self.complete_move_intent(&intent).await.map(|_| ())
            }
            (None, Some(node), None, Some(destination_kind))
                if destination_kind == expected_kind
                    && node.kind == expected_kind
                    && node.head_seq == intent.result_head_seq =>
            {
                self.clear_move_intent()
            }
            _ => Err(LocalError::Integrity(format!(
                "move intent contradicts repository or filesystem state: {} -> {}",
                intent.source, intent.destination
            ))),
        }
    }

    async fn append_move_intent(
        &self,
        intent: &MoveIntent,
    ) -> Result<synck_vcs::types::AcceptedCommit> {
        let location = Location {
            parent_item_id: intent.new_parent_id.clone(),
            name: intent.new_name.clone(),
        };
        let profile_intent = match intent.item_type.as_str() {
            "FILE" => Intent::UpdateFile {
                item_id: intent.item_id.clone(),
                base_seq: intent.base_seq,
                location: Some(location),
                content: None,
            },
            "FOLDER" => Intent::UpdateFolder {
                item_id: intent.item_id.clone(),
                base_seq: intent.base_seq,
                location,
            },
            other => {
                return Err(LocalError::Integrity(format!(
                    "move intent has unsupported item type {other}"
                )));
            }
        };
        let mut intents = vec![profile_intent];
        let current = self.current_file_folder_context(&mut intents)?;
        let materialized = validate_and_materialize(
            self.config().repo_id(),
            &IntentProposal {
                message: Some(intent.message.clone()),
                intents,
            },
            &current,
        )?;
        self.core()
            .append_file_folder_checked(
                self.config().repo_id().clone(),
                CommitProposal {
                    author_id: self.config().author().to_string(),
                    committer_id: self.config().author().to_string(),
                    message: Some(intent.message.clone()),
                    occurred_at: None,
                    operation_id: Some(intent.operation_id.clone()),
                    events: materialized.events,
                },
                &[],
                &[],
            )
            .await
            .map_err(Into::into)
    }

    async fn complete_move_intent(&self, intent: &MoveIntent) -> Result<String> {
        if self.config().reserves_file_version_namespace() {
            reject_reserved_root_path(&intent.destination)
                .map_err(|error| move_pending_error(intent, error))?;
        }
        let accepted = self
            .append_move_intent(intent)
            .await
            .map_err(|error| move_pending_error(intent, error))?;
        let commit_id = accepted.commit_id.to_string();
        if let Err(error) = self.recover().await {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: error.to_string(),
            });
        }
        if let Err(error) = self.refresh_worktree_identities().await {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: error.to_string(),
            });
        }
        if let Err(error) = self
            .verify_completed_move(intent)
            .await
            .and_then(|()| self.clear_move_intent())
        {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: error.to_string(),
            });
        }
        Ok(commit_id)
    }

    async fn verify_completed_move(&self, intent: &MoveIntent) -> Result<()> {
        let committed = self.committed_tree().await?;
        let node = committed.get(&intent.destination).ok_or_else(|| {
            LocalError::Integrity(format!(
                "completed move has no destination item: {}",
                intent.destination
            ))
        })?;
        if node.item_id != intent.item_id || node.head_seq != intent.result_head_seq {
            return Err(LocalError::Integrity(format!(
                "completed move has the wrong item identity or sequence: {}",
                intent.destination
            )));
        }
        Ok(())
    }

    fn write_move_intent(&self, intent: &MoveIntent) -> Result<()> {
        self.write_json_intent(MOVE_INTENT_NAME, "move", intent)
    }

    fn read_move_intent(&self) -> Result<Option<MoveIntent>> {
        self.read_json_intent(MOVE_INTENT_NAME, "move")
    }

    fn clear_move_intent(&self) -> Result<()> {
        self.clear_intent(MOVE_INTENT_NAME)
    }

    fn write_recover_intent(&self, intent: &RecoverIntent) -> Result<()> {
        self.write_json_intent(RECOVER_INTENT_NAME, "recover", intent)
    }

    fn read_recover_intent(&self) -> Result<Option<RecoverIntent>> {
        self.read_json_intent(RECOVER_INTENT_NAME, "recover")
    }

    fn clear_recover_intent(&self) -> Result<()> {
        self.clear_intent(RECOVER_INTENT_NAME)
    }

    fn write_revert_intent(&self, intent: &RevertIntent) -> Result<()> {
        self.write_json_intent(REVERT_INTENT_NAME, "revert", intent)
    }

    fn read_revert_intent(&self) -> Result<Option<RevertIntent>> {
        self.read_json_intent(REVERT_INTENT_NAME, "revert")
    }

    fn clear_revert_intent(&self) -> Result<()> {
        self.clear_intent(REVERT_INTENT_NAME)
    }

    pub(crate) fn validate_pending_operations(&self) -> Result<Option<&'static str>> {
        let intent_names = [
            crate::name_repair_journal::JOURNAL,
            MOVE_INTENT_NAME,
            RECOVER_INTENT_NAME,
            REVERT_INTENT_NAME,
            WORKTREE_RESTORE_INTENT_NAME,
            WORKING_TREE_EDIT_INTENT_NAME,
        ];
        let mut present = Vec::new();
        for name in intent_names {
            let path = self.metadata_path().join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => present.push(name),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(LocalError::Io {
                        action: "inspect operation intent",
                        path,
                        source,
                    });
                }
            }
        }
        if present.len() > 1 {
            return Err(LocalError::Integrity(format!(
                "repository contains simultaneous operation intents: {}",
                present.join(", ")
            )));
        }

        let stage_names = [
            REVERT_STAGE_NAME,
            WORKTREE_RESTORE_STAGE_NAME,
            WORKING_TREE_EDIT_STAGE_NAME,
        ];
        let mut present_stages = Vec::new();
        for name in stage_names {
            let path = self.metadata_path().join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => present_stages.push(name),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(LocalError::Io {
                        action: "inspect operation staging",
                        path,
                        source,
                    });
                }
            }
        }
        if present_stages.len() > 1 {
            return Err(LocalError::Integrity(format!(
                "repository contains simultaneous operation staging: {}",
                present_stages.join(", ")
            )));
        }

        let stage = self.metadata_path().join(WORKING_TREE_EDIT_STAGE_NAME);
        match std::fs::symlink_metadata(&stage) {
            Ok(metadata) => {
                if present.as_slice() != [WORKING_TREE_EDIT_INTENT_NAME]
                    || !metadata.file_type().is_dir()
                    || is_link_or_reparse(&metadata)
                {
                    return Err(LocalError::Integrity(
                        "repository contains invalid or orphaned working-tree edit staging"
                            .to_string(),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect working-tree edit staging",
                    path: stage,
                    source,
                });
            }
        }

        let stage = self.metadata_path().join(REVERT_STAGE_NAME);
        match std::fs::symlink_metadata(&stage) {
            Ok(metadata) => {
                if present.as_slice() != [REVERT_INTENT_NAME]
                    || !metadata.file_type().is_dir()
                    || is_link_or_reparse(&metadata)
                {
                    return Err(LocalError::Integrity(
                        "repository contains invalid or orphaned revert staging".to_string(),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect revert staging",
                    path: stage,
                    source,
                });
            }
        }

        let stage = self.metadata_path().join(WORKTREE_RESTORE_STAGE_NAME);
        match std::fs::symlink_metadata(&stage) {
            Ok(metadata) => {
                if present.as_slice() != [WORKTREE_RESTORE_INTENT_NAME]
                    || !metadata.file_type().is_dir()
                    || is_link_or_reparse(&metadata)
                {
                    return Err(LocalError::Integrity(
                        "repository contains invalid or orphaned worktree restore staging"
                            .to_string(),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect worktree restore staging",
                    path: stage,
                    source,
                });
            }
        }

        Ok(present.first().copied())
    }

    pub(crate) fn write_json_intent<T: Serialize>(
        &self,
        name: &str,
        temporary_prefix: &str,
        intent: &T,
    ) -> Result<()> {
        let intent_path = self.metadata_path().join(name);
        match std::fs::symlink_metadata(&intent_path) {
            Ok(_) => {
                return Err(LocalError::Integrity(format!(
                    "repository already contains a {temporary_prefix} intent"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect operation intent",
                    path: intent_path,
                    source,
                });
            }
        }
        let temporary_path = self
            .metadata_path()
            .join("tmp")
            .join(format!("{temporary_prefix}-{}", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = StdOpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary_path)
                .at("create operation intent", &temporary_path)?;
            serde_json::to_writer_pretty(&mut file, intent)?;
            file.write_all(b"\n")
                .at("write operation intent", &temporary_path)?;
            file.sync_all()
                .at("sync operation intent", &temporary_path)?;
            rename_noreplace(&temporary_path, &intent_path)
                .at("publish operation intent", &intent_path)?;
            sync_directory(self.metadata_path())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary_path);
        }
        result
    }

    pub(crate) fn read_json_intent<T: for<'de> Deserialize<'de>>(
        &self,
        name: &str,
        label: &str,
    ) -> Result<Option<T>> {
        let path = self.metadata_path().join(name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect operation intent",
                    path,
                    source,
                });
            }
        };
        if !metadata.file_type().is_file() || is_link_or_reparse(&metadata) {
            return Err(LocalError::Integrity(format!(
                "{label} intent is not a regular file: {}",
                path.display()
            )));
        }
        let bytes = std::fs::read(&path).at("read operation intent", &path)?;
        Ok(Some(serde_json::from_slice(&bytes).map_err(|error| {
            LocalError::Integrity(format!("invalid {label} intent: {error}"))
        })?))
    }

    pub(crate) fn clear_intent(&self, name: &str) -> Result<()> {
        let path = self.metadata_path().join(name);
        std::fs::remove_file(&path).at("remove operation intent", &path)?;
        sync_directory(self.metadata_path())
    }

    pub async fn log(&self, limit: Option<usize>) -> Result<Vec<LogEntry>> {
        self.recover_current().await?;
        self.log_recovered(limit).await
    }

    async fn log_recovered(&self, limit: Option<usize>) -> Result<Vec<LogEntry>> {
        let limit = limit.unwrap_or(LOG_DEFAULT_LIMIT);
        if limit == 0 || limit > LOG_MAX_LIMIT {
            return Err(LocalError::Invalid(format!(
                "log limit must be between 1 and {LOG_MAX_LIMIT}"
            )));
        }
        let connection = self.storage().open_readonly().map_err(|error| {
            LocalError::Invalid(format!("cannot read repository database: {error}"))
        })?;
        let maximum: Option<i64> = connection.query_row(
            "SELECT max(repo_seq) FROM vcs_commits WHERE repo_id = ?1",
            [self.config().repo_id().as_str()],
            |row| row.get(0),
        )?;
        let Some(maximum) = maximum else {
            return Ok(Vec::new());
        };
        let offset = i64::try_from(limit - 1).expect("validated log limit fits SQLite INTEGER");
        let start = connection
            .query_row(
                "SELECT repo_seq FROM vcs_commits
                 WHERE repo_id = ?1 AND repo_seq IS NOT NULL
                 ORDER BY repo_seq DESC LIMIT 1 OFFSET ?2",
                params![self.config().repo_id().as_str(), offset],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(1);
        let mut commits = self
            .core()
            .list_commits_by_repo_seq(self.config().repo_id().clone(), start, Some(maximum), limit)
            .await?;
        commits.reverse();
        commits
            .into_iter()
            .map(|commit| {
                Ok(LogEntry {
                    repo_seq: commit.repo_seq.expect("sequenced query returns repo_seq"),
                    commit_id: commit.commit_id.to_string(),
                    created_at: commit.created_at,
                    author: commit.author_id,
                    message: commit.message,
                    event_count: checked_commit_event_count(commit.event_count, &commit.commit_id)?,
                    events_digest: commit.events_digest.to_string(),
                })
            })
            .collect()
    }

    pub async fn detailed_log(&self, limit: Option<usize>) -> Result<Vec<DetailedLogEntry>> {
        let commits = self.log(limit).await?;
        if commits.is_empty() {
            return Ok(Vec::new());
        }
        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        #[cfg(test)]
        record_historical_work(self.config().repo_id(), HistoricalWorkKind::StateStore, 1);
        // `log` recovered first; a checkout's recovery already caught the index up.
        if self.checkout_paths().is_none() {
            self.catch_up_state_index_in(&mut store).await?;
        }
        let newest_repo_seq = commits[0].repo_seq;
        self.require_historical_projection(&mut store, newest_repo_seq)
            .await?;
        let mut roots = Vec::with_capacity(commits.len());
        for commit in &commits {
            let commit_id = CommitId::parse(&commit.commit_id).map_err(|error| {
                LocalError::Integrity(format!("repository contains invalid commit ID: {error}"))
            })?;
            roots.push(
                self.historical_root_after_projection(
                    &mut store,
                    commit.repo_seq,
                    Some(&commit_id),
                )
                .await?
                .1,
            );
        }
        let adjacent = match self.previous_repo_seq(commits.last().unwrap().repo_seq)? {
            Some(repo_seq) => Some(
                self.historical_root_after_projection(&mut store, repo_seq, None)
                    .await?
                    .1,
            ),
            None => None,
        };
        let mut entries = Vec::with_capacity(commits.len());
        for (index, commit) in commits.into_iter().enumerate() {
            let previous = roots.get(index + 1).or(adjacent.as_ref());
            let mut changes = self
                .historical_changes_between_roots(&mut store, previous, &roots[index])
                .await?;
            let changes_total = changes.len();
            changes.truncate(LOG_CHANGE_LIMIT);
            entries.push(DetailedLogEntry {
                commit,
                changes,
                changes_total,
            });
        }
        Ok(entries)
    }

    pub async fn detailed_event_log(
        &self,
        limit: Option<usize>,
    ) -> Result<Vec<DetailedEventLogEntry>> {
        let entries = self.detailed_log(limit).await?;
        let event_total = entries
            .iter()
            .map(|entry| entry.commit.event_count)
            .sum::<usize>();
        if event_total > MAX_EVENT_PAGE_SIZE {
            return Err(LocalError::Invalid(format!(
                "event log selection contains {event_total} events; reduce --limit or inspect one commit with show --events"
            )));
        }
        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        self.catch_up_state_index_in(&mut store).await?;
        let mut output = Vec::with_capacity(entries.len());
        for entry in entries {
            let events = self
                .complete_commit_events_in(
                    &mut store,
                    &entry.commit.commit_id,
                    entry.commit.repo_seq,
                    entry.commit.event_count,
                    &entry.commit.events_digest,
                )
                .await?;
            output.push(DetailedEventLogEntry { entry, events });
        }
        Ok(output)
    }

    pub async fn show(&self, input: &str) -> Result<HistoryCommit> {
        self.recover_current().await?;
        let commit = self.resolve_revision(input).await?;
        let commit_id = commit.commit_id.clone();
        let repo_seq = commit.repo_seq.ok_or_else(|| {
            LocalError::Integrity(format!("commit {commit_id} has no repository sequence"))
        })?;
        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        #[cfg(test)]
        record_historical_work(self.config().repo_id(), HistoricalWorkKind::StateStore, 1);
        self.catch_up_state_index_in(&mut store).await?;
        self.require_historical_projection(&mut store, repo_seq)
            .await?;
        let root = self
            .historical_root_after_projection(&mut store, repo_seq, Some(&commit_id))
            .await?
            .1;
        let previous = match self.previous_repo_seq(repo_seq)? {
            Some(previous_repo_seq) => Some(
                self.historical_root_after_projection(&mut store, previous_repo_seq, None)
                    .await?
                    .1,
            ),
            None => None,
        };
        let changes = self
            .historical_changes_between_roots(&mut store, previous.as_ref(), &root)
            .await?;
        history_commit(commit, changes)
    }

    pub async fn show_events(&self, input: &str) -> Result<EventHistoryCommit> {
        let commit = self.show(input).await?;
        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        self.catch_up_state_index_in(&mut store).await?;
        let events = self
            .complete_commit_events_in(
                &mut store,
                &commit.commit_id,
                commit.repo_seq,
                commit.event_count,
                &commit.events_digest,
            )
            .await?;
        Ok(EventHistoryCommit { commit, events })
    }

    pub async fn show_event_page(
        &self,
        input: &str,
        after_commit_event_idx: Option<i32>,
        limit: usize,
    ) -> Result<CommitEventPage> {
        if limit == 0 || limit > MAX_EVENT_PAGE_SIZE {
            return Err(LocalError::Invalid(format!(
                "event page size must be between 1 and {MAX_EVENT_PAGE_SIZE}"
            )));
        }
        if after_commit_event_idx.is_some_and(|cursor| cursor < 0) {
            return Err(LocalError::Invalid(
                "event page cursor must be nonnegative".to_string(),
            ));
        }

        self.recover_current().await?;
        let commit = self.resolve_revision(input).await?;
        let commit_id = commit.commit_id.clone();
        let repo_seq = commit.repo_seq.ok_or_else(|| {
            LocalError::Integrity(format!("commit {commit_id} has no repository sequence"))
        })?;
        let event_count = checked_commit_event_count(commit.event_count, &commit.commit_id)?;
        let start = match after_commit_event_idx {
            Some(cursor) => usize::try_from(cursor)
                .ok()
                .and_then(|cursor| cursor.checked_add(1))
                .ok_or_else(|| LocalError::Invalid("event page cursor is invalid".to_string()))?,
            None => 0,
        };
        if start > event_count {
            return Err(LocalError::Invalid(
                "event page cursor is beyond the commit".to_string(),
            ));
        }
        let page_len = limit.min(event_count - start);
        if page_len == 0 {
            return Ok(CommitEventPage {
                events: Vec::new(),
                total_event_count: event_count,
                next_after_commit_event_idx: None,
            });
        }

        let rows = self
            .core()
            .list_repo_commit_events(
                self.config().repo_id().clone(),
                Some(RepoEventCursor {
                    repo_seq,
                    commit_event_idx: after_commit_event_idx.unwrap_or(-1),
                }),
                page_len,
            )
            .await?;
        if rows.len() != page_len {
            return Err(LocalError::Integrity(format!(
                "commit {commit_id} event page expected {page_len} rows but stores {}",
                rows.len()
            )));
        }

        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        self.catch_up_state_index_in(&mut store).await?;
        let (events, _) = self
            .present_commit_event_rows_in(&mut store, &commit_id, repo_seq, start, rows, false)
            .await?;
        let end = start + events.len();
        Ok(CommitEventPage {
            events,
            total_event_count: event_count,
            next_after_commit_event_idx: (end < event_count)
                .then(|| i32::try_from(end - 1))
                .transpose()
                .map_err(|_| {
                    LocalError::Integrity("commit event index exceeds supported range".to_string())
                })?,
        })
    }

    async fn complete_commit_events_in<S: StateIndexStore>(
        &self,
        store: &mut S,
        commit_id: &str,
        repo_seq: i64,
        event_count: usize,
        events_digest: &str,
    ) -> Result<Vec<CommitEvent>> {
        let parsed_commit_id = CommitId::parse(commit_id).map_err(|error| {
            LocalError::Integrity(format!("repository contains invalid commit ID: {error}"))
        })?;
        let rows = self
            .core()
            .list_commit_events(
                self.config().repo_id().clone(),
                parsed_commit_id.clone(),
                event_count + 1,
            )
            .await?;
        if rows.len() != event_count {
            return Err(LocalError::Integrity(format!(
                "commit {commit_id} declares {event_count} events but stores {}",
                rows.len()
            )));
        }

        let (output, calculated_events_digest) = self
            .present_commit_event_rows_in(store, &parsed_commit_id, repo_seq, 0, rows, true)
            .await?;
        if calculated_events_digest.as_deref() != Some(events_digest) {
            return Err(LocalError::Integrity(format!(
                "commit {commit_id} events digest disagrees with its ordered event payloads"
            )));
        }
        Ok(output)
    }

    async fn present_commit_event_rows_in<S: StateIndexStore>(
        &self,
        store: &mut S,
        commit_id: &CommitId,
        repo_seq: i64,
        expected_start_index: usize,
        rows: Vec<CommitEventRow>,
        calculate_commit_digest: bool,
    ) -> Result<(Vec<CommitEvent>, Option<String>)> {
        self.require_historical_projection(store, repo_seq).await?;
        let root = self
            .historical_root_after_projection(store, repo_seq, Some(commit_id))
            .await?
            .1;
        let previous = match self.previous_repo_seq(repo_seq)? {
            Some(previous_repo_seq) => Some(
                self.historical_root_after_projection(store, previous_repo_seq, None)
                    .await?
                    .1,
            ),
            None => None,
        };

        let tagged_digest = rows
            .iter()
            .any(|event| matches!(event, CommitEventRow::Ref(_)));
        let mut commit_hasher = Sha256::new();
        let mut output = Vec::with_capacity(rows.len());
        for (offset, event) in rows.into_iter().enumerate() {
            let index = expected_start_index.checked_add(offset).ok_or_else(|| {
                LocalError::Integrity("commit event index exceeds supported range".to_string())
            })?;
            let expected_index = i32::try_from(index).map_err(|_| {
                LocalError::Integrity("commit event index exceeds supported range".to_string())
            })?;
            match event {
                CommitEventRow::Item(event) => {
                    if event.repo_id != *self.config().repo_id()
                        || event.commit_id != *commit_id
                        || event.repo_seq != Some(repo_seq)
                        || event.commit_event_idx != expected_index
                    {
                        return Err(LocalError::Integrity(format!(
                            "commit {commit_id} event {index} has an inconsistent envelope"
                        )));
                    }
                    let proposal: EventProposal = serde_json::from_slice(&event.payload).map_err(
                        |error| {
                            LocalError::Integrity(format!(
                                "commit {commit_id} event {index} has invalid canonical JSON: {error}"
                            ))
                        },
                    )?;
                    if proposal.item_id.as_ref() != Some(&event.item_id)
                        || proposal.event_type != event.event_type
                    {
                        return Err(LocalError::Integrity(format!(
                            "commit {commit_id} event {index} payload disagrees with its envelope"
                        )));
                    }
                    let canonical = canonical_event_proposal_bytes(&proposal).map_err(|error| {
                        LocalError::Integrity(format!(
                            "commit {commit_id} event {index} cannot be canonicalized: {error}"
                        ))
                    })?;
                    if canonical != event.payload {
                        return Err(LocalError::Integrity(format!(
                            "commit {commit_id} event {index} payload is not canonical"
                        )));
                    }
                    let payload_digest = compute_sha256_digest_bytes(&event.payload);
                    if sha256_prefixed_hex_from_digest_bytes(payload_digest)
                        != event.event_payload_digest.as_str()
                    {
                        return Err(LocalError::Integrity(format!(
                            "commit {commit_id} event {index} payload digest disagrees with its bytes"
                        )));
                    }
                    if calculate_commit_digest {
                        if tagged_digest {
                            commit_hasher.update([COMMIT_EVENT_FAMILY_ITEM]);
                        }
                        commit_hasher.update(payload_digest);
                    }
                    let presentation =
                        commit_event_presentation(store, &root, previous.as_ref(), &proposal)
                            .await?;
                    output.push(CommitEvent {
                        repo_id: event.repo_id.to_string(),
                        commit_id: event.commit_id.to_string(),
                        repo_seq,
                        commit_event_idx: event.commit_event_idx,
                        item_id: event.item_id.to_string(),
                        display_item_id: presentation.item_id.to_string(),
                        item_type: presentation.item_type,
                        item_name: presentation.item_name,
                        item_name_before: presentation.item_name_before,
                        item_seq: event.item_seq,
                        event_type: event.event_type,
                        payload: String::from_utf8(canonical)
                            .expect("canonical event JSON is valid UTF-8"),
                        event_payload_digest: event.event_payload_digest.to_string(),
                        created_at: event.created_at,
                        parent_folder_id: event.parent_folder_id.map(|value| value.to_string()),
                        parent_folder_name: presentation.parent_folder_name,
                        parent_folder_id_before: event
                            .parent_folder_id_before
                            .map(|value| value.to_string()),
                        parent_folder_name_before: presentation.parent_folder_name_before,
                        display_parent_folder_id: presentation
                            .parent_folder_id
                            .map(|value| value.to_string()),
                        display_parent_folder_id_before: presentation
                            .parent_folder_id_before
                            .map(|value| value.to_string()),
                    });
                }
                CommitEventRow::Ref(event) => {
                    if event.repo_id != *self.config().repo_id()
                        || event.commit_id != *commit_id
                        || event.repo_seq != Some(repo_seq)
                        || event.commit_event_idx != expected_index
                    {
                        return Err(LocalError::Integrity(format!(
                            "commit {commit_id} ref event {index} has an inconsistent envelope"
                        )));
                    }
                    let proposal: RefEventProposal = serde_json::from_slice(&event.payload)
                        .map_err(|error| {
                            LocalError::Integrity(format!(
                                "commit {commit_id} ref event {index} has invalid canonical JSON: {error}"
                            ))
                        })?;
                    if proposal.owner_item_id != event.owner_item_id
                        || proposal.event_type != event.event_type
                    {
                        return Err(LocalError::Integrity(format!(
                            "commit {commit_id} ref event {index} payload disagrees with its envelope"
                        )));
                    }
                    let canonical = canonical_ref_event_proposal_bytes(&proposal).map_err(
                        |error| {
                            LocalError::Integrity(format!(
                                "commit {commit_id} ref event {index} cannot be canonicalized: {error}"
                            ))
                        },
                    )?;
                    if canonical != event.payload {
                        return Err(LocalError::Integrity(format!(
                            "commit {commit_id} ref event {index} payload is not canonical"
                        )));
                    }
                    let payload_digest = compute_sha256_digest_bytes(&event.payload);
                    if sha256_prefixed_hex_from_digest_bytes(payload_digest)
                        != event.event_payload_digest.as_str()
                    {
                        return Err(LocalError::Integrity(format!(
                            "commit {commit_id} ref event {index} payload digest disagrees with its bytes"
                        )));
                    }
                    if calculate_commit_digest {
                        commit_hasher.update([COMMIT_EVENT_FAMILY_REF]);
                        commit_hasher.update(payload_digest);
                    }
                    let presentation =
                        ref_commit_event_presentation(store, &root, previous.as_ref(), &proposal)
                            .await?;
                    output.push(CommitEvent {
                        repo_id: event.repo_id.to_string(),
                        commit_id: event.commit_id.to_string(),
                        repo_seq,
                        commit_event_idx: event.commit_event_idx,
                        item_id: event.owner_item_id.to_string(),
                        display_item_id: presentation.item_id.to_string(),
                        item_type: presentation.item_type,
                        item_name: presentation.item_name,
                        item_name_before: presentation.item_name_before,
                        item_seq: event.ref_seq,
                        event_type: event.event_type,
                        payload: String::from_utf8(canonical)
                            .expect("canonical ref-event JSON is valid UTF-8"),
                        event_payload_digest: event.event_payload_digest.to_string(),
                        created_at: event.created_at,
                        parent_folder_id: None,
                        parent_folder_name: presentation.parent_folder_name,
                        parent_folder_id_before: None,
                        parent_folder_name_before: presentation.parent_folder_name_before,
                        display_parent_folder_id: presentation
                            .parent_folder_id
                            .map(|value| value.to_string()),
                        display_parent_folder_id_before: presentation
                            .parent_folder_id_before
                            .map(|value| value.to_string()),
                    });
                }
            }
        }
        let calculated_events_digest = calculate_commit_digest
            .then(|| sha256_prefixed_hex_from_digest_bytes(commit_hasher.finalize().into()));
        Ok((output, calculated_events_digest))
    }

    pub(crate) async fn resolve_revision(&self, input: &str) -> Result<CommitRow> {
        let head_offset = if input == "HEAD" {
            Some(0)
        } else if let Some(value) = input.strip_prefix("HEAD~") {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(LocalError::Invalid(format!(
                    "invalid revision {input:?}; expected HEAD, HEAD~N, a repository sequence, or a complete commit ID"
                )));
            }
            let offset = value.parse::<usize>().map_err(|_| {
                LocalError::Invalid(format!("revision offset is too large: {input}"))
            })?;
            if offset >= LOG_MAX_LIMIT {
                return Err(LocalError::Invalid(format!(
                    "revision offset must be less than {LOG_MAX_LIMIT}"
                )));
            }
            Some(offset)
        } else {
            None
        };

        if let Some(offset) = head_offset {
            let entry = self
                .log_recovered(Some(offset + 1))
                .await?
                .into_iter()
                .nth(offset)
                .ok_or_else(|| LocalError::Invalid(format!("revision does not exist: {input}")))?;
            let commit_id = CommitId::parse(&entry.commit_id).map_err(|error| {
                LocalError::Integrity(format!("repository contains invalid commit ID: {error}"))
            })?;
            return self
                .core()
                .get_commit(self.config().repo_id().clone(), commit_id.clone())
                .await?
                .ok_or_else(|| LocalError::Invalid(format!("commit does not exist: {commit_id}")));
        }

        if !input.is_empty() && input.bytes().all(|byte| byte.is_ascii_digit()) {
            if input.starts_with('0') {
                return Err(LocalError::Invalid(format!(
                    "invalid repository sequence revision: {input}"
                )));
            }
            let repo_seq = input.parse::<i64>().map_err(|_| {
                LocalError::Invalid(format!("repository sequence is too large: {input}"))
            })?;
            let commit = self
                .commit_at_repo_seq(repo_seq)
                .await?
                .ok_or_else(|| LocalError::Invalid(format!("revision does not exist: {input}")))?;
            return Ok(commit);
        }

        let commit_id = {
            let commit_id = CommitId::parse(input)
                .map_err(|error| LocalError::Invalid(format!("invalid commit ID: {error}")))?;
            if commit_id.as_str() != input {
                return Err(LocalError::Invalid(
                    "commit ID must use canonical lowercase form".to_string(),
                ));
            }
            commit_id
        };
        self.core()
            .get_commit(self.config().repo_id().clone(), commit_id.clone())
            .await?
            .ok_or_else(|| LocalError::Invalid(format!("commit does not exist: {commit_id}")))
    }

    async fn commit_at_repo_seq(&self, repo_seq: i64) -> Result<Option<CommitRow>> {
        Ok(self
            .core()
            .list_commits_by_repo_seq(self.config().repo_id().clone(), repo_seq, Some(repo_seq), 1)
            .await?
            .into_iter()
            .next()
            .filter(|commit| commit.repo_seq == Some(repo_seq)))
    }

    fn previous_repo_seq(&self, repo_seq: i64) -> Result<Option<i64>> {
        let connection = self.storage().open_readonly().map_err(|error| {
            LocalError::Integrity(format!("cannot read previous repository sequence: {error}"))
        })?;
        connection
            .query_row(
                "SELECT max(repo_seq) FROM vcs_commits
                 WHERE repo_id = ?1 AND repo_seq < ?2",
                params![self.config().repo_id().as_str(), repo_seq],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    async fn historical_tree_at(
        &self,
        revision: &str,
    ) -> Result<(CommitRow, BTreeMap<String, CommittedNode>)> {
        let commit = self.resolve_revision(revision).await?;
        let repo_seq = commit.repo_seq.ok_or_else(|| {
            LocalError::Integrity(format!(
                "commit {} has no repository sequence",
                commit.commit_id
            ))
        })?;
        let items = self
            .historical_items_at_commit(repo_seq, &commit.commit_id)
            .await?;
        let tree = self.historical_committed_tree_from_state(items)?;
        Ok((commit, tree))
    }

    async fn historical_subtree_at(
        &self,
        revision: &str,
        selected: &str,
    ) -> Result<(CommitRow, BTreeMap<String, CommittedNode>)> {
        let commit = self.resolve_revision(revision).await?;
        let repo_seq = commit.repo_seq.ok_or_else(|| {
            LocalError::Integrity(format!(
                "commit {} has no repository sequence",
                commit.commit_id
            ))
        })?;
        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        let (_, root) = self
            .historical_root_at_commit(&mut store, repo_seq, &commit.commit_id)
            .await?;
        let mut state = StateTree::new(&mut store);

        let components = selected.split('/').collect::<Vec<_>>();
        let mut parent_id = None;
        let mut selected_folder = None;
        for (index, component) in components.iter().enumerate() {
            let name_key = compute_name_key(component).map_err(|_| {
                LocalError::Invalid(format!(
                    "historical folder does not exist at {}: {selected}",
                    commit.commit_id
                ))
            })?;
            let key = NamespaceKey::new(parent_id.clone(), name_key);
            let item_id = state
                .get_namespace(root.namespace_root, &key)
                .await
                .map_err(|error| LocalError::Integrity(error.to_string()))?
                .ok_or_else(|| {
                    LocalError::Invalid(format!(
                        "historical folder does not exist at {}: {selected}",
                        commit.commit_id
                    ))
                })?;
            let item = state
                .get_item(root.item_map_root, &item_id)
                .await
                .map_err(|error| LocalError::Integrity(error.to_string()))?
                .ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "historical namespace references missing item {item_id}"
                    ))
                })?;
            if item.lifecycle_state != LifecycleState::Live
                || item.parent_folder_id != key.parent_folder_id
                || item.name_key != key.name_key
            {
                return Err(LocalError::Integrity(format!(
                    "historical namespace disagrees for item {item_id}"
                )));
            }
            if item.name != *component {
                return Err(LocalError::Invalid(format!(
                    "historical folder does not exist at {}: {selected}",
                    commit.commit_id
                )));
            }
            if item.item_type != "FOLDER" {
                if index + 1 == components.len() {
                    return Err(LocalError::Invalid(format!(
                        "historical path is not a folder: {selected}"
                    )));
                }
                return Err(LocalError::Invalid(format!(
                    "historical folder does not exist at {}: {selected}",
                    commit.commit_id
                )));
            }
            parent_id = Some(item_id);
            selected_folder = Some(item);
        }

        let selected_folder = selected_folder.expect("a normalized item path is not empty");
        let mut folders = vec![(selected_folder.item_id.clone(), String::new())];
        let mut seen_folders = HashSet::from([selected_folder.item_id]);
        let mut items = BTreeMap::new();
        let mut paths = HashMap::new();
        while let Some((folder_id, folder_path)) = folders.pop() {
            let mut cursor = None;
            loop {
                let page = state
                    .child_page(
                        root.item_map_root,
                        root.namespace_root,
                        Some(&folder_id),
                        cursor.as_ref(),
                    )
                    .await
                    .map_err(|error| LocalError::Integrity(error.to_string()))?;
                for (_, item) in page.entries {
                    if item.item_type == synck_file_folder::file_version_set::ITEM_TYPE {
                        continue;
                    }
                    if self.checkout_paths().is_some()
                        && !matches!(item.item_type.as_str(), "FILE" | "FOLDER")
                    {
                        // Not part of a checkout's filesystem view.
                        continue;
                    }
                    if !matches!(item.item_type.as_str(), "FILE" | "FOLDER") {
                        return Err(LocalError::Integrity(format!(
                            "unsupported historical item type {} for {}",
                            item.item_type, item.item_id
                        )));
                    }
                    let path = if folder_path.is_empty() {
                        item.name.clone()
                    } else {
                        format!("{folder_path}/{}", item.name)
                    };
                    let item_id = item.item_id.clone();
                    if items.contains_key(&item_id) {
                        return Err(LocalError::Integrity(format!(
                            "historical subtree references item {item_id} more than once"
                        )));
                    }
                    let is_folder = item.item_type == "FOLDER";
                    items.insert(item_id.clone(), item);
                    paths.insert(item_id.clone(), path.clone());
                    if is_folder {
                        if !seen_folders.insert(item_id.clone()) {
                            return Err(LocalError::Integrity(format!(
                                "historical state contains a folder cycle at {item_id}"
                            )));
                        }
                        folders.push((item_id, path));
                    }
                }
                let Some(next_cursor) = page.next_cursor else {
                    break;
                };
                cursor = Some(next_cursor);
            }
        }
        let tree = self.historical_committed_tree_from_paths(items, paths)?;
        Ok((commit, tree))
    }

    pub(crate) async fn historical_changed_trees(
        &self,
        from: &str,
        to: &str,
    ) -> Result<HistoricalTreeDelta> {
        let from_commit = self.resolve_revision(from).await?;
        let to_commit = self.resolve_revision(to).await?;
        let from_repo_seq = from_commit.repo_seq.ok_or_else(|| {
            LocalError::Integrity(format!(
                "commit {} has no repository sequence",
                from_commit.commit_id
            ))
        })?;
        let to_repo_seq = to_commit.repo_seq.ok_or_else(|| {
            LocalError::Integrity(format!(
                "commit {} has no repository sequence",
                to_commit.commit_id
            ))
        })?;
        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        let (_, from_root) = self
            .historical_root_at_commit(&mut store, from_repo_seq, &from_commit.commit_id)
            .await?;
        let (_, to_root) = self
            .historical_root_at_commit(&mut store, to_repo_seq, &to_commit.commit_id)
            .await?;
        let changed = StateTree::new(&mut store)
            .diff_items(from_root.item_map_root, to_root.item_map_root)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        let changed_namespace = StateTree::new(&mut store)
            .diff_namespace(from_root.namespace_root, to_root.namespace_root)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        let from_items = self
            .historical_selected_items(&mut store, &from_root, &changed)
            .await?;
        let to_items = self
            .historical_selected_items(&mut store, &to_root, &changed)
            .await?;
        validate_historical_namespace_delta(&changed, &changed_namespace, &from_items, &to_items)?;
        drop(store);
        let (mut from_items, mut to_items, mut changed) = (from_items, to_items, changed);
        self.narrow_checkout_delta(&mut from_items, &mut to_items, &mut changed);
        let from_tree = self.historical_committed_tree_from_state(from_items)?;
        let to_tree = self.historical_committed_tree_from_state(to_items)?;
        Ok(HistoricalTreeDelta {
            from_commit,
            from_tree,
            to_commit,
            to_tree,
            changed_item_ids: changed,
        })
    }

    async fn historical_items_at_commit(
        &self,
        repo_seq: i64,
        commit_id: &CommitId,
    ) -> Result<BTreeMap<ItemId, ItemValue>> {
        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        let (_, root) = self
            .historical_root_at_commit(&mut store, repo_seq, commit_id)
            .await?;
        let mut state = StateTree::new(&mut store);
        let (mut items, _) = state
            .validated_state(root.item_map_root, root.namespace_root)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        self.retain_checkout_filesystem_view(&mut items);
        Ok(items)
    }

    /// A complete-repository checkout shows history through the same
    /// filesystem view as its working tree: FILE and FOLDER items whose
    /// ancestors are all folders. Chats, application records and files owned
    /// by them remain canonical repository data, read through items.
    /// Narrow an already validated historical delta to the checkout's
    /// filesystem view. A no-op outside complete-repository checkouts.
    fn narrow_checkout_delta(
        &self,
        from: &mut BTreeMap<ItemId, ItemValue>,
        to: &mut BTreeMap<ItemId, ItemValue>,
        changed: &mut BTreeSet<ItemId>,
    ) {
        if self.checkout_paths().is_none() {
            return;
        }
        self.retain_checkout_filesystem_view(from);
        self.retain_checkout_filesystem_view(to);
        changed.retain(|id| from.contains_key(id) || to.contains_key(id));
    }

    fn retain_checkout_filesystem_view(&self, items: &mut BTreeMap<ItemId, ItemValue>) {
        if self.checkout_paths().is_none() {
            return;
        }
        items.retain(|_, item| matches!(item.item_type.as_str(), "FILE" | "FOLDER"));
        loop {
            let folders = items
                .iter()
                .filter(|(_, item)| item.item_type == "FOLDER")
                .map(|(id, _)| id.clone())
                .collect::<HashSet<_>>();
            let before = items.len();
            items.retain(|_, item| {
                item.parent_folder_id
                    .as_ref()
                    .is_none_or(|parent| folders.contains(parent))
            });
            if before == items.len() {
                break;
            }
        }
    }

    pub(crate) async fn historical_root_at_commit<S: StateIndexStore>(
        &self,
        store: &mut S,
        repo_seq: i64,
        commit_id: &CommitId,
    ) -> Result<(StateRootId, RootHeader)> {
        self.require_historical_projection(store, repo_seq).await?;
        self.historical_root_after_projection(store, repo_seq, Some(commit_id))
            .await
    }

    async fn require_historical_projection<S: StateIndexStore>(
        &self,
        store: &mut S,
        repo_seq: i64,
    ) -> Result<()> {
        let head = store
            .projection_head()
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?
            .ok_or_else(|| LocalError::Integrity("state projection head is missing".to_string()))?;
        if head.applied_repo_seq < repo_seq {
            return Err(LocalError::Integrity(format!(
                "state index is behind repository sequence {repo_seq}"
            )));
        }
        Ok(())
    }

    async fn historical_root_after_projection<S: StateIndexStore>(
        &self,
        store: &mut S,
        repo_seq: i64,
        expected_commit_id: Option<&CommitId>,
    ) -> Result<(StateRootId, RootHeader)> {
        let record = store
            .root_at(repo_seq)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?
            .ok_or_else(|| {
                LocalError::Integrity(format!(
                    "state root is missing at repository sequence {repo_seq}"
                ))
            })?;
        if expected_commit_id.is_some_and(|commit_id| record.commit_id != *commit_id) {
            return Err(LocalError::Integrity(format!(
                "repository sequence {repo_seq} resolves to the wrong commit"
            )));
        }
        let mut state = StateTree::new(&mut *store);
        let root = state
            .read_root(record.state_root)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        if root.repo_id != *self.config().repo_id()
            || root.repo_seq != repo_seq
            || root.commit_id.as_ref() != Some(&record.commit_id)
        {
            return Err(LocalError::Integrity(format!(
                "state root disagrees with repository sequence {repo_seq}"
            )));
        }
        Ok((record.state_root, root))
    }

    async fn historical_changes_between_roots<S: StateIndexStore>(
        &self,
        store: &mut S,
        previous: Option<&RootHeader>,
        current: &RootHeader,
    ) -> Result<Vec<Change>> {
        #[cfg(test)]
        record_historical_work(self.config().repo_id(), HistoricalWorkKind::CommitDelta, 1);
        let Some(previous) = previous else {
            let (items, _) = StateTree::new(&mut *store)
                .validated_state(current.item_map_root, current.namespace_root)
                .await
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
            let mut items = items;
            self.retain_checkout_filesystem_view(&mut items);
            return initial_historical_changes(items);
        };
        let changed = StateTree::new(&mut *store)
            .diff_items(previous.item_map_root, current.item_map_root)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        let changed_namespace = StateTree::new(&mut *store)
            .diff_namespace(previous.namespace_root, current.namespace_root)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        let previous_items = self
            .historical_selected_items(store, previous, &changed)
            .await?;
        let current_items = self
            .historical_selected_items(store, current, &changed)
            .await?;
        validate_historical_namespace_delta(
            &changed,
            &changed_namespace,
            &previous_items,
            &current_items,
        )?;
        let (mut previous_items, mut current_items, mut changed) =
            (previous_items, current_items, changed);
        self.narrow_checkout_delta(&mut previous_items, &mut current_items, &mut changed);
        let previous_tree = self.historical_committed_tree_from_state(previous_items)?;
        let current_tree = self.historical_committed_tree_from_state(current_items)?;
        Ok(compare_trees(&previous_tree, &current_tree, &changed))
    }

    async fn historical_selected_items<S: StateIndexStore>(
        &self,
        store: &mut S,
        root: &RootHeader,
        selected: &BTreeSet<ItemId>,
    ) -> Result<BTreeMap<ItemId, ItemValue>> {
        let mut state = StateTree::new(&mut *store);
        let mut items = BTreeMap::new();
        for selected_id in selected {
            let Some(selected_item) = state
                .get_item(root.item_map_root, selected_id)
                .await
                .map_err(|error| LocalError::Integrity(error.to_string()))?
            else {
                continue;
            };
            if selected_item.lifecycle_state != LifecycleState::Live {
                continue;
            }

            let mut item = selected_item;
            let mut visiting = HashSet::new();
            loop {
                if items.contains_key(&item.item_id) {
                    break;
                }
                if !visiting.insert(item.item_id.clone()) {
                    return Err(LocalError::Integrity(format!(
                        "historical state contains a parent cycle at {}",
                        item.item_id
                    )));
                }
                if item.lifecycle_state != LifecycleState::Live
                    || self.checkout_paths().is_none()
                        && !matches!(item.item_type.as_str(), "FILE" | "FOLDER")
                        && item.item_type != synck_file_folder::file_version_set::ITEM_TYPE
                {
                    return Err(LocalError::Integrity(format!(
                        "historical live item {} has an invalid type or lifecycle",
                        item.item_id
                    )));
                }
                let namespace =
                    NamespaceKey::new(item.parent_folder_id.clone(), item.name_key.clone());
                let namespace_item = state
                    .get_namespace(root.namespace_root, &namespace)
                    .await
                    .map_err(|error| LocalError::Integrity(error.to_string()))?;
                if namespace_item.as_ref() != Some(&item.item_id) {
                    return Err(LocalError::Integrity(format!(
                        "historical namespace disagrees for item {}",
                        item.item_id
                    )));
                }

                let parent_id = item.parent_folder_id.clone();
                items.insert(item.item_id.clone(), item);
                let Some(parent_id) = parent_id else {
                    break;
                };
                item = state
                    .get_item(root.item_map_root, &parent_id)
                    .await
                    .map_err(|error| LocalError::Integrity(error.to_string()))?
                    .ok_or_else(|| {
                        LocalError::Integrity(format!(
                            "historical state is missing parent {parent_id}"
                        ))
                    })?;
                if item.item_type != "FOLDER" || item.lifecycle_state != LifecycleState::Live {
                    return Err(LocalError::Integrity(format!(
                        "historical item {selected_id} has a non-live or non-folder parent {parent_id}"
                    )));
                }
            }
        }
        Ok(items)
    }

    async fn historical_items_before(&self, repo_seq: i64) -> Result<BTreeMap<ItemId, ItemValue>> {
        let Some(previous_repo_seq) = self.previous_repo_seq(repo_seq)? else {
            return Ok(BTreeMap::new());
        };
        let previous = self
            .commit_at_repo_seq(previous_repo_seq)
            .await?
            .ok_or_else(|| {
                LocalError::Integrity(format!(
                    "previous repository sequence {previous_repo_seq} has no commit"
                ))
            })?;
        self.historical_items_at_commit(previous_repo_seq, &previous.commit_id)
            .await
    }

    async fn historical_nodes_at_commit(
        &self,
        repo_seq: i64,
        commit_id: &CommitId,
    ) -> Result<HashMap<ItemId, HistoryNode>> {
        let items = self.historical_items_at_commit(repo_seq, commit_id).await?;
        self.historical_nodes_from_state(items)
    }

    async fn historical_nodes_before(&self, repo_seq: i64) -> Result<HashMap<ItemId, HistoryNode>> {
        let items = self.historical_items_before(repo_seq).await?;
        self.historical_nodes_from_state(items)
    }

    fn historical_committed_tree_from_state(
        &self,
        mut items: BTreeMap<ItemId, ItemValue>,
    ) -> Result<BTreeMap<String, CommittedNode>> {
        items.retain(|_, item| item.item_type != synck_file_folder::file_version_set::ITEM_TYPE);
        let mut paths = HashMap::new();
        let mut visiting = HashSet::new();
        for (item_id, item) in &items {
            if item.lifecycle_state == LifecycleState::Live {
                historical_state_path(item_id, &items, &mut paths, &mut visiting)?;
            }
        }

        self.historical_committed_tree_from_paths(items, paths)
    }

    fn historical_committed_tree_from_paths(
        &self,
        items: BTreeMap<ItemId, ItemValue>,
        mut paths: HashMap<ItemId, String>,
    ) -> Result<BTreeMap<String, CommittedNode>> {
        let mut tree = BTreeMap::new();
        let contents = self.historical_file_contents(&items, true)?;
        for (item_id, item) in items {
            if item.lifecycle_state != LifecycleState::Live {
                continue;
            }
            let kind = match item.item_type.as_str() {
                "FILE" => NodeKind::File,
                "FOLDER" => NodeKind::Folder,
                other => {
                    return Err(LocalError::Integrity(format!(
                        "unsupported historical item type {other} for {item_id}"
                    )));
                }
            };
            let (blob_id, size) = match kind {
                NodeKind::File => {
                    let (blob_id, size) = contents.get(&item_id).ok_or_else(|| {
                        LocalError::Integrity(format!(
                            "historical FILE {item_id} has no authenticated content"
                        ))
                    })?;
                    (Some(blob_id.clone()), Some(*size))
                }
                NodeKind::Folder => {
                    if item.content.is_some() {
                        return Err(LocalError::Integrity(format!(
                            "historical FOLDER {item_id} has file content"
                        )));
                    }
                    (None, None)
                }
            };
            let path = paths.remove(&item_id).ok_or_else(|| {
                LocalError::Integrity(format!(
                    "historical state has no resolved path for {item_id}"
                ))
            })?;
            if tree
                .insert(
                    path.clone(),
                    CommittedNode {
                        item_id,
                        kind,
                        head_seq: item.head_seq,
                        blob_id,
                        size,
                    },
                )
                .is_some()
            {
                return Err(LocalError::Integrity(format!(
                    "multiple historical items resolve to {path}"
                )));
            }
        }
        Ok(tree)
    }

    fn historical_nodes_from_state(
        &self,
        mut items: BTreeMap<ItemId, ItemValue>,
    ) -> Result<HashMap<ItemId, HistoryNode>> {
        items.retain(|_, item| item.item_type != synck_file_folder::file_version_set::ITEM_TYPE);
        self.retain_checkout_filesystem_view(&mut items);
        let contents = self.historical_file_contents(&items, false)?;
        items
            .into_iter()
            .map(|(item_id, item)| {
                let kind = match item.item_type.as_str() {
                    "FILE" => NodeKind::File,
                    "FOLDER" => NodeKind::Folder,
                    other => {
                        return Err(LocalError::Integrity(format!(
                            "unsupported historical item type {other} for {item_id}"
                        )));
                    }
                };
                let (blob_id, size) = match kind {
                    NodeKind::File => {
                        let (blob_id, size) = contents.get(&item_id).ok_or_else(|| {
                            LocalError::Integrity(format!(
                                "historical FILE {item_id} has no authenticated content"
                            ))
                        })?;
                        (Some(blob_id.clone()), Some(*size))
                    }
                    NodeKind::Folder => {
                        if item.content.is_some() {
                            return Err(LocalError::Integrity(format!(
                                "historical FOLDER {item_id} has file content"
                            )));
                        }
                        (None, None)
                    }
                };
                Ok((
                    item_id,
                    HistoryNode {
                        parent_id: item.parent_folder_id,
                        name: item.name,
                        kind,
                        live: item.lifecycle_state == LifecycleState::Live,
                        head_seq: item.head_seq,
                        blob_id,
                        size,
                        deletion: None,
                    },
                ))
            })
            .collect()
    }

    pub(crate) fn historical_file_contents(
        &self,
        items: &BTreeMap<ItemId, ItemValue>,
        live_only: bool,
    ) -> Result<HashMap<ItemId, (BlobId, i64)>> {
        const CONTENT_BATCH: usize = 256;

        let requested = items
            .values()
            .filter(|item| {
                item.item_type == "FILE"
                    && (!live_only || item.lifecycle_state == LifecycleState::Live)
            })
            .map(|item| {
                item.content
                    .clone()
                    .map(|content| (item.item_id.clone(), content))
                    .ok_or_else(|| {
                        LocalError::Integrity(format!(
                            "historical FILE {} has no content pointer",
                            item.item_id
                        ))
                    })
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        if requested.is_empty() {
            return Ok(HashMap::new());
        }

        let connection = self.storage().open_readonly().map_err(|error| {
            LocalError::Integrity(format!("cannot read historical FILE events: {error}"))
        })?;
        let entries = requested.iter().collect::<Vec<_>>();
        let mut contents = HashMap::new();
        for chunk in entries.chunks(CONTENT_BATCH) {
            let values = std::iter::repeat_n("(?, ?)", chunk.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "WITH requested(item_id, item_seq) AS (VALUES {values})
                 SELECT e.item_id, e.item_seq, e.event_type, e.payload,
                        e.event_payload_digest
                 FROM vcs_events e
                 JOIN requested r
                   ON r.item_id = e.item_id AND r.item_seq = e.item_seq
                 WHERE e.repo_id = ?
                 ORDER BY e.item_id"
            );
            let mut parameters = Vec::with_capacity(chunk.len() * 2 + 1);
            for (item_id, pointer) in chunk {
                parameters.push(SqlValue::Text(item_id.as_str().to_string()));
                parameters.push(SqlValue::Integer(pointer.item_seq));
            }
            parameters.push(SqlValue::Text(self.config().repo_id().as_str().to_string()));
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(parameters), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })?;
            for row in rows {
                let (item_id, item_seq, event_type, payload, stored_digest) = row?;
                let item_id = ItemId::parse(item_id).map_err(|error| {
                    LocalError::Integrity(format!("invalid historical event item ID: {error}"))
                })?;
                let pointer = requested.get(&item_id).ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "historical content query returned unexpected item {item_id}"
                    ))
                })?;
                if pointer.item_seq != item_seq {
                    return Err(LocalError::Integrity(format!(
                        "historical content query returned the wrong sequence for {item_id}"
                    )));
                }
                let stored_digest: [u8; 32] = stored_digest.try_into().map_err(|_| {
                    LocalError::Integrity(format!(
                        "historical content event for {item_id} has an invalid digest"
                    ))
                })?;
                if stored_digest != pointer.event_digest
                    || compute_sha256_digest_bytes(&payload) != pointer.event_digest
                {
                    return Err(LocalError::Integrity(format!(
                        "historical content event digest disagrees for {item_id}"
                    )));
                }
                let proposal: EventProposal =
                    serde_json::from_slice(&payload).map_err(|error| {
                        LocalError::Integrity(format!(
                            "invalid historical content event for {item_id}: {error}"
                        ))
                    })?;
                if canonical_event_proposal_bytes(&proposal)? != payload
                    || proposal.item_id.as_ref() != Some(&item_id)
                    || proposal.event_type != event_type
                {
                    return Err(LocalError::Integrity(format!(
                        "historical content event identity disagrees for {item_id}"
                    )));
                }
                let (blob_id, size) = content_from_event(&proposal)?;
                if contents.insert(item_id.clone(), (blob_id, size)).is_some() {
                    return Err(LocalError::Integrity(format!(
                        "duplicate historical content event for {item_id}"
                    )));
                }
            }
        }
        if contents.len() != requested.len() {
            let missing = requested
                .keys()
                .find(|item_id| !contents.contains_key(*item_id))
                .expect("different map sizes imply a missing item");
            return Err(LocalError::Integrity(format!(
                "historical content event is missing for {missing}"
            )));
        }
        Ok(contents)
    }

    pub async fn history(&self, caller: &Path, input: &Path) -> Result<Vec<HistoryCommit>> {
        self.recover_current().await?;
        let display = normalize_input_path(self.root_path(), caller, input)?;
        let (item_id, item_type) = self.resolve_current_item_path(&display)?;
        let event_groups = self.item_event_groups(&item_id).await?;
        let Some(last_group) = event_groups.last() else {
            return Err(LocalError::Integrity(format!(
                "tracked item {item_id} has no events"
            )));
        };
        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        #[cfg(test)]
        record_historical_work(self.config().repo_id(), HistoricalWorkKind::StateStore, 1);
        self.catch_up_state_index_in(&mut store).await?;
        self.require_historical_projection(&mut store, last_group.0)
            .await?;
        let item_kinds = BTreeMap::from([(item_id.clone(), item_type.clone())]);
        let mut output = Vec::with_capacity(event_groups.len());
        for (repo_seq, commit_id, events) in event_groups {
            let commit = self
                .core()
                .get_commit(self.config().repo_id().clone(), commit_id.clone())
                .await?
                .ok_or_else(|| {
                    LocalError::Integrity(format!("history references missing commit {commit_id}"))
                })?;
            if commit.repo_seq != Some(repo_seq) {
                return Err(LocalError::Integrity(format!(
                    "history commit {commit_id} has the wrong repository sequence"
                )));
            }
            let proposals = events
                .iter()
                .map(|event| validate_historical_item_event(event, &item_id))
                .collect::<Result<Vec<_>>>()?;
            let profiles =
                classify_proposal_commit(commit.message.as_deref(), &proposals, &item_kinds)
                    .map_err(LocalError::Integrity)?;
            let current_root = self
                .historical_root_after_projection(&mut store, repo_seq, Some(&commit_id))
                .await?
                .1;
            let previous_root = match self.previous_repo_seq(repo_seq)? {
                Some(previous_repo_seq) => Some(
                    self.historical_root_after_projection(&mut store, previous_repo_seq, None)
                        .await?
                        .1,
                ),
                None => None,
            };
            let (_, before_path) = match previous_root.as_ref() {
                Some(root) => {
                    self.historical_item_and_live_path(&mut store, root, &item_id)
                        .await?
                }
                None => (None, None),
            };
            let (current_item, after_path) = self
                .historical_item_and_live_path(&mut store, &current_root, &item_id)
                .await?;
            let current_item = current_item.ok_or_else(|| {
                LocalError::Integrity(format!(
                    "history root at repository sequence {repo_seq} is missing {item_id}"
                ))
            })?;
            let last_item_seq = events.last().expect("event group is nonempty").item_seq;
            if current_item.item_type != item_type || current_item.head_seq != last_item_seq {
                return Err(LocalError::Integrity(format!(
                    "history root at repository sequence {repo_seq} disagrees for {item_id}"
                )));
            }
            let changes = sort_changes(render_history_changes(
                &item_type,
                &profiles,
                before_path,
                after_path,
            ));
            if changes.is_empty() {
                continue;
            }
            output.push(history_commit(commit, changes)?);
        }
        Ok(output)
    }

    pub(crate) fn resolve_current_item_path(&self, display: &str) -> Result<(ItemId, String)> {
        let connection = self.storage().open_readonly().map_err(|error| {
            LocalError::Integrity(format!("cannot resolve current item path: {error}"))
        })?;
        let mut parent_id: Option<ItemId> = None;
        let mut resolved = None;
        let components = display.split('/').collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            let name_key = compute_name_key(component).map_err(|error| {
                LocalError::Invalid(format!(
                    "invalid item path component {component:?}: {error}"
                ))
            })?;
            let row = connection
                .query_row(
                    "SELECT item_id, item_type, name FROM vcs_items
                     WHERE repo_id = ?1 AND parent_folder_id IS ?2 AND name_key = ?3
                       AND lifecycle_state = 'LIVE' AND item_type IN ('FILE', 'FOLDER')",
                    params![
                        self.config().repo_id().as_str(),
                        parent_id.as_ref().map(ItemId::as_str),
                        name_key,
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;
            let Some((item_id, item_type, stored_name)) = row else {
                return Err(LocalError::Invalid(format!(
                    "path is not a tracked live item: {display}"
                )));
            };
            if stored_name != *component || index + 1 < components.len() && item_type != "FOLDER" {
                return Err(LocalError::Invalid(format!(
                    "path is not a tracked live item: {display}"
                )));
            }
            let item_id = ItemId::parse(item_id).map_err(|error| {
                LocalError::Integrity(format!("invalid stored item ID: {error}"))
            })?;
            parent_id = Some(item_id.clone());
            resolved = Some((item_id, item_type));
        }
        resolved.ok_or_else(|| LocalError::Invalid("path does not name an item".to_string()))
    }

    async fn item_event_groups(
        &self,
        item_id: &ItemId,
    ) -> Result<Vec<(i64, CommitId, Vec<EventRow>)>> {
        let mut groups: Vec<(i64, CommitId, Vec<EventRow>)> = Vec::new();
        let mut from_seq = 1_i64;
        loop {
            let events = self
                .core()
                .list_events(
                    self.config().repo_id().clone(),
                    item_id.clone(),
                    from_seq,
                    PAGE_SIZE,
                )
                .await?;
            #[cfg(test)]
            {
                record_historical_work(
                    self.config().repo_id(),
                    HistoricalWorkKind::ItemEventPage,
                    1,
                );
                record_historical_work(
                    self.config().repo_id(),
                    HistoricalWorkKind::ItemEventRow,
                    events.len(),
                );
            }
            if events.is_empty() {
                break;
            }
            let count = events.len();
            for event in events {
                if event.item_id != *item_id || event.item_seq != from_seq {
                    return Err(LocalError::Integrity(format!(
                        "history event sequence disagrees for {item_id}"
                    )));
                }
                let repo_seq = event.repo_seq.ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "history event {} for {item_id} is not sequenced",
                        event.item_seq
                    ))
                })?;
                if let Some((group_repo_seq, group_commit_id, group_events)) = groups.last_mut()
                    && *group_repo_seq == repo_seq
                {
                    if *group_commit_id != event.commit_id {
                        return Err(LocalError::Integrity(format!(
                            "repository sequence {repo_seq} contains multiple history commits"
                        )));
                    }
                    group_events.push(event);
                } else {
                    groups.push((repo_seq, event.commit_id.clone(), vec![event]));
                }
                from_seq = from_seq.checked_add(1).ok_or_else(|| {
                    LocalError::Integrity(format!("history item sequence overflow for {item_id}"))
                })?;
            }
            if count < PAGE_SIZE {
                break;
            }
        }
        Ok(groups)
    }

    async fn historical_item_and_live_path<S: StateIndexStore>(
        &self,
        store: &mut S,
        root: &RootHeader,
        item_id: &ItemId,
    ) -> Result<(Option<ItemValue>, Option<String>)> {
        let mut state = StateTree::new(&mut *store);
        #[cfg(test)]
        record_historical_work(self.config().repo_id(), HistoricalWorkKind::PathItemRead, 1);
        let Some(selected) = state
            .get_item(root.item_map_root, item_id)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?
        else {
            return Ok((None, None));
        };
        if selected.lifecycle_state != LifecycleState::Live {
            return Ok((Some(selected), None));
        }
        let mut current = selected.clone();
        let mut components = Vec::new();
        let mut visiting = HashSet::new();
        loop {
            if !visiting.insert(current.item_id.clone()) {
                return Err(LocalError::Integrity(format!(
                    "historical state contains a parent cycle at {}",
                    current.item_id
                )));
            }
            if current.lifecycle_state != LifecycleState::Live
                || !matches!(current.item_type.as_str(), "FILE" | "FOLDER")
            {
                return Err(LocalError::Integrity(format!(
                    "historical path contains an invalid item {}",
                    current.item_id
                )));
            }
            let namespace =
                NamespaceKey::new(current.parent_folder_id.clone(), current.name_key.clone());
            let namespace_item = state
                .get_namespace(root.namespace_root, &namespace)
                .await
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
            #[cfg(test)]
            record_historical_work(
                self.config().repo_id(),
                HistoricalWorkKind::PathNamespaceRead,
                1,
            );
            if namespace_item.as_ref() != Some(&current.item_id) {
                return Err(LocalError::Integrity(format!(
                    "historical namespace disagrees for item {}",
                    current.item_id
                )));
            }
            components.push(current.name.clone());
            let Some(parent_id) = current.parent_folder_id.clone() else {
                break;
            };
            #[cfg(test)]
            record_historical_work(self.config().repo_id(), HistoricalWorkKind::PathItemRead, 1);
            current = state
                .get_item(root.item_map_root, &parent_id)
                .await
                .map_err(|error| LocalError::Integrity(error.to_string()))?
                .ok_or_else(|| {
                    LocalError::Integrity(format!("historical state is missing parent {parent_id}"))
                })?;
            if current.item_type != "FOLDER" {
                return Err(LocalError::Integrity(format!(
                    "historical item {item_id} has non-folder parent {parent_id}"
                )));
            }
        }
        components.reverse();
        Ok((Some(selected), Some(components.join("/"))))
    }

    #[cfg(test)]
    fn historical_work_counts(&self) -> [usize; 6] {
        let repo_id = self.config().repo_id().as_str();
        let records = HISTORICAL_WORK.lock().unwrap();
        let mut counts = [0; 6];
        for (_, kind, count) in records
            .iter()
            .filter(|(recorded, _, _)| recorded == repo_id)
        {
            let index = match kind {
                HistoricalWorkKind::StateStore => 0,
                HistoricalWorkKind::CommitDelta => 1,
                HistoricalWorkKind::ItemEventPage => 2,
                HistoricalWorkKind::ItemEventRow => 3,
                HistoricalWorkKind::PathItemRead => 4,
                HistoricalWorkKind::PathNamespaceRead => 5,
            };
            counts[index] += count;
        }
        counts
    }

    pub async fn deleted(&self) -> Result<Vec<DeletedEntry>> {
        self.recover().await?;
        let replay = self.replay_history(None).await?;
        let mut output = Vec::new();
        for (item_id, node) in &replay.nodes {
            if node.live {
                continue;
            }
            let deletion = node.deletion.as_ref().ok_or_else(|| {
                LocalError::Integrity(format!("trashed item {item_id} has no deletion event"))
            })?;
            output.push(DeletedEntry {
                item_id: item_id.to_string(),
                item_type: node_kind_name(node.kind).to_string(),
                path: history_path(item_id, &replay.nodes)?
                    .expect("replayed item has a historical path"),
                repo_seq: deletion.repo_seq,
                commit_id: deletion.commit_id.to_string(),
                deleted_at: deletion.deleted_at,
            });
        }
        output.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.item_id.cmp(&right.item_id))
        });
        Ok(output)
    }

    pub async fn recover_item(&self, input: &str, message: &str) -> Result<String> {
        if message.is_empty() {
            return Err(LocalError::Invalid(
                "recovery message must not be empty".to_string(),
            ));
        }
        let root_item_id = match ItemId::parse(input) {
            Ok(id) if id.as_str() == input => id,
            _ => {
                let matches = self
                    .deleted()
                    .await?
                    .into_iter()
                    .filter(|entry| entry.path == input)
                    .collect::<Vec<_>>();
                if matches.len() != 1 {
                    return Err(LocalError::Invalid("recovery path must identify exactly one deleted item; use its item ID when ambiguous".into()));
                }
                ItemId::parse(&matches[0].item_id)
                    .map_err(|e| LocalError::Integrity(e.to_string()))?
            }
        };
        let (_, _, changes, _) = self.changes().await?;
        if !changes.is_empty() {
            return Err(LocalError::Invalid(
                "synck recover requires a clean working tree".to_string(),
            ));
        }
        let replay = self.replay_history(None).await?;
        let intent = build_recover_intent(&root_item_id, message, &replay.nodes)?;
        if self.config().reserves_file_version_namespace() {
            reject_reserved_recover_items(&intent.items)?;
        }
        if intent
            .items
            .iter()
            .any(|item| !self.checkout_observes(&item.path))
        {
            return Err(LocalError::Invalid(
                "recovery crosses the checkout selection; widen it first".into(),
            ));
        }
        validate_recover_destinations(self.root_path(), &intent, false)?;
        self.validate_recover_objects(&intent)?;
        self.write_recover_intent(&intent)?;
        self.complete_recover_intent(&intent, true).await
    }

    pub(crate) async fn recover_pending_recover(&self) -> Result<()> {
        let Some(intent) = self.read_recover_intent()? else {
            return Ok(());
        };
        self.complete_recover_intent(&intent, false)
            .await
            .map(|_| ())
    }

    async fn complete_recover_intent(
        &self,
        intent: &RecoverIntent,
        objects_validated: bool,
    ) -> Result<String> {
        if self.config().reserves_file_version_namespace() {
            reject_reserved_recover_items(&intent.items)
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
        }
        self.recover().await?;
        let replay = self.replay_history(None).await?;
        let accepted = validate_recover_intent(self.root_path(), intent, &replay.nodes)?;
        let commit_id = if accepted {
            self.operation_commit_id(&intent.operation_id)?
                .ok_or_else(|| {
                    LocalError::Integrity("accepted recover intent has no commit".to_string())
                })?
        } else {
            validate_recover_destinations(self.root_path(), intent, false)?;
            if !objects_validated {
                self.validate_recover_objects(intent)?;
            }
            self.append_recover_intent(intent)
                .await?
                .commit_id
                .to_string()
        };
        if let Err(error) = self.recover().await {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: error.to_string(),
            });
        }
        let replay =
            self.replay_history(None)
                .await
                .map_err(|error| LocalError::AcceptedNeedsRecovery {
                    commit_id: commit_id.clone(),
                    detail: error.to_string(),
                })?;
        if !validate_recover_intent(self.root_path(), intent, &replay.nodes).map_err(|error| {
            LocalError::AcceptedNeedsRecovery {
                commit_id: commit_id.clone(),
                detail: error.to_string(),
            }
        })? {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: "recovery commit did not restore the intended items".to_string(),
            });
        }
        let materialized = async {
            self.materialize_recover_intent(intent)?;
            validate_recover_destinations(self.root_path(), intent, true)?;
            self.refresh_worktree_identities().await?;
            self.clear_recover_intent()
        }
        .await;
        if let Err(error) = materialized {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: error.to_string(),
            });
        }
        Ok(commit_id)
    }

    fn validate_recover_objects(&self, intent: &RecoverIntent) -> Result<()> {
        for item in &intent.items {
            if item.item_type != "FILE" {
                continue;
            }
            let blob_id = item
                .blob_id
                .as_ref()
                .expect("validated recovered FILE has a blob");
            let size = u64::try_from(item.size.expect("validated recovered FILE has a size"))
                .map_err(|_| {
                    LocalError::Integrity(format!("negative object size for {blob_id}"))
                })?;
            self.verify_blob(blob_id, size)?;
        }
        Ok(())
    }

    async fn append_recover_intent(
        &self,
        intent: &RecoverIntent,
    ) -> Result<synck_vcs::types::AcceptedCommit> {
        let root = intent
            .items
            .iter()
            .find(|item| item.item_id == intent.root_item_id)
            .ok_or_else(|| LocalError::Integrity("recover intent has no root item".to_string()))?;
        let mut descendants = intent
            .items
            .iter()
            .filter(|item| item.item_id != intent.root_item_id)
            .map(|item| ItemBase {
                item_id: item.item_id.clone(),
                base_seq: item.base_seq,
            })
            .collect::<Vec<_>>();
        descendants.sort_by(|left, right| left.item_id.cmp(&right.item_id));
        let mut intents = vec![Intent::Restore {
            root: ItemBase {
                item_id: root.item_id.clone(),
                base_seq: root.base_seq,
            },
            trash_commit_id: intent.deletion_commit_id.clone(),
            descendants,
            location: None,
        }];
        let current = self.current_file_folder_context(&mut intents)?;
        let materialized = validate_and_materialize(
            self.config().repo_id(),
            &IntentProposal {
                message: Some(intent.message.clone()),
                intents,
            },
            &current,
        )?;
        self.core()
            .append_file_folder_checked(
                self.config().repo_id().clone(),
                CommitProposal {
                    author_id: self.config().author().to_string(),
                    committer_id: self.config().author().to_string(),
                    message: Some(intent.message.clone()),
                    occurred_at: None,
                    operation_id: Some(intent.operation_id.clone()),
                    events: materialized.events,
                },
                &[],
                &materialized.subtree_guards,
            )
            .await
            .map_err(Into::into)
    }

    pub(crate) fn operation_commit_id(&self, operation_id: &str) -> Result<Option<String>> {
        let connection = self.storage().open_readonly().map_err(|error| {
            LocalError::Invalid(format!("cannot read repository database: {error}"))
        })?;
        let digest: Option<String> = connection
            .query_row(
                "SELECT lower(hex(commit_id)) FROM vcs_commits
                 WHERE repo_id = ?1 AND operation_id = ?2",
                params![self.config().repo_id().as_str(), operation_id],
                |row| row.get(0),
            )
            .optional()?;
        digest
            .map(|digest| {
                CommitId::parse(format!("sha256:{digest}"))
                    .map(|commit_id| commit_id.to_string())
                    .map_err(|error| {
                        LocalError::Integrity(format!("operation has invalid commit ID: {error}"))
                    })
            })
            .transpose()
    }

    fn materialize_recover_intent(&self, intent: &RecoverIntent) -> Result<()> {
        for item in &intent.items {
            if item.item_type == "FOLDER" {
                self.materialize_recovered_folder(&item.path)?;
            }
        }
        for item in &intent.items {
            if item.item_type == "FILE" {
                self.materialize_recovered_file(
                    &item.path,
                    item.blob_id
                        .as_ref()
                        .expect("validated recovered FILE has a blob"),
                    item.size.expect("validated recovered FILE has a size"),
                )?;
            }
        }
        Ok(())
    }

    fn materialize_recovered_folder(&self, display: &str) -> Result<()> {
        let relative = display_to_path(display);
        let parent = relative
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let name = relative.file_name().ok_or_else(|| {
            LocalError::Integrity(format!("invalid recovery folder path: {display}"))
        })?;
        let parent_dir = self
            .root()
            .open_dir_nofollow(parent)
            .at("open recovery parent", parent)?;
        match parent_dir.symlink_metadata(name) {
            Ok(metadata) if metadata.file_type().is_dir() && !is_cap_link_or_reparse(&metadata) => {
            }
            Ok(_) => {
                return Err(LocalError::Invalid(format!(
                    "recovery target is not a directory: {display}"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                parent_dir
                    .create_dir(name)
                    .at("create recovered directory", display)?;
                sync_directory(&self.root_path().join(parent))?;
            }
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect recovery target",
                    path: relative,
                    source,
                });
            }
        }
        Ok(())
    }

    fn materialize_recovered_file(
        &self,
        display: &str,
        blob_id: &BlobId,
        expected_size: i64,
    ) -> Result<()> {
        let relative = display_to_path(display);
        match self.root().symlink_metadata(&relative) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() || is_cap_link_or_reparse(&metadata) {
                    return Err(LocalError::Invalid(format!(
                        "recovery target is not a regular file: {display}"
                    )));
                }
                verify_working_file(self.root_path(), display, blob_id, expected_size)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.write_working_file(display, blob_id, expected_size, false)
            }
            Err(source) => Err(LocalError::Io {
                action: "inspect recovery target",
                path: relative,
                source,
            }),
        }
    }

    async fn replay_history(&self, through_repo_seq: Option<i64>) -> Result<ReplayResult> {
        if self.checkout_paths().is_some() {
            self.catch_up_state_index().await?;
            let head = if let Some(seq) = through_repo_seq {
                self.commit_at_repo_seq(seq).await?
            } else {
                match self.log(Some(1)).await?.first() {
                    Some(head) => self.commit_at_repo_seq(head.repo_seq).await?,
                    None => None,
                }
            };
            let Some(head) = head else {
                return Ok(ReplayResult {
                    nodes: HashMap::new(),
                });
            };
            let mut nodes = self
                .historical_nodes_at_commit(head.repo_seq.expect("sequenced head"), &head.commit_id)
                .await?;
            for (id, node) in nodes.iter_mut().filter(|(_, node)| !node.live) {
                let mut from = 1;
                loop {
                    let events = self
                        .core()
                        .list_events(self.repository_id().clone(), id.clone(), from, PAGE_SIZE)
                        .await?;
                    if events.is_empty() {
                        break;
                    }
                    for event in &events {
                        if event.item_seq <= node.head_seq
                            && matches!(event.event_type.as_str(), "FILE::TRASH" | "FOLDER::TRASH")
                        {
                            node.deletion = Some(DeletionInfo {
                                repo_seq: event.repo_seq.ok_or_else(|| {
                                    LocalError::Integrity("unsequenced deletion".into())
                                })?,
                                commit_id: event.commit_id.clone(),
                                deleted_at: event.created_at,
                            });
                        }
                        from = event.item_seq + 1;
                    }
                    if events.len() < PAGE_SIZE || from > node.head_seq {
                        break;
                    }
                }
            }
            return Ok(ReplayResult { nodes });
        }
        let mut nodes = HashMap::new();
        let mut item_kinds = BTreeMap::new();
        let mut cursor = None;
        let mut pending = None;
        loop {
            let events = self
                .core()
                .list_repo_commit_events(self.config().repo_id().clone(), cursor.clone(), PAGE_SIZE)
                .await?;
            if events.is_empty() {
                break;
            }
            let count = events.len();
            for event in events {
                let repo_seq = event.repo_seq().ok_or_else(|| {
                    LocalError::Integrity("sequenced event has no repository sequence".to_string())
                })?;
                if through_repo_seq.is_some_and(|maximum| repo_seq > maximum) {
                    finish_replay_commit(pending.take(), &mut nodes, &mut item_kinds)?;
                    return Ok(ReplayResult { nodes });
                }
                if pending.as_ref().is_some_and(|pending: &ReplayAccumulator| {
                    pending.commit.repo_seq != Some(repo_seq)
                }) {
                    finish_replay_commit(pending.take(), &mut nodes, &mut item_kinds)?;
                }
                cursor = Some(RepoEventCursor {
                    repo_seq,
                    commit_event_idx: event.commit_event_idx(),
                });
                if pending.is_none() {
                    let commit = self
                        .core()
                        .get_commit(self.config().repo_id().clone(), event.commit_id().clone())
                        .await?
                        .ok_or_else(|| {
                            LocalError::Integrity(format!(
                                "event references missing commit {}",
                                event.commit_id()
                            ))
                        })?;
                    pending = Some(begin_replay_commit(commit)?);
                }
                append_replay_event(pending.as_mut().expect("replay commit initialized"), event)?;
            }
            if count < PAGE_SIZE {
                break;
            }
        }
        finish_replay_commit(pending, &mut nodes, &mut item_kinds)?;
        Ok(ReplayResult { nodes })
    }

    pub async fn versions(&self, caller: &Path, input: &Path) -> Result<Vec<VersionEntry>> {
        self.recover().await?;
        let display = normalize_input_path(self.root_path(), caller, input)?;
        let committed = self.committed_tree().await?;
        let node = committed.get(&display).ok_or_else(|| {
            LocalError::Invalid(format!("path is not a tracked live file: {display}"))
        })?;
        if node.kind != NodeKind::File {
            return Err(LocalError::Invalid(format!(
                "path is not a tracked live file: {display}"
            )));
        }
        let mut output = Vec::new();
        let mut from_seq = 1_i64;
        loop {
            let events = self
                .core()
                .list_events(
                    self.config().repo_id().clone(),
                    node.item_id.clone(),
                    from_seq,
                    PAGE_SIZE,
                )
                .await?;
            if events.is_empty() {
                break;
            }
            for event in &events {
                if event.event_type == "FILE::SET_CONTENT" {
                    let proposal = parse_event(event)?;
                    if proposal.set_content.is_none() {
                        from_seq = event.item_seq + 1;
                        continue;
                    }
                    let (blob_id, size) = content_from_event(&proposal)?;
                    output.push(VersionEntry {
                        item_seq: event.item_seq,
                        commit_id: event.commit_id.to_string(),
                        created_at: event.created_at,
                        blob_id: blob_id.to_string(),
                        size,
                    });
                }
                from_seq = event.item_seq + 1;
            }
            if events.len() < PAGE_SIZE {
                break;
            }
        }
        Ok(output)
    }

    pub async fn restore(
        &self,
        caller: &Path,
        input: &Path,
        requested_seq: Option<i64>,
    ) -> Result<()> {
        let (display, blob_id, size) = self
            .resolve_file_revision(caller, input, requested_seq)
            .await?;
        self.write_working_file(&display, &blob_id, size, true)?;
        self.refresh_worktree_identities().await
    }

    pub async fn restore_worktree(&self) -> Result<bool> {
        self.restore_worktree_checked(None).await
    }

    pub(crate) async fn restore_checkout_worktree(
        &self,
        expected: &[WorktreeRestoreEntry],
    ) -> Result<()> {
        self.restore_worktree_checked(Some(expected))
            .await
            .map(|_| ())
    }

    async fn restore_worktree_checked(
        &self,
        expected: Option<&[WorktreeRestoreEntry]>,
    ) -> Result<bool> {
        self.recover_pending_worktree_restore().await?;
        let (committed, working, changes, working_ids) = self.changes().await?;
        if changes.is_empty() {
            return Ok(false);
        }
        let mut retained_directories = BTreeSet::new();
        let mut retained_sources = BTreeSet::new();
        if let Some(expected) = expected {
            let all = self.complete_file_tree().await?;
            let mut observed =
                crate::worktree::scan_canonical_without_ignore(self.root(), &all, true)?;
            self.filter_checkout_observation(&mut observed);
            if worktree_restore_manifest(&observed, &HashMap::new())? != expected {
                return Err(LocalError::Invalid("working tree changed during checkout materialization; preserve the files and retry recovery after resolving the change".into()));
            }
            let source_paths: BTreeSet<_> = observed.keys().cloned().collect();
            retained_directories = self
                .filter_imported_email_sources(&committed, &mut observed)
                .await?;
            retained_sources = source_paths
                .into_iter()
                .filter(|path| !observed.contains_key(path))
                .collect();
            retained_sources.extend(retained_directories.iter().cloned());
        }
        let head = self.log(Some(1)).await?.into_iter().next();
        let mut items = build_worktree_restore_items(&committed, &working, &working_ids)?;
        items.retain(|item| {
            !(item.item_type == "FOLDER"
                && item.before.is_none()
                && retained_directories.contains(&item.after.path))
        });
        reject_ignored_restore_descendants(
            self.root(),
            &committed,
            &items,
            &working,
            &retained_sources,
        )?;
        self.validate_inverse_objects(&items)?;
        self.validate_inverse_destinations(&items)?;
        let intent = WorktreeRestoreIntent {
            version: WORKTREE_RESTORE_INTENT_VERSION,
            base_commit_id: head
                .as_ref()
                .map(|head| {
                    CommitId::parse(&head.commit_id).map_err(|error| {
                        LocalError::Integrity(format!("repository HEAD has invalid ID: {error}"))
                    })
                })
                .transpose()?,
            base_repo_seq: head.as_ref().map_or(0, |head| head.repo_seq),
            source_tree: worktree_restore_manifest(&working, &working_ids)?,
            items,
        };
        self.write_json_intent(WORKTREE_RESTORE_INTENT_NAME, "worktree-restore", &intent)?;
        self.complete_worktree_restore(&intent, true).await?;
        Ok(true)
    }

    pub(crate) async fn recover_pending_worktree_restore(&self) -> Result<()> {
        let Some(intent) = self.read_json_intent::<WorktreeRestoreIntent>(
            WORKTREE_RESTORE_INTENT_NAME,
            "worktree restore",
        )?
        else {
            return Ok(());
        };
        self.complete_worktree_restore(&intent, false).await
    }

    async fn complete_worktree_restore(
        &self,
        intent: &WorktreeRestoreIntent,
        source_validated: bool,
    ) -> Result<()> {
        self.recover().await?;
        validate_worktree_restore_fixed_fields(intent)?;
        let latest = self.log(Some(1)).await?.into_iter().next();
        let base_matches = match (&intent.base_commit_id, latest.as_ref()) {
            (None, None) => intent.base_repo_seq == 0,
            (Some(base), Some(latest)) => {
                latest.repo_seq == intent.base_repo_seq && latest.commit_id == base.to_string()
            }
            _ => false,
        };
        if !base_matches {
            return Err(LocalError::Integrity(
                "worktree restore base is no longer repository HEAD".to_string(),
            ));
        }
        let committed = self.committed_tree().await?;
        validate_worktree_restore_target(intent, &committed)?;
        let stage = self.metadata_path().join(WORKTREE_RESTORE_STAGE_NAME);
        if !path_exists_nofollow(&stage)? {
            if self.status().await?.is_empty() {
                self.refresh_worktree_identities().await?;
                return self.clear_intent(WORKTREE_RESTORE_INTENT_NAME);
            }
            if !source_validated {
                let (_, working, _, working_ids) = self.changes().await?;
                if worktree_restore_manifest(&working, &working_ids)? != intent.source_tree {
                    return Err(LocalError::Integrity(
                        "working tree changed after the restore intent was written".to_string(),
                    ));
                }
            }
            self.validate_inverse_objects(&intent.items)?;
            self.validate_inverse_destinations(&intent.items)?;
        }
        self.materialize_inverse_items(&intent.items, WORKTREE_RESTORE_STAGE_NAME)?;
        let changes = self.status().await?;
        if !changes.is_empty() {
            return Err(LocalError::Integrity(
                "restored working tree does not match accepted history".to_string(),
            ));
        }
        self.refresh_worktree_identities().await?;
        self.remove_inverse_stage(WORKTREE_RESTORE_STAGE_NAME)?;
        self.clear_intent(WORKTREE_RESTORE_INTENT_NAME)
    }

    pub async fn export(
        &self,
        caller: &Path,
        input: &Path,
        destination: &Path,
        requested_seq: Option<i64>,
    ) -> Result<()> {
        let (_, blob_id, size) = self
            .resolve_file_revision(caller, input, requested_seq)
            .await?;
        let (parent, name, destination) =
            resolve_export_destination(caller, destination, self.metadata_path())?;
        let parent_dir = Dir::open_ambient_dir(&parent, ambient_authority())
            .at("open export parent", &parent)?;
        match parent_dir.symlink_metadata(&name) {
            Ok(_) => {
                return Err(LocalError::Invalid(format!(
                    "export destination already exists: {}",
                    destination.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect export destination",
                    path: destination,
                    source,
                });
            }
        }
        let temporary =
            self.copy_object_to_temporary(&parent_dir, &parent, &destination, &blob_id, size)?;
        self.publish_temporary_file(&parent_dir, &parent, &temporary, &name, &destination, false)
    }

    pub async fn export_tree(
        &self,
        caller: &Path,
        revision: &str,
        folder: Option<&Path>,
        destination: &Path,
    ) -> Result<()> {
        self.recover().await?;
        let tree = if let Some(folder) = folder {
            let selected = normalize_input_path(self.root_path(), caller, folder)?;
            self.historical_subtree_at(revision, &selected).await?.1
        } else {
            self.historical_tree_at(revision).await?.1
        };

        let (parent, name, destination) =
            resolve_export_destination(caller, destination, self.metadata_path())?;
        let temporary_name = OsString::from(format!(
            ".{}.synck-tmp-{}",
            name.to_string_lossy(),
            Uuid::new_v4()
        ));
        let temporary = parent.join(&temporary_name);
        std::fs::create_dir(&temporary).at("create historical export staging", &temporary)?;
        let result = (|| -> Result<()> {
            materialize_tree(self, &temporary, &tree)?;
            let staged = Dir::open_ambient_dir(&temporary, ambient_authority())
                .at("open historical export staging", &temporary)?;
            let working = crate::worktree::scan_materialized_tree(&staged, &tree)?;
            validate_materialized_tree(&tree, &working)?;
            drop(staged);
            rename_noreplace(&temporary, &destination)
                .at("publish historical export", &destination)?;
            sync_directory(&parent)
        })();
        if result.is_err() && path_exists_nofollow(&temporary)? {
            remove_tree_nofollow(&temporary)?;
            sync_directory(&parent)?;
        }
        result
    }

    async fn resolve_file_revision(
        &self,
        caller: &Path,
        input: &Path,
        requested_seq: Option<i64>,
    ) -> Result<(String, BlobId, i64)> {
        self.recover().await?;
        let display = normalize_input_path(self.root_path(), caller, input)?;
        let committed = self.committed_tree().await?;
        let node = committed.get(&display).ok_or_else(|| {
            LocalError::Invalid(format!("path is not a tracked live file: {display}"))
        })?;
        if node.kind != NodeKind::File {
            return Err(LocalError::Invalid(format!(
                "path is not a tracked live file: {display}"
            )));
        }
        let (blob_id, size) = match requested_seq {
            None => (
                node.blob_id
                    .clone()
                    .expect("a committed FILE has a blob projection"),
                node.size.expect("a committed FILE has projected size"),
            ),
            Some(item_seq) if item_seq > 0 => {
                let events = self
                    .core()
                    .list_events(
                        self.config().repo_id().clone(),
                        node.item_id.clone(),
                        item_seq,
                        1,
                    )
                    .await?;
                let event = events
                    .first()
                    .filter(|event| event.item_seq == item_seq)
                    .ok_or_else(|| {
                        LocalError::Invalid(format!(
                            "item sequence {item_seq} does not exist for {display}"
                        ))
                    })?;
                if event.event_type != "FILE::SET_CONTENT" {
                    return Err(LocalError::Invalid(format!(
                        "item sequence {item_seq} is not a file content version"
                    )));
                }
                let proposal = parse_event(event)?;
                if proposal.set_content.is_none() {
                    return Err(LocalError::Invalid(format!(
                        "item sequence {item_seq} is not a file content version"
                    )));
                }
                content_from_event(&proposal)?
            }
            Some(_) => {
                return Err(LocalError::Invalid(
                    "item sequence must be greater than zero".to_string(),
                ));
            }
        };
        Ok((display, blob_id, size))
    }

    fn capture_changed_files(
        &self,
        working: &BTreeMap<String, WorkingNode>,
        changes: &[Change],
    ) -> Result<HashMap<String, BlobId>> {
        let mut objects = HashMap::new();
        for change in changes {
            if !matches!(change.kind, ChangeKind::Add | ChangeKind::Modify)
                && !change.kind.is_relocation()
            {
                continue;
            }
            let Some(node) = working.get(&change.path) else {
                continue;
            };
            if node.kind != NodeKind::File {
                continue;
            }
            if change.kind.is_relocation() && !change.content_changed {
                continue;
            }
            let digest = node.digest_hex.as_deref().expect("scanned FILE has digest");
            let size = node.size.expect("scanned FILE has size");
            let relative = display_to_path(&change.path);
            let blob_id =
                self.install_object_before_commit(&relative, &change.path, digest, size)?;
            objects.insert(change.path.clone(), blob_id);
        }
        if !objects.is_empty() {
            self.checkpoint_commit_objects()?;
        }
        Ok(objects)
    }

    pub(crate) fn write_working_file(
        &self,
        display: &str,
        blob_id: &BlobId,
        expected_size: i64,
        overwrite: bool,
    ) -> Result<()> {
        let relative = display_to_path(display);
        let parent = match relative.parent() {
            Some(path) if !path.as_os_str().is_empty() => path,
            _ => Path::new("."),
        };
        let name = relative
            .file_name()
            .ok_or_else(|| LocalError::Invalid(format!("invalid restore path: {display}")))?;
        let parent_dir = self
            .root()
            .open_dir_nofollow(parent)
            .at("open restore parent", parent)?;
        match parent_dir.symlink_metadata(name) {
            Ok(metadata) => {
                if !overwrite {
                    return Err(LocalError::Invalid(format!(
                        "recovery target already exists: {display}"
                    )));
                }
                if !metadata.file_type().is_file() || is_cap_link_or_reparse(&metadata) {
                    return Err(LocalError::Invalid(format!(
                        "restore target is not a regular file: {display}"
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect restore target",
                    path: relative,
                    source,
                });
            }
        }
        let temporary = self.copy_object_to_temporary(
            &parent_dir,
            &self.root_path().join(parent),
            Path::new(display),
            blob_id,
            expected_size,
        )?;
        self.publish_temporary_file(
            &parent_dir,
            &self.root_path().join(parent),
            &temporary,
            name,
            Path::new(display),
            overwrite,
        )
    }

    fn copy_object_to_temporary(
        &self,
        parent_dir: &Dir,
        parent: &Path,
        display: &Path,
        blob_id: &BlobId,
        expected_size: i64,
    ) -> Result<String> {
        let temporary_name = format!(".synck-tmp-{}", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .create_new(true)
            .write(true)
            .follow(FollowSymlinks::No);
        let mut temporary = parent_dir
            .open_with(&temporary_name, &options)
            .at("create temporary file", display)?;
        if let Err(error) = (|| -> Result<()> {
            let expected_size = u64::try_from(expected_size).map_err(|_| {
                LocalError::Integrity(format!("negative object size for {blob_id}"))
            })?;
            self.copy_blob(blob_id, expected_size, &mut temporary)?;
            temporary.sync_all().at("sync", display)
        })() {
            parent_dir
                .remove_file(&temporary_name)
                .at("remove temporary file", display)?;
            sync_directory(parent)?;
            return Err(error);
        }
        Ok(temporary_name)
    }

    fn publish_temporary_file(
        &self,
        parent_dir: &Dir,
        parent: &Path,
        temporary_name: &str,
        name: &OsStr,
        display: &Path,
        overwrite: bool,
    ) -> Result<()> {
        let temporary = parent.join(temporary_name);
        let destination = parent.join(name);
        let result = (|| -> Result<()> {
            if overwrite {
                #[cfg(not(windows))]
                parent_dir
                    .rename(temporary_name, parent_dir, name)
                    .at("replace", display)?;
                #[cfg(windows)]
                replace_file_windows(&temporary, &destination).at("replace", display)?;
            } else {
                rename_noreplace(&temporary, &destination).at("publish file", display)?;
            }
            sync_directory(parent)
        })();
        if result.is_err() && path_exists_nofollow(&temporary)? {
            parent_dir
                .remove_file(temporary_name)
                .at("remove temporary file", &temporary)?;
            sync_directory(parent)?;
        }
        result
    }
}

fn build_revert_intent(
    repo_id: &RepoId,
    selected: (&CommitId, i64),
    publish_base: (&CommitId, i64),
    message: &str,
    before: &HashMap<ItemId, HistoryNode>,
    after: &HashMap<ItemId, HistoryNode>,
    current: &HashMap<ItemId, HistoryNode>,
) -> Result<RevertIntent> {
    let (base_commit_id, base_repo_seq) = selected;
    let (publish_base_commit_id, publish_base_repo_seq) = publish_base;
    let selected = derive_revert_items(before, after)?;
    let items = prepare_revert_items(&selected, after, current)?;
    if items.is_empty() {
        return Err(LocalError::Invalid(
            "selected commit has no reversible working-tree change".to_string(),
        ));
    }
    if items.len() > i32::MAX as usize {
        return Err(LocalError::Invalid(
            "revert exceeds SQLite event count range".to_string(),
        ));
    }
    let materialized = materialize_revert(repo_id, message, &items, current)?;
    Ok(RevertIntent {
        version: REVERT_INTENT_VERSION,
        operation_id: Uuid::new_v4().to_string(),
        base_commit_id: base_commit_id.clone(),
        base_repo_seq,
        publish_base_commit_id: publish_base_commit_id.clone(),
        publish_base_repo_seq,
        message: message.to_string(),
        items,
        events: materialized.events,
        subtree_guards: materialized
            .subtree_guards
            .into_iter()
            .map(|guard| DurableSubtreeGuard {
                root_item_id: guard.root_item_id,
                expected_non_tombstoned_descendants: guard.expected_non_tombstoned_descendants,
            })
            .collect(),
    })
}

fn materialize_revert(
    repo_id: &RepoId,
    message: &str,
    items: &[RevertIntentItem],
    current_nodes: &HashMap<ItemId, HistoryNode>,
) -> Result<synck_file_folder::MaterializedProposal> {
    let all_current = profile_current_from_history(current_nodes)?;
    let children = profile_children(&all_current);
    let trash_ids = items
        .iter()
        .filter(|item| item.after.live && item.before.as_ref().is_none_or(|before| !before.live))
        .map(|item| item.item_id.clone())
        .collect::<BTreeSet<_>>();
    let restore_ids = items
        .iter()
        .filter(|item| !item.after.live && item.before.as_ref().is_some_and(|before| before.live))
        .map(|item| item.item_id.clone())
        .collect::<BTreeSet<_>>();
    let mut intents = Vec::new();

    for root_id in selected_lifecycle_roots(&trash_ids, &all_current)? {
        let root = &all_current[&root_id];
        let mut descendants = profile_descendants(&root_id, &all_current, &children)?
            .into_iter()
            .filter(|item| item.lifecycle_state == ProfileLifecycleState::Live)
            .map(|item| ItemBase {
                item_id: item.item_id.clone(),
                base_seq: item.head_seq,
            })
            .collect::<Vec<_>>();
        descendants.sort_by(|left, right| left.item_id.cmp(&right.item_id));
        intents.push(Intent::Trash {
            root: ItemBase {
                item_id: root_id,
                base_seq: root.head_seq,
            },
            descendants,
        });
    }
    for root_id in selected_lifecycle_roots(&restore_ids, &all_current)? {
        let root = &all_current[&root_id];
        let trash_commit_id = root.trash_commit_id.clone().ok_or_else(|| {
            LocalError::Integrity(format!("revert restore root {root_id} has no trash commit"))
        })?;
        let mut descendants = profile_descendants(&root_id, &all_current, &children)?
            .into_iter()
            .filter(|item| {
                item.lifecycle_state == ProfileLifecycleState::Trash
                    && item.trash_commit_id.as_ref() == Some(&trash_commit_id)
            })
            .map(|item| ItemBase {
                item_id: item.item_id.clone(),
                base_seq: item.head_seq,
            })
            .collect::<Vec<_>>();
        descendants.sort_by(|left, right| left.item_id.cmp(&right.item_id));
        let before = items
            .iter()
            .find(|item| item.item_id == root_id)
            .and_then(|item| item.before.as_ref())
            .expect("restore root has a live before state");
        let location =
            (root.parent_item_id != before.parent_id || root.name != before.name).then(|| {
                Location {
                    parent_item_id: before.parent_id.clone(),
                    name: before.name.clone(),
                }
            });
        intents.push(Intent::Restore {
            root: ItemBase {
                item_id: root_id,
                base_seq: root.head_seq,
            },
            trash_commit_id,
            descendants,
            location,
        });
    }
    for item in items {
        let Some(before) = item.before.as_ref() else {
            continue;
        };
        if before.live != item.after.live {
            continue;
        }
        let location_changed =
            before.parent_id != item.after.parent_id || before.name != item.after.name;
        let location = location_changed.then(|| Location {
            parent_item_id: before.parent_id.clone(),
            name: before.name.clone(),
        });
        match item.item_type.as_str() {
            "FOLDER" => {
                if let Some(location) = location {
                    intents.push(Intent::UpdateFolder {
                        item_id: item.item_id.clone(),
                        base_seq: item.after.head_seq,
                        location,
                    });
                }
            }
            "FILE" => {
                let content_changed =
                    before.blob_id != item.after.blob_id || before.size != item.after.size;
                let content = content_changed
                    .then(|| revert_blob_ref(item, before))
                    .transpose()?;
                if location.is_some() || content.is_some() {
                    intents.push(Intent::UpdateFile {
                        item_id: item.item_id.clone(),
                        base_seq: item.after.head_seq,
                        location,
                        content,
                    });
                }
            }
            other => {
                return Err(LocalError::Integrity(format!(
                    "revert item has unsupported type {other}"
                )));
            }
        }
    }
    let current = profile_context_for_intents(&intents, &all_current, &children)?;
    validate_and_materialize(
        repo_id,
        &IntentProposal {
            message: Some(message.to_string()),
            intents,
        },
        &current,
    )
    .map_err(LocalError::from)
}

fn profile_current_from_history(
    nodes: &HashMap<ItemId, HistoryNode>,
) -> Result<BTreeMap<ItemId, CurrentItem>> {
    nodes
        .iter()
        .map(|(item_id, node)| {
            let content = match node.kind {
                NodeKind::File => Some(BlobRef {
                    blob_id: node.blob_id.clone().ok_or_else(|| {
                        LocalError::Integrity(format!("historical FILE {item_id} has no blob"))
                    })?,
                    size: u64::try_from(node.size.ok_or_else(|| {
                        LocalError::Integrity(format!("historical FILE {item_id} has no size"))
                    })?)
                    .map_err(|_| {
                        LocalError::Integrity(format!(
                            "historical FILE {item_id} has negative size"
                        ))
                    })?,
                }),
                NodeKind::Folder => None,
            };
            let lifecycle_state = if node.live {
                ProfileLifecycleState::Live
            } else {
                ProfileLifecycleState::Trash
            };
            Ok((
                item_id.clone(),
                CurrentItem {
                    item_id: item_id.clone(),
                    item_type: node_kind_name(node.kind).to_string(),
                    parent_item_id: node.parent_id.clone(),
                    name: node.name.clone(),
                    name_key: compute_name_key(&node.name).map_err(|error| {
                        LocalError::Integrity(format!(
                            "invalid historical item name {:?}: {error}",
                            node.name
                        ))
                    })?,
                    head_seq: node.head_seq,
                    lifecycle_state,
                    trash_commit_id: node
                        .deletion
                        .as_ref()
                        .map(|deletion| deletion.commit_id.clone()),
                    content,
                },
            ))
        })
        .collect()
}

fn selected_lifecycle_roots(
    selected: &BTreeSet<ItemId>,
    current: &BTreeMap<ItemId, CurrentItem>,
) -> Result<Vec<ItemId>> {
    let mut roots = Vec::new();
    for item_id in selected {
        let mut parent = current
            .get(item_id)
            .and_then(|item| item.parent_item_id.as_ref());
        let mut visited = HashSet::new();
        let mut nested = false;
        while let Some(parent_id) = parent {
            if !visited.insert(parent_id.clone()) {
                return Err(LocalError::Integrity(format!(
                    "historical folder cycle at {parent_id}"
                )));
            }
            if selected.contains(parent_id) {
                nested = true;
                break;
            }
            parent = current
                .get(parent_id)
                .and_then(|item| item.parent_item_id.as_ref());
        }
        if !nested {
            roots.push(item_id.clone());
        }
    }
    Ok(roots)
}

fn profile_descendants<'a>(
    root_id: &ItemId,
    current: &'a BTreeMap<ItemId, CurrentItem>,
    children: &BTreeMap<ItemId, Vec<ItemId>>,
) -> Result<Vec<&'a CurrentItem>> {
    let mut descendants = Vec::new();
    let mut pending = children.get(root_id).cloned().unwrap_or_default();
    let mut visited = BTreeSet::new();
    while let Some(item_id) = pending.pop() {
        if !visited.insert(item_id.clone()) {
            return Err(LocalError::Integrity(format!(
                "historical folder cycle at {item_id}"
            )));
        }
        descendants.push(&current[&item_id]);
        if let Some(nested) = children.get(&item_id) {
            pending.extend(nested.iter().cloned());
        }
    }
    Ok(descendants)
}

fn profile_children(current: &BTreeMap<ItemId, CurrentItem>) -> BTreeMap<ItemId, Vec<ItemId>> {
    let mut children = BTreeMap::<ItemId, Vec<ItemId>>::new();
    for item in current.values() {
        if let Some(parent_id) = &item.parent_item_id {
            children
                .entry(parent_id.clone())
                .or_default()
                .push(item.item_id.clone());
        }
    }
    children
}

fn profile_context_for_intents(
    intents: &[Intent],
    all: &BTreeMap<ItemId, CurrentItem>,
    children: &BTreeMap<ItemId, Vec<ItemId>>,
) -> Result<BTreeMap<ItemId, CurrentItem>> {
    let created = intents
        .iter()
        .filter_map(|intent| match intent {
            Intent::CreateFolder { item_id, .. } | Intent::CreateFile { item_id, .. } => {
                Some(item_id.clone())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut required = BTreeSet::new();
    let mut ancestor_frontier = BTreeSet::new();
    let mut namespace_keys = BTreeSet::new();

    for intent in intents {
        match intent {
            Intent::CreateFolder { location, .. } | Intent::CreateFile { location, .. } => {
                record_profile_location(
                    location,
                    &created,
                    &mut ancestor_frontier,
                    &mut namespace_keys,
                )?;
            }
            Intent::UpdateFolder {
                item_id, location, ..
            } => {
                required.insert(item_id.clone());
                record_profile_location(
                    location,
                    &created,
                    &mut ancestor_frontier,
                    &mut namespace_keys,
                )?;
            }
            Intent::UpdateFile {
                item_id, location, ..
            } => {
                required.insert(item_id.clone());
                if let Some(location) = location {
                    record_profile_location(
                        location,
                        &created,
                        &mut ancestor_frontier,
                        &mut namespace_keys,
                    )?;
                }
            }
            Intent::Trash { root, descendants } => {
                required.insert(root.item_id.clone());
                required.extend(descendants.iter().map(|item| item.item_id.clone()));
                required.extend(
                    profile_descendants(&root.item_id, all, children)?
                        .into_iter()
                        .map(|item| item.item_id.clone()),
                );
            }
            Intent::Restore {
                root,
                trash_commit_id,
                descendants,
                location,
            } => {
                required.insert(root.item_id.clone());
                required.extend(descendants.iter().map(|item| item.item_id.clone()));
                let closure = profile_descendants(&root.item_id, all, children)?;
                required.extend(closure.iter().map(|item| item.item_id.clone()));
                let root_item = &all[&root.item_id];
                let root_location = location.clone().unwrap_or_else(|| Location {
                    parent_item_id: root_item.parent_item_id.clone(),
                    name: root_item.name.clone(),
                });
                record_profile_location(
                    &root_location,
                    &created,
                    &mut ancestor_frontier,
                    &mut namespace_keys,
                )?;
                for item in closure {
                    if item.lifecycle_state == ProfileLifecycleState::Trash
                        && item.trash_commit_id.as_ref() == Some(trash_commit_id)
                    {
                        namespace_keys.insert((item.parent_item_id.clone(), item.name_key.clone()));
                    }
                }
            }
        }
    }

    let mut visited = BTreeSet::new();
    while let Some(item_id) = ancestor_frontier.pop_first() {
        if created.contains(&item_id) || !visited.insert(item_id.clone()) {
            continue;
        }
        let item = all.get(&item_id).ok_or_else(|| {
            LocalError::Integrity(format!("historical parent {item_id} is missing"))
        })?;
        required.insert(item_id);
        if let Some(parent_id) = &item.parent_item_id {
            ancestor_frontier.insert(parent_id.clone());
        }
    }

    for item in all.values() {
        if item.lifecycle_state == ProfileLifecycleState::Live
            && namespace_keys.contains(&(item.parent_item_id.clone(), item.name_key.clone()))
        {
            required.insert(item.item_id.clone());
        }
    }
    required
        .into_iter()
        .map(|item_id| {
            all.get(&item_id)
                .cloned()
                .map(|item| (item_id.clone(), item))
                .ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "historical FILE/FOLDER context is missing {item_id}"
                    ))
                })
        })
        .collect()
}

fn record_profile_location(
    location: &Location,
    created: &BTreeSet<ItemId>,
    ancestor_frontier: &mut BTreeSet<ItemId>,
    namespace_keys: &mut BTreeSet<(Option<ItemId>, String)>,
) -> Result<()> {
    if let Some(parent_id) = &location.parent_item_id
        && !created.contains(parent_id)
    {
        ancestor_frontier.insert(parent_id.clone());
    }
    namespace_keys.insert((
        location.parent_item_id.clone(),
        compute_name_key(&location.name)
            .map_err(|error| LocalError::Integrity(format!("invalid historical name: {error}")))?,
    ));
    Ok(())
}

fn revert_blob_ref(item: &RevertIntentItem, before: &RevertItemState) -> Result<BlobRef> {
    Ok(BlobRef {
        blob_id: before.blob_id.clone().ok_or_else(|| {
            LocalError::Integrity(format!("revert FILE {} has no blob", item.item_id))
        })?,
        size: u64::try_from(before.size.ok_or_else(|| {
            LocalError::Integrity(format!("revert FILE {} has no size", item.item_id))
        })?)
        .map_err(|_| {
            LocalError::Integrity(format!("revert FILE {} has negative size", item.item_id))
        })?,
    })
}

fn build_worktree_restore_items(
    committed: &BTreeMap<String, CommittedNode>,
    working: &BTreeMap<String, WorkingNode>,
    working_ids: &HashMap<String, ItemId>,
) -> Result<Vec<RevertIntentItem>> {
    let mut path_ids = working_ids.clone();
    for path in working.keys() {
        if !path_ids.contains_key(path) {
            path_ids.insert(path.clone(), ItemId::new());
        }
    }
    let working_by_item: HashMap<&ItemId, &String> = working_ids
        .iter()
        .map(|(path, item_id)| (item_id, path))
        .collect();
    let mut items = Vec::new();
    for (path, node) in committed {
        let before = committed_restore_state(path, node, committed)?;
        let after = working_by_item
            .get(&node.item_id)
            .map(|working_path| {
                working_restore_state(
                    working_path,
                    &working[*working_path],
                    node.head_seq,
                    &path_ids,
                )
            })
            .transpose()?;
        match after {
            Some(after) if same_revert_value(&before, &after) => {}
            Some(after) => items.push(RevertIntentItem {
                item_id: node.item_id.clone(),
                item_type: node_kind_name(node.kind).to_string(),
                before: Some(before),
                after,
            }),
            None => {
                let mut absent = before.clone();
                absent.live = false;
                items.push(RevertIntentItem {
                    item_id: node.item_id.clone(),
                    item_type: node_kind_name(node.kind).to_string(),
                    before: Some(before),
                    after: absent,
                });
            }
        }
    }
    for (path, node) in working {
        if working_ids.contains_key(path) {
            continue;
        }
        let item_id = path_ids
            .get(path)
            .expect("every working path has a restore identity")
            .clone();
        items.push(RevertIntentItem {
            item_id,
            item_type: node_kind_name(node.kind).to_string(),
            before: None,
            after: working_restore_state(path, node, 1, &path_ids)?,
        });
    }
    items.sort_by(|left, right| left.item_id.cmp(&right.item_id));
    Ok(items)
}

fn committed_restore_state(
    path: &str,
    node: &CommittedNode,
    tree: &BTreeMap<String, CommittedNode>,
) -> Result<RevertItemState> {
    let parent_id = match path.rsplit_once('/') {
        None => None,
        Some((parent, _)) => Some(
            tree.get(parent)
                .filter(|parent| parent.kind == NodeKind::Folder)
                .ok_or_else(|| {
                    LocalError::Integrity(format!("committed restore parent is missing: {parent}"))
                })?
                .item_id
                .clone(),
        ),
    };
    Ok(RevertItemState {
        parent_id,
        name: restore_final_name(path).to_string(),
        path: path.to_string(),
        live: true,
        head_seq: node.head_seq,
        blob_id: node.blob_id.clone(),
        size: node.size,
    })
}

fn working_restore_state(
    path: &str,
    node: &WorkingNode,
    head_seq: i64,
    path_ids: &HashMap<String, ItemId>,
) -> Result<RevertItemState> {
    let parent_id = path
        .rsplit_once('/')
        .map(|(parent, _)| {
            path_ids.get(parent).cloned().ok_or_else(|| {
                LocalError::Integrity(format!("working restore parent is missing: {parent}"))
            })
        })
        .transpose()?;
    let (blob_id, size) = match node.kind {
        NodeKind::File => {
            let digest = node.digest_hex.as_ref().ok_or_else(|| {
                LocalError::Integrity(format!("working restore FILE has no digest: {path}"))
            })?;
            let blob_id = BlobId::parse(format!("sha256:{digest}")).map_err(|error| {
                LocalError::Integrity(format!("working restore FILE has invalid digest: {error}"))
            })?;
            let size = i64::try_from(node.size.ok_or_else(|| {
                LocalError::Integrity(format!("working restore FILE has no size: {path}"))
            })?)
            .map_err(|_| LocalError::Invalid(format!("working FILE is too large: {path}")))?;
            (Some(blob_id), Some(size))
        }
        NodeKind::Folder => (None, None),
    };
    Ok(RevertItemState {
        parent_id,
        name: restore_final_name(path).to_string(),
        path: path.to_string(),
        live: true,
        head_seq,
        blob_id,
        size,
    })
}

pub(crate) fn worktree_restore_manifest(
    working: &BTreeMap<String, WorkingNode>,
    working_ids: &HashMap<String, ItemId>,
) -> Result<Vec<WorktreeRestoreEntry>> {
    working
        .iter()
        .map(|(path, node)| {
            let (blob_id, size) = match node.kind {
                NodeKind::File => {
                    let digest = node.digest_hex.as_ref().ok_or_else(|| {
                        LocalError::Integrity(format!("working FILE has no digest: {path}"))
                    })?;
                    (
                        Some(BlobId::parse(format!("sha256:{digest}")).map_err(|error| {
                            LocalError::Integrity(format!(
                                "working FILE has invalid digest: {error}"
                            ))
                        })?),
                        Some(
                            i64::try_from(node.size.ok_or_else(|| {
                                LocalError::Integrity(format!("working FILE has no size: {path}"))
                            })?)
                            .map_err(|_| {
                                LocalError::Invalid(format!("working FILE is too large: {path}"))
                            })?,
                        ),
                    )
                }
                NodeKind::Folder => (None, None),
            };
            Ok(WorktreeRestoreEntry {
                path: path.clone(),
                item_id: working_ids.get(path).cloned(),
                item_type: node_kind_name(node.kind).to_string(),
                blob_id,
                size,
            })
        })
        .collect()
}

fn reject_ignored_restore_descendants(
    root: &Dir,
    committed: &BTreeMap<String, CommittedNode>,
    items: &[RevertIntentItem],
    working: &BTreeMap<String, WorkingNode>,
    retained_sources: &BTreeSet<String>,
) -> Result<()> {
    let ignore_changes = items.iter().any(|item| {
        item.after.path == ".synckignore"
            || item
                .before
                .as_ref()
                .is_some_and(|before| before.path == ".synckignore")
    });
    if ignore_changes {
        let strict = crate::worktree::scan_canonical_without_ignore(root, committed, false)?;
        if let Some(path) = strict
            .keys()
            .find(|path| !working.contains_key(*path) && !retained_sources.contains(*path))
        {
            return Err(LocalError::Invalid(format!(
                "restore of .synckignore would expose ignored entry: {path}"
            )));
        }
    }
    for item in items.iter().filter(|item| {
        item.item_type == "FOLDER"
            && item.after.live
            && item
                .before
                .as_ref()
                .is_none_or(|before| before.path != item.after.path)
    }) {
        let relative = display_to_path(&item.after.path);
        let directory = root
            .open_dir_nofollow(&relative)
            .at("open restore source folder", &relative)?;
        let nested = crate::worktree::scan_materialized_tree(&directory, &BTreeMap::new())?;
        for relative_path in nested.keys() {
            let path = format!("{}/{}", item.after.path, relative_path);
            if !working.contains_key(&path) {
                return Err(LocalError::Invalid(format!(
                    "restore would move or remove ignored entry: {path}"
                )));
            }
        }
    }
    Ok(())
}

fn restore_final_name(path: &str) -> &str {
    path.rsplit('/').next().expect("nonempty restore path")
}

fn validate_worktree_restore_fixed_fields(intent: &WorktreeRestoreIntent) -> Result<()> {
    let mut item_ids = HashSet::new();
    if intent.version != WORKTREE_RESTORE_INTENT_VERSION
        || intent.base_repo_seq < 0
        || (intent.base_commit_id.is_none() != (intent.base_repo_seq == 0))
        || intent.items.is_empty()
        || intent.items.len() > i32::MAX as usize
        || intent
            .items
            .iter()
            .any(|item| !item_ids.insert(item.item_id.clone()))
    {
        return Err(LocalError::Integrity(
            "worktree restore intent has invalid fixed fields".to_string(),
        ));
    }
    validate_worktree_restore_manifest(&intent.source_tree)
}

fn validate_worktree_restore_manifest(entries: &[WorktreeRestoreEntry]) -> Result<()> {
    let mut previous: Option<&str> = None;
    let mut paths = HashMap::new();
    let mut item_ids = HashSet::new();
    let mut siblings: HashMap<&str, HashSet<String>> = HashMap::new();
    for entry in entries {
        let kind = parse_node_kind(&entry.item_type)?;
        if entry.path.is_empty()
            || previous.is_some_and(|previous| previous >= entry.path.as_str())
            || paths.insert(entry.path.as_str(), kind).is_some()
            || entry
                .item_id
                .as_ref()
                .is_some_and(|item_id| !item_ids.insert(item_id.clone()))
            || match kind {
                NodeKind::File => entry.blob_id.is_none() || entry.size.is_none(),
                NodeKind::Folder => entry.blob_id.is_some() || entry.size.is_some(),
            }
        {
            return Err(LocalError::Integrity(
                "worktree restore intent has an invalid source entry".to_string(),
            ));
        }
        let (parent, name) = entry.path.rsplit_once('/').unwrap_or(("", &entry.path));
        let key = compute_name_key(name).map_err(|error| {
            LocalError::Integrity(format!("invalid worktree restore name {name:?}: {error}"))
        })?;
        if !siblings.entry(parent).or_default().insert(key) {
            return Err(LocalError::Integrity(format!(
                "worktree restore intent has a portable collision below {}",
                if parent.is_empty() { "." } else { parent }
            )));
        }
        previous = Some(&entry.path);
    }
    for entry in entries {
        if let Some((parent, _)) = entry.path.rsplit_once('/')
            && paths.get(parent) != Some(&NodeKind::Folder)
        {
            return Err(LocalError::Integrity(format!(
                "worktree restore intent is missing parent {parent}"
            )));
        }
    }
    Ok(())
}

fn validate_worktree_restore_target(
    intent: &WorktreeRestoreIntent,
    committed: &BTreeMap<String, CommittedNode>,
) -> Result<()> {
    let committed_by_id: HashMap<&ItemId, (&String, &CommittedNode)> = committed
        .iter()
        .map(|(path, node)| (&node.item_id, (path, node)))
        .collect();
    for item in &intent.items {
        if let Some(before) = &item.before {
            let (path, node) = committed_by_id.get(&item.item_id).ok_or_else(|| {
                LocalError::Integrity(format!(
                    "worktree restore target item is not committed: {}",
                    item.item_id
                ))
            })?;
            let expected = committed_restore_state(path, node, committed)?;
            if &expected != before || node_kind_name(node.kind) != item.item_type {
                return Err(LocalError::Integrity(format!(
                    "worktree restore target differs for item {}",
                    item.item_id
                )));
            }
        } else if committed_by_id.contains_key(&item.item_id) {
            return Err(LocalError::Integrity(format!(
                "untracked restore item uses committed identity {}",
                item.item_id
            )));
        }
        if item.after.live {
            let source = intent
                .source_tree
                .iter()
                .find(|entry| entry.path == item.after.path)
                .ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "worktree restore source is missing {}",
                        item.after.path
                    ))
                })?;
            let expected_id = item.before.as_ref().map(|_| &item.item_id);
            if source.item_id.as_ref() != expected_id
                || source.item_type != item.item_type
                || source.blob_id != item.after.blob_id
                || source.size != item.after.size
            {
                return Err(LocalError::Integrity(format!(
                    "worktree restore source differs at {}",
                    item.after.path
                )));
            }
        }
    }
    Ok(())
}

fn prepare_revert_items(
    selected: &[RevertIntentItem],
    selected_after: &HashMap<ItemId, HistoryNode>,
    current: &HashMap<ItemId, HistoryNode>,
) -> Result<Vec<RevertIntentItem>> {
    let mut desired = current.clone();
    for item in selected {
        let current_node = current.get(&item.item_id).ok_or_else(|| {
            LocalError::Integrity(format!(
                "revert target item {} is missing from current history",
                item.item_id
            ))
        })?;
        if node_kind_name(current_node.kind) != item.item_type {
            return Err(LocalError::Integrity(format!(
                "revert target item {} changed type",
                item.item_id
            )));
        }
        let current_state = revert_item_state(&item.item_id, current_node, current)?;
        let structure_changed = item.before.as_ref().is_some_and(|before| {
            before.parent_id != item.after.parent_id || before.name != item.after.name
        });
        let content_changed = item.before.as_ref().is_some_and(|before| {
            before.blob_id != item.after.blob_id || before.size != item.after.size
        });
        let lifecycle_changed = item
            .before
            .as_ref()
            .is_none_or(|before| before.live != item.after.live);

        if item.before.is_none() {
            if current_state != item.after {
                return Err(revert_overlap(&item.item_id));
            }
            for candidate in current.keys() {
                if !selected_after.contains_key(candidate)
                    && is_history_descendant(candidate, &item.item_id, current)?
                {
                    return Err(revert_overlap(&item.item_id));
                }
            }
            desired
                .get_mut(&item.item_id)
                .expect("current revert item exists")
                .live = false;
            continue;
        }

        let before = item.before.as_ref().expect("non-create revert item");
        if current_state.live != item.after.live
            || (lifecycle_changed && !same_revert_value(&current_state, &item.after))
            || (structure_changed
                && (current_state.parent_id != item.after.parent_id
                    || current_state.name != item.after.name))
            || (content_changed
                && (current_state.blob_id != item.after.blob_id
                    || current_state.size != item.after.size))
        {
            return Err(revert_overlap(&item.item_id));
        }
        let node = desired
            .get_mut(&item.item_id)
            .expect("current revert item exists");
        if lifecycle_changed {
            node.live = before.live;
        }
        if structure_changed {
            node.parent_id = before.parent_id.clone();
            node.name = before.name.clone();
        }
        if content_changed {
            node.blob_id = before.blob_id.clone();
            node.size = before.size;
        }
    }

    selected
        .iter()
        .map(|item| {
            let after = revert_item_state(
                &item.item_id,
                current
                    .get(&item.item_id)
                    .expect("validated current revert item"),
                current,
            )?;
            let before = item
                .before
                .as_ref()
                .map(|_| {
                    revert_item_state(
                        &item.item_id,
                        desired
                            .get(&item.item_id)
                            .expect("desired revert item exists"),
                        &desired,
                    )
                })
                .transpose()?;
            Ok(RevertIntentItem {
                item_id: item.item_id.clone(),
                item_type: item.item_type.clone(),
                before,
                after,
            })
        })
        .collect()
}

fn revert_overlap(item_id: &ItemId) -> LocalError {
    LocalError::Invalid(format!(
        "cannot revert because later history overlaps item {item_id}"
    ))
}

fn is_history_descendant(
    candidate: &ItemId,
    ancestor: &ItemId,
    nodes: &HashMap<ItemId, HistoryNode>,
) -> Result<bool> {
    let mut current = candidate;
    let mut visited = HashSet::new();
    while let Some(parent) = nodes.get(current).and_then(|node| node.parent_id.as_ref()) {
        if !visited.insert(current.clone()) {
            return Err(LocalError::Integrity(format!(
                "historical folder cycle at {current}"
            )));
        }
        if parent == ancestor {
            return Ok(true);
        }
        current = parent;
    }
    Ok(false)
}

fn derive_revert_items(
    before: &HashMap<ItemId, HistoryNode>,
    after: &HashMap<ItemId, HistoryNode>,
) -> Result<Vec<RevertIntentItem>> {
    let mut item_ids = after.keys().collect::<Vec<_>>();
    item_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let mut items = Vec::new();
    for item_id in item_ids {
        let after_node = &after[item_id];
        let after_state = revert_item_state(item_id, after_node, after)?;
        let before_state = before
            .get(item_id)
            .map(|node| revert_item_state(item_id, node, before))
            .transpose()?;
        if before_state
            .as_ref()
            .is_some_and(|state| same_revert_value(state, &after_state))
        {
            continue;
        }
        items.push(RevertIntentItem {
            item_id: item_id.clone(),
            item_type: node_kind_name(after_node.kind).to_string(),
            before: before_state,
            after: after_state,
        });
    }
    for item_id in before.keys() {
        if !after.contains_key(item_id) {
            return Err(LocalError::Integrity(format!(
                "historical item {item_id} disappears from replay"
            )));
        }
    }
    Ok(items)
}

fn revert_item_state(
    item_id: &ItemId,
    node: &HistoryNode,
    nodes: &HashMap<ItemId, HistoryNode>,
) -> Result<RevertItemState> {
    let path = history_path(item_id, nodes)?.expect("replayed item has a path");
    match node.kind {
        NodeKind::File if node.blob_id.is_none() || node.size.is_none() => {
            return Err(LocalError::Integrity(format!(
                "historical FILE {item_id} has no content"
            )));
        }
        NodeKind::Folder if node.blob_id.is_some() || node.size.is_some() => {
            return Err(LocalError::Integrity(format!(
                "historical FOLDER {item_id} has file content"
            )));
        }
        _ => {}
    }
    Ok(RevertItemState {
        parent_id: node.parent_id.clone(),
        name: node.name.clone(),
        path,
        live: node.live,
        head_seq: node.head_seq,
        blob_id: node.blob_id.clone(),
        size: node.size,
    })
}

fn same_revert_value(left: &RevertItemState, right: &RevertItemState) -> bool {
    left.parent_id == right.parent_id
        && left.name == right.name
        && left.live == right.live
        && left.blob_id == right.blob_id
        && left.size == right.size
}

fn validate_revert_intent(
    repo_id: &RepoId,
    intent: &RevertIntent,
    before: &HashMap<ItemId, HistoryNode>,
    after: &HashMap<ItemId, HistoryNode>,
    publish_base: &HashMap<ItemId, HistoryNode>,
) -> Result<()> {
    validate_revert_fixed_fields(intent)?;
    let selected = derive_revert_items(before, after)?;
    let expected = prepare_revert_items(&selected, after, publish_base)?;
    if intent.items != expected {
        return Err(LocalError::Integrity(
            "revert intent does not exactly invert its base commit".to_string(),
        ));
    }
    let materialized = materialize_revert(repo_id, &intent.message, &intent.items, publish_base)
        .map_err(|error| LocalError::Integrity(error.to_string()))?;
    if !same_event_proposals(&intent.events, &materialized.events)?
        || intent.subtree_guards
            != materialized
                .subtree_guards
                .into_iter()
                .map(|guard| DurableSubtreeGuard {
                    root_item_id: guard.root_item_id,
                    expected_non_tombstoned_descendants: guard.expected_non_tombstoned_descendants,
                })
                .collect::<Vec<_>>()
    {
        return Err(LocalError::Integrity(
            "revert intent profile materialization is inconsistent".to_string(),
        ));
    }
    Ok(())
}

fn same_event_proposals(left: &[EventProposal], right: &[EventProposal]) -> Result<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    left.iter()
        .zip(right)
        .map(|(left, right)| {
            Ok(canonical_event_proposal_bytes(left)? == canonical_event_proposal_bytes(right)?)
        })
        .try_fold(true, |equal, pair| pair.map(|pair| equal && pair))
}

fn validate_revert_fixed_fields(intent: &RevertIntent) -> Result<()> {
    let operation_id = Uuid::parse_str(&intent.operation_id)
        .map_err(|error| LocalError::Integrity(format!("invalid revert operation ID: {error}")))?;
    if intent.version != REVERT_INTENT_VERSION
        || operation_id.to_string() != intent.operation_id
        || intent.base_repo_seq <= 0
        || intent.publish_base_repo_seq < intent.base_repo_seq
        || intent.message.is_empty()
        || intent.items.is_empty()
        || intent.events.is_empty()
        || intent.items.len() > i32::MAX as usize
    {
        return Err(LocalError::Integrity(
            "revert intent has invalid fixed fields".to_string(),
        ));
    }
    Ok(())
}

fn validate_reverted_nodes(
    intent: &RevertIntent,
    after: &HashMap<ItemId, HistoryNode>,
    current: &HashMap<ItemId, HistoryNode>,
) -> Result<()> {
    if current.len() != after.len() {
        return Err(LocalError::Integrity(
            "accepted revert changed the repository item set".to_string(),
        ));
    }
    let affected: HashMap<&ItemId, &RevertIntentItem> = intent
        .items
        .iter()
        .map(|item| (&item.item_id, item))
        .collect();
    for (item_id, after_node) in after {
        let current_node = current
            .get(item_id)
            .ok_or_else(|| LocalError::Integrity(format!("accepted revert lost item {item_id}")))?;
        let actual = revert_item_state(item_id, current_node, current)?;
        let matches = match affected.get(item_id) {
            None => {
                let expected = revert_item_state(item_id, after_node, after)?;
                actual.head_seq == expected.head_seq && same_revert_value(&actual, &expected)
            }
            Some(item) => {
                let mut expected = item.before.clone().unwrap_or_else(|| item.after.clone());
                if item.before.is_none() {
                    expected.live = false;
                }
                expected.head_seq = item
                    .after
                    .head_seq
                    .checked_add(revert_event_count(item)?)
                    .ok_or_else(|| {
                        LocalError::Integrity(format!(
                            "revert item {} sequence overflows",
                            item.item_id
                        ))
                    })?;
                actual == expected
            }
        };
        if !matches {
            return Err(LocalError::Integrity(format!(
                "accepted revert produced the wrong state for item {item_id}"
            )));
        }
    }
    Ok(())
}

fn revert_event_count(item: &RevertIntentItem) -> Result<i64> {
    let Some(before) = item.before.as_ref() else {
        return Ok(1);
    };
    if before.live != item.after.live {
        return Ok(1);
    }
    let count = i64::from(before.parent_id != item.after.parent_id)
        + i64::from(before.name != item.after.name)
        + i64::from(
            item.item_type == "FILE"
                && (before.blob_id != item.after.blob_id || before.size != item.after.size),
        );
    if count == 0 {
        return Err(LocalError::Integrity(format!(
            "revert item {} has no inverse event",
            item.item_id
        )));
    }
    Ok(count)
}

fn expected_revert_commit_id(intent: &RevertIntent, author: &str) -> Result<CommitId> {
    let mut digest = Sha256::new();
    for event in &intent.events {
        let canonical = canonical_event_proposal_bytes(event)?;
        digest.update([synck_vcs::types::COMMIT_EVENT_FAMILY_ITEM]);
        digest.update(compute_sha256_digest_bytes(&canonical));
    }
    let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
        digest.finalize().into(),
    ))
    .expect("internal SHA-256 digest is valid");
    let commit = canonical_commit_bytes_for_hash(
        &author.to_string(),
        &author.to_string(),
        None,
        Some(&intent.message),
        None,
        intent.events.len(),
        &events_digest,
    )?;
    Ok(compute_commit_id(&commit))
}

fn validate_inverse_stage_entries(stage: &Path, items: &[RevertIntentItem]) -> Result<()> {
    let expected: HashSet<&str> = items.iter().map(|item| item.item_id.as_str()).collect();
    for entry in std::fs::read_dir(stage).at("read revert staging", stage)? {
        let entry = entry.at("read revert staging", stage)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(LocalError::Integrity(
                "revert staging contains a non-Unicode name".to_string(),
            ));
        };
        if !expected.contains(name) {
            return Err(LocalError::Integrity(format!(
                "revert staging contains unexpected entry {name:?}"
            )));
        }
    }
    Ok(())
}

fn entry_matches_revert_state(
    path: &Path,
    item_type: &str,
    state: &RevertItemState,
) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect reverted entry",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    match item_type {
        "FOLDER" => Ok(metadata.file_type().is_dir() && !is_link_or_reparse(&metadata)),
        "FILE" => {
            if !metadata.file_type().is_file() || is_link_or_reparse(&metadata) {
                return Ok(false);
            }
            let blob_id = state.blob_id.as_ref().ok_or_else(|| {
                LocalError::Integrity("revert FILE state has no blob".to_string())
            })?;
            let digest = blob_id
                .as_str()
                .strip_prefix("sha256:")
                .expect("BlobId has a SHA-256 prefix");
            let size = u64::try_from(state.size.ok_or_else(|| {
                LocalError::Integrity("revert FILE state has no size".to_string())
            })?)
            .map_err(|_| LocalError::Integrity("revert FILE has negative size".to_string()))?;
            match verify_object_file(path, digest, size) {
                Ok(()) => Ok(true),
                Err(LocalError::Integrity(_)) => Ok(false),
                Err(error) => Err(error),
            }
        }
        _ => Err(LocalError::Integrity(format!(
            "invalid revert item type {item_type:?}"
        ))),
    }
}

fn path_exists_nofollow(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(LocalError::Io {
            action: "inspect reverted entry",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn sync_display_parent(root: &Path, display: &str) -> Result<()> {
    let relative = display_to_path(display);
    let parent = relative
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map_or_else(|| root.to_path_buf(), |path| root.join(path));
    sync_directory(&parent)
}

fn build_recover_intent(
    root_item_id: &ItemId,
    message: &str,
    nodes: &HashMap<ItemId, HistoryNode>,
) -> Result<RecoverIntent> {
    let root = nodes
        .get(root_item_id)
        .ok_or_else(|| LocalError::Invalid(format!("item does not exist: {root_item_id}")))?;
    if root.live {
        return Err(LocalError::Invalid(format!(
            "item is not in trash: {root_item_id}"
        )));
    }
    if let Some(parent_id) = &root.parent_id {
        let parent = nodes.get(parent_id).ok_or_else(|| {
            LocalError::Integrity(format!(
                "trashed item {root_item_id} has missing parent {parent_id}"
            ))
        })?;
        if !parent.live {
            return Err(LocalError::Invalid(format!(
                "item has trashed parent {parent_id}; recover the parent instead"
            )));
        }
    }
    let deletion_commit_id = root
        .deletion
        .as_ref()
        .ok_or_else(|| {
            LocalError::Integrity(format!("trashed item {root_item_id} has no deletion event"))
        })?
        .commit_id
        .clone();
    let mut selected = Vec::new();
    for (item_id, node) in nodes {
        let same_deletion = node
            .deletion
            .as_ref()
            .is_some_and(|deletion| deletion.commit_id == deletion_commit_id);
        if item_id == root_item_id
            || (root.kind == NodeKind::Folder
                && !node.live
                && same_deletion
                && is_descendant_of(item_id, root_item_id, nodes)?)
        {
            selected.push((
                item_id,
                node,
                history_path(item_id, nodes)?.expect("selected item exists"),
            ));
        }
    }
    selected.sort_by(|(left_id, _, left), (right_id, _, right)| {
        path_depth(left)
            .cmp(&path_depth(right))
            .then_with(|| left.cmp(right))
            .then_with(|| left_id.as_str().cmp(right_id.as_str()))
    });

    let selected_ids: Vec<ItemId> = selected
        .iter()
        .map(|(item_id, _, _)| (*item_id).clone())
        .collect();
    validate_recover_names(nodes, &selected_ids)?;

    let mut items = Vec::with_capacity(selected.len());
    for (item_id, node, path) in selected {
        let (blob_id, size) = match node.kind {
            NodeKind::File => (
                Some(node.blob_id.clone().ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "trashed FILE {item_id} has no committed content"
                    ))
                })?),
                Some(node.size.ok_or_else(|| {
                    LocalError::Integrity(format!("trashed FILE {item_id} has no content size"))
                })?),
            ),
            NodeKind::Folder => (None, None),
        };
        items.push(RecoverIntentItem {
            item_id: item_id.clone(),
            item_type: node_kind_name(node.kind).to_string(),
            parent_id: node.parent_id.clone(),
            name: node.name.clone(),
            path,
            base_seq: node.head_seq,
            blob_id,
            size,
        });
    }
    if items.len() > i32::MAX as usize {
        return Err(LocalError::Invalid(
            "recovery exceeds SQLite event count range".to_string(),
        ));
    }
    Ok(RecoverIntent {
        version: RECOVER_INTENT_VERSION,
        operation_id: Uuid::new_v4().to_string(),
        root_item_id: root_item_id.clone(),
        deletion_commit_id,
        message: message.to_string(),
        items,
    })
}

fn validate_recover_intent(
    root: &Path,
    intent: &RecoverIntent,
    nodes: &HashMap<ItemId, HistoryNode>,
) -> Result<bool> {
    let operation_id = Uuid::parse_str(&intent.operation_id)
        .map_err(|error| LocalError::Integrity(format!("invalid recover operation ID: {error}")))?;
    if intent.version != RECOVER_INTENT_VERSION
        || operation_id.to_string() != intent.operation_id
        || intent.message.is_empty()
        || intent.items.is_empty()
        || intent.items[0].item_id != intent.root_item_id
        || intent.items.len() > i32::MAX as usize
    {
        return Err(LocalError::Integrity(
            "recover intent has invalid fixed fields".to_string(),
        ));
    }

    let mut seen_ids = HashSet::new();
    let mut seen_paths = HashSet::new();
    let mut previous: Option<(usize, &str, &str)> = None;
    let mut pre_append = true;
    let mut accepted = true;
    for item in &intent.items {
        if item.base_seq <= 0
            || !seen_ids.insert(item.item_id.clone())
            || !seen_paths.insert(item.path.as_str())
        {
            return Err(LocalError::Integrity(
                "recover intent contains duplicate or invalid items".to_string(),
            ));
        }
        let canonical =
            normalize_input_path(root, root, Path::new(&item.path)).map_err(|error| {
                LocalError::Integrity(format!("invalid recover intent path: {error}"))
            })?;
        if canonical != item.path {
            return Err(LocalError::Integrity(
                "recover intent path is not canonical".to_string(),
            ));
        }
        let order = (
            path_depth(&item.path),
            item.path.as_str(),
            item.item_id.as_str(),
        );
        if previous.is_some_and(|previous| previous > order) {
            return Err(LocalError::Integrity(
                "recover intent items are not in canonical order".to_string(),
            ));
        }
        previous = Some(order);

        let node = nodes.get(&item.item_id).ok_or_else(|| {
            LocalError::Integrity(format!("recover intent item is missing: {}", item.item_id))
        })?;
        let expected_path = history_path(&item.item_id, nodes)?.expect("intent item exists");
        let content_matches = match node.kind {
            NodeKind::File => {
                item.blob_id == node.blob_id && item.size == node.size && item.blob_id.is_some()
            }
            NodeKind::Folder => item.blob_id.is_none() && item.size.is_none(),
        };
        if item.item_type != node_kind_name(node.kind)
            || item.parent_id != node.parent_id
            || item.name != node.name
            || item.path != expected_path
            || !content_matches
        {
            return Err(LocalError::Integrity(format!(
                "recover intent contradicts item {}",
                item.item_id
            )));
        }
        pre_append &= !node.live
            && node.head_seq == item.base_seq
            && node
                .deletion
                .as_ref()
                .is_some_and(|deletion| deletion.commit_id == intent.deletion_commit_id);
        accepted &= node.live && node.head_seq == item.base_seq + 1 && node.deletion.is_none();
    }
    if !pre_append && !accepted {
        return Err(LocalError::Integrity(
            "recover intent has mixed or contradictory item state".to_string(),
        ));
    }

    for (item_id, node) in nodes {
        if !seen_ids.contains(item_id)
            && !node.live
            && node
                .deletion
                .as_ref()
                .is_some_and(|deletion| deletion.commit_id == intent.deletion_commit_id)
            && is_descendant_of(item_id, &intent.root_item_id, nodes)?
        {
            return Err(LocalError::Integrity(format!(
                "recover intent omits deleted descendant {item_id}"
            )));
        }
    }
    Ok(accepted)
}

fn validate_recover_names(nodes: &HashMap<ItemId, HistoryNode>, selected: &[ItemId]) -> Result<()> {
    let selected_set: HashSet<&ItemId> = selected.iter().collect();
    let mut occupied: HashMap<(Option<ItemId>, String), ItemId> = HashMap::new();
    for (item_id, node) in nodes {
        if !node.live || selected_set.contains(item_id) {
            continue;
        }
        let key = compute_name_key(&node.name).map_err(|error| {
            LocalError::Integrity(format!("invalid stored item name {:?}: {error}", node.name))
        })?;
        occupied.insert((node.parent_id.clone(), key), item_id.clone());
    }
    for item_id in selected {
        let node = &nodes[item_id];
        let key = compute_name_key(&node.name).map_err(|error| {
            LocalError::Integrity(format!("invalid stored item name {:?}: {error}", node.name))
        })?;
        if let Some(conflict) = occupied.insert((node.parent_id.clone(), key), item_id.clone()) {
            return Err(LocalError::Invalid(format!(
                "recovery item {item_id} conflicts with item {conflict}"
            )));
        }
    }
    Ok(())
}

fn reject_reserved_root_path(path: &str) -> Result<()> {
    if !path.contains('/') && is_reserved_root_name(path) {
        return Err(LocalError::Invalid(format!(
            "reserved internal root name: {path}"
        )));
    }
    Ok(())
}

fn reject_reserved_recover_items(items: &[RecoverIntentItem]) -> Result<()> {
    for item in items {
        if item.parent_id.is_none() && is_reserved_root_name(&item.name) {
            return Err(LocalError::Invalid(format!(
                "cannot recover reserved internal root name: {}",
                item.name
            )));
        }
    }
    Ok(())
}

fn reject_reserved_revert_items(items: &[RevertIntentItem]) -> Result<()> {
    for state in items
        .iter()
        .filter_map(|item| item.before.as_ref())
        .filter(|state| state.live)
    {
        if state.parent_id.is_none() && is_reserved_root_name(&state.name) {
            return Err(LocalError::Invalid(format!(
                "cannot revert to reserved internal root name: {}",
                state.name
            )));
        }
    }
    Ok(())
}

fn is_descendant_of(
    item_id: &ItemId,
    ancestor_id: &ItemId,
    nodes: &HashMap<ItemId, HistoryNode>,
) -> Result<bool> {
    let mut current = Some(item_id);
    let mut visiting = HashSet::new();
    while let Some(id) = current {
        if id == ancestor_id {
            return Ok(item_id != ancestor_id);
        }
        if !visiting.insert(id.clone()) {
            return Err(LocalError::Integrity(format!(
                "historical folder cycle at {id}"
            )));
        }
        current = nodes
            .get(id)
            .ok_or_else(|| LocalError::Integrity(format!("historical item {id} is missing")))?
            .parent_id
            .as_ref();
    }
    Ok(false)
}

fn validate_recover_destinations(
    root: &Path,
    intent: &RecoverIntent,
    materialized: bool,
) -> Result<()> {
    for item in &intent.items {
        let path = root.join(display_to_path(&item.path));
        match std::fs::symlink_metadata(&path) {
            Ok(_) if !materialized => {
                return Err(LocalError::Invalid(format!(
                    "recovery target already exists: {}",
                    item.path
                )));
            }
            Ok(metadata)
                if item.item_type == "FOLDER"
                    && metadata.file_type().is_dir()
                    && !is_link_or_reparse(&metadata) => {}
            Ok(metadata)
                if item.item_type == "FILE"
                    && metadata.file_type().is_file()
                    && !is_link_or_reparse(&metadata) =>
            {
                verify_working_file(
                    root,
                    &item.path,
                    item.blob_id
                        .as_ref()
                        .expect("validated recovered FILE has a blob"),
                    item.size.expect("validated recovered FILE has a size"),
                )?;
            }
            Ok(_) => {
                return Err(LocalError::Invalid(format!(
                    "recovery target has the wrong type: {}",
                    item.path
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !materialized => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(LocalError::Integrity(format!(
                    "recovery target was not materialized: {}",
                    item.path
                )));
            }
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect recovery target",
                    path,
                    source,
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn verify_working_file(
    root: &Path,
    display: &str,
    blob_id: &BlobId,
    expected_size: i64,
) -> Result<()> {
    let relative = display_to_path(display);
    let parent = relative
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = relative
        .file_name()
        .ok_or_else(|| LocalError::Integrity(format!("invalid recovered file path: {display}")))?;
    let root_dir = Dir::open_ambient_dir(root, ambient_authority()).at("open repository", root)?;
    let parent_dir = root_dir
        .open_dir_nofollow(parent)
        .at("open recovery parent", parent)?;
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = parent_dir
        .open_with(name, &options)
        .at("open recovered file", display)?;
    let metadata = file.metadata().at("inspect recovered file", display)?;
    if !metadata.file_type().is_file() || is_cap_link_or_reparse(&metadata) {
        return Err(LocalError::Invalid(format!(
            "recovery target is not a regular file: {display}"
        )));
    }
    let expected_size = u64::try_from(expected_size)
        .map_err(|_| LocalError::Integrity(format!("negative object size for {blob_id}")))?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 128 * 1_024];
    loop {
        let count = file.read(&mut buffer).at("read recovered file", display)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size = size.checked_add(count as u64).ok_or_else(|| {
            LocalError::Integrity(format!("recovered file size overflow for {display}"))
        })?;
    }
    let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
    if size != expected_size || actual != blob_id.as_str() {
        return Err(LocalError::Invalid(format!(
            "recovery target content differs from committed bytes: {display}"
        )));
    }
    Ok(())
}

fn begin_replay_commit(commit: CommitRow) -> Result<ReplayAccumulator> {
    commit.repo_seq.ok_or_else(|| {
        LocalError::Integrity(format!("commit {} is not sequenced", commit.commit_id))
    })?;
    Ok(ReplayAccumulator {
        commit,
        events: Vec::new(),
    })
}

fn append_replay_event(replay: &mut ReplayAccumulator, event: CommitEventRow) -> Result<()> {
    if event.repo_seq() != replay.commit.repo_seq || event.commit_id() != &replay.commit.commit_id {
        return Err(LocalError::Integrity(format!(
            "repository sequence {} contains multiple commits",
            replay.commit.repo_seq.expect("replay commit is sequenced")
        )));
    }
    replay.events.push(event);
    Ok(())
}

fn finish_replay_commit(
    replay: Option<ReplayAccumulator>,
    nodes: &mut HashMap<ItemId, HistoryNode>,
    replay_item_kinds: &mut BTreeMap<ItemId, String>,
) -> Result<()> {
    let Some(replay) = replay else {
        return Ok(());
    };
    let item_events = replay
        .events
        .iter()
        .filter_map(|event| match event {
            CommitEventRow::Item(event) => Some(event.clone()),
            CommitEventRow::Ref(_) => None,
        })
        .collect::<Vec<_>>();
    let (target_ids, created_types) =
        commit_item_kind_requirements(&item_events).map_err(LocalError::Integrity)?;
    if let Some(item_id) = created_types
        .keys()
        .find(|item_id| replay_item_kinds.contains_key(*item_id))
    {
        return Err(LocalError::Integrity(format!(
            "historical item {item_id} is created more than once"
        )));
    }
    let item_kinds = target_ids
        .iter()
        .map(|item_id| {
            replay_item_kinds
                .get(item_id)
                .cloned()
                .map(|item_type| (item_id.clone(), item_type))
                .ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "historical event targets {item_id} before creation"
                    ))
                })
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let classified = classify_complete_commit(&replay.commit, &replay.events, &item_kinds)
        .map_err(LocalError::Integrity)?;
    let item_rows_by_key = item_events
        .iter()
        .map(|row| ((row.item_id.clone(), row.item_seq), row))
        .collect::<HashMap<_, _>>();
    for classified in classified {
        let row = item_rows_by_key
            .get(&(classified.item_id.clone(), classified.item_seq))
            .copied()
            .ok_or_else(|| {
                LocalError::Integrity(
                    "classified event does not match the complete commit".to_string(),
                )
            })?;
        apply_history_event(row, &classified.proposal, &classified.profile, nodes)?;
    }
    replay_item_kinds.extend(created_types);
    Ok(())
}

fn history_commit(commit: CommitRow, changes: Vec<Change>) -> Result<HistoryCommit> {
    let repo_seq = commit.repo_seq.ok_or_else(|| {
        LocalError::Integrity(format!(
            "commit {} has no repository sequence",
            commit.commit_id
        ))
    })?;
    Ok(HistoryCommit {
        repo_seq,
        commit_id: commit.commit_id.to_string(),
        created_at: commit.created_at,
        author: commit.author_id,
        message: commit.message,
        changes,
        event_count: checked_commit_event_count(commit.event_count, &commit.commit_id)?,
        events_digest: commit.events_digest.to_string(),
    })
}

struct CommitEventPresentation {
    item_id: ItemId,
    item_type: String,
    item_name: String,
    item_name_before: Option<String>,
    parent_folder_id: Option<ItemId>,
    parent_folder_name: Option<String>,
    parent_folder_id_before: Option<ItemId>,
    parent_folder_name_before: Option<String>,
}

async fn commit_event_presentation<S: StateIndexStore>(
    store: &mut S,
    root: &RootHeader,
    previous: Option<&RootHeader>,
    proposal: &EventProposal,
) -> Result<CommitEventPresentation> {
    let item_id = commit_event_display_item_id(proposal)?;
    let item = StateTree::new(&mut *store)
        .get_item(root.item_map_root, &item_id)
        .await
        .map_err(|error| LocalError::Integrity(error.to_string()))?
        .ok_or_else(|| {
            LocalError::Integrity(format!(
                "commit event display item {item_id} is missing from its historical root"
            ))
        })?;
    let parent_folder_name =
        historical_parent_name(store, root, item.parent_folder_id.as_ref()).await?;
    let before_item = match previous {
        Some(previous) => StateTree::new(&mut *store)
            .get_item(previous.item_map_root, &item_id)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?,
        None => None,
    };
    let item_name_before = proposal
        .set_name
        .as_ref()
        .and(before_item.as_ref().map(|value| value.name.clone()));
    let parent_folder_id_before = before_item
        .as_ref()
        .and_then(|value| value.parent_folder_id.clone());
    let parent_folder_name_before = match (previous, parent_folder_id_before.as_ref()) {
        (Some(previous), parent_id) => historical_parent_name(store, previous, parent_id).await?,
        (None, _) => None,
    };
    Ok(CommitEventPresentation {
        item_id,
        item_type: item.item_type,
        item_name: item.name,
        item_name_before,
        parent_folder_id: item.parent_folder_id,
        parent_folder_name,
        parent_folder_id_before,
        parent_folder_name_before,
    })
}

async fn ref_commit_event_presentation<S: StateIndexStore>(
    store: &mut S,
    root: &RootHeader,
    previous: Option<&RootHeader>,
    proposal: &RefEventProposal,
) -> Result<CommitEventPresentation> {
    let item = StateTree::new(&mut *store)
        .get_item(root.item_map_root, &proposal.owner_item_id)
        .await
        .map_err(|error| LocalError::Integrity(error.to_string()))?
        .ok_or_else(|| {
            LocalError::Integrity(format!(
                "ref event owner {} is missing from its historical root",
                proposal.owner_item_id
            ))
        })?;
    if item.item_type != "FILE" {
        return Err(LocalError::Integrity(format!(
            "ref event owner {} is not a FILE",
            proposal.owner_item_id
        )));
    }
    let before = match previous {
        Some(previous) => StateTree::new(&mut *store)
            .get_item(previous.item_map_root, &proposal.owner_item_id)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?,
        None => None,
    };
    let parent_folder_name =
        historical_parent_name(store, root, item.parent_folder_id.as_ref()).await?;
    let parent_folder_id_before = before
        .as_ref()
        .and_then(|before| before.parent_folder_id.clone());
    let parent_folder_name_before = match previous {
        Some(previous) => {
            historical_parent_name(store, previous, parent_folder_id_before.as_ref()).await?
        }
        None => None,
    };
    Ok(CommitEventPresentation {
        item_id: proposal.owner_item_id.clone(),
        item_type: item.item_type,
        item_name: item.name,
        item_name_before: before.map(|before| before.name),
        parent_folder_id: item.parent_folder_id,
        parent_folder_name,
        parent_folder_id_before,
        parent_folder_name_before,
    })
}

fn commit_event_display_item_id(proposal: &EventProposal) -> Result<ItemId> {
    if !proposal
        .event_type
        .as_str()
        .starts_with("FILE_VERSION_SET::")
    {
        return proposal
            .item_id
            .clone()
            .ok_or_else(|| LocalError::Integrity("commit event has no item ID".to_string()));
    }
    if proposal.event_type.as_str() == synck_file_folder::file_version_set::CREATE_EVENT {
        let name = proposal
            .create_item
            .as_ref()
            .map(|item| item.name.as_str())
            .ok_or_else(|| {
                LocalError::Integrity("FILE_VERSION_SET::CREATE has no item".to_string())
            })?;
        return synck_file_folder::file_version_set::file_item_id_from_name(name)
            .map_err(LocalError::Integrity);
    }
    let file_item_id = proposal
        .set_metadata
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .and_then(|metadata| metadata.get("file_item_id"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            LocalError::Integrity(format!("{} has no file_item_id", proposal.event_type))
        })?;
    ItemId::parse(file_item_id).map_err(|error| {
        LocalError::Integrity(format!(
            "{} has invalid file_item_id: {error}",
            proposal.event_type
        ))
    })
}

async fn historical_parent_name<S: StateIndexStore>(
    store: &mut S,
    root: &RootHeader,
    parent_id: Option<&ItemId>,
) -> Result<Option<String>> {
    let Some(parent_id) = parent_id else {
        return Ok(None);
    };
    let parent = StateTree::new(&mut *store)
        .get_item(root.item_map_root, parent_id)
        .await
        .map_err(|error| LocalError::Integrity(error.to_string()))?
        .ok_or_else(|| {
            LocalError::Integrity(format!(
                "commit event display parent {parent_id} is missing from its historical root"
            ))
        })?;
    Ok(Some(parent.name))
}

pub(crate) fn checked_commit_event_count(event_count: i32, commit_id: &CommitId) -> Result<usize> {
    let event_count = usize::try_from(event_count).map_err(|_| {
        LocalError::Integrity(format!("commit {commit_id} has a negative event count"))
    })?;
    if event_count == 0 {
        return Err(LocalError::Integrity(format!(
            "commit {commit_id} event count {event_count} is outside the FILE/FOLDER profile limit"
        )));
    }
    Ok(event_count)
}

fn history_path(item_id: &ItemId, nodes: &HashMap<ItemId, HistoryNode>) -> Result<Option<String>> {
    let Some(_) = nodes.get(item_id) else {
        return Ok(None);
    };
    let mut visiting = HashSet::new();
    history_path_inner(item_id, nodes, &mut visiting).map(Some)
}

fn validate_historical_namespace_delta(
    changed_items: &BTreeSet<ItemId>,
    changed_namespace: &BTreeSet<NamespaceKey>,
    from: &BTreeMap<ItemId, ItemValue>,
    to: &BTreeMap<ItemId, ItemValue>,
) -> Result<()> {
    let mut expected = BTreeSet::new();
    for item_id in changed_items {
        let from_key = from
            .get(item_id)
            .map(|item| NamespaceKey::new(item.parent_folder_id.clone(), item.name_key.clone()));
        let to_key = to
            .get(item_id)
            .map(|item| NamespaceKey::new(item.parent_folder_id.clone(), item.name_key.clone()));
        if from_key != to_key {
            expected.extend(from_key);
            expected.extend(to_key);
        }
    }
    if &expected != changed_namespace {
        return Err(LocalError::Integrity(
            "historical item and namespace changes disagree".to_string(),
        ));
    }
    Ok(())
}

fn initial_historical_changes(items: BTreeMap<ItemId, ItemValue>) -> Result<Vec<Change>> {
    let mut paths = HashMap::new();
    let mut visiting = HashSet::new();
    let mut changes = Vec::new();
    for (item_id, item) in &items {
        if item.lifecycle_state != LifecycleState::Live
            || item.item_type == synck_file_folder::file_version_set::ITEM_TYPE
        {
            continue;
        }
        if item.item_type != "FILE" && item.item_type != "FOLDER" {
            return Err(LocalError::Integrity(format!(
                "unsupported historical item type {} for {item_id}",
                item.item_type
            )));
        }
        changes.push(Change {
            kind: ChangeKind::Add,
            item_type: item.item_type.clone(),
            path: historical_state_path(item_id, &items, &mut paths, &mut visiting)?,
            source_path: None,
            content_changed: false,
        });
    }
    Ok(sort_changes(changes))
}

fn historical_state_path(
    item_id: &ItemId,
    items: &BTreeMap<ItemId, ItemValue>,
    paths: &mut HashMap<ItemId, String>,
    visiting: &mut HashSet<ItemId>,
) -> Result<String> {
    if let Some(path) = paths.get(item_id) {
        return Ok(path.clone());
    }
    if !visiting.insert(item_id.clone()) {
        return Err(LocalError::Integrity(format!(
            "historical state contains a parent cycle at {item_id}"
        )));
    }
    let item = items.get(item_id).ok_or_else(|| {
        LocalError::Integrity(format!("historical state is missing item {item_id}"))
    })?;
    if item.lifecycle_state != LifecycleState::Live {
        return Err(LocalError::Integrity(format!(
            "cannot build a path for non-live historical item {item_id}"
        )));
    }
    let path = match item.parent_folder_id.as_ref() {
        None => item.name.clone(),
        Some(parent_id) => {
            let parent = items.get(parent_id).ok_or_else(|| {
                LocalError::Integrity(format!(
                    "historical item {item_id} has missing parent {parent_id}"
                ))
            })?;
            if parent.item_type != "FOLDER" || parent.lifecycle_state != LifecycleState::Live {
                return Err(LocalError::Integrity(format!(
                    "historical item {item_id} has a non-live or non-folder parent {parent_id}"
                )));
            }
            format!(
                "{}/{}",
                historical_state_path(parent_id, items, paths, visiting)?,
                item.name
            )
        }
    };
    visiting.remove(item_id);
    paths.insert(item_id.clone(), path.clone());
    Ok(path)
}

fn history_path_inner(
    item_id: &ItemId,
    nodes: &HashMap<ItemId, HistoryNode>,
    visiting: &mut HashSet<ItemId>,
) -> Result<String> {
    if !visiting.insert(item_id.clone()) {
        return Err(LocalError::Integrity(format!(
            "historical folder cycle at {item_id}"
        )));
    }
    let node = nodes.get(item_id).ok_or_else(|| {
        LocalError::Integrity(format!("historical item {item_id} does not exist"))
    })?;
    let path = match &node.parent_id {
        None => node.name.clone(),
        Some(parent_id) => {
            let parent = nodes.get(parent_id).ok_or_else(|| {
                LocalError::Integrity(format!(
                    "historical item {item_id} has missing parent {parent_id}"
                ))
            })?;
            if parent.kind != NodeKind::Folder {
                return Err(LocalError::Integrity(format!(
                    "historical item {item_id} has non-folder parent {parent_id}"
                )));
            }
            format!(
                "{}/{}",
                history_path_inner(parent_id, nodes, visiting)?,
                node.name
            )
        }
    };
    visiting.remove(item_id);
    Ok(path)
}

fn apply_history_event(
    event: &EventRow,
    proposal: &EventProposal,
    profile: &LocalEvent,
    nodes: &mut HashMap<ItemId, HistoryNode>,
) -> Result<()> {
    let item_id = &event.item_id;
    match profile {
        LocalEvent::Create { item_type } => {
            let create = proposal
                .create_item
                .as_ref()
                .expect("classified CREATE has create_item");
            validate_history_parent(create.parent_folder_id.as_ref(), nodes, item_id)?;
            let kind = history_kind(item_type);
            if nodes
                .insert(
                    item_id.clone(),
                    HistoryNode {
                        parent_id: create.parent_folder_id.clone(),
                        name: create.name.clone(),
                        kind,
                        live: true,
                        head_seq: event.item_seq,
                        blob_id: None,
                        size: None,
                        deletion: None,
                    },
                )
                .is_some()
            {
                return Err(LocalError::Integrity(format!(
                    "historical item {item_id} is created more than once"
                )));
            }
        }
        LocalEvent::SetParent {
            item_type,
            parent_item_id,
        } => {
            validate_history_parent(parent_item_id.as_ref(), nodes, item_id)?;
            let node = nodes.get_mut(item_id).ok_or_else(|| {
                LocalError::Integrity(format!(
                    "historical parent change precedes creation of {item_id}"
                ))
            })?;
            if node.kind != history_kind(item_type) || !node.live {
                return Err(LocalError::Integrity(format!(
                    "historical parent change targets the wrong kind or trashed item {item_id}"
                )));
            }
            node.parent_id = parent_item_id.clone();
            node.head_seq = event.item_seq;
        }
        LocalEvent::SetName { item_type, name } => {
            let node = nodes.get_mut(item_id).ok_or_else(|| {
                LocalError::Integrity(format!(
                    "historical name change precedes creation of {item_id}"
                ))
            })?;
            if node.kind != history_kind(item_type) || !node.live {
                return Err(LocalError::Integrity(format!(
                    "historical name change targets the wrong kind or trashed item {item_id}"
                )));
            }
            node.name = name.clone();
            node.head_seq = event.item_seq;
        }
        LocalEvent::Trash { item_type } => {
            let node = nodes.get_mut(item_id).ok_or_else(|| {
                LocalError::Integrity(format!("historical trash precedes creation of {item_id}"))
            })?;
            if node.kind != history_kind(item_type) || !node.live {
                return Err(LocalError::Integrity(format!(
                    "historical trash targets the wrong kind or trashed item {item_id}"
                )));
            }
            node.live = false;
            node.head_seq = event.item_seq;
            node.deletion = Some(DeletionInfo {
                repo_seq: event.repo_seq.expect("sequenced replay event has repo_seq"),
                commit_id: event.commit_id.clone(),
                deleted_at: event.created_at,
            });
        }
        LocalEvent::Restore { item_type } => {
            let parent_id = nodes
                .get(item_id)
                .ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "historical restore precedes creation of {item_id}"
                    ))
                })?
                .parent_id
                .clone();
            validate_history_parent(parent_id.as_ref(), nodes, item_id)?;
            let node = nodes.get_mut(item_id).expect("restored node was checked");
            if node.kind != history_kind(item_type) || node.live {
                return Err(LocalError::Integrity(format!(
                    "historical restore targets the wrong kind or live item {item_id}"
                )));
            }
            node.live = true;
            node.head_seq = event.item_seq;
            node.deletion = None;
        }
        LocalEvent::SetContent { blob_id, size } => {
            let node = nodes.get_mut(item_id).ok_or_else(|| {
                LocalError::Integrity(format!("historical content precedes creation of {item_id}"))
            })?;
            if node.kind != NodeKind::File || !node.live {
                return Err(LocalError::Integrity(format!(
                    "historical content targets a non-live FILE {item_id}"
                )));
            }
            node.blob_id = Some(blob_id.clone());
            node.size = Some(*size);
            node.head_seq = event.item_seq;
        }
        LocalEvent::VersionSetCreate | LocalEvent::VersionSetMutation => {}
    }
    Ok(())
}

fn history_kind(item_type: &str) -> NodeKind {
    match item_type {
        "FILE" => NodeKind::File,
        "FOLDER" => NodeKind::Folder,
        _ => unreachable!("local profile has only FILE and FOLDER"),
    }
}

fn validate_history_parent(
    parent_id: Option<&ItemId>,
    nodes: &HashMap<ItemId, HistoryNode>,
    item_id: &ItemId,
) -> Result<()> {
    let Some(parent_id) = parent_id else {
        return Ok(());
    };
    let parent = nodes.get(parent_id).ok_or_else(|| {
        LocalError::Integrity(format!(
            "historical item {item_id} has missing parent {parent_id}"
        ))
    })?;
    if parent.kind != NodeKind::Folder || !parent.live {
        return Err(LocalError::Integrity(format!(
            "historical item {item_id} has non-live FOLDER parent {parent_id}"
        )));
    }
    Ok(())
}

fn render_history_changes(
    item_type: &str,
    profiles: &[LocalEvent],
    before_path: Option<String>,
    after_path: Option<String>,
) -> Vec<Change> {
    if profiles
        .iter()
        .any(|profile| matches!(profile, LocalEvent::Create { .. }))
    {
        return vec![Change {
            kind: ChangeKind::Add,
            item_type: item_type.to_owned(),
            path: after_path.expect("created item has a path"),
            source_path: None,
            content_changed: false,
        }];
    }
    if profiles
        .iter()
        .any(|profile| matches!(profile, LocalEvent::Trash { .. }))
    {
        return vec![Change {
            kind: ChangeKind::Delete,
            item_type: item_type.to_owned(),
            path: before_path.expect("trashed item has a path"),
            source_path: None,
            content_changed: false,
        }];
    }
    if profiles
        .iter()
        .any(|profile| matches!(profile, LocalEvent::Restore { .. }))
    {
        return vec![Change {
            kind: ChangeKind::Add,
            item_type: item_type.to_owned(),
            path: after_path.expect("restored item has a path"),
            source_path: None,
            content_changed: false,
        }];
    }
    let parent_changed = profiles
        .iter()
        .any(|profile| matches!(profile, LocalEvent::SetParent { .. }));
    let name_changed = profiles
        .iter()
        .any(|profile| matches!(profile, LocalEvent::SetName { .. }));
    let content = profiles
        .iter()
        .any(|profile| matches!(profile, LocalEvent::SetContent { .. }));
    if let Some(kind) = ChangeKind::relocation(parent_changed, name_changed)
        && before_path != after_path
    {
        return vec![Change {
            kind,
            item_type: item_type.to_owned(),
            path: after_path.clone().expect("moved item has a path"),
            source_path: Some(before_path.expect("moved item had a path")),
            content_changed: content,
        }];
    }
    if content {
        return vec![Change {
            kind: ChangeKind::Modify,
            item_type: item_type.to_owned(),
            path: after_path.expect("content item has a path"),
            source_path: None,
            content_changed: true,
        }];
    }
    Vec::new()
}

pub(crate) fn sort_changes(mut changes: Vec<Change>) -> Vec<Change> {
    changes.sort_by(|left, right| left.path.cmp(&right.path).then(left.kind.cmp(&right.kind)));
    changes
}

fn validate_move_intent(
    root: &Path,
    committed: &BTreeMap<String, CommittedNode>,
    intent: &MoveIntent,
) -> Result<()> {
    let operation_id = Uuid::parse_str(&intent.operation_id)
        .map_err(|error| LocalError::Integrity(format!("invalid move operation ID: {error}")))?;
    if intent.version != MOVE_INTENT_VERSION
        || operation_id.to_string() != intent.operation_id
        || intent.message.is_empty()
        || intent.base_seq <= 0
        || intent.result_head_seq <= intent.base_seq
    {
        return Err(LocalError::Integrity(
            "move intent has invalid fixed fields".to_string(),
        ));
    }
    let source = normalize_input_path(root, root, Path::new(&intent.source))
        .map_err(|error| LocalError::Integrity(format!("invalid move source: {error}")))?;
    let destination = normalize_input_path(root, root, Path::new(&intent.destination))
        .map_err(|error| LocalError::Integrity(format!("invalid move destination: {error}")))?;
    if source != intent.source || destination != intent.destination || source == destination {
        return Err(LocalError::Integrity(
            "move intent paths are not canonical".to_string(),
        ));
    }
    let kind = parse_node_kind(&intent.item_type)?;
    let (destination_parent, new_name) = split_display_path(&destination);
    if new_name != intent.new_name {
        return Err(LocalError::Integrity(
            "move intent destination name is inconsistent".to_string(),
        ));
    }
    if kind == NodeKind::Folder
        && destination_parent
            .is_some_and(|parent| parent == source || parent.starts_with(&format!("{source}/")))
    {
        return Err(LocalError::Integrity(
            "move intent places a folder below itself".to_string(),
        ));
    }
    let expected_parent_id = destination_parent
        .map(|parent| {
            let node = committed.get(parent).ok_or_else(|| {
                LocalError::Integrity(format!(
                    "move intent destination parent is not live: {parent}"
                ))
            })?;
            if node.kind != NodeKind::Folder {
                return Err(LocalError::Integrity(format!(
                    "move intent destination parent is not a folder: {parent}"
                )));
            }
            Ok(node.item_id.clone())
        })
        .transpose()?;
    if expected_parent_id != intent.new_parent_id {
        return Err(LocalError::Integrity(
            "move intent destination parent identity is inconsistent".to_string(),
        ));
    }
    let source_parent_id = split_display_path(&source)
        .0
        .map(|parent| {
            committed
                .get(parent)
                .map(|node| node.item_id.clone())
                .ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "move intent source parent is not live: {parent}"
                    ))
                })
        })
        .transpose()?;
    let event_count = i64::from(source_parent_id != intent.new_parent_id)
        + i64::from(split_display_path(&source).1 != intent.new_name);
    if intent.base_seq.checked_add(event_count) != Some(intent.result_head_seq) {
        return Err(LocalError::Integrity(
            "move intent result sequence is inconsistent".to_string(),
        ));
    }
    reject_destination_name_conflict(committed, &intent.item_id, destination_parent, new_name)
        .map_err(|error| LocalError::Integrity(error.to_string()))?;
    Ok(())
}

fn reject_destination_name_conflict(
    committed: &BTreeMap<String, CommittedNode>,
    moved_item_id: &ItemId,
    destination_parent: Option<&str>,
    destination_name: &str,
) -> Result<()> {
    let destination_key = compute_name_key(destination_name)
        .map_err(|error| LocalError::Invalid(format!("invalid move destination name: {error}")))?;
    for (path, node) in committed {
        if &node.item_id == moved_item_id {
            continue;
        }
        let (parent, name) = split_display_path(path);
        if parent == destination_parent
            && compute_name_key(name).map_err(|error| {
                LocalError::Integrity(format!("invalid stored item name {name:?}: {error}"))
            })? == destination_key
        {
            return Err(LocalError::Invalid(format!(
                "move destination conflicts with tracked item {path}"
            )));
        }
    }
    Ok(())
}

fn split_display_path(path: &str) -> (Option<&str>, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (Some(parent), name),
        None => (None, path),
    }
}

fn node_kind_name(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::File => "FILE",
        NodeKind::Folder => "FOLDER",
    }
}

fn parse_node_kind(value: &str) -> Result<NodeKind> {
    match value {
        "FILE" => Ok(NodeKind::File),
        "FOLDER" => Ok(NodeKind::Folder),
        _ => Err(LocalError::Integrity(format!(
            "invalid move item type {value:?}"
        ))),
    }
}

fn filesystem_node_kind(root: &Path, display: &str) -> Result<Option<NodeKind>> {
    let path = root.join(display_to_path(display));
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect move path",
                path,
                source,
            });
        }
    };
    if metadata.file_type().is_file() && !is_link_or_reparse(&metadata) {
        Ok(Some(NodeKind::File))
    } else if metadata.file_type().is_dir() && !is_link_or_reparse(&metadata) {
        Ok(Some(NodeKind::Folder))
    } else {
        Err(LocalError::Integrity(format!(
            "move path is not a regular file or directory: {}",
            path.display()
        )))
    }
}

fn sync_move_parents(root: &Path, source: &str, destination: &str) -> Result<()> {
    let source_parent = display_to_path(source)
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map_or_else(|| root.to_path_buf(), |path| root.join(path));
    let destination_parent = display_to_path(destination)
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map_or_else(|| root.to_path_buf(), |path| root.join(path));
    sync_directory(&source_parent)?;
    if destination_parent != source_parent {
        sync_directory(&destination_parent)?;
    }
    Ok(())
}

pub(crate) fn validate_commit_message(message: Option<&str>) -> Result<()> {
    if message == Some("") {
        return Err(LocalError::Invalid(
            "commit message must not be empty".to_string(),
        ));
    }
    if message.is_some_and(|value| value.len() > synck_file_folder::MAX_MESSAGE_BYTES) {
        return Err(LocalError::FileFolder(SemanticError {
            code: SemanticErrorCode::MessageTooLarge,
            detail: "message exceeds the UTF-8 byte limit".to_string(),
        }));
    }
    Ok(())
}

fn expected_file_objects(
    working: &BTreeMap<String, WorkingNode>,
    changes: &[Change],
) -> Result<HashMap<String, BlobId>> {
    let mut objects = HashMap::new();
    for change in changes {
        if !matches!(change.kind, ChangeKind::Add | ChangeKind::Modify)
            && !change.kind.is_relocation()
        {
            continue;
        }
        let Some(node) = working.get(&change.path) else {
            continue;
        };
        if node.kind != NodeKind::File || (change.kind.is_relocation() && !change.content_changed) {
            continue;
        }
        let digest = node.digest_hex.as_deref().ok_or_else(|| {
            LocalError::Integrity(format!("scanned FILE has no digest: {}", change.path))
        })?;
        let blob_id = BlobId::parse(format!("sha256:{digest}")).map_err(|error| {
            LocalError::Integrity(format!(
                "scanned FILE has an invalid digest for {}: {error}",
                change.path
            ))
        })?;
        objects.insert(change.path.clone(), blob_id);
    }
    Ok(objects)
}

pub(crate) fn select_changes(
    root: &Path,
    caller: &Path,
    inputs: &[PathBuf],
    committed: &BTreeMap<String, CommittedNode>,
    working: &BTreeMap<String, WorkingNode>,
    working_ids: &HashMap<String, ItemId>,
    changes: &[Change],
) -> Result<Vec<Change>> {
    if inputs.is_empty() {
        return Ok(changes.to_vec());
    }

    let committed_paths = committed
        .iter()
        .map(|(path, node)| (&node.item_id, path))
        .collect::<HashMap<_, _>>();
    let working_paths = working_ids
        .iter()
        .map(|(path, item_id)| (item_id, path))
        .collect::<HashMap<_, _>>();
    let mut scopes = Vec::with_capacity(inputs.len() * 2);
    for input in inputs {
        let scope = normalize_input_path(root, caller, input)?;
        if !committed.contains_key(&scope) && !working.contains_key(&scope) {
            return Err(LocalError::Invalid(format!(
                "selected path does not exist in the working tree or accepted state: {scope}"
            )));
        }

        let committed_item = committed.get(&scope).map(|node| &node.item_id);
        let working_item = working_ids.get(&scope);
        let recursive = committed
            .get(&scope)
            .is_some_and(|node| node.kind == NodeKind::Folder)
            || working
                .get(&scope)
                .is_some_and(|node| node.kind == NodeKind::Folder);
        let selected_item = working_item.or(committed_item);
        if let Some(item_id) = selected_item {
            let working_path = working_paths.get(item_id).copied();
            for path in [committed_paths.get(item_id), working_paths.get(item_id)]
                .into_iter()
                .flatten()
            {
                if committed
                    .get(*path)
                    .is_some_and(|node| node.item_id != *item_id)
                    || working_path.is_some_and(|working_path| {
                        working_path != *path && working.contains_key(*path)
                    })
                {
                    return Err(LocalError::Invalid(format!(
                        "selected item path is reused by another item in accepted or working state: {path}"
                    )));
                }
            }
        }
        add_selection_scope(
            &mut scopes,
            SelectionScope {
                path: scope,
                recursive,
            },
        );
        if let Some(item_id) = selected_item {
            if let Some(path) = committed_paths.get(item_id) {
                add_selection_scope(
                    &mut scopes,
                    SelectionScope {
                        path: (*path).clone(),
                        recursive,
                    },
                );
            }
            if let Some(path) = working_paths.get(item_id) {
                add_selection_scope(
                    &mut scopes,
                    SelectionScope {
                        path: (*path).clone(),
                        recursive,
                    },
                );
            }
        }
    }

    let mut selected = changes
        .iter()
        .enumerate()
        .filter(|(_, change)| {
            scopes.iter().any(|scope| {
                selection_scope_contains(scope, &change.path)
                    || change
                        .source_path
                        .as_deref()
                        .is_some_and(|path| selection_scope_contains(scope, path))
            })
        })
        .map(|(index, _)| index)
        .collect::<HashSet<_>>();

    let dependencies = changes
        .iter()
        .enumerate()
        .filter(|(_, change)| change.kind == ChangeKind::Add)
        .map(|(index, change)| (change.path.as_str(), index))
        .collect::<HashMap<_, _>>();
    let same_path_deletions = changes
        .iter()
        .enumerate()
        .filter(|(_, change)| change.kind == ChangeKind::Delete)
        .map(|(index, change)| (change.path.as_str(), index))
        .collect::<HashMap<_, _>>();
    for index in selected.iter().copied().collect::<Vec<_>>() {
        let mut parent = changes[index]
            .path
            .rsplit_once('/')
            .map(|(parent, _)| parent);
        while let Some(path) = parent {
            if let Some(dependency) = dependencies.get(path) {
                selected.insert(*dependency);
                if let Some(deletion) = same_path_deletions.get(path) {
                    selected.insert(*deletion);
                }
            }
            parent = path.rsplit_once('/').map(|(parent, _)| parent);
        }
    }

    Ok(changes
        .iter()
        .enumerate()
        .filter(|(index, _)| selected.contains(index))
        .map(|(_, change)| change.clone())
        .collect())
}

struct SelectionScope {
    path: String,
    recursive: bool,
}

fn add_selection_scope(scopes: &mut Vec<SelectionScope>, scope: SelectionScope) {
    if scopes.iter().any(|existing| {
        (existing.recursive && path_in_scope(&scope.path, &existing.path))
            || (!scope.recursive && existing.path == scope.path)
    }) {
        return;
    }
    if scope.recursive {
        scopes.retain(|existing| !path_in_scope(&existing.path, &scope.path));
    }
    scopes.push(scope);
}

fn selection_scope_contains(scope: &SelectionScope, path: &str) -> bool {
    path == scope.path || (scope.recursive && path_in_scope(path, &scope.path))
}

fn path_in_scope(path: &str, scope: &str) -> bool {
    path == scope
        || path
            .strip_prefix(scope)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn move_pending_error(intent: &MoveIntent, error: LocalError) -> LocalError {
    LocalError::MoveNeedsRecovery {
        from_path: intent.source.clone(),
        to_path: intent.destination.clone(),
        detail: error.to_string(),
    }
}

pub(crate) struct BuiltEvents {
    pub events: Vec<EventProposal>,
    pub ref_events: Vec<RefEventProposal>,
    pub subtree_guards: Vec<SubtreeClosureGuard>,
}

pub(crate) struct BuiltIntents {
    pub intents: Vec<Intent>,
    pub created_ids: HashMap<String, ItemId>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_intents(
    committed: &BTreeMap<String, CommittedNode>,
    working: &BTreeMap<String, WorkingNode>,
    changes: &[Change],
    working_ids: &HashMap<String, ItemId>,
    objects: &HashMap<String, BlobId>,
    root_parent_id: Option<ItemId>,
) -> Result<BuiltIntents> {
    let trash_paths = changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Delete)
        .map(|change| change.path.as_str())
        .collect::<HashSet<_>>();
    let mut trash_roots = trash_paths
        .iter()
        .copied()
        .filter(|path| {
            !trash_paths
                .iter()
                .any(|ancestor| *ancestor != *path && path_in_scope(path, ancestor))
        })
        .collect::<Vec<_>>();
    trash_roots.sort();
    let mut intents = Vec::new();
    for path in trash_roots {
        let root = committed.get(path).ok_or_else(|| {
            LocalError::Integrity(format!("deleted path is not committed: {path}"))
        })?;
        intents.push(Intent::Trash {
            root: ItemBase {
                item_id: root.item_id.clone(),
                base_seq: root.head_seq,
            },
            descendants: Vec::new(),
        });
    }

    let new_paths: HashSet<&str> = changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Add)
        .map(|change| change.path.as_str())
        .collect();
    let mut new_ids = HashMap::new();
    for (path, _) in working
        .iter()
        .filter(|(path, _)| new_paths.contains(path.as_str()))
    {
        new_ids.insert(path.clone(), ItemId::new());
    }
    let mut folders: Vec<&String> = new_ids
        .keys()
        .filter(|path| working[*path].kind == NodeKind::Folder)
        .collect();
    folders.sort_by(|left, right| {
        path_depth(left)
            .cmp(&path_depth(right))
            .then_with(|| left.cmp(right))
    });
    for path in folders {
        let item_id = new_ids[path].clone();
        intents.push(Intent::CreateFolder {
            item_id,
            location: Location {
                parent_item_id: parent_id(
                    path,
                    committed,
                    working_ids,
                    &new_ids,
                    root_parent_id.clone(),
                )?,
                name: path_name(path).to_string(),
            },
        });
    }

    let mut moves = changes
        .iter()
        .filter(|change| change.kind.is_relocation())
        .collect::<Vec<_>>();
    moves.sort_by(|left, right| {
        path_depth(&left.path)
            .cmp(&path_depth(&right.path))
            .then_with(|| left.path.cmp(&right.path))
    });
    for change in moves {
        let source = change
            .source_path
            .as_deref()
            .expect("relocation has a source path");
        let old = &committed[source];
        let node = &working[&change.path];
        let location = Location {
            parent_item_id: parent_id(
                &change.path,
                committed,
                working_ids,
                &new_ids,
                root_parent_id.clone(),
            )?,
            name: path_name(&change.path).to_string(),
        };
        match old.kind {
            NodeKind::Folder => intents.push(Intent::UpdateFolder {
                item_id: old.item_id.clone(),
                base_seq: old.head_seq,
                location,
            }),
            NodeKind::File => intents.push(Intent::UpdateFile {
                item_id: old.item_id.clone(),
                base_seq: old.head_seq,
                location: Some(location),
                content: objects
                    .get(&change.path)
                    .map(|blob_id| blob_ref(blob_id, node.size.expect("moved FILE has size")))
                    .transpose()?,
            }),
        }
    }

    let mut files: Vec<&String> = new_ids
        .keys()
        .filter(|path| working[*path].kind == NodeKind::File)
        .collect();
    files.sort();
    for path in files {
        let item_id = new_ids[path].clone();
        let node = &working[path];
        intents.push(Intent::CreateFile {
            item_id,
            location: Location {
                parent_item_id: parent_id(
                    path,
                    committed,
                    working_ids,
                    &new_ids,
                    root_parent_id.clone(),
                )?,
                name: path_name(path).to_string(),
            },
            content: blob_ref(&objects[path], node.size.expect("new FILE has size"))?,
        });
    }

    for change in changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Modify)
    {
        let node = &working[&change.path];
        let source = change.source_path.as_deref().unwrap_or(&change.path);
        let old = &committed[source];
        intents.push(Intent::UpdateFile {
            item_id: old.item_id.clone(),
            base_seq: old.head_seq,
            location: None,
            content: Some(blob_ref(
                &objects[&change.path],
                node.size.expect("modified FILE has size"),
            )?),
        });
    }
    Ok(BuiltIntents {
        intents,
        created_ids: new_ids,
    })
}

pub(crate) fn materialize_events(
    repo_id: &RepoId,
    message: Option<&str>,
    intents: Vec<Intent>,
    current: &BTreeMap<ItemId, CurrentItem>,
) -> Result<BuiltEvents> {
    let materialized = validate_and_materialize(
        repo_id,
        &IntentProposal {
            message: message.map(str::to_string),
            intents,
        },
        current,
    )?;
    let mut events = Vec::new();
    let mut ref_events = Vec::new();
    for proposal in materialized.events {
        let initial_file =
            proposal.event_type == "FILE::SET_CONTENT" && proposal.base_seq.is_none();
        let file_id = proposal.item_id.clone();
        events.push(proposal);
        if initial_file {
            let file_id = file_id.expect("materialized content has an item ID");
            let source_commit_event_idx = i32::try_from(events.len() - 1)
                .map_err(|_| LocalError::Invalid("commit event index overflow".to_string()))?;
            let content = current_or_created_blob(&events, &file_id)?;
            let version_id = Uuid::now_v7().to_string();
            let size = i64::try_from(content.size).map_err(|_| {
                LocalError::Invalid("file is too large for local storage".to_string())
            })?;
            ref_events.push(
                synck_file_folder::file_version_set::automatic_initial_ref_event(
                    repo_id,
                    &file_id,
                    &version_id,
                    0,
                    2,
                    source_commit_event_idx,
                    content.blob_id.to_string(),
                    Some(size),
                )
                .map_err(LocalError::Invalid)?,
            );
        }
    }
    let (target_ids, _) = proposal_item_kind_requirements(&events);
    let kinds = target_ids
        .iter()
        .map(|item_id| {
            let item = &current[item_id];
            synck_file_folder::ImmutableItemKind {
                item_id: item_id.clone(),
                item_type: item.item_type.clone(),
            }
        })
        .collect::<Vec<_>>();
    validate_projected_proposal(message, &events, &kinds)?;
    Ok(BuiltEvents {
        events,
        ref_events,
        subtree_guards: materialized.subtree_guards,
    })
}

fn path_name(path: &str) -> &str {
    path.rsplit('/').next().expect("tracked path is nonempty")
}

fn blob_ref(blob_id: &BlobId, size: u64) -> Result<BlobRef> {
    Ok(BlobRef {
        blob_id: blob_id.clone(),
        size,
    })
}

fn current_or_created_blob(events: &[EventProposal], file_id: &ItemId) -> Result<BlobRef> {
    let event = events
        .iter()
        .rev()
        .find(|event| {
            event.item_id.as_ref() == Some(file_id) && event.event_type == "FILE::SET_CONTENT"
        })
        .ok_or_else(|| LocalError::Integrity("materialized FILE has no content".to_string()))?;
    let (blob_id, size) =
        crate::event_profile::parse_content(event).map_err(LocalError::Integrity)?;
    Ok(BlobRef {
        blob_id,
        size: u64::try_from(size).map_err(|_| {
            LocalError::Integrity("materialized FILE has negative size".to_string())
        })?,
    })
}

fn parent_id(
    path: &str,
    committed: &BTreeMap<String, CommittedNode>,
    working_ids: &HashMap<String, ItemId>,
    new_ids: &HashMap<String, ItemId>,
    root_parent_id: Option<ItemId>,
) -> Result<Option<ItemId>> {
    let Some((parent, _)) = path.rsplit_once('/') else {
        return Ok(root_parent_id);
    };
    if let Some(item_id) = new_ids.get(parent) {
        return Ok(Some(item_id.clone()));
    }
    if let Some(item_id) = working_ids.get(parent) {
        return Ok(Some(item_id.clone()));
    }
    let parent_node = committed
        .get(parent)
        .ok_or_else(|| LocalError::Integrity(format!("new path {path} has no parent item")))?;
    if parent_node.kind != NodeKind::Folder {
        return Err(LocalError::Integrity(format!(
            "new path {path} has a non-folder parent"
        )));
    }
    Ok(Some(parent_node.item_id.clone()))
}

fn path_depth(path: &str) -> usize {
    path.bytes().filter(|byte| *byte == b'/').count() + 1
}

fn parse_event(event: &synck_vcs::types::EventRow) -> Result<EventProposal> {
    serde_json::from_slice(&event.payload)
        .map_err(|error| LocalError::Integrity(format!("invalid event payload: {error}")))
}

fn validate_historical_item_event(event: &EventRow, item_id: &ItemId) -> Result<EventProposal> {
    if compute_sha256_digest_bytes(&event.payload)
        != sha256_digest_to_digest_bytes(&event.event_payload_digest)
    {
        return Err(LocalError::Integrity(format!(
            "historical event payload digest disagrees for {item_id} at item sequence {}",
            event.item_seq
        )));
    }
    let proposal = parse_event(event)?;
    if canonical_event_proposal_bytes(&proposal)? != event.payload
        || proposal.item_id.as_ref() != Some(item_id)
        || proposal.event_type != event.event_type
    {
        return Err(LocalError::Integrity(format!(
            "historical event identity disagrees for {item_id} at item sequence {}",
            event.item_seq
        )));
    }
    Ok(proposal)
}

fn content_from_event(event: &EventProposal) -> Result<(BlobId, i64)> {
    let blob_id = match event.set_content.as_ref() {
        Some(EventContent::Blob { blob_id }) => BlobId::parse(blob_id).map_err(|error| {
            LocalError::Integrity(format!("invalid blob identifier in event: {error}"))
        })?,
        _ => {
            return Err(LocalError::Integrity(
                "file content event has no blob".to_string(),
            ));
        }
    };
    let size = event
        .set_metadata
        .as_ref()
        .and_then(|value| value.pointer("/content/size"))
        .and_then(serde_json::Value::as_i64)
        .filter(|size| *size >= 0)
        .ok_or_else(|| LocalError::Integrity("file content event has invalid size".to_string()))?;
    Ok((blob_id, size))
}

pub(crate) fn normalize_input_path(root: &Path, caller: &Path, input: &Path) -> Result<String> {
    if input.is_absolute() {
        return Err(LocalError::Invalid("path must be relative".to_string()));
    }
    let caller = caller
        .canonicalize()
        .at("resolve current directory", caller.to_path_buf())?;
    let relative_caller = caller
        .strip_prefix(root)
        .map_err(|_| LocalError::Invalid("current directory is outside repository".to_string()))?;
    let mut parts = Vec::new();
    for component in relative_caller.components().chain(input.components()) {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| LocalError::Invalid("path is not valid Unicode".to_string()))?;
                parts.push(value.to_string());
            }
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(LocalError::Invalid(
                        "path escapes repository root".to_string(),
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(LocalError::Invalid("path must be relative".to_string()));
            }
        }
    }
    if parts.is_empty() {
        return Err(LocalError::Invalid(
            "path does not name an item".to_string(),
        ));
    }
    Ok(parts.join("/"))
}

fn resolve_export_destination(
    caller: &Path,
    input: &Path,
    metadata_path: &Path,
) -> Result<(PathBuf, OsString, PathBuf)> {
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        caller.join(input)
    };
    let name = match candidate.components().next_back() {
        Some(Component::Normal(name)) => name.to_os_string(),
        _ => {
            return Err(LocalError::Invalid(
                "export destination must name a file".to_string(),
            ));
        }
    };
    let unresolved_parent = candidate.parent().ok_or_else(|| {
        LocalError::Invalid("export destination has no parent directory".to_string())
    })?;
    let metadata = std::fs::symlink_metadata(unresolved_parent)
        .at("inspect export parent", unresolved_parent)?;
    if !metadata.file_type().is_dir() || is_link_or_reparse(&metadata) {
        return Err(LocalError::Invalid(format!(
            "export parent is not a real directory: {}",
            unresolved_parent.display()
        )));
    }
    let parent = unresolved_parent
        .canonicalize()
        .at("resolve export parent", unresolved_parent)?;
    if parent.starts_with(metadata_path) {
        return Err(LocalError::Invalid(
            "export destination must be outside .synck".to_string(),
        ));
    }
    let destination = parent.join(&name);
    Ok((parent, name, destination))
}

pub(crate) fn display_to_path(display: &str) -> PathBuf {
    display.split('/').collect()
}

#[cfg(windows)]
fn replace_file_windows(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source =
        windows_extended_length_path_units(&source.as_os_str().encode_wide().collect::<Vec<_>>());
    let destination = windows_extended_length_path_units(
        &destination.as_os_str().encode_wide().collect::<Vec<_>>(),
    );
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use cap_std::{ambient_authority, fs::Dir};
    use serde::Deserialize;
    use sha2::{Digest, Sha256};
    use synck_file_folder::{
        CurrentItem, ImmutableItemKind, IntentProposal, project_proposal_events,
        validate_and_materialize,
    };
    use synck_vcs::types::{
        COMMIT_FORMAT_VERSION, CommitId, CommitRow, EventProposal, EventRow, ItemId, RepoId,
        Sha256Digest, canonical_commit_bytes_for_hash, canonical_event_proposal_bytes,
        compute_commit_id, compute_sha256_digest_bytes, sha256_prefixed_hex_from_digest_bytes,
    };

    #[cfg(windows)]
    use super::entry_matches_revert_state;
    use super::{
        CommitOutcome, RecoverIntentItem, RevertIntentItem, RevertItemState,
        WORKTREE_RESTORE_INTENT_NAME, WORKTREE_RESTORE_INTENT_VERSION, WORKTREE_RESTORE_STAGE_NAME,
        WorktreeRestoreIntent, build_recover_intent, build_revert_intent,
        build_worktree_restore_items, materialize_events, proposal_item_kind_requirements,
        reject_reserved_recover_items, reject_reserved_revert_items, worktree_restore_manifest,
    };
    use crate::LocalError;
    use crate::event_profile::{LocalEvent, classify_commit};
    use crate::init;
    use crate::repository::{Repository, windows_extended_length_path_units};

    const CONFORMANCE_FIXTURE: &str =
        include_str!("../../synck-file-folder/tests/fixtures/v1/conformance.json");

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ConformanceFixture {
        repo_id: RepoId,
        cases: Vec<ConformanceCase>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ConformanceCase {
        name: String,
        proposal: IntentProposal,
        current: Vec<CurrentItem>,
        expected_error: Option<String>,
    }

    fn canonical_events(events: &[EventProposal]) -> Vec<Vec<u8>> {
        events
            .iter()
            .map(|event| canonical_event_proposal_bytes(event).unwrap())
            .collect()
    }

    fn accepted_rows(
        repo_id: &RepoId,
        message: Option<&str>,
        events: &[EventProposal],
        current: &BTreeMap<ItemId, CurrentItem>,
    ) -> (CommitRow, Vec<EventRow>) {
        let created_at = "2026-08-20T12:00:00Z".parse().unwrap();
        let mut next_item_seq = BTreeMap::new();
        let mut rows = Vec::with_capacity(events.len());
        let mut aggregate_digest = Sha256::new();
        for (index, event) in events.iter().enumerate() {
            let item_id = event.item_id.clone().unwrap();
            let item_seq = next_item_seq.entry(item_id.clone()).or_insert_with(|| {
                event
                    .create_item
                    .as_ref()
                    .map_or_else(|| current.get(&item_id).unwrap().head_seq + 1, |_| 1)
            });
            let payload = canonical_event_proposal_bytes(event).unwrap();
            let digest = compute_sha256_digest_bytes(&payload);
            aggregate_digest.update([synck_vcs::types::COMMIT_EVENT_FAMILY_ITEM]);
            aggregate_digest.update(digest);
            rows.push(EventRow {
                repo_id: repo_id.clone(),
                item_id,
                item_seq: *item_seq,
                event_type: event.event_type.clone(),
                commit_id: CommitId::parse(format!("sha256:{}", "0".repeat(64))).unwrap(),
                commit_event_idx: index as i32,
                repo_seq: Some(1),
                payload,
                event_payload_digest: Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
                    digest,
                ))
                .unwrap(),
                created_at,
                parent_folder_id: None,
                parent_folder_id_before: None,
            });
            *item_seq += 1;
        }
        let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
            aggregate_digest.finalize().into(),
        ))
        .unwrap();
        let commit_id = compute_commit_id(
            &canonical_commit_bytes_for_hash(
                &"test".to_string(),
                &"test".to_string(),
                None,
                message,
                None,
                events.len(),
                &events_digest,
            )
            .unwrap(),
        );
        for row in &mut rows {
            row.commit_id = commit_id.clone();
        }
        (
            CommitRow {
                repo_id: repo_id.clone(),
                commit_id,
                repo_seq: Some(1),
                format_version: COMMIT_FORMAT_VERSION,
                import_job_id: None,
                author_id: "test".to_string(),
                committer_id: "test".to_string(),
                message: message.map(str::to_string),
                occurred_at: None,
                created_at,
                event_count: events.len() as i32,
                events_digest,
            },
            rows,
        )
    }

    #[test]
    fn local_adapter_matches_shared_conformance_corpus() {
        let fixture: ConformanceFixture = serde_json::from_str(CONFORMANCE_FIXTURE).unwrap();
        assert_eq!(synck_file_folder::WORKTREE_CONTROL_NAME_KEY, ".synck");
        for case in fixture.cases {
            let current: BTreeMap<_, _> = case
                .current
                .into_iter()
                .map(|item| (item.item_id.clone(), item))
                .collect();
            let reference = validate_and_materialize(&fixture.repo_id, &case.proposal, &current);
            let local = materialize_events(
                &fixture.repo_id,
                case.proposal.message.as_deref(),
                case.proposal.intents.clone(),
                &current,
            );
            match (reference, local, case.expected_error) {
                (Ok(reference), Ok(local), None) => {
                    let (target_ids, _) = proposal_item_kind_requirements(&local.events);
                    let kinds = target_ids
                        .iter()
                        .map(|item_id| ImmutableItemKind {
                            item_id: item_id.clone(),
                            item_type: current[item_id].item_type.clone(),
                        })
                        .collect::<Vec<_>>();
                    let projected = project_proposal_events(&local.events, &kinds).unwrap();
                    assert_eq!(
                        canonical_events(&projected),
                        canonical_events(&reference.events),
                        "{}",
                        case.name
                    );

                    let (commit, rows) = accepted_rows(
                        &fixture.repo_id,
                        case.proposal.message.as_deref(),
                        &local.events,
                        &current,
                    );
                    let item_kinds = current
                        .iter()
                        .map(|(item_id, item)| (item_id.clone(), item.item_type.clone()))
                        .collect();
                    let classified = classify_commit(&commit, &rows, &item_kinds).unwrap();
                    let visible = classified
                        .into_iter()
                        .filter_map(|event| match event.profile {
                            LocalEvent::VersionSetCreate | LocalEvent::VersionSetMutation => None,
                            _ => Some(event.proposal),
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(
                        canonical_events(&visible),
                        canonical_events(&reference.events),
                        "{}",
                        case.name
                    );
                }
                (Err(reference), Err(LocalError::FileFolder(local)), Some(expected)) => {
                    assert_eq!(reference.code, local.code, "{}", case.name);
                    assert_eq!(local.code.as_str(), expected, "{}", case.name);
                }
                (reference, local, expected) => panic!(
                    "{} diverged: reference={}, local={}, expected_error={expected:?}",
                    case.name,
                    reference
                        .as_ref()
                        .err()
                        .map_or("accepted", |error| error.code.as_str()),
                    match &local {
                        Ok(_) => "accepted",
                        Err(LocalError::FileFolder(error)) => error.code.as_str(),
                        Err(_) => "non-semantic-error",
                    }
                ),
            }
        }
    }

    #[test]
    fn windows_absolute_paths_use_the_extended_length_prefix() {
        let input: Vec<u16> = r"C:\work\file".encode_utf16().collect();
        let expected: Vec<u16> = "\\\\?\\C:\\work\\file\0".encode_utf16().collect();
        assert_eq!(windows_extended_length_path_units(&input), expected);
    }

    #[test]
    fn recovery_and_revert_reject_a_reserved_live_root_target() {
        let item_id = ItemId::new();
        let name = ".__synck_file_version_set_visible".to_string();
        let recover = RecoverIntentItem {
            item_id: item_id.clone(),
            item_type: "FOLDER".to_string(),
            parent_id: None,
            name: name.clone(),
            path: name.clone(),
            base_seq: 2,
            blob_id: None,
            size: None,
        };
        assert!(reject_reserved_recover_items(&[recover]).is_err());

        let live = RevertItemState {
            parent_id: None,
            name: name.clone(),
            path: name.clone(),
            live: true,
            head_seq: 1,
            blob_id: None,
            size: None,
        };
        let trashed = RevertItemState {
            live: false,
            head_seq: 2,
            ..live.clone()
        };
        let revert = RevertIntentItem {
            item_id,
            item_type: "FOLDER".to_string(),
            before: Some(live),
            after: trashed,
        };
        assert!(reject_reserved_revert_items(&[revert]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn inverse_stage_rejects_a_windows_junction() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target");
        let junction = temporary.path().join("junction");
        fs::create_dir(&target).unwrap();
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "mklink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let state = RevertItemState {
            parent_id: None,
            name: "folder".to_string(),
            path: "folder".to_string(),
            live: true,
            head_seq: 1,
            blob_id: None,
            size: None,
        };
        assert!(!entry_matches_revert_state(&junction, "FOLDER", &state).unwrap());
    }

    #[tokio::test]
    async fn export_publication_never_replaces_an_intervening_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document.bin"), b"committed").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        let (_, blob_id, size) = repository
            .resolve_file_revision(root, std::path::Path::new("document.bin"), None)
            .await
            .unwrap();
        let output = tempfile::tempdir().unwrap();
        let parent = Dir::open_ambient_dir(output.path(), ambient_authority()).unwrap();
        let destination = output.path().join("export.bin");
        let staged = repository
            .copy_object_to_temporary(&parent, output.path(), &destination, &blob_id, size)
            .unwrap();
        fs::write(&destination, b"intervening").unwrap();

        repository
            .publish_temporary_file(
                &parent,
                output.path(),
                &staged,
                std::ffi::OsStr::new("export.bin"),
                &destination,
                false,
            )
            .unwrap_err();

        assert_eq!(fs::read(&destination).unwrap(), b"intervening");
        assert!(!output.path().join(staged).exists());
    }

    #[tokio::test]
    async fn discovery_materializes_an_accepted_recover_intent_once() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document.bin"), b"recoverable").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        let item_id = repository
            .committed_tree()
            .await
            .unwrap()
            .get("document.bin")
            .unwrap()
            .item_id
            .clone();
        fs::remove_file(root.join("document.bin")).unwrap();
        repository.commit("delete document").await.unwrap();

        let replay = repository.replay_history(None).await.unwrap();
        let intent = build_recover_intent(&item_id, "recover document", &replay.nodes).unwrap();
        repository.validate_recover_objects(&intent).unwrap();
        repository.write_recover_intent(&intent).unwrap();
        repository.append_recover_intent(&intent).await.unwrap();
        repository.recover().await.unwrap();
        assert!(!root.join("document.bin").exists());
        assert!(root.join(".synck/recover.json").is_file());
        drop(repository);

        let recovered = Repository::discover(root).await.unwrap();
        assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"recoverable");
        assert!(!root.join(".synck/recover.json").exists());
        assert_eq!(
            recovered
                .log(None)
                .await
                .unwrap()
                .iter()
                .filter(|commit| commit.message.as_deref() == Some("recover document"))
                .count(),
            1
        );
        assert!(recovered.status().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn discovery_completes_an_accepted_partially_staged_revert_once() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document.bin"), b"first").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        fs::write(root.join("document.bin"), b"second").unwrap();
        repository.commit("edit document").await.unwrap();

        let head = repository.log(Some(1)).await.unwrap().remove(0);
        let before = repository
            .replay_history(Some(head.repo_seq - 1))
            .await
            .unwrap();
        let after = repository
            .replay_history(Some(head.repo_seq))
            .await
            .unwrap();
        let intent = build_revert_intent(
            repository.config().repo_id(),
            (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
            (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
            "revert document",
            &before.nodes,
            &after.nodes,
            &after.nodes,
        )
        .unwrap();
        repository.validate_revert_objects(&intent).unwrap();
        repository.write_revert_intent(&intent).unwrap();
        repository.append_revert_intent(&intent).await.unwrap();
        repository.recover().await.unwrap();
        let stage = repository.ensure_revert_stage().unwrap();
        repository
            .stage_revert_item(&stage, &intent.items[0])
            .unwrap();
        assert!(!root.join("document.bin").exists());
        assert!(stage.join(intent.items[0].item_id.as_str()).is_file());
        drop(repository);

        let recovered = Repository::discover(root).await.unwrap();
        assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"first");
        assert!(!root.join(".synck/revert.json").exists());
        assert!(!root.join(".synck/revert-stage").exists());
        assert_eq!(
            recovered
                .log(None)
                .await
                .unwrap()
                .iter()
                .filter(|commit| commit.message.as_deref() == Some("revert document"))
                .count(),
            1
        );
        assert!(recovered.status().await.unwrap().is_empty());
        assert!(recovered.fsck().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn discovery_appends_a_persisted_unaccepted_revert_once() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document.bin"), b"first").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        fs::write(root.join("document.bin"), b"second").unwrap();
        repository.commit("edit document").await.unwrap();

        let head = repository.log(Some(1)).await.unwrap().remove(0);
        let before = repository
            .replay_history(Some(head.repo_seq - 1))
            .await
            .unwrap();
        let after = repository
            .replay_history(Some(head.repo_seq))
            .await
            .unwrap();
        let intent = build_revert_intent(
            repository.config().repo_id(),
            (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
            (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
            "revert document",
            &before.nodes,
            &after.nodes,
            &after.nodes,
        )
        .unwrap();
        repository.write_revert_intent(&intent).unwrap();
        drop(repository);

        let recovered = Repository::discover(root).await.unwrap();
        assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"first");
        assert_eq!(
            recovered
                .log(None)
                .await
                .unwrap()
                .iter()
                .filter(|commit| commit.message.as_deref() == Some("revert document"))
                .count(),
            1
        );
        assert!(recovered.status().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn discovery_refuses_a_changed_preappend_revert_worktree() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document.bin"), b"first").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        fs::write(root.join("document.bin"), b"second").unwrap();
        repository.commit("edit document").await.unwrap();

        let head = repository.log(Some(1)).await.unwrap().remove(0);
        let before = repository
            .replay_history(Some(head.repo_seq - 1))
            .await
            .unwrap();
        let after = repository
            .replay_history(Some(head.repo_seq))
            .await
            .unwrap();
        let intent = build_revert_intent(
            repository.config().repo_id(),
            (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
            (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
            "revert document",
            &before.nodes,
            &after.nodes,
            &after.nodes,
        )
        .unwrap();
        repository.write_revert_intent(&intent).unwrap();
        drop(repository);
        fs::write(root.join("document.bin"), b"third").unwrap();

        let error = match Repository::discover(root).await {
            Ok(_) => panic!("changed pre-append revert unexpectedly recovered"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("working tree changed after the revert intent")
        );
    }

    #[tokio::test]
    async fn discovery_clears_a_completed_worktree_restore_intent() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document.bin"), b"before").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        fs::write(root.join("document.bin"), b"after").unwrap();
        fs::write(root.join("added.bin"), b"added").unwrap();
        let (committed, working, _, working_ids) = repository.changes().await.unwrap();
        let head = repository.log(Some(1)).await.unwrap().remove(0);
        let intent = WorktreeRestoreIntent {
            version: WORKTREE_RESTORE_INTENT_VERSION,
            base_commit_id: Some(CommitId::parse(&head.commit_id).unwrap()),
            base_repo_seq: head.repo_seq,
            source_tree: worktree_restore_manifest(&working, &working_ids).unwrap(),
            items: build_worktree_restore_items(&committed, &working, &working_ids).unwrap(),
        };
        repository
            .write_json_intent(WORKTREE_RESTORE_INTENT_NAME, "worktree-restore", &intent)
            .unwrap();
        repository
            .materialize_inverse_items(&intent.items, WORKTREE_RESTORE_STAGE_NAME)
            .unwrap();
        repository
            .remove_inverse_stage(WORKTREE_RESTORE_STAGE_NAME)
            .unwrap();
        drop(repository);

        let recovered = Repository::discover(root).await.unwrap();
        assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"before");
        assert!(!root.join("added.bin").exists());
        assert!(!root.join(".synck/worktree-restore.json").exists());
        assert!(recovered.status().await.unwrap().is_empty());
        assert!(recovered.fsck().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ordinary_commit_reconciles_once_and_publishes_only_affected_identities() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("changed.bin"), b"before").unwrap();
        fs::write(root.join("unrelated.bin"), b"unrelated").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();

        let historical_before = repository.state_index_catch_ups();
        let work_before = repository.commit_work_counters();
        fs::write(root.join("changed.bin"), b"after").unwrap();
        repository.commit("edit one file").await.unwrap();

        let work_after = repository.commit_work_counters();
        // A checkout keeps its history index current: recovery before the
        // commit and recovery after sequencing each catch up incrementally.
        assert_eq!(repository.state_index_catch_ups() - historical_before, 2);
        assert_eq!(work_after.0 - work_before.0, 1);
        assert_eq!(work_after.1 - work_before.1, 1);
        assert_eq!(repository.log(Some(1)).await.unwrap().len(), 1);
        assert_eq!(repository.state_index_catch_ups() - historical_before, 3);
        assert!(repository.status().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn commit_checkpoints_changed_chunked_objects_once() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        for index in 0..3 {
            fs::write(root.join(format!("document-{index}.bin")), b"before").unwrap();
        }
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();

        let before = repository.object_store().durability_checkpoint_count();
        for index in 0..3 {
            fs::write(root.join(format!("document-{index}.bin")), b"after").unwrap();
        }
        repository.commit("edit three files").await.unwrap();
        let after = repository.object_store().durability_checkpoint_count();

        assert_eq!(after - before, 1);
        assert!(repository.status().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn accepted_commit_recovers_after_exact_identity_publication_fails() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document.bin"), b"before").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        fs::write(root.join("document.bin"), b"after").unwrap();

        let database = root.join(".synck/repository.local.sqlite3");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_identity_update
                 BEFORE UPDATE ON local_worktree_identities
                 BEGIN
                     SELECT RAISE(ABORT, 'injected identity publication failure');
                 END;",
            )
            .unwrap();
        drop(connection);

        let accepted = match repository.commit("accepted once").await.unwrap_err() {
            crate::LocalError::AcceptedNeedsRecovery { commit_id, .. } => commit_id,
            error => panic!("unexpected commit error: {error}"),
        };
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("DROP TRIGGER reject_identity_update")
            .unwrap();
        drop(connection);
        drop(repository);

        let recovered = Repository::discover(root).await.unwrap();
        assert!(recovered.status().await.unwrap().is_empty());
        let commits = recovered.log(None).await.unwrap();
        assert_eq!(
            commits
                .iter()
                .filter(|commit| commit.message.as_deref() == Some("accepted once"))
                .count(),
            1
        );
        assert!(commits.iter().any(|commit| commit.commit_id == accepted));
    }

    async fn commit_captured_snapshot_then(repository: &Repository, mutate: impl FnOnce()) {
        let (committed, working, changes, working_ids) = repository.changes().await.unwrap();
        let objects = repository
            .capture_changed_files(&working, &changes)
            .unwrap();
        let built_intents =
            super::build_intents(&committed, &working, &changes, &working_ids, &objects, None)
                .unwrap();
        let mut intents = built_intents.intents;
        let current = repository
            .current_file_folder_context(&mut intents)
            .unwrap();
        let built = super::materialize_events(
            repository.config().repo_id(),
            Some("captured"),
            intents,
            &current,
        )
        .unwrap();
        repository
            .core()
            .append_file_folder_checked_with_refs(
                repository.config().repo_id().clone(),
                synck_vcs::types::CommitProposal {
                    author_id: repository.config().author().to_string(),
                    committer_id: repository.config().author().to_string(),
                    message: Some("captured".to_string()),
                    occurred_at: None,
                    operation_id: Some(uuid::Uuid::new_v4().to_string()),
                    events: built.events,
                },
                built.ref_events,
                &[],
                &built.subtree_guards,
            )
            .await
            .unwrap();
        repository.recover_current().await.unwrap();
        mutate();
        repository
            .publish_committed_identities(
                &committed,
                &working,
                &changes,
                &working_ids,
                &built_intents.created_ids,
            )
            .unwrap();
    }

    #[tokio::test]
    async fn post_capture_filesystem_changes_remain_dirty() {
        for scenario in ["edit", "replace", "rename", "delete", "file-to-folder"] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path();
            fs::write(root.join("document.bin"), b"before").unwrap();
            init(root, "author").await.unwrap();
            let repository = Repository::discover(root).await.unwrap();
            repository.commit("initial").await.unwrap();
            fs::write(root.join("document.bin"), b"captured").unwrap();

            commit_captured_snapshot_then(&repository, || match scenario {
                "edit" => fs::write(root.join("document.bin"), b"later").unwrap(),
                "replace" => {
                    fs::write(root.join("replacement.bin"), b"replacement").unwrap();
                    fs::remove_file(root.join("document.bin")).unwrap();
                    fs::rename(root.join("replacement.bin"), root.join("document.bin")).unwrap();
                }
                "rename" => {
                    fs::rename(root.join("document.bin"), root.join("renamed.bin")).unwrap()
                }
                "delete" => fs::remove_file(root.join("document.bin")).unwrap(),
                "file-to-folder" => {
                    fs::remove_file(root.join("document.bin")).unwrap();
                    fs::create_dir(root.join("document.bin")).unwrap();
                }
                _ => unreachable!(),
            })
            .await;

            let changes = repository.status().await.unwrap();
            if scenario == "file-to-folder" {
                assert_eq!(
                    changes
                        .iter()
                        .filter(|change| change.path == "document.bin")
                        .map(|change| change.kind)
                        .collect::<Vec<_>>(),
                    vec![crate::ChangeKind::Delete, crate::ChangeKind::Add],
                    "{scenario} was hidden by exact identity publication: {changes:?}"
                );
            } else {
                let expected = match scenario {
                    "edit" | "replace" => crate::ChangeKind::Modify,
                    "rename" => crate::ChangeKind::Rename,
                    "delete" => crate::ChangeKind::Delete,
                    _ => unreachable!(),
                };
                assert!(
                    changes.iter().any(|change| change.kind == expected),
                    "{scenario} was hidden by exact identity publication: {changes:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn detailed_log_uses_one_state_store_and_one_delta_per_commit() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document.bin"), b"zero").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        for revision in 1..=2 {
            fs::write(root.join("document.bin"), revision.to_string()).unwrap();
            repository
                .commit(&format!("revision {revision}"))
                .await
                .unwrap();
        }

        let before = repository.historical_work_counts();
        let catch_ups_before = repository.state_index_catch_ups();
        assert_eq!(repository.detailed_log(Some(3)).await.unwrap().len(), 3);
        let after = repository.historical_work_counts();
        assert_eq!(after[0] - before[0], 1);
        assert_eq!(after[1] - before[1], 3);
        assert_eq!(repository.state_index_catch_ups() - catch_ups_before, 1);
    }

    async fn history_work_with_unrelated_folders(unrelated: usize) -> [usize; 6] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document.bin"), b"zero").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        for revision in 1..=2 {
            fs::write(root.join("document.bin"), revision.to_string()).unwrap();
            repository
                .commit(&format!("revision {revision}"))
                .await
                .unwrap();
        }
        for start in (0..unrelated).step_by(3_000) {
            for index in start..std::cmp::min(start + 3_000, unrelated) {
                fs::create_dir(root.join(format!("unrelated-{index:05}"))).unwrap();
            }
            repository.commit("unrelated folders").await.unwrap();
        }

        let before = repository.historical_work_counts();
        assert_eq!(
            repository
                .history(root, std::path::Path::new("document.bin"))
                .await
                .unwrap()
                .len(),
            3
        );
        let after = repository.historical_work_counts();
        std::array::from_fn(|index| after[index] - before[index])
    }

    #[tokio::test]
    async fn item_history_work_ignores_ten_thousand_unrelated_items() {
        let empty = history_work_with_unrelated_folders(0).await;
        let large = history_work_with_unrelated_folders(10_000).await;
        assert_eq!(large, empty);
        assert_eq!(large[0], 1);
        assert_eq!(large[2], 1);
        assert_eq!(large[3], 4);
    }

    #[tokio::test]
    async fn item_history_rejects_target_event_corruption_after_projection() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document.bin"), b"content").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        assert_eq!(
            repository
                .history(root, std::path::Path::new("document.bin"))
                .await
                .unwrap()
                .len(),
            1
        );
        let item_id = repository
            .committed_tree()
            .await
            .unwrap()
            .get("document.bin")
            .unwrap()
            .item_id
            .clone();
        let connection =
            rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
        connection
            .execute(
                "UPDATE vcs_events SET payload = x'7b7d'
                 WHERE repo_id = ?1 AND item_id = ?2 AND event_type = 'FILE::SET_CONTENT'",
                rusqlite::params![repository.config().repo_id().as_str(), item_id.as_str()],
            )
            .unwrap();
        drop(connection);

        let error = repository
            .history(root, std::path::Path::new("document.bin"))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("historical event payload digest disagrees"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn commit_history_carries_tree_types_and_event_display_subjects() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::create_dir(root.join("Folder")).unwrap();
        fs::write(root.join("Folder/book.xlsx"), b"content").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        let CommitOutcome::Committed(commit_id) =
            repository.commit("initial content").await.unwrap()
        else {
            panic!("initial content commit was unexpectedly clean");
        };

        let shown = repository.show_events(&commit_id).await.unwrap();
        assert!(
            shown
                .commit
                .changes
                .iter()
                .any(|change| { change.path == "Folder" && change.item_type == "FOLDER" })
        );
        assert!(
            shown
                .commit
                .changes
                .iter()
                .any(|change| { change.path == "Folder/book.xlsx" && change.item_type == "FILE" })
        );

        let file_events = shown
            .events
            .iter()
            .filter(|event| event.item_name == "book.xlsx")
            .collect::<Vec<_>>();
        assert_eq!(file_events.len(), 3);
        assert!(file_events.iter().all(|event| event.item_type == "FILE"));
        assert!(
            file_events
                .iter()
                .all(|event| { event.parent_folder_name.as_deref() == Some("Folder") })
        );
        assert!(
            file_events
                .iter()
                .any(|event| event.event_type == "FILE_VERSION::CREATE")
        );

        let first_page = repository
            .show_event_page(&commit_id, None, 2)
            .await
            .unwrap();
        assert_eq!(first_page.events.len(), 2);
        assert_eq!(first_page.total_event_count, shown.events.len());
        let cursor = first_page.next_after_commit_event_idx.unwrap();
        let second_page = repository
            .show_event_page(&commit_id, Some(cursor), 50)
            .await
            .unwrap();
        assert_eq!(second_page.next_after_commit_event_idx, None);
        let paged_event_indexes = first_page
            .events
            .iter()
            .chain(&second_page.events)
            .map(|event| event.commit_event_idx)
            .collect::<Vec<_>>();
        assert_eq!(
            paged_event_indexes,
            shown
                .events
                .iter()
                .map(|event| event.commit_event_idx)
                .collect::<Vec<_>>()
        );
    }
}
