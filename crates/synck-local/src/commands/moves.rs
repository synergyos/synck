//! `synck mv`: move a tracked item and keep its identity.

use super::*;

pub(super) const MOVE_INTENT_VERSION: u32 = 2;
pub(super) const MOVE_INTENT_NAME: &str = "move.json";
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MoveIntent {
    pub(super) version: u32,
    pub(super) operation_id: String,
    pub(super) source: String,
    pub(super) destination: String,
    pub(super) item_id: ItemId,
    pub(super) item_type: String,
    pub(super) base_seq: i64,
    pub(super) result_head_seq: i64,
    pub(super) new_parent_id: Option<ItemId>,
    pub(super) new_name: String,
    pub(super) message: String,
}

pub(super) fn validate_move_intent(
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

pub(super) fn reject_destination_name_conflict(
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

pub(super) fn sync_move_parents(root: &Path, source: &str, destination: &str) -> Result<()> {
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

pub(super) fn move_pending_error(intent: &MoveIntent, error: LocalError) -> LocalError {
    LocalError::MoveNeedsRecovery {
        from_path: intent.source.clone(),
        to_path: intent.destination.clone(),
        detail: error.to_string(),
    }
}

impl Repository {
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
        self.guard_agent_paths(&[&source, &destination])?;
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

    pub(super) async fn append_move_intent(
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

    pub(super) async fn complete_move_intent(&self, intent: &MoveIntent) -> Result<String> {
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

    pub(super) async fn verify_completed_move(&self, intent: &MoveIntent) -> Result<()> {
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

    pub(super) fn write_move_intent(&self, intent: &MoveIntent) -> Result<()> {
        self.write_json_intent(MOVE_INTENT_NAME, "move", intent)
    }

    pub(super) fn read_move_intent(&self) -> Result<Option<MoveIntent>> {
        self.read_json_intent(MOVE_INTENT_NAME, "move")
    }

    pub(super) fn clear_move_intent(&self) -> Result<()> {
        self.clear_intent(MOVE_INTENT_NAME)
    }
}
