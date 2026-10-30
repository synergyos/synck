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
    ItemValue, NamespaceKey, item_namespace, RootHeader, StateIndexStore, StateProjectorStore, StateRootId, StateTree,
};
use synck_vcs::types::{
    ObjectId, COMMIT_EVENT_FAMILY_ITEM, COMMIT_EVENT_FAMILY_REF, CommitEventRow, CommitId,
    CommitProposal, CommitRow, EventProposal, EventRow, ItemId, LifecycleState,
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
    proposal_item_kind_requirements,
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

mod commit;
mod export;
mod history;
mod moves;
mod recover;
mod restore;
mod revert;
#[cfg(test)]
mod tests;

pub use commit::*;
pub use history::*;
use moves::*;
pub use recover::*;
pub(crate) use restore::*;
use revert::*;

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

pub(crate) fn verify_working_file(
    repository: &Repository,
    display: &str,
    blob_id: &ObjectId,
    expected_size: i64,
) -> Result<()> {
    let root = repository.root_path();
    let expected_digest = repository.plain_digest(blob_id)?;
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
    let actual = hex::encode(hasher.finalize());
    if size != expected_size || actual != expected_digest {
        return Err(LocalError::Invalid(format!(
            "recovery target content differs from committed bytes: {display}"
        )));
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

fn path_name(path: &str) -> &str {
    path.rsplit('/').next().expect("tracked path is nonempty")
}

fn blob_ref(blob_id: &ObjectId, size: u64) -> Result<BlobRef> {
    Ok(BlobRef {
        blob_id: blob_id.clone(),
        size,
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
    event.proposal()
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

fn content_from_event(event: &EventProposal) -> Result<(ObjectId, i64)> {
    let blob_id = match event.set_content.as_ref().and_then(|content| content.only("bytes")) {
        Some(blob_id) => ObjectId::parse(blob_id.as_str()).map_err(|error| {
            LocalError::Integrity(format!("invalid blob identifier in event: {error}"))
        })?,
        _ => {
            return Err(LocalError::Integrity(
                "file content event has no blob".to_string(),
            ));
        }
    };
    let size = event
        .set_metadata.as_ref().and_then(synck_vcs::metadata::MetadataRef::value)
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

pub(crate) fn display_to_path(display: &str) -> PathBuf {
    display.split('/').collect()
}

// A local crash-recovery record carries payload bytes alongside canonical
// envelopes. This encoding is never used for stored events or their hashes.
mod durable_events {
    use super::EventProposal;
    use serde::{Deserialize, Serialize};
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Record {
        event: EventProposal,
        metadata_hex: Option<String>,
    }
    pub fn serialize<S: serde::Serializer>(events: &[EventProposal], serializer: S) -> Result<S::Ok, S::Error> {
        events.iter().map(|event| {
            let metadata_hex = event.set_metadata.as_ref().map(|reference| reference.payload()
                .map(|metadata| hex::encode(metadata.payload()))
                .ok_or_else(|| serde::ser::Error::custom("recovery intent requires metadata bytes"))).transpose()?;
            Ok(Record { event:event.clone(), metadata_hex })
        }).collect::<Result<Vec<_>, S::Error>>()?.serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<EventProposal>, D::Error> {
        Vec::<Record>::deserialize(deserializer)?.into_iter().map(|mut record| {
            match (&mut record.event.set_metadata, record.metadata_hex) {
                (Some(reference), Some(bytes)) => {
                    let bytes = hex::decode(bytes).map_err(serde::de::Error::custom)?;
                    let metadata = synck_vcs::metadata::Metadata::verify(&reference.id(), bytes).map_err(serde::de::Error::custom)?;
                    reference.resolve(metadata).map_err(serde::de::Error::custom)?;
                }
                (None, None) => {}
                _ => return Err(serde::de::Error::custom("recovery metadata does not match event")),
            }
            Ok(record.event)
        }).collect()
    }
}

impl Repository {
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
}
