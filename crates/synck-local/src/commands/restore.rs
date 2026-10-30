//! `synck restore` and `synck restore-tree`: replace working files with committed bytes.

use super::*;

pub(super) const WORKTREE_RESTORE_INTENT_VERSION: u32 = 2;
pub(super) const WORKTREE_RESTORE_INTENT_NAME: &str = "worktree-restore.json";
pub(super) const WORKTREE_RESTORE_STAGE_NAME: &str = "worktree-restore-stage";
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorktreeRestoreIntent {
    pub(super) version: u32,
    pub(super) base_commit_id: Option<CommitId>,
    pub(super) base_repo_seq: i64,
    pub(super) source_tree: Vec<WorktreeRestoreEntry>,
    pub(super) items: Vec<RevertIntentItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorktreeRestoreEntry {
    pub(super) path: String,
    pub(super) item_id: Option<ItemId>,
    pub(super) item_type: String,
    pub(super) plain_digest: Option<String>,
    pub(super) size: Option<i64>,
}

pub(super) fn build_worktree_restore_items(
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

pub(super) fn committed_restore_state(
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
        plain_digest: node.plain_digest.clone(),
        parent_id,
        name: restore_final_name(path).to_string(),
        path: path.to_string(),
        live: true,
        head_seq: node.head_seq,
        blob_id: node.blob_id.clone(),
        size: node.size,
    })
}

pub(super) fn working_restore_state(
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
    let (plain_digest, size) = match node.kind {
        NodeKind::File => {
            let digest = node.digest_hex.as_ref().ok_or_else(|| {
                LocalError::Integrity(format!("working restore FILE has no digest: {path}"))
            })?;
            let size = i64::try_from(node.size.ok_or_else(|| {
                LocalError::Integrity(format!("working restore FILE has no size: {path}"))
            })?)
            .map_err(|_| LocalError::Invalid(format!("working FILE is too large: {path}")))?;
            (Some(digest.clone()), Some(size))
        }
        NodeKind::Folder => (None, None),
    };
    Ok(RevertItemState {
        parent_id,
        name: restore_final_name(path).to_string(),
        path: path.to_string(),
        live: true,
        head_seq,
        blob_id: None,
        plain_digest,
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
            let (plain_digest, size) = match node.kind {
                NodeKind::File => {
                    let digest = node.digest_hex.as_ref().ok_or_else(|| {
                        LocalError::Integrity(format!("working FILE has no digest: {path}"))
                    })?;
                    (
                        Some(digest.clone()),
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
                plain_digest,
                size,
            })
        })
        .collect()
}

pub(super) fn reject_ignored_restore_descendants(
    root: &Dir,
    committed: &BTreeMap<String, CommittedNode>,
    items: &[RevertIntentItem],
    working: &BTreeMap<String, WorkingNode>,
) -> Result<()> {
    let ignore_changes = items.iter().any(|item| {
        item.after.path == ".synckignore"
            || item
                .before
                .as_ref()
                .is_some_and(|before| before.path == ".synckignore")
    });
    if ignore_changes {
        let strict = crate::worktree::scan_canonical_without_ignore(root, committed)?;
        if let Some(path) = strict.keys().find(|path| !working.contains_key(*path)) {
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

pub(super) fn restore_final_name(path: &str) -> &str {
    path.rsplit('/').next().expect("nonempty restore path")
}

pub(super) fn validate_worktree_restore_fixed_fields(intent: &WorktreeRestoreIntent) -> Result<()> {
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

pub(super) fn validate_worktree_restore_manifest(entries: &[WorktreeRestoreEntry]) -> Result<()> {
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
                NodeKind::File => entry.plain_digest.is_none() || entry.size.is_none(),
                NodeKind::Folder => entry.plain_digest.is_some() || entry.size.is_some(),
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

pub(super) fn validate_worktree_restore_target(
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
                || source.plain_digest != item.after.plain_digest
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

impl Repository {
    pub async fn restore(
        &self,
        caller: &Path,
        input: &Path,
        requested_seq: Option<i64>,
    ) -> Result<()> {
        let (display, blob_id, size) = self
            .resolve_file_revision(caller, input, requested_seq)
            .await?;
        self.guard_agent_paths(&[&display])?;
        self.write_working_file(&display, &blob_id, size, true)?;
        self.refresh_worktree_identities().await
    }

    pub async fn restore_worktree(&self) -> Result<bool> {
        self.guard_agent_all()?;
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

    pub(super) async fn restore_worktree_checked(
        &self,
        expected: Option<&[WorktreeRestoreEntry]>,
    ) -> Result<bool> {
        self.recover_pending_worktree_restore().await?;
        let (committed, working, changes, working_ids) = self.changes().await?;
        if changes.is_empty() {
            return Ok(false);
        }
        if let Some(expected) = expected {
            let all = self.complete_file_tree().await?;
            let mut observed =
                crate::worktree::scan_canonical_without_ignore(self.root(), &all)?;
            self.filter_checkout_observation(&mut observed);
            if worktree_restore_manifest(&observed, &HashMap::new())? != expected {
                return Err(LocalError::Invalid("working tree changed during checkout materialization; preserve the files and retry recovery after resolving the change".into()));
            }
        }
        let head = self.log(Some(1)).await?.into_iter().next();
        let items = build_worktree_restore_items(&committed, &working, &working_ids)?;
        reject_ignored_restore_descendants(self.root(), &committed, &items, &working)?;
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

    pub(super) async fn complete_worktree_restore(
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
}
