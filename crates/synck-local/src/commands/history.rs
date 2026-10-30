//! `log`, `show`, `history`, `versions` and the historical trees they read.

use super::*;

pub(super) const LOG_DEFAULT_LIMIT: usize = 20;
pub(super) const LOG_MAX_LIMIT: usize = 1_000;
pub(super) const LOG_CHANGE_LIMIT: usize = 100;
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

#[derive(Clone, Debug)]
pub(super) struct HistoryNode {
    pub(super) parent_id: Option<ItemId>,
    pub(super) name: String,
    pub(super) kind: NodeKind,
    pub(super) live: bool,
    pub(super) head_seq: i64,
    pub(super) blob_id: Option<ObjectId>,
    pub(super) size: Option<i64>,
    pub(super) deletion: Option<DeletionInfo>,
}

#[derive(Debug)]
pub(super) struct ReplayAccumulator {
    pub(super) commit: CommitRow,
    pub(super) events: Vec<CommitEventRow>,
}

pub(super) struct ReplayResult {
    pub(super) nodes: HashMap<ItemId, HistoryNode>,
}

pub(super) fn begin_replay_commit(commit: CommitRow) -> Result<ReplayAccumulator> {
    commit.repo_seq.ok_or_else(|| {
        LocalError::Integrity(format!("commit {} is not sequenced", commit.commit_id))
    })?;
    Ok(ReplayAccumulator {
        commit,
        events: Vec::new(),
    })
}

pub(super) fn append_replay_event(replay: &mut ReplayAccumulator, event: CommitEventRow) -> Result<()> {
    if event.repo_seq() != replay.commit.repo_seq || event.commit_id() != &replay.commit.commit_id {
        return Err(LocalError::Integrity(format!(
            "repository sequence {} contains multiple commits",
            replay.commit.repo_seq.expect("replay commit is sequenced")
        )));
    }
    replay.events.push(event);
    Ok(())
}

pub(super) fn finish_replay_commit(
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

pub(super) fn history_commit(commit: CommitRow, changes: Vec<Change>) -> Result<HistoryCommit> {
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

pub(super) struct CommitEventPresentation {
    pub(super) item_id: ItemId,
    pub(super) item_type: String,
    pub(super) item_name: String,
    pub(super) item_name_before: Option<String>,
    pub(super) parent_folder_id: Option<ItemId>,
    pub(super) parent_folder_name: Option<String>,
    pub(super) parent_folder_id_before: Option<ItemId>,
    pub(super) parent_folder_name_before: Option<String>,
}

pub(super) async fn commit_event_presentation<S: StateIndexStore>(
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
        .and(before_item.as_ref().map(display_name));
    let parent_folder_id_before = before_item
        .as_ref()
        .and_then(|value| value.parent_folder_id.clone());
    let parent_folder_name_before = match (previous, parent_folder_id_before.as_ref()) {
        (Some(previous), parent_id) => historical_parent_name(store, previous, parent_id).await?,
        (None, _) => None,
    };
    let item_name = display_name(&item);
    Ok(CommitEventPresentation {
        item_id,
        item_type: item.item_type,
        item_name,
        item_name_before,
        parent_folder_id: item.parent_folder_id,
        parent_folder_name,
        parent_folder_id_before,
        parent_folder_name_before,
    })
}

pub(super) async fn ref_commit_event_presentation<S: StateIndexStore>(
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
    let item_name = display_name(&item);
    Ok(CommitEventPresentation {
        item_id: proposal.owner_item_id.clone(),
        item_type: item.item_type,
        item_name,
        item_name_before: before.as_ref().map(display_name),
        parent_folder_id: item.parent_folder_id,
        parent_folder_name,
        parent_folder_id_before,
        parent_folder_name_before,
    })
}

pub(super) fn commit_event_display_item_id(proposal: &EventProposal) -> Result<ItemId> {
    proposal.item_id.clone().ok_or_else(|| LocalError::Integrity("commit event has no item ID".into()))
}

pub(super) async fn historical_parent_name<S: StateIndexStore>(
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
    Ok(parent.name)
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

pub(super) fn history_path(item_id: &ItemId, nodes: &HashMap<ItemId, HistoryNode>) -> Result<Option<String>> {
    let Some(_) = nodes.get(item_id) else {
        return Ok(None);
    };
    let mut visiting = HashSet::new();
    history_path_inner(item_id, nodes, &mut visiting).map(Some)
}

pub(super) fn validate_historical_namespace_delta(
    changed_items: &BTreeSet<ItemId>,
    changed_namespace: &BTreeSet<NamespaceKey>,
    from: &BTreeMap<ItemId, ItemValue>,
    to: &BTreeMap<ItemId, ItemValue>,
) -> Result<()> {
    let mut expected = BTreeSet::new();
    for item_id in changed_items {
        let from_key = from.get(item_id).and_then(item_namespace);
        let to_key = to.get(item_id).and_then(item_namespace);
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

pub(super) fn initial_historical_changes(items: BTreeMap<ItemId, ItemValue>) -> Result<Vec<Change>> {
    let mut paths = HashMap::new();
    let mut visiting = HashSet::new();
    let mut changes = Vec::new();
    for (item_id, item) in &items {
        if item.lifecycle_state != LifecycleState::Live || item.name.is_none() {
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

pub(super) fn historical_state_path(
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
    let name = tree_name(item)?;
    let path = match item.parent_folder_id.as_ref() {
        None => name.to_string(),
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
                name
            )
        }
    };
    visiting.remove(item_id);
    paths.insert(item_id.clone(), path.clone());
    Ok(path)
}

pub(super) fn history_path_inner(
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

pub(super) fn apply_history_event(
    event: &EventRow,
    proposal: &EventProposal,
    profile: &LocalEvent,
    nodes: &mut HashMap<ItemId, HistoryNode>,
) -> Result<()> {
    let item_id = &event.item_id;
    match profile {
        LocalEvent::Create { item_type, content } => {
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
                        name: create.name.clone().ok_or_else(|| {
                            LocalError::Integrity(format!("tree item {item_id} has no name"))
                        })?,
                        kind,
                        live: true,
                        head_seq: event.item_seq,
                        blob_id: content.as_ref().map(|(blob_id, _)| blob_id.clone()),
                        size: content.as_ref().map(|(_, size)| *size),
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
    }
    Ok(())
}

pub(super) fn tree_name(item: &ItemValue) -> Result<&str> {
    item.name
        .as_deref()
        .ok_or_else(|| LocalError::Integrity(format!("tree item {} has no name", item.item_id)))
}

pub(super) fn tree_namespace(item: &ItemValue) -> Result<NamespaceKey> {
    item_namespace(item)
        .ok_or_else(|| LocalError::Integrity(format!("tree item {} has no name", item.item_id)))
}

/// A record has no name of its own: its title belongs to its family.
pub(super) fn display_name(item: &ItemValue) -> String {
    item.name
        .clone()
        .unwrap_or_else(|| item.item_id.to_string())
}

pub(super) fn history_kind(item_type: &str) -> NodeKind {
    match item_type {
        "FILE" => NodeKind::File,
        "FOLDER" => NodeKind::Folder,
        _ => unreachable!("local profile has only FILE and FOLDER"),
    }
}

pub(super) fn validate_history_parent(
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

pub(super) fn render_history_changes(
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

impl Repository {
    pub async fn log(&self, limit: Option<usize>) -> Result<Vec<LogEntry>> {
        self.recover_current().await?;
        self.log_recovered(limit).await
    }

    pub(super) async fn log_recovered(&self, limit: Option<usize>) -> Result<Vec<LogEntry>> {
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
        self.catch_up_state_index_in(&mut store).await?;
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

    pub(super) async fn complete_commit_events_in<S: StateProjectorStore>(
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

    pub(super) async fn present_commit_event_rows_in<S: StateProjectorStore>(
        &self,
        store: &mut S,
        commit_id: &CommitId,
        repo_seq: i64,
        expected_start_index: usize,
        rows: Vec<CommitEventRow>,
        calculate_commit_digest: bool,
    ) -> Result<(Vec<CommitEvent>, Option<String>)> {
        self.catch_up_state_index_in(store).await?;
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
                    let proposal: EventProposal = event.proposal().map_err(
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
                        commit_hasher.update([COMMIT_EVENT_FAMILY_ITEM]);
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
                    let proposal: RefEventProposal = event.proposal()
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

    pub(super) async fn commit_at_repo_seq(&self, repo_seq: i64) -> Result<Option<CommitRow>> {
        Ok(self
            .core()
            .list_commits_by_repo_seq(self.config().repo_id().clone(), repo_seq, Some(repo_seq), 1)
            .await?
            .into_iter()
            .next()
            .filter(|commit| commit.repo_seq == Some(repo_seq)))
    }

    pub(super) fn previous_repo_seq(&self, repo_seq: i64) -> Result<Option<i64>> {
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

    pub(super) async fn historical_tree_at(
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

    pub(super) async fn historical_subtree_at(
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
                || item.name_key.as_ref() != Some(&key.name_key)
            {
                return Err(LocalError::Integrity(format!(
                    "historical namespace disagrees for item {item_id}"
                )));
            }
            if item.name.as_deref() != Some(component) {
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
                    let name = tree_name(&item)?;
                    let path = if folder_path.is_empty() {
                        name.to_string()
                    } else {
                        format!("{folder_path}/{name}")
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

    pub(super) async fn historical_items_at_commit(
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
    pub(super) fn narrow_checkout_delta(
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

    pub(super) fn retain_checkout_filesystem_view(&self, items: &mut BTreeMap<ItemId, ItemValue>) {
        // Records have no path.
        items.retain(|_, item| item.name.is_some());
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

    pub(crate) async fn historical_root_at_commit<S: StateProjectorStore>(
        &self,
        store: &mut S,
        repo_seq: i64,
        commit_id: &CommitId,
    ) -> Result<(StateRootId, RootHeader)> {
        self.catch_up_state_index_in(store).await?;
        self.historical_root_after_projection(store, repo_seq, Some(commit_id))
            .await
    }

    pub(super) async fn historical_root_after_projection<S: StateIndexStore>(
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

    pub(super) async fn historical_changes_between_roots<S: StateIndexStore>(
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

    pub(super) async fn historical_selected_items<S: StateIndexStore>(
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
            // A record has no path.
            if selected_item.lifecycle_state != LifecycleState::Live
                || selected_item.name.is_none()
            {
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
                {
                    return Err(LocalError::Integrity(format!(
                        "historical live item {} has an invalid type or lifecycle",
                        item.item_id
                    )));
                }
                let namespace = tree_namespace(&item)?;
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

    pub(super) async fn historical_items_before(&self, repo_seq: i64) -> Result<BTreeMap<ItemId, ItemValue>> {
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

    pub(super) async fn historical_nodes_at_commit(
        &self,
        repo_seq: i64,
        commit_id: &CommitId,
    ) -> Result<HashMap<ItemId, HistoryNode>> {
        let items = self.historical_items_at_commit(repo_seq, commit_id).await?;
        self.historical_nodes_from_state(items)
    }

    pub(super) async fn historical_nodes_before(&self, repo_seq: i64) -> Result<HashMap<ItemId, HistoryNode>> {
        let items = self.historical_items_before(repo_seq).await?;
        self.historical_nodes_from_state(items)
    }

    pub(super) fn historical_committed_tree_from_state(
        &self,
        items: BTreeMap<ItemId, ItemValue>,
    ) -> Result<BTreeMap<String, CommittedNode>> {
        let mut paths = HashMap::new();
        let mut visiting = HashSet::new();
        for (item_id, item) in &items {
            if item.lifecycle_state == LifecycleState::Live {
                historical_state_path(item_id, &items, &mut paths, &mut visiting)?;
            }
        }

        self.historical_committed_tree_from_paths(items, paths)
    }

    pub(super) fn historical_committed_tree_from_paths(
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
                        plain_digest: blob_id.as_ref().map(|id| self.plain_digest(id)).transpose()?,
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

    pub(super) fn historical_nodes_from_state(
        &self,
        mut items: BTreeMap<ItemId, ItemValue>,
    ) -> Result<HashMap<ItemId, HistoryNode>> {
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
                let name = tree_name(&item)?.to_string();
                Ok((
                    item_id,
                    HistoryNode {
                        parent_id: item.parent_folder_id,
                        name,
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
    ) -> Result<HashMap<ItemId, (ObjectId, i64)>> {
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
                        e.event_payload_digest,
                        (SELECT m.payload FROM vcs_metadata_objects m WHERE m.repo_id=e.repo_id
                         AND m.object_id=unhex(substr(json_extract(CAST(e.payload AS TEXT), '$.metadata_id'),8)))
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
                    row.get::<_, Option<Vec<u8>>>(5)?,
                ))
            })?;
            for row in rows {
                let (item_id, item_seq, event_type, payload, stored_digest, metadata) = row?;
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
                let mut proposal: EventProposal =
                    serde_json::from_slice(&payload).map_err(|error| {
                        LocalError::Integrity(format!(
                            "invalid historical content event for {item_id}: {error}"
                        ))
                    })?;
                if let Some(bytes) = metadata {
                    let reference = proposal.set_metadata.as_mut().ok_or_else(|| LocalError::Integrity("unexpected metadata payload".into()))?;
                    let resolved = synck_vcs::metadata::Metadata::verify(&reference.id(), bytes).map_err(|e| LocalError::Integrity(e.to_string()))?;
                    reference.resolve(resolved).map_err(|e| LocalError::Integrity(e.to_string()))?;
                }
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
        if event_groups.is_empty() {
            return Err(LocalError::Integrity(format!(
                "tracked item {item_id} has no events"
            )));
        }
        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        #[cfg(test)]
        record_historical_work(self.config().repo_id(), HistoricalWorkKind::StateStore, 1);
        self.catch_up_state_index_in(&mut store).await?;
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

    pub(super) async fn item_event_groups(
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

    pub(super) async fn historical_item_and_live_path<S: StateIndexStore>(
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
            let namespace = tree_namespace(&current)?;
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
            components.push(tree_name(&current)?.to_string());
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
    pub(super) fn historical_work_counts(&self) -> [usize; 6] {
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

    pub(super) async fn replay_history(&self, through_repo_seq: Option<i64>) -> Result<ReplayResult> {
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
                if synck_file_folder::is_file_content_event(&event.event_type) {
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
}
