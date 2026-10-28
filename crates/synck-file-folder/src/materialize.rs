use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
use std::cell::Cell;

use serde_json::json;
use synck_vcs::naming::compute_name_key;
use synck_vcs::types::{
    CreateItem, EventContent, EventProposal, ItemId, RepoId,
    Sha256Digest, SubtreeClosureGuard, canonical_event_proposal_bytes,
};

use crate::error::{SemanticError, SemanticErrorCode};
use crate::model::{
    BlobRef, CurrentItem, FileFolderKind, ImmutableItemKind, Intent, IntentProposal, ItemBase,
    Location, ProfileLifecycleState,
};
use crate::operation::{compute_operation_id, normalize_intents};
use crate::project::validate_projected_proposal;
use crate::{MAX_EVENT_PAYLOAD_BYTES, MAX_MESSAGE_BYTES, WORKTREE_CONTROL_NAME_KEY};

#[derive(Clone, Debug)]
pub struct MaterializedProposal {
    pub operation_id: Sha256Digest,
    pub message: Option<String>,
    pub events: Vec<EventProposal>,
    pub subtree_guards: Vec<SubtreeClosureGuard>,
    pub blobs: Vec<BlobRef>,
}

#[derive(Clone)]
struct FinalItem {
    kind: FileFolderKind,
    parent_item_id: Option<ItemId>,
    name: String,
    name_key: String,
    lifecycle_state: ProfileLifecycleState,
}

#[cfg(test)]
#[derive(Clone, Copy, Default)]
struct TraversalVisits {
    current_edges: usize,
    lifecycle_nodes: usize,
    exposure_nodes: usize,
    path_nodes: usize,
}

#[cfg(test)]
thread_local! {
    static TRAVERSAL_VISITS: Cell<TraversalVisits> = Cell::new(TraversalVisits::default());
}

pub fn validate_and_materialize(
    repo_id: &RepoId,
    proposal: &IntentProposal,
    current: &BTreeMap<ItemId, CurrentItem>,
) -> Result<MaterializedProposal, SemanticError> {
    validate_and_materialize_with_content_revisions(repo_id, proposal, current, &BTreeSet::new())
}

/// Internal complete-content producers may require a revision even when primary
/// bytes are unchanged. Public profile intents and ordinary no-op rules stay intact.
pub fn validate_and_materialize_with_content_revisions(
    repo_id: &RepoId,
    proposal: &IntentProposal,
    current: &BTreeMap<ItemId, CurrentItem>,
    content_revisions: &BTreeSet<ItemId>,
) -> Result<MaterializedProposal, SemanticError> {
    for owner in content_revisions {
        if !proposal.intents.iter().any(|intent| {
            matches!(intent,
            Intent::UpdateFile {item_id, content: Some(_), ..} if item_id == owner)
        }) {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "forced content revision requires UpdateFile with content",
            ));
        }
    }
    if proposal
        .message
        .as_ref()
        .is_some_and(|message| message.len() > MAX_MESSAGE_BYTES)
    {
        return Err(error(
            SemanticErrorCode::MessageTooLarge,
            "message exceeds the UTF-8 byte limit",
        ));
    }
    let current_children = validate_current_map(current)?;
    let intents = normalize_intents(&proposal.intents)?;
    validate_intent_ownership(&intents)?;

    let mut final_items = current
        .iter()
        .filter_map(|(item_id, item)| {
            FileFolderKind::from_item_type(&item.item_type).map(|kind| {
                (
                    item_id.clone(),
                    FinalItem {
                        kind,
                        parent_item_id: item.parent_item_id.clone(),
                        name: item.name.clone(),
                        name_key: item.name_key.clone(),
                        lifecycle_state: item.lifecycle_state,
                    },
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    let create_kinds = created_kinds(&intents);
    let mut events = Vec::new();
    let mut guards = Vec::new();
    let mut blobs_by_id = BTreeMap::new();
    let mut lifecycle_items = BTreeSet::new();
    let mut exposed_items = BTreeSet::new();
    let mut relocated_folders = BTreeSet::new();

    for intent in &intents {
        match intent {
            Intent::CreateFolder { item_id, location } => {
                validate_create(item_id, location, current, &create_kinds)?;
                insert_created_final(&mut final_items, item_id, FileFolderKind::Folder, location)?;
                exposed_items.insert(item_id.clone());
                events.push(create_event(item_id, FileFolderKind::Folder, location));
            }
            Intent::CreateFile {
                item_id,
                location,
                content,
            } => {
                validate_create(item_id, location, current, &create_kinds)?;
                record_blob(&mut blobs_by_id, content)?;
                insert_created_final(&mut final_items, item_id, FileFolderKind::File, location)?;
                exposed_items.insert(item_id.clone());
                events.push(create_event(item_id, FileFolderKind::File, location));
                events.push(content_event(item_id, None, content));
            }
            Intent::UpdateFolder {
                item_id,
                base_seq,
                location,
            } => {
                let item =
                    require_live_current(current, item_id, FileFolderKind::Folder, *base_seq)?;
                validate_location(location)?;
                validate_existing_location_parent(current, &create_kinds, location)?;
                let changed = append_location_events(
                    &mut events,
                    item_id,
                    FileFolderKind::Folder,
                    *base_seq,
                    item,
                    location,
                );
                if !changed {
                    return Err(error(SemanticErrorCode::NoOp, "update_folder is a no-op"));
                }
                update_final_location(&mut final_items, item_id, location)?;
                relocated_folders.insert(item_id.clone());
            }
            Intent::UpdateFile {
                item_id,
                base_seq,
                location,
                content,
            } => {
                let item = require_live_current(current, item_id, FileFolderKind::File, *base_seq)?;
                let mut changed = false;
                if let Some(location) = location {
                    validate_location(location)?;
                    validate_existing_location_parent(current, &create_kinds, location)?;
                    changed |= append_location_events(
                        &mut events,
                        item_id,
                        FileFolderKind::File,
                        *base_seq,
                        item,
                        location,
                    );
                    update_final_location(&mut final_items, item_id, location)?;
                    exposed_items.insert(item_id.clone());
                }
                if let Some(content) = content {
                    record_blob(&mut blobs_by_id, content)?;
                    if item.content.as_ref() != Some(content) || content_revisions.contains(item_id)
                    {
                        events.push(content_event(item_id, Some(*base_seq), content));
                        changed = true;
                    }
                }
                if !changed {
                    return Err(error(SemanticErrorCode::NoOp, "update_file is a no-op"));
                }
            }
            Intent::Trash { root, descendants } => {
                let root_item = require_lifecycle_root(current, root, ProfileLifecycleState::Live)?;
                let kind =
                    FileFolderKind::from_item_type(&root_item.item_type).ok_or_else(|| {
                        error(
                            SemanticErrorCode::LifecycleMismatch,
                            "lifecycle root is not FILE or FOLDER",
                        )
                    })?;
                let closure = lifecycle_closure(
                    current,
                    &current_children,
                    &root.item_id,
                    kind,
                    &mut lifecycle_items,
                )?;
                let expected: Vec<ItemBase> = closure
                    .iter()
                    .filter(|(item, _)| item.lifecycle_state == ProfileLifecycleState::Live)
                    .map(|(item, _)| ItemBase {
                        item_id: item.item_id.clone(),
                        base_seq: item.head_seq,
                    })
                    .collect();
                require_exact_descendants(current, &root.item_id, descendants, &expected)?;
                if kind == FileFolderKind::Folder {
                    guards.push(make_guard(root, &closure));
                }
                let mut affected = closure
                    .iter()
                    .filter(|(item, _)| item.lifecycle_state == ProfileLifecycleState::Live)
                    .map(|(item, depth)| (item.item_id.clone(), item.head_seq, *depth))
                    .collect::<Vec<_>>();
                affected
                    .sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));
                for (item_id, base_seq, _) in affected {
                    let item = final_items
                        .get_mut(&item_id)
                        .expect("validated profile item");
                    events.push(lifecycle_event(&item_id, item.kind, base_seq, true));
                    item.lifecycle_state = ProfileLifecycleState::Trash;
                }
                events.push(lifecycle_event(&root.item_id, kind, root.base_seq, true));
                final_items
                    .get_mut(&root.item_id)
                    .expect("validated profile root")
                    .lifecycle_state = ProfileLifecycleState::Trash;
            }
            Intent::Restore {
                root,
                trash_commit_id,
                descendants,
                location,
            } => {
                let root_item =
                    require_lifecycle_root(current, root, ProfileLifecycleState::Trash)?;
                if root_item.trash_commit_id.as_ref() != Some(trash_commit_id) {
                    return Err(error(
                        SemanticErrorCode::LifecycleMismatch,
                        "restore root trash provenance does not match",
                    ));
                }
                let kind =
                    FileFolderKind::from_item_type(&root_item.item_type).ok_or_else(|| {
                        error(
                            SemanticErrorCode::LifecycleMismatch,
                            "lifecycle root is not FILE or FOLDER",
                        )
                    })?;
                let closure = lifecycle_closure(
                    current,
                    &current_children,
                    &root.item_id,
                    kind,
                    &mut lifecycle_items,
                )?;
                let expected: Vec<ItemBase> = closure
                    .iter()
                    .filter(|(item, _)| {
                        item.lifecycle_state == ProfileLifecycleState::Trash
                            && item.trash_commit_id.as_ref() == Some(trash_commit_id)
                    })
                    .map(|(item, _)| ItemBase {
                        item_id: item.item_id.clone(),
                        base_seq: item.head_seq,
                    })
                    .collect();
                require_exact_descendants(current, &root.item_id, descendants, &expected)?;
                if kind == FileFolderKind::Folder {
                    guards.push(make_guard(root, &closure));
                }
                if let Some(location) = location {
                    validate_location(location)?;
                    validate_existing_location_parent(current, &create_kinds, location)?;
                    append_location_events(
                        &mut events,
                        &root.item_id,
                        kind,
                        root.base_seq,
                        root_item,
                        location,
                    );
                    update_final_location(&mut final_items, &root.item_id, location)?;
                }
                events.push(lifecycle_event(&root.item_id, kind, root.base_seq, false));
                final_items
                    .get_mut(&root.item_id)
                    .expect("validated profile root")
                    .lifecycle_state = ProfileLifecycleState::Live;
                exposed_items.insert(root.item_id.clone());
                let mut affected = closure
                    .iter()
                    .filter(|(item, _)| {
                        item.lifecycle_state == ProfileLifecycleState::Trash
                            && item.trash_commit_id.as_ref() == Some(trash_commit_id)
                    })
                    .map(|(item, depth)| (item.item_id.clone(), item.head_seq, *depth))
                    .collect::<Vec<_>>();
                affected
                    .sort_by(|left, right| left.2.cmp(&right.2).then_with(|| left.0.cmp(&right.0)));
                for (item_id, base_seq, _) in affected {
                    let item = final_items
                        .get_mut(&item_id)
                        .expect("validated profile item");
                    events.push(lifecycle_event(&item_id, item.kind, base_seq, false));
                    item.lifecycle_state = ProfileLifecycleState::Live;
                    exposed_items.insert(item_id);
                }
            }
        }
    }

    expose_relocated_subtrees(&final_items, &relocated_folders, &mut exposed_items);
    validate_final_graph(&final_items, &exposed_items)?;
    validate_event_limits(&events)?;
    let created_ids: BTreeSet<ItemId> = events
        .iter()
        .filter(|event| event.create_item.is_some())
        .filter_map(|event| event.item_id.clone())
        .collect();
    let mut target_ids = BTreeSet::new();
    for event in &events {
        if let Some(item_id) = &event.item_id
            && !created_ids.contains(item_id)
        {
            target_ids.insert(item_id.clone());
        }
    }
    let immutable_kinds = target_ids
        .into_iter()
        .map(|item_id| {
            let item = current
                .get(&item_id)
                .expect("validated existing event target");
            ImmutableItemKind {
                item_id,
                item_type: item.item_type.clone(),
            }
        })
        .collect::<Vec<_>>();
    validate_projected_proposal(proposal.message.as_deref(), &events, &immutable_kinds)?;
    let operation_id = compute_operation_id(repo_id, proposal.message.as_deref(), &intents)?;
    Ok(MaterializedProposal {
        operation_id,
        message: proposal.message.clone(),
        events,
        subtree_guards: guards,
        blobs: blobs_by_id.into_values().collect(),
    })
}

fn validate_current_map(
    current: &BTreeMap<ItemId, CurrentItem>,
) -> Result<BTreeMap<ItemId, Vec<ItemId>>, SemanticError> {
    let mut children = BTreeMap::<ItemId, Vec<ItemId>>::new();
    for (key, item) in current {
        if key != &item.item_id {
            return Err(error(
                SemanticErrorCode::InvalidIdentifier,
                "current-state key does not match item_id",
            ));
        }
        if item.head_seq <= 0 {
            return Err(error(
                SemanticErrorCode::BaseSeqMismatch,
                "current item head must be positive",
            ));
        }
        if FileFolderKind::from_item_type(&item.item_type).is_some()
            && compute_name_key(&item.name).map_err(invalid_name)? != item.name_key
        {
            return Err(error(
                SemanticErrorCode::InvalidName,
                "current name_key does not match name",
            ));
        }
        if let Some(parent_id) = &item.parent_item_id {
            #[cfg(test)]
            TRAVERSAL_VISITS.with(|cell| {
                let mut visits = cell.get();
                visits.current_edges += 1;
                cell.set(visits);
            });
            children
                .entry(parent_id.clone())
                .or_default()
                .push(item.item_id.clone());
        }
    }
    Ok(children)
}

fn validate_intent_ownership(intents: &[Intent]) -> Result<(), SemanticError> {
    let mut owned = BTreeSet::new();
    for intent in intents {
        if !owned.insert(intent.root_id().clone()) {
            return Err(error(
                SemanticErrorCode::ConflictingIntent,
                "an item occurs in more than one intent",
            ));
        }
        match intent {
            Intent::Trash { descendants, .. } | Intent::Restore { descendants, .. } => {
                for descendant in descendants {
                    if !owned.insert(descendant.item_id.clone()) {
                        return Err(error(
                            SemanticErrorCode::ConflictingIntent,
                            "lifecycle intents overlap",
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn created_kinds(intents: &[Intent]) -> BTreeMap<ItemId, FileFolderKind> {
    intents
        .iter()
        .filter_map(|intent| match intent {
            Intent::CreateFolder { item_id, .. } => Some((item_id.clone(), FileFolderKind::Folder)),
            Intent::CreateFile { item_id, .. } => Some((item_id.clone(), FileFolderKind::File)),
            _ => None,
        })
        .collect()
}

fn validate_create(
    item_id: &ItemId,
    location: &Location,
    current: &BTreeMap<ItemId, CurrentItem>,
    create_kinds: &BTreeMap<ItemId, FileFolderKind>,
) -> Result<(), SemanticError> {
    if current.contains_key(item_id) {
        return Err(error(
            SemanticErrorCode::ItemAlreadyExists,
            "create target already exists",
        ));
    }
    validate_location(location)?;
    if let Some(parent_id) = &location.parent_item_id {
        if let Some(kind) = create_kinds.get(parent_id) {
            if *kind != FileFolderKind::Folder {
                return Err(error(
                    SemanticErrorCode::ParentNotFolder,
                    "created parent is not a folder",
                ));
            }
        } else {
            require_live_parent(current, parent_id)?;
        }
    }
    Ok(())
}

fn validate_existing_location_parent(
    current: &BTreeMap<ItemId, CurrentItem>,
    create_kinds: &BTreeMap<ItemId, FileFolderKind>,
    location: &Location,
) -> Result<(), SemanticError> {
    if let Some(parent_id) = &location.parent_item_id {
        match create_kinds.get(parent_id) {
            Some(FileFolderKind::Folder) => {}
            Some(FileFolderKind::File) => {
                return Err(error(
                    SemanticErrorCode::ParentNotFolder,
                    "created parent is not a folder",
                ));
            }
            None => require_live_parent(current, parent_id)?,
        }
    }
    Ok(())
}

fn require_live_parent(
    current: &BTreeMap<ItemId, CurrentItem>,
    parent_id: &ItemId,
) -> Result<(), SemanticError> {
    let parent = current.get(parent_id).ok_or_else(|| {
        error(
            SemanticErrorCode::ParentNotFound,
            "referenced parent is missing",
        )
    })?;
    if parent.item_type != "FOLDER" {
        return Err(error(
            SemanticErrorCode::ParentNotFolder,
            "referenced parent is not a folder",
        ));
    }
    if parent.lifecycle_state != ProfileLifecycleState::Live {
        return Err(error(
            SemanticErrorCode::ParentNotLive,
            "referenced parent is not live",
        ));
    }
    Ok(())
}

fn require_live_current<'a>(
    current: &'a BTreeMap<ItemId, CurrentItem>,
    item_id: &ItemId,
    expected_kind: FileFolderKind,
    base_seq: i64,
) -> Result<&'a CurrentItem, SemanticError> {
    let item = current
        .get(item_id)
        .ok_or_else(|| error(SemanticErrorCode::ItemNotFound, "update target is missing"))?;
    if item.item_type != expected_kind.as_str() {
        return Err(error(
            SemanticErrorCode::ConflictingIntent,
            "intent kind does not match target kind",
        ));
    }
    if item.lifecycle_state == ProfileLifecycleState::Tombstoned {
        return Err(error(
            SemanticErrorCode::ItemTombstoned,
            "update target is tombstoned",
        ));
    }
    if item.lifecycle_state != ProfileLifecycleState::Live {
        return Err(error(
            SemanticErrorCode::LifecycleMismatch,
            "ordinary update target is not live",
        ));
    }
    require_base(item, base_seq)?;
    Ok(item)
}

fn require_lifecycle_root<'a>(
    current: &'a BTreeMap<ItemId, CurrentItem>,
    root: &ItemBase,
    expected_state: ProfileLifecycleState,
) -> Result<&'a CurrentItem, SemanticError> {
    let item = current
        .get(&root.item_id)
        .ok_or_else(|| error(SemanticErrorCode::ItemNotFound, "lifecycle root is missing"))?;
    if item.lifecycle_state == ProfileLifecycleState::Tombstoned {
        return Err(error(
            SemanticErrorCode::ItemTombstoned,
            "lifecycle root is tombstoned",
        ));
    }
    if item.lifecycle_state != expected_state {
        return Err(error(
            SemanticErrorCode::LifecycleMismatch,
            "lifecycle root state does not match intent",
        ));
    }
    require_base(item, root.base_seq)?;
    Ok(item)
}

fn require_base(item: &CurrentItem, base_seq: i64) -> Result<(), SemanticError> {
    if base_seq <= 0 || item.head_seq != base_seq {
        return Err(error(
            SemanticErrorCode::BaseSeqMismatch,
            "intent base_seq does not match current head",
        ));
    }
    Ok(())
}

fn validate_location(location: &Location) -> Result<(), SemanticError> {
    compute_name_key(&location.name).map_err(invalid_name)?;
    Ok(())
}

fn insert_created_final(
    final_items: &mut BTreeMap<ItemId, FinalItem>,
    item_id: &ItemId,
    kind: FileFolderKind,
    location: &Location,
) -> Result<(), SemanticError> {
    let name_key = compute_name_key(&location.name).map_err(invalid_name)?;
    final_items.insert(
        item_id.clone(),
        FinalItem {
            kind,
            parent_item_id: location.parent_item_id.clone(),
            name: location.name.clone(),
            name_key,
            lifecycle_state: ProfileLifecycleState::Live,
        },
    );
    Ok(())
}

fn update_final_location(
    final_items: &mut BTreeMap<ItemId, FinalItem>,
    item_id: &ItemId,
    location: &Location,
) -> Result<(), SemanticError> {
    let item = final_items.get_mut(item_id).ok_or_else(|| {
        error(
            SemanticErrorCode::ItemNotFound,
            "location target is missing",
        )
    })?;
    item.parent_item_id = location.parent_item_id.clone();
    item.name = location.name.clone();
    item.name_key = compute_name_key(&location.name).map_err(invalid_name)?;
    Ok(())
}

fn append_location_events(
    events: &mut Vec<EventProposal>,
    item_id: &ItemId,
    kind: FileFolderKind,
    base_seq: i64,
    current: &CurrentItem,
    location: &Location,
) -> bool {
    let mut changed = false;
    if current.parent_item_id != location.parent_item_id {
        events.push(set_parent_event(
            item_id,
            kind,
            base_seq,
            location.parent_item_id.clone(),
        ));
        changed = true;
    }
    if current.name != location.name {
        events.push(set_name_event(
            item_id,
            kind,
            base_seq,
            location.name.clone(),
        ));
        changed = true;
    }
    changed
}

fn lifecycle_closure<'a>(
    current: &'a BTreeMap<ItemId, CurrentItem>,
    children: &BTreeMap<ItemId, Vec<ItemId>>,
    root_id: &ItemId,
    root_kind: FileFolderKind,
    lifecycle_items: &mut BTreeSet<ItemId>,
) -> Result<Vec<(&'a CurrentItem, usize)>, SemanticError> {
    if !lifecycle_items.insert(root_id.clone()) {
        return Err(error(
            SemanticErrorCode::ConflictingIntent,
            "lifecycle guards overlap",
        ));
    }
    if root_kind == FileFolderKind::File {
        return Ok(Vec::new());
    }
    let mut closure = Vec::new();
    let mut pending = children
        .get(root_id)
        .into_iter()
        .flatten()
        .rev()
        .map(|item_id| (item_id.clone(), 1usize))
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::from([root_id.clone()]);
    while let Some((item_id, depth)) = pending.pop() {
        #[cfg(test)]
        TRAVERSAL_VISITS.with(|cell| {
            let mut visits = cell.get();
            visits.lifecycle_nodes += 1;
            cell.set(visits);
        });
        if !seen.insert(item_id.clone()) {
            return Err(error(
                SemanticErrorCode::CycleDetected,
                "current subtree contains a cycle",
            ));
        }
        let item = &current[&item_id];
        if item.lifecycle_state != ProfileLifecycleState::Tombstoned {
            if !lifecycle_items.insert(item_id.clone()) {
                return Err(error(
                    SemanticErrorCode::ConflictingIntent,
                    "lifecycle guards overlap",
                ));
            }
            closure.push((item, depth));
        }
        if let Some(nested) = children.get(&item_id) {
            pending.extend(
                nested
                    .iter()
                    .rev()
                    .map(|child_id| (child_id.clone(), depth + 1)),
            );
        }
    }
    closure.sort_by(|left, right| left.0.item_id.cmp(&right.0.item_id));
    Ok(closure)
}

fn require_exact_descendants(
    current: &BTreeMap<ItemId, CurrentItem>,
    root_id: &ItemId,
    supplied: &[ItemBase],
    expected: &[ItemBase],
) -> Result<(), SemanticError> {
    if supplied != expected {
        diagnose_unreachable_descendants(current, root_id, supplied)?;
        return Err(error(
            SemanticErrorCode::LifecycleMismatch,
            "lifecycle descendants do not match current closure",
        ));
    }
    Ok(())
}

fn diagnose_unreachable_descendants(
    current: &BTreeMap<ItemId, CurrentItem>,
    root_id: &ItemId,
    supplied: &[ItemBase],
) -> Result<(), SemanticError> {
    for supplied in supplied {
        let Some(item) = current.get(&supplied.item_id) else {
            continue;
        };
        if item.lifecycle_state == ProfileLifecycleState::Tombstoned {
            continue;
        }
        let mut parent_id = item.parent_item_id.as_ref();
        let mut seen = BTreeSet::new();
        while let Some(parent) = parent_id {
            if parent == root_id {
                break;
            }
            if !seen.insert(parent.clone()) {
                return Err(error(
                    SemanticErrorCode::CycleDetected,
                    "current subtree contains a cycle",
                ));
            }
            let parent_item = current.get(parent).ok_or_else(|| {
                error(
                    SemanticErrorCode::ParentNotFound,
                    "current subtree has a missing ancestor",
                )
            })?;
            parent_id = parent_item.parent_item_id.as_ref();
        }
    }
    Ok(())
}

fn make_guard(root: &ItemBase, closure: &[(&CurrentItem, usize)]) -> SubtreeClosureGuard {
    let mut descendants = Vec::with_capacity(closure.len());
    for (item, _) in closure {
        descendants.push((item.item_id.clone(), item.head_seq));
    }
    SubtreeClosureGuard {
        root_item_id: root.item_id.clone(),
        expected_non_tombstoned_descendants: descendants,
        require_empty_live_descendants: false,
    }
}

fn expose_relocated_subtrees(
    final_items: &BTreeMap<ItemId, FinalItem>,
    root_ids: &BTreeSet<ItemId>,
    exposed: &mut BTreeSet<ItemId>,
) {
    let mut children = BTreeMap::<ItemId, Vec<ItemId>>::new();
    for (item_id, item) in final_items {
        if let Some(parent_id) = &item.parent_item_id {
            children
                .entry(parent_id.clone())
                .or_default()
                .push(item_id.clone());
        }
    }
    let mut pending = root_ids.iter().cloned().collect::<Vec<_>>();
    while let Some(item_id) = pending.pop() {
        if !exposed.insert(item_id.clone()) {
            continue;
        }
        #[cfg(test)]
        TRAVERSAL_VISITS.with(|cell| {
            let mut visits = cell.get();
            visits.exposure_nodes += 1;
            cell.set(visits);
        });
        if let Some(nested) = children.get(&item_id) {
            pending.extend(nested.iter().cloned());
        }
    }
}

fn validate_final_graph(
    items: &BTreeMap<ItemId, FinalItem>,
    exposed_items: &BTreeSet<ItemId>,
) -> Result<(), SemanticError> {
    let mut namespaces = BTreeSet::new();
    let mut path_states = BTreeMap::new();
    for (item_id, item) in items {
        if item.lifecycle_state != ProfileLifecycleState::Live {
            continue;
        }
        if !namespaces.insert((item.parent_item_id.clone(), item.name_key.clone())) {
            return Err(error(
                SemanticErrorCode::NameConflict,
                "two live items claim the same namespace key",
            ));
        }
        if exposed_items.contains(item_id) {
            validate_live_path(items, item_id, &mut path_states)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PathState {
    Visiting,
    Valid,
}

fn validate_live_path(
    items: &BTreeMap<ItemId, FinalItem>,
    start_id: &ItemId,
    states: &mut BTreeMap<ItemId, PathState>,
) -> Result<(), SemanticError> {
    let mut current = start_id.clone();
    let mut path = Vec::new();
    loop {
        match states.get(&current) {
            Some(PathState::Valid) => break,
            Some(PathState::Visiting) => {
                return Err(error(
                    SemanticErrorCode::CycleDetected,
                    "final parent graph contains a cycle",
                ));
            }
            None => {}
        }
        #[cfg(test)]
        TRAVERSAL_VISITS.with(|cell| {
            let mut visits = cell.get();
            visits.path_nodes += 1;
            cell.set(visits);
        });
        let item = items.get(&current).ok_or_else(|| {
            error(
                SemanticErrorCode::ParentNotFound,
                "final parent graph has a missing item",
            )
        })?;
        if item.lifecycle_state != ProfileLifecycleState::Live {
            return Err(error(
                SemanticErrorCode::ParentNotLive,
                "final live path contains a non-live ancestor",
            ));
        }
        if item.name_key == WORKTREE_CONTROL_NAME_KEY {
            return Err(error(
                SemanticErrorCode::ReservedWorktreePath,
                "operation exposes the reserved worktree component",
            ));
        }
        states.insert(current.clone(), PathState::Visiting);
        path.push(current.clone());
        match &item.parent_item_id {
            Some(parent_id) => {
                let parent = items.get(parent_id).ok_or_else(|| {
                    error(SemanticErrorCode::ParentNotFound, "final parent is missing")
                })?;
                if parent.kind != FileFolderKind::Folder {
                    return Err(error(
                        SemanticErrorCode::ParentNotFolder,
                        "final parent is not a folder",
                    ));
                }
                current = parent_id.clone();
            }
            None => break,
        }
    }
    for item_id in path {
        states.insert(item_id, PathState::Valid);
    }
    Ok(())
}

fn record_blob(
    blobs: &mut BTreeMap<String, BlobRef>,
    content: &BlobRef,
) -> Result<(), SemanticError> {
    if content.size > crate::MAX_SAFE_JSON_INTEGER {
        return Err(error(
            SemanticErrorCode::InvalidContent,
            "content size exceeds the exact JSON integer range",
        ));
    }
    let key = content.blob_id.as_str().to_string();
    if blobs
        .insert(key, content.clone())
        .is_some_and(|existing| existing.size != content.size)
    {
        return Err(error(
            SemanticErrorCode::InvalidContent,
            "one blob_id was supplied with different sizes",
        ));
    }
    Ok(())
}

fn validate_event_limits(events: &[EventProposal]) -> Result<(), SemanticError> {
    for event in events {
        let bytes = canonical_event_proposal_bytes(event).map_err(|failure| {
            error(
                SemanticErrorCode::MalformedProfileEvent,
                format!("event canonicalization failed: {failure}"),
            )
        })?;
        if bytes.len() > MAX_EVENT_PAYLOAD_BYTES {
            return Err(error(
                SemanticErrorCode::TooManyEvents,
                "one event exceeds the payload byte limit",
            ));
        }
    }
    Ok(())
}

fn create_event(item_id: &ItemId, kind: FileFolderKind, location: &Location) -> EventProposal {
    let mut proposal = base_event(item_id, kind, "CREATE", None);
    proposal.create_item = Some(CreateItem {
        item_type: kind.as_str().to_string(),
        parent_folder_id: location.parent_item_id.clone(),
        name: location.name.clone(),
    });
    proposal
}

fn set_parent_event(
    item_id: &ItemId,
    kind: FileFolderKind,
    base_seq: i64,
    parent_item_id: Option<ItemId>,
) -> EventProposal {
    let mut proposal = base_event(item_id, kind, "SET_PARENT", Some(base_seq));
    proposal.set_parent = Some(parent_item_id);
    proposal
}

fn set_name_event(
    item_id: &ItemId,
    kind: FileFolderKind,
    base_seq: i64,
    name: String,
) -> EventProposal {
    let mut proposal = base_event(item_id, kind, "SET_NAME", Some(base_seq));
    proposal.set_name = Some(name);
    proposal
}

fn content_event(item_id: &ItemId, base_seq: Option<i64>, content: &BlobRef) -> EventProposal {
    let mut proposal = base_event(item_id, FileFolderKind::File, "SET_CONTENT", base_seq);
    proposal.set_content = Some(EventContent::Blob {
        blob_id: content.blob_id.as_str().to_string(),
    });
    proposal.set_metadata = Some(json!({"content": {"size": content.size}}));
    proposal
}

fn lifecycle_event(
    item_id: &ItemId,
    kind: FileFolderKind,
    base_seq: i64,
    in_trash: bool,
) -> EventProposal {
    let mut proposal = base_event(
        item_id,
        kind,
        if in_trash { "TRASH" } else { "RESTORE" },
        Some(base_seq),
    );
    proposal.set_in_trash = Some(in_trash);
    proposal
}

fn base_event(
    item_id: &ItemId,
    kind: FileFolderKind,
    verb: &str,
    base_seq: Option<i64>,
) -> EventProposal {
    EventProposal {
        item_id: Some(item_id.clone()),
        event_type: format!("{}::{verb}", kind.as_str()),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq,
    }
}

fn invalid_name(failure: synck_vcs::naming::NameError) -> SemanticError {
    error(SemanticErrorCode::InvalidName, failure.to_string())
}

fn error(code: SemanticErrorCode, detail: impl Into<String>) -> SemanticError {
    SemanticError::new(code, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEEP_ITEMS: usize = 8_001;

    fn item_id(number: usize) -> ItemId {
        ItemId::parse(format!("0198e901-d208-7670-ae29-{number:012x}")).unwrap()
    }

    fn deep_folders() -> BTreeMap<ItemId, CurrentItem> {
        let mut items = BTreeMap::new();
        for index in 0..DEEP_ITEMS {
            let id = item_id(index);
            let name = format!("folder-{index}");
            items.insert(
                id.clone(),
                CurrentItem {
                    item_id: id,
                    item_type: "FOLDER".to_string(),
                    parent_item_id: (index > 0).then(|| item_id(index - 1)),
                    name_key: compute_name_key(&name).unwrap(),
                    name,
                    head_seq: 1,
                    lifecycle_state: ProfileLifecycleState::Live,
                    trash_commit_id: None,
                    content: None,
                },
            );
        }
        items
    }

    fn reset_visits() {
        TRAVERSAL_VISITS.set(TraversalVisits::default());
    }

    fn visits() -> TraversalVisits {
        TRAVERSAL_VISITS.get()
    }

    #[test]
    fn deep_lifecycle_visits_each_supplied_edge_once() {
        let current = deep_folders();
        let root_id = item_id(0);
        let descendants = (1..DEEP_ITEMS)
            .map(|index| ItemBase {
                item_id: item_id(index),
                base_seq: 1,
            })
            .collect();
        let proposal = IntentProposal {
            message: None,
            intents: vec![Intent::Trash {
                root: ItemBase {
                    item_id: root_id,
                    base_seq: 1,
                },
                descendants,
            }],
        };

        reset_visits();
        let output = validate_and_materialize(&RepoId::new(), &proposal, &current).unwrap();
        let visits = visits();
        assert_eq!(output.events.len(), DEEP_ITEMS);
        assert_eq!(visits.current_edges, DEEP_ITEMS - 1);
        assert_eq!(visits.lifecycle_nodes, DEEP_ITEMS - 1);
        assert_eq!(visits.exposure_nodes, 0);
        assert_eq!(visits.path_nodes, 0);
    }

    #[test]
    fn deep_relocation_memoizes_each_final_path_node() {
        let current = deep_folders();
        let root_id = item_id(0);
        let proposal = IntentProposal {
            message: None,
            intents: vec![Intent::UpdateFolder {
                item_id: root_id,
                base_seq: 1,
                location: Location {
                    parent_item_id: None,
                    name: "renamed-root".to_string(),
                },
            }],
        };

        reset_visits();
        let output = validate_and_materialize(&RepoId::new(), &proposal, &current).unwrap();
        let visits = visits();
        assert_eq!(output.events.len(), 1);
        assert_eq!(visits.current_edges, DEEP_ITEMS - 1);
        assert_eq!(visits.lifecycle_nodes, 0);
        assert_eq!(visits.exposure_nodes, DEEP_ITEMS);
        assert_eq!(visits.path_nodes, DEEP_ITEMS);
    }
    #[test]
    fn complete_content_forces_equal_primary_revision_only_for_valid_file_updates() {
        let owner = item_id(1);
        let content = BlobRef {
            blob_id: synck_vcs::types::BlobId::parse(format!("sha256:{}", "ab".repeat(32)))
                .unwrap(),
            size: 0,
        };
        let item = CurrentItem {
            item_id: owner.clone(),
            item_type: "FILE".into(),
            parent_item_id: None,
            name: "file".into(),
            name_key: "file".into(),
            head_seq: 7,
            lifecycle_state: ProfileLifecycleState::Live,
            trash_commit_id: None,
            content: Some(content.clone()),
        };
        let current = BTreeMap::from([(owner.clone(), item)]);
        let proposal = IntentProposal {
            message: None,
            intents: vec![Intent::UpdateFile {
                item_id: owner.clone(),
                base_seq: 7,
                location: None,
                content: Some(content),
            }],
        };
        let repo = RepoId::new();
        assert!(validate_and_materialize(&repo, &proposal, &current).is_err());
        let forced = BTreeSet::from([owner.clone()]);
        let result =
            validate_and_materialize_with_content_revisions(&repo, &proposal, &current, &forced)
                .unwrap();
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, "FILE::SET_CONTENT");
        assert_eq!(result.events[0].base_seq, Some(7));
        assert!(
            validate_and_materialize_with_content_revisions(
                &repo,
                &proposal,
                &current,
                &BTreeSet::from([item_id(2)])
            )
            .is_err()
        );
        let mut invalid = proposal.clone();
        if let Intent::UpdateFile { content, .. } = &mut invalid.intents[0] {
            *content = None;
        }
        assert!(
            validate_and_materialize_with_content_revisions(&repo, &invalid, &current, &forced)
                .is_err()
        );
    }
}
