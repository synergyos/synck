//! `synck deleted` and `synck recover`: list and bring back trashed items.

use super::*;

pub(super) const RECOVER_INTENT_VERSION: u32 = 1;
pub(super) const RECOVER_INTENT_NAME: &str = "recover.json";
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecoverIntent {
    pub(super) version: u32,
    pub(super) operation_id: String,
    pub(super) root_item_id: ItemId,
    pub(super) deletion_commit_id: CommitId,
    pub(super) message: String,
    pub(super) items: Vec<RecoverIntentItem>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecoverIntentItem {
    pub(super) item_id: ItemId,
    pub(super) item_type: String,
    pub(super) parent_id: Option<ItemId>,
    pub(super) name: String,
    pub(super) path: String,
    pub(super) base_seq: i64,
    pub(super) blob_id: Option<ObjectId>,
    pub(super) size: Option<i64>,
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
pub(super) struct DeletionInfo {
    pub(super) repo_seq: i64,
    pub(super) commit_id: CommitId,
    pub(super) deleted_at: DateTime<Utc>,
}

pub(super) fn build_recover_intent(
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

pub(super) fn validate_recover_intent(
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

pub(super) fn validate_recover_names(nodes: &HashMap<ItemId, HistoryNode>, selected: &[ItemId]) -> Result<()> {
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

pub(super) fn is_descendant_of(
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

pub(super) fn validate_recover_destinations(
    repository: &Repository,
    intent: &RecoverIntent,
    materialized: bool,
) -> Result<()> {
    let root = repository.root_path();
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
                    repository,
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

impl Repository {
    pub(super) fn write_recover_intent(&self, intent: &RecoverIntent) -> Result<()> {
        self.write_json_intent(RECOVER_INTENT_NAME, "recover", intent)
    }

    pub(super) fn read_recover_intent(&self) -> Result<Option<RecoverIntent>> {
        self.read_json_intent(RECOVER_INTENT_NAME, "recover")
    }

    pub(super) fn clear_recover_intent(&self) -> Result<()> {
        self.clear_intent(RECOVER_INTENT_NAME)
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
        self.guard_agent_all()?;
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
        if intent
            .items
            .iter()
            .any(|item| !self.checkout_observes(&item.path))
        {
            return Err(LocalError::Invalid(
                "recovery crosses the checkout selection; widen it first".into(),
            ));
        }
        validate_recover_destinations(self, &intent, false)?;
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

    pub(super) async fn complete_recover_intent(
        &self,
        intent: &RecoverIntent,
        objects_validated: bool,
    ) -> Result<String> {
        self.recover().await?;
        let replay = self.replay_history(None).await?;
        let accepted = validate_recover_intent(self.root_path(), intent, &replay.nodes)?;
        let commit_id = if accepted {
            self.operation_commit_id(&intent.operation_id)?
                .ok_or_else(|| {
                    LocalError::Integrity("accepted recover intent has no commit".to_string())
                })?
        } else {
            validate_recover_destinations(self, intent, false)?;
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
            validate_recover_destinations(self, intent, true)?;
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

    pub(super) fn validate_recover_objects(&self, intent: &RecoverIntent) -> Result<()> {
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

    pub(super) async fn append_recover_intent(
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

    pub(super) fn materialize_recover_intent(&self, intent: &RecoverIntent) -> Result<()> {
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

    pub(super) fn materialize_recovered_folder(&self, display: &str) -> Result<()> {
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

    pub(super) fn materialize_recovered_file(
        &self,
        display: &str,
        blob_id: &ObjectId,
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
                verify_working_file(self, display, blob_id, expected_size)
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
}
