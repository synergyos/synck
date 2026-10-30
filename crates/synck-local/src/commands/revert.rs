//! `synck revert`: append a commit that reverses an accepted one.

use super::*;

pub(super) const REVERT_INTENT_VERSION: u32 = 4;
pub(super) const REVERT_INTENT_NAME: &str = "revert.json";
pub(super) const REVERT_STAGE_NAME: &str = "revert-stage";
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RevertIntent {
    pub(super) version: u32,
    pub(super) operation_id: String,
    pub(super) base_commit_id: CommitId,
    pub(super) base_repo_seq: i64,
    pub(super) publish_base_commit_id: CommitId,
    pub(super) publish_base_repo_seq: i64,
    pub(super) message: String,
    pub(super) items: Vec<RevertIntentItem>,
    #[serde(with = "durable_events")]
    pub(super) events: Vec<EventProposal>,
    pub(super) subtree_guards: Vec<DurableSubtreeGuard>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DurableSubtreeGuard {
    pub(super) root_item_id: ItemId,
    pub(super) expected_non_tombstoned_descendants: Vec<(ItemId, i64)>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RevertIntentItem {
    pub(super) item_id: ItemId,
    pub(super) item_type: String,
    pub(super) before: Option<RevertItemState>,
    pub(super) after: RevertItemState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RevertItemState {
    pub(super) plain_digest: Option<String>,
    pub(super) parent_id: Option<ItemId>,
    pub(super) name: String,
    pub(super) path: String,
    pub(super) live: bool,
    pub(super) head_seq: i64,
    pub(super) blob_id: Option<ObjectId>,
    pub(super) size: Option<i64>,
}

pub(super) fn build_revert_intent(
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

pub(super) fn materialize_revert(
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

pub(super) fn revert_blob_ref(item: &RevertIntentItem, before: &RevertItemState) -> Result<BlobRef> {
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

pub(super) fn prepare_revert_items(
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

pub(super) fn revert_overlap(item_id: &ItemId) -> LocalError {
    LocalError::Invalid(format!(
        "cannot revert because later history overlaps item {item_id}"
    ))
}

pub(super) fn is_history_descendant(
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

pub(super) fn derive_revert_items(
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

pub(super) fn revert_item_state(
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
        plain_digest: None,
        parent_id: node.parent_id.clone(),
        name: node.name.clone(),
        path,
        live: node.live,
        head_seq: node.head_seq,
        blob_id: node.blob_id.clone(),
        size: node.size,
    })
}

pub(super) fn same_revert_value(left: &RevertItemState, right: &RevertItemState) -> bool {
    left.parent_id == right.parent_id
        && left.name == right.name
        && left.live == right.live
        && match (&left.plain_digest, &right.plain_digest) {
            (Some(left), Some(right)) => left == right,
            _ => left.blob_id == right.blob_id,
        }
        && left.size == right.size
}

pub(super) fn validate_revert_intent(
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

pub(super) fn same_event_proposals(left: &[EventProposal], right: &[EventProposal]) -> Result<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    left.iter()
        .zip(right)
        .map(|(left, right)| {
            if left.set_metadata.as_ref().and_then(synck_vcs::metadata::MetadataRef::value)
                != right.set_metadata.as_ref().and_then(synck_vcs::metadata::MetadataRef::value) { return Ok(false); }
            let mut right = right.clone();
            right.set_metadata = left.set_metadata.clone();
            Ok(canonical_event_proposal_bytes(left)? == canonical_event_proposal_bytes(&right)?)
        })
        .try_fold(true, |equal, pair| pair.map(|pair| equal && pair))
}

pub(super) fn validate_revert_fixed_fields(intent: &RevertIntent) -> Result<()> {
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

pub(super) fn validate_reverted_nodes(
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

pub(super) fn revert_event_count(item: &RevertIntentItem) -> Result<i64> {
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

pub(super) fn expected_revert_commit_id(intent: &RevertIntent, author: &str) -> Result<CommitId> {
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
        intent.events.len(),
        &events_digest,
    )?;
    Ok(compute_commit_id(&commit))
}

pub(super) fn validate_inverse_stage_entries(stage: &Path, items: &[RevertIntentItem]) -> Result<()> {
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

pub(super) fn entry_matches_revert_state(
    repository: &Repository,
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
            let digest = match &state.plain_digest {
                Some(digest) => digest.clone(),
                None => repository.plain_digest(state.blob_id.as_ref().ok_or_else(|| LocalError::Integrity("revert FILE has neither content nor plaintext digest".into()))?)?,
            };
            let size = u64::try_from(state.size.ok_or_else(|| {
                LocalError::Integrity("revert FILE state has no size".to_string())
            })?)
            .map_err(|_| LocalError::Integrity("revert FILE has negative size".to_string()))?;
            match verify_object_file(path, &digest, size) {
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

impl Repository {
    pub async fn revert(&self, revision: &str, message: &str) -> Result<String> {
        self.guard_agent_all()?;
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

    pub(super) async fn complete_revert_intent(
        &self,
        intent: &RevertIntent,
        objects_validated: bool,
    ) -> Result<String> {
        validate_revert_fixed_fields(intent)?;
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

    pub(super) async fn append_revert_intent(
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

    pub(super) fn validate_inverse_objects(&self, items: &[RevertIntentItem]) -> Result<()> {
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
    pub(super) fn validate_revert_objects(&self, intent: &RevertIntent) -> Result<()> {
        self.validate_inverse_objects(&intent.items)
    }

    pub(super) fn validate_inverse_destinations(&self, items: &[RevertIntentItem]) -> Result<()> {
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
                if parent.is_none() && existing_name == ".synck" {
                    continue;
                }
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

    pub(super) fn materialize_inverse_items(
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

    pub(super) fn ensure_inverse_stage(&self, stage_name: &str) -> Result<PathBuf> {
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
    pub(super) fn ensure_revert_stage(&self) -> Result<PathBuf> {
        self.ensure_inverse_stage(REVERT_STAGE_NAME)
    }

    pub(super) fn stage_revert_item(&self, stage: &Path, item: &RevertIntentItem) -> Result<()> {
        let staged = stage.join(item.item_id.as_str());
        if entry_matches_revert_state(self, &staged, &item.item_type, &item.after)? {
            return Ok(());
        }
        if path_exists_nofollow(&staged)? {
            return Err(LocalError::Integrity(format!(
                "revert staging entry contradicts item {}",
                item.item_id
            )));
        }

        let source = self.root_path().join(display_to_path(&item.after.path));
        if entry_matches_revert_state(self, &source, &item.item_type, &item.after)? {
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
                && entry_matches_revert_state(self, &source, &item.item_type, before)?
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
            if entry_matches_revert_state(self, &target, &item.item_type, before)? {
                return Ok(());
            }
            return Err(LocalError::Integrity(format!(
                "revert item {} is absent from source, staging, and target",
                item.item_id
            )));
        }
        Ok(())
    }

    pub(super) fn publish_revert_folder(&self, stage: &Path, item: &RevertIntentItem) -> Result<()> {
        let before = item.before.as_ref().expect("filtered revert folder");
        let target = self.root_path().join(display_to_path(&before.path));
        if entry_matches_revert_state(self, &target, "FOLDER", before)? {
            return Ok(());
        }
        if path_exists_nofollow(&target)? {
            return Err(LocalError::Integrity(format!(
                "revert folder target contradicts {}",
                before.path
            )));
        }
        let staged = stage.join(item.item_id.as_str());
        if entry_matches_revert_state(self, &staged, "FOLDER", &item.after)? {
            rename_noreplace(&staged, &target).at("publish reverted folder", &target)?;
            sync_directory(stage)?;
            sync_display_parent(self.root_path(), &before.path)
        } else {
            self.materialize_recovered_folder(&before.path)
        }
    }

    pub(super) fn publish_revert_file(&self, stage: &Path, item: &RevertIntentItem) -> Result<()> {
        let before = item.before.as_ref().expect("filtered revert file");
        let target = self.root_path().join(display_to_path(&before.path));
        if entry_matches_revert_state(self, &target, "FILE", before)? {
            return Ok(());
        }
        if path_exists_nofollow(&target)? {
            return Err(LocalError::Integrity(format!(
                "revert file target contradicts {}",
                before.path
            )));
        }
        let staged = stage.join(item.item_id.as_str());
        if entry_matches_revert_state(self, &staged, "FILE", before)? {
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

    pub(super) fn remove_inverse_stage(&self, stage_name: &str) -> Result<()> {
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

    pub(super) fn write_revert_intent(&self, intent: &RevertIntent) -> Result<()> {
        self.write_json_intent(REVERT_INTENT_NAME, "revert", intent)
    }

    pub(super) fn read_revert_intent(&self) -> Result<Option<RevertIntent>> {
        self.read_json_intent(REVERT_INTENT_NAME, "revert")
    }

    pub(super) fn clear_revert_intent(&self) -> Result<()> {
        self.clear_intent(REVERT_INTENT_NAME)
    }
}
