use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Seek, Write};
use std::path::{Component, Path, PathBuf};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use synck_vcs::naming::compute_name_key;
use uuid::Uuid;

use crate::error::{IoContext, LocalError, Result, WorkingTreeEditError};
use crate::repository::{Repository, sync_open_directory as sync_open_directory_handle};
use crate::worktree::{
    FileIdentity, NodeKind, WorkingNode, directory_identity, scan_canonical_without_ignore,
    scan_entry_without_ignore,
};

pub(crate) const WORKING_TREE_EDIT_INTENT_NAME: &str = "working-tree-edit.json";
pub(crate) const WORKING_TREE_EDIT_STAGE_NAME: &str = "working-tree-edit-stage";
const WORKING_TREE_EDIT_INTENT_VERSION: u32 = 2;
const WORKING_TREE_EDIT_STAGE_ENTRY: &str = "entry";

#[cfg(test)]
thread_local! {
    static DIRECTORY_SYNC_TRACE: std::cell::RefCell<Option<Vec<PathBuf>>> = const {
        std::cell::RefCell::new(None)
    };
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum WorkingTreeEditOperation {
    Delete,
    Move,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum WorkingTreeEditPhase {
    Prepared,
    Quarantined,
    Published,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkingTreeEditIntent {
    version: u32,
    operation: WorkingTreeEditOperation,
    phase: WorkingTreeEditPhase,
    source: String,
    destination: Option<String>,
    expected: WorkingTreeEditAuthority,
    source_parent_identity: Option<FileIdentity>,
    destination_parent_identity: Option<FileIdentity>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkingTreeEditAuthority {
    kind: WorkingTreeEntryKind,
    identity: FileIdentity,
    size: Option<u64>,
    digest_hex: Option<String>,
    subtree_digest: [u8; 32],
}

impl WorkingTreeEditAuthority {
    fn from_entry(entry: &WorkingTreeEntry) -> Self {
        Self {
            kind: entry.kind,
            identity: entry.identity,
            size: entry.size,
            digest_hex: entry.digest_hex.clone(),
            subtree_digest: entry.subtree_digest,
        }
    }

    fn matches(&self, entry: &WorkingTreeEntry) -> bool {
        self.kind == entry.kind
            && self.identity == entry.identity
            && self.size == entry.size
            && self.digest_hex == entry.digest_hex
            && self.subtree_digest == entry.subtree_digest
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum WorkingTreeEntryKind {
    File,
    Folder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingTreeEntry {
    pub path: String,
    pub kind: WorkingTreeEntryKind,
    pub item_id: Option<String>,
    pub item_seq: Option<i64>,
    pub content_item_seq: Option<i64>,
    pub size: Option<u64>,
    pub modified_at: Option<DateTime<Utc>>,
    identity: FileIdentity,
    digest_hex: Option<String>,
    subtree_digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingTreeSnapshot {
    pub changes: Vec<crate::Change>,
    pub entries: Vec<WorkingTreeEntry>,
}

impl Repository {
    /// Returns the live canonical working tree. Paths are display data; callers must retain and
    /// return the opaque entry value for writes instead of constructing write paths.
    pub async fn working_tree(&self) -> Result<Vec<WorkingTreeEntry>> {
        Ok(self.inspect_working_tree().await?.entries)
    }

    pub async fn inspect_working_tree(&self) -> Result<WorkingTreeSnapshot> {
        // The Files view and write fences cover the complete live tree. Ignore rules affect
        // commits, not what a user may inspect or recursively delete from the desktop app.
        let (committed, working, changes, working_ids) = self.inspect_changes().await?;
        let tracked = self
            .tracked_items()
            .await?
            .into_iter()
            .map(|item| (item.item_id.clone(), item))
            .collect::<BTreeMap<_, _>>();
        let committed_by_id = committed
            .values()
            .map(|node| (node.item_id.to_string(), node))
            .collect::<BTreeMap<_, _>>();

        let mut entries = working
            .into_iter()
            .map(|(path, node)| {
                let item_id = working_ids.get(&path).map(ToString::to_string);
                let committed_node = item_id
                    .as_ref()
                    .and_then(|item_id| committed_by_id.get(item_id).copied());
                let tracked_item = item_id.as_ref().and_then(|item_id| tracked.get(item_id));
                let modified_at =
                    filesystem_modified_at(&self.root_path().join(display_path(&path)))
                        .or_else(|| tracked_item.map(|item| item.modified_at));
                Ok(WorkingTreeEntry {
                    path,
                    kind: node_kind(node.kind),
                    item_id,
                    item_seq: committed_node.map(|node| node.head_seq),
                    content_item_seq: tracked_item.and_then(|item| item.content_item_seq),
                    size: node.size,
                    modified_at,
                    identity: node.identity,
                    digest_hex: node.digest_hex,
                    subtree_digest: [0; 32],
                })
            })
            .collect::<Result<Vec<_>>>()?;
        populate_subtree_digests(&mut entries);
        Ok(WorkingTreeSnapshot { changes, entries })
    }

    fn require_checkout_destination(&self, path: &str) -> Result<()> {
        if let Some(paths) = self.checkout_paths() {
            if !crate::checkout::selected(paths, path) {
                return Err(LocalError::Invalid(
                    "destination is outside checkout selection".into(),
                ));
            }
        }
        Ok(())
    }

    fn require_checkout_entry_mutation(&self, entry: &WorkingTreeEntry) -> Result<()> {
        if let Some(paths) = self.checkout_paths() {
            if entry.kind == WorkingTreeEntryKind::Folder
                && !paths.is_empty()
                && !paths.iter().any(|root| {
                    entry.path == *root
                        || entry
                            .path
                            .strip_prefix(root)
                            .is_some_and(|p| p.starts_with('/'))
                })
            {
                return Err(LocalError::Invalid(
                    "cannot move or delete a partial checkout ancestor".into(),
                ));
            }
        }
        Ok(())
    }

    pub async fn create_working_folder(
        &self,
        parent: Option<&WorkingTreeEntry>,
        name: &str,
    ) -> Result<WorkingTreeEntry> {
        let current = self.verified_working_tree(parent.into_iter()).await?;
        let destination = destination_path(&current, parent, name, None)?;
        self.require_checkout_destination(&destination)?;
        let path = self.root_path().join(display_path(&destination));
        let parent_dir = open_verified_parent(self, parent)?;
        parent_dir
            .create_dir(name)
            .at("create working folder", &path)?;
        sync_open_directory(
            &parent_dir,
            path.parent().expect("validated path has a parent"),
        )?;
        self.working_entry_at(&destination).await
    }

    pub async fn create_working_file(
        &self,
        parent: Option<&WorkingTreeEntry>,
        name: &str,
    ) -> Result<WorkingTreeEntry> {
        let current = self.verified_working_tree(parent.into_iter()).await?;
        let destination = destination_path(&current, parent, name, None)?;
        self.require_checkout_destination(&destination)?;
        let path = self.root_path().join(display_path(&destination));
        let parent_dir = open_verified_parent(self, parent)?;
        let mut options = CapOpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let file = parent_dir
            .open_with(name, &options)
            .at("create working file", &path)?;
        if let Err(error) = file.sync_all().at("sync working file", &path) {
            let _ = parent_dir.remove_file(name);
            return Err(error);
        }
        sync_open_directory(
            &parent_dir,
            path.parent().expect("validated path has a parent"),
        )?;
        self.working_entry_at(&destination).await
    }

    pub async fn rename_working_entry(
        &self,
        entry: &WorkingTreeEntry,
        new_name: &str,
    ) -> Result<WorkingTreeEntry> {
        let current = self.verified_working_tree([entry]).await?;
        let old_name = leaf_name(&entry.path);
        if old_name == new_name {
            return Err(WorkingTreeEditError::SameName.into());
        }
        let parent = parent_path(&entry.path);
        let destination = destination_path_for_parent(&current, parent, new_name, Some(entry))?;
        self.require_checkout_destination(&destination)?;
        self.require_checkout_entry_mutation(entry)?;
        let source_parent = working_parent(&current, &entry.path)?;
        self.move_working_path(entry, source_parent, &destination, source_parent)
            .await
    }

    pub async fn move_working_entry(
        &self,
        entry: &WorkingTreeEntry,
        destination_parent: Option<&WorkingTreeEntry>,
    ) -> Result<WorkingTreeEntry> {
        let current = self
            .verified_working_tree([Some(entry), destination_parent].into_iter().flatten())
            .await?;
        if entry.kind == WorkingTreeEntryKind::Folder
            && destination_parent.is_some_and(|parent| {
                parent.path == entry.path || is_descendant(&parent.path, &entry.path)
            })
        {
            return Err(WorkingTreeEditError::FolderBelowItself.into());
        }
        let destination = destination_path(
            &current,
            destination_parent,
            leaf_name(&entry.path),
            Some(entry),
        )?;
        self.require_checkout_destination(&destination)?;
        self.require_checkout_entry_mutation(entry)?;
        if destination == entry.path {
            return Err(WorkingTreeEditError::SameFolder.into());
        }
        let source_parent = working_parent(&current, &entry.path)?;
        self.move_working_path(entry, source_parent, &destination, destination_parent)
            .await
    }

    pub async fn delete_working_entry(&self, entry: &WorkingTreeEntry) -> Result<()> {
        self.require_checkout_entry_mutation(entry)?;
        let current = self.verified_working_tree([entry]).await?;
        let source_parent = working_parent(&current, &entry.path)?;
        let mut intent = self.quarantine_working_entry(
            entry,
            source_parent,
            WorkingTreeEditOperation::Delete,
            None,
            None,
        )?;
        let result = (|| -> Result<()> {
            self.verify_quarantined_entry(entry)?;
            let stage = self.open_working_tree_edit_stage()?;
            match entry.kind {
                WorkingTreeEntryKind::File => stage
                    .remove_file(WORKING_TREE_EDIT_STAGE_ENTRY)
                    .at("delete quarantined working file", &entry.path)?,
                WorkingTreeEntryKind::Folder => stage
                    .open_dir_nofollow(WORKING_TREE_EDIT_STAGE_ENTRY)
                    .at("open quarantined working folder", &entry.path)?
                    .remove_open_dir_all()
                    .at("delete quarantined working folder", &entry.path)?,
            }
            sync_open_directory(&stage, &self.working_tree_edit_stage_path())?;
            drop(stage);
            self.mark_working_tree_edit_published(&mut intent)?;
            self.finish_working_tree_edit()
        })();
        if let Err(error) = result {
            return Err(self.restore_after_failure(&intent, error));
        }
        Ok(())
    }

    pub async fn import_working_files(
        &self,
        parent: Option<&WorkingTreeEntry>,
        sources: &[PathBuf],
    ) -> Result<Vec<WorkingTreeEntry>> {
        if sources.is_empty() {
            return Err(WorkingTreeEditError::NoFilesSelected.into());
        }
        let current = self.verified_working_tree(parent.into_iter()).await?;
        let mut destinations = Vec::with_capacity(sources.len());
        let mut inputs = Vec::with_capacity(sources.len());
        let mut name_keys = BTreeSet::new();
        for source in sources {
            let mut input = open_import_source(source)?;
            let metadata = input.metadata().at("inspect import source", source)?;
            if !metadata.file_type().is_file() || crate::repository::is_link_or_reparse(&metadata) {
                return Err(WorkingTreeEditError::InvalidImportSource.into());
            }
            let name = source
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    LocalError::WorkingTree(WorkingTreeEditError::InvalidImportSource)
                })?;
            let name_key =
                compute_name_key(name).map_err(|_| WorkingTreeEditError::InvalidImportSource)?;
            if !name_keys.insert(name_key) {
                return Err(WorkingTreeEditError::DuplicateImportNames.into());
            }
            let destination = destination_path(&current, parent, name, None)?;
            self.require_checkout_destination(&destination)?;
            destinations.push(destination);
            input.rewind().at("rewind import source", source)?;
            inputs.push(input);
        }

        let mut published = Vec::new();
        let destination_parent_dir = open_verified_parent(self, parent)?;
        for ((source, mut input), destination) in sources.iter().zip(inputs).zip(&destinations) {
            let destination_path = self.root_path().join(display_path(destination));
            let parent_path = destination_path.parent().ok_or_else(|| {
                LocalError::Integrity("working destination has no parent".to_string())
            })?;
            let temporary_name = format!(".synck-tmp-{}", Uuid::new_v4());
            let temporary = parent_path.join(&temporary_name);
            let copy_result = (|| -> Result<()> {
                let mut options = CapOpenOptions::new();
                options
                    .write(true)
                    .create_new(true)
                    .follow(FollowSymlinks::No);
                let mut output = destination_parent_dir
                    .open_with(&temporary_name, &options)
                    .at("create import temporary", &temporary)?;
                io::copy(&mut input, &mut output).at("copy import source", source)?;
                output.sync_all().at("sync imported file", &temporary)?;
                rename_between_noreplace(
                    &destination_parent_dir,
                    &temporary_name,
                    &destination_parent_dir,
                    leaf_name(destination),
                    &temporary,
                    &destination_path,
                )
                .at("publish imported file", &destination_path)?;
                sync_open_directory(&destination_parent_dir, parent_path)?;
                Ok(())
            })();
            if let Err(error) = copy_result {
                let _ = destination_parent_dir.remove_file(&temporary_name);
                return Err(error);
            }
            published.push(destination.clone());
        }

        let refreshed = self.working_tree().await?;
        published
            .iter()
            .map(|path| {
                refreshed
                    .iter()
                    .find(|entry| entry.path == *path)
                    .cloned()
                    .ok_or_else(|| {
                        LocalError::Integrity(format!(
                            "published working file is missing from refresh: {path}"
                        ))
                    })
            })
            .collect()
    }

    async fn move_working_path(
        &self,
        entry: &WorkingTreeEntry,
        source_parent: Option<&WorkingTreeEntry>,
        destination: &str,
        destination_parent_entry: Option<&WorkingTreeEntry>,
    ) -> Result<WorkingTreeEntry> {
        let destination_path = self.root_path().join(display_path(destination));
        let mut intent = self.quarantine_working_entry(
            entry,
            source_parent,
            WorkingTreeEditOperation::Move,
            Some(destination),
            destination_parent_entry,
        )?;
        if let Err(error) = self.verify_quarantined_entry(entry) {
            return Err(self.restore_after_failure(&intent, error));
        }
        let stage = match self.open_working_tree_edit_stage() {
            Ok(stage) => stage,
            Err(error) => return Err(self.restore_after_failure(&intent, error)),
        };
        let destination_parent = match open_verified_parent(self, destination_parent_entry) {
            Ok(parent) => parent,
            Err(error) => return Err(self.restore_after_failure(&intent, error)),
        };
        if let Err(error) = rename_between_noreplace(
            &stage,
            WORKING_TREE_EDIT_STAGE_ENTRY,
            &destination_parent,
            leaf_name(destination),
            &self
                .working_tree_edit_stage_path()
                .join(WORKING_TREE_EDIT_STAGE_ENTRY),
            &destination_path,
        )
        .at("move working item", &destination_path)
        {
            drop(stage);
            return Err(self.restore_after_failure(&intent, error));
        }
        let durability = sync_rename_directories(
            &destination_parent,
            destination_path
                .parent()
                .expect("validated destination has a parent"),
            &stage,
            &self.working_tree_edit_stage_path(),
        );
        if let Err(error) = durability {
            return Err(LocalError::Integrity(format!(
                "working item moved but directory sync failed ({error}); repository recovery is pending"
            )));
        }
        drop(stage);
        if let Err(error) = self.mark_working_tree_edit_published(&mut intent) {
            return Err(LocalError::Integrity(format!(
                "working item moved but journal publication failed ({error}); repository recovery is pending"
            )));
        }
        if let Err(error) = self.finish_working_tree_edit() {
            return Err(LocalError::Integrity(format!(
                "working item moved but journal cleanup failed ({error}); repository recovery is pending"
            )));
        }
        self.working_entry_at(destination).await
    }

    fn quarantine_working_entry(
        &self,
        entry: &WorkingTreeEntry,
        source_parent: Option<&WorkingTreeEntry>,
        operation: WorkingTreeEditOperation,
        destination: Option<&str>,
        destination_parent: Option<&WorkingTreeEntry>,
    ) -> Result<WorkingTreeEditIntent> {
        let intent = WorkingTreeEditIntent {
            version: WORKING_TREE_EDIT_INTENT_VERSION,
            operation,
            phase: WorkingTreeEditPhase::Prepared,
            source: entry.path.clone(),
            destination: destination.map(str::to_owned),
            expected: WorkingTreeEditAuthority::from_entry(entry),
            source_parent_identity: source_parent.map(|parent| parent.identity),
            destination_parent_identity: destination_parent.map(|parent| parent.identity),
        };
        self.write_working_tree_edit_intent(&intent)?;
        let metadata_dir = self.open_metadata_dir()?;
        if let Err(error) = metadata_dir.create_dir(WORKING_TREE_EDIT_STAGE_NAME).at(
            "create working-tree edit staging",
            self.working_tree_edit_stage_path(),
        ) {
            return Err(match self.clear_working_tree_edit_intent() {
                Ok(()) => error,
                Err(cleanup_error) => LocalError::Integrity(format!(
                    "working-tree edit failed ({error}); intent cleanup failed ({cleanup_error}); repository recovery is pending"
                )),
            });
        }
        if let Err(error) = sync_open_directory(&metadata_dir, self.metadata_path()) {
            return Err(self.abort_empty_working_tree_edit(error));
        }

        let source_path = self.root_path().join(display_path(&entry.path));
        let source_parent = match open_verified_parent(self, source_parent) {
            Ok(parent) => parent,
            Err(error) => return Err(self.abort_empty_working_tree_edit(error)),
        };
        let stage = match self.open_working_tree_edit_stage() {
            Ok(stage) => stage,
            Err(error) => return Err(self.abort_empty_working_tree_edit(error)),
        };
        if let Err(error) = rename_between_noreplace(
            &source_parent,
            leaf_name(&entry.path),
            &stage,
            WORKING_TREE_EDIT_STAGE_ENTRY,
            &source_path,
            &self
                .working_tree_edit_stage_path()
                .join(WORKING_TREE_EDIT_STAGE_ENTRY),
        )
        .at("quarantine working item", &source_path)
        {
            drop(stage);
            return Err(self.abort_empty_working_tree_edit(error));
        }
        if let Err(error) = sync_rename_directories(
            &stage,
            &self.working_tree_edit_stage_path(),
            &source_parent,
            source_path.parent().expect("validated source has a parent"),
        ) {
            drop(stage);
            return Err(self.restore_after_failure(&intent, error));
        }
        let mut intent = intent;
        intent.phase = WorkingTreeEditPhase::Quarantined;
        if let Err(error) = self.replace_working_tree_edit_intent(&intent) {
            drop(stage);
            return Err(self.restore_after_failure(&intent, error));
        }
        Ok(intent)
    }

    fn verify_quarantined_entry(&self, expected: &WorkingTreeEntry) -> Result<()> {
        let stage = self.open_working_tree_edit_stage()?;
        let nodes = scan_canonical_without_ignore(&stage, &BTreeMap::new())?;
        let mut entries = nodes
            .into_iter()
            .map(|(path, node)| {
                let path = if path == WORKING_TREE_EDIT_STAGE_ENTRY {
                    expected.path.clone()
                } else if let Some(suffix) = path.strip_prefix("entry/") {
                    format!("{}/{suffix}", expected.path)
                } else {
                    return Err(LocalError::Integrity(format!(
                        "unexpected working-tree staging entry: {path}"
                    )));
                };
                Ok(working_entry_from_node(path, node, None, None, None, None))
            })
            .collect::<Result<Vec<_>>>()?;
        populate_subtree_digests(&mut entries);
        let actual = entries
            .iter()
            .find(|entry| entry.path == expected.path)
            .ok_or(WorkingTreeEditError::Stale)?;
        if actual.kind != expected.kind
            || actual.identity != expected.identity
            || actual.size != expected.size
            || actual.digest_hex != expected.digest_hex
            || actual.subtree_digest != expected.subtree_digest
        {
            return Err(WorkingTreeEditError::Stale.into());
        }
        Ok(())
    }

    fn restore_quarantined_entry(&self, intent: &WorkingTreeEditIntent) -> Result<()> {
        let stage = self.open_working_tree_edit_stage()?;
        let source_parent = open_intent_parent(
            self,
            parent_path(&intent.source),
            intent.source_parent_identity,
        )?;
        let source_path = self.root_path().join(display_path(&intent.source));
        rename_between_noreplace(
            &stage,
            WORKING_TREE_EDIT_STAGE_ENTRY,
            &source_parent,
            leaf_name(&intent.source),
            &self
                .working_tree_edit_stage_path()
                .join(WORKING_TREE_EDIT_STAGE_ENTRY),
            &source_path,
        )
        .at("restore quarantined working item", &source_path)?;
        sync_rename_directories(
            &source_parent,
            source_path.parent().expect("validated source has a parent"),
            &stage,
            &self.working_tree_edit_stage_path(),
        )?;
        drop(stage);
        self.finish_working_tree_edit()
    }

    fn working_tree_edit_stage_path(&self) -> PathBuf {
        self.metadata_path().join(WORKING_TREE_EDIT_STAGE_NAME)
    }

    fn open_metadata_dir(&self) -> Result<Dir> {
        self.root()
            .open_dir_nofollow(".synck")
            .at("open repository metadata", self.metadata_path())
    }

    fn open_working_tree_edit_stage(&self) -> Result<Dir> {
        self.open_metadata_dir()?
            .open_dir_nofollow(WORKING_TREE_EDIT_STAGE_NAME)
            .at(
                "open working-tree edit staging",
                self.working_tree_edit_stage_path(),
            )
    }

    fn write_working_tree_edit_intent(&self, intent: &WorkingTreeEditIntent) -> Result<()> {
        self.store_working_tree_edit_intent(intent, false)
    }

    fn replace_working_tree_edit_intent(&self, intent: &WorkingTreeEditIntent) -> Result<()> {
        self.store_working_tree_edit_intent(intent, true)
    }

    fn store_working_tree_edit_intent(
        &self,
        intent: &WorkingTreeEditIntent,
        replace_existing: bool,
    ) -> Result<()> {
        let metadata = self.open_metadata_dir()?;
        let intent_path = self.metadata_path().join(WORKING_TREE_EDIT_INTENT_NAME);
        match metadata.symlink_metadata(WORKING_TREE_EDIT_INTENT_NAME) {
            Ok(existing) if replace_existing => {
                if !existing.file_type().is_file()
                    || crate::repository::is_cap_link_or_reparse(&existing)
                {
                    return Err(LocalError::Integrity(format!(
                        "working-tree edit intent is not a regular file: {}",
                        intent_path.display()
                    )));
                }
            }
            Ok(_) => {
                return Err(LocalError::Integrity(
                    "repository already contains a working-tree edit intent".to_string(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && !replace_existing => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect working-tree edit intent",
                    path: intent_path,
                    source,
                });
            }
        }

        let temporary_dir = metadata.open_dir_nofollow("tmp").at(
            "open repository temporary directory",
            self.metadata_path().join("tmp"),
        )?;
        let temporary_name = format!("working-tree-edit-{}", Uuid::new_v4());
        let temporary_path = self.metadata_path().join("tmp").join(&temporary_name);
        let result = (|| -> Result<()> {
            let mut options = CapOpenOptions::new();
            options
                .write(true)
                .create_new(true)
                .follow(FollowSymlinks::No);
            let mut file = temporary_dir
                .open_with(&temporary_name, &options)
                .at("create working-tree edit intent", &temporary_path)?;
            serde_json::to_writer_pretty(&mut file, intent)?;
            file.write_all(b"\n")
                .at("write working-tree edit intent", &temporary_path)?;
            file.sync_all()
                .at("sync working-tree edit intent", &temporary_path)?;
            if replace_existing {
                temporary_dir
                    .rename(&temporary_name, &metadata, WORKING_TREE_EDIT_INTENT_NAME)
                    .at("replace working-tree edit intent", &intent_path)?;
            } else {
                rename_between_noreplace(
                    &temporary_dir,
                    &temporary_name,
                    &metadata,
                    WORKING_TREE_EDIT_INTENT_NAME,
                    &temporary_path,
                    &intent_path,
                )
                .at("publish working-tree edit intent", &intent_path)?;
            }
            sync_rename_directories(
                &metadata,
                self.metadata_path(),
                &temporary_dir,
                &self.metadata_path().join("tmp"),
            )
        })();
        if result.is_err() {
            let _ = temporary_dir.remove_file(&temporary_name);
        }
        result
    }

    fn read_working_tree_edit_intent(&self) -> Result<Option<WorkingTreeEditIntent>> {
        let metadata = self.open_metadata_dir()?;
        let intent_path = self.metadata_path().join(WORKING_TREE_EDIT_INTENT_NAME);
        let entry = match metadata.symlink_metadata(WORKING_TREE_EDIT_INTENT_NAME) {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect working-tree edit intent",
                    path: intent_path,
                    source,
                });
            }
        };
        if !entry.file_type().is_file() || crate::repository::is_cap_link_or_reparse(&entry) {
            return Err(LocalError::Integrity(format!(
                "working-tree edit intent is not a regular file: {}",
                intent_path.display()
            )));
        }
        let mut options = CapOpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = metadata
            .open_with(WORKING_TREE_EDIT_INTENT_NAME, &options)
            .at("open working-tree edit intent", &intent_path)?;
        let opened = file
            .metadata()
            .at("inspect working-tree edit intent", &intent_path)?;
        if !opened.file_type().is_file() || crate::repository::is_cap_link_or_reparse(&opened) {
            return Err(LocalError::Integrity(format!(
                "working-tree edit intent is not a regular file: {}",
                intent_path.display()
            )));
        }
        Ok(Some(serde_json::from_reader(file).map_err(|error| {
            LocalError::Integrity(format!("invalid working-tree edit intent: {error}"))
        })?))
    }

    fn finish_working_tree_edit(&self) -> Result<()> {
        let metadata = self.open_metadata_dir()?;
        metadata.remove_dir(WORKING_TREE_EDIT_STAGE_NAME).at(
            "remove working-tree edit staging",
            self.working_tree_edit_stage_path(),
        )?;
        sync_open_directory(&metadata, self.metadata_path())?;
        self.clear_working_tree_edit_intent()
    }

    fn clear_working_tree_edit_intent(&self) -> Result<()> {
        let metadata = self.open_metadata_dir()?;
        metadata.remove_file(WORKING_TREE_EDIT_INTENT_NAME).at(
            "remove operation intent",
            self.metadata_path().join(WORKING_TREE_EDIT_INTENT_NAME),
        )?;
        sync_open_directory(&metadata, self.metadata_path())
    }

    fn mark_working_tree_edit_published(&self, intent: &mut WorkingTreeEditIntent) -> Result<()> {
        intent.phase = WorkingTreeEditPhase::Published;
        self.replace_working_tree_edit_intent(intent)
    }

    fn abort_empty_working_tree_edit(&self, original: LocalError) -> LocalError {
        let cleanup = (|| -> Result<()> {
            let metadata = self.open_metadata_dir()?;
            metadata.remove_dir(WORKING_TREE_EDIT_STAGE_NAME).at(
                "remove empty working-tree edit staging",
                self.working_tree_edit_stage_path(),
            )?;
            sync_open_directory(&metadata, self.metadata_path())?;
            self.clear_working_tree_edit_intent()
        })();
        match cleanup {
            Ok(()) => original,
            Err(cleanup_error) => LocalError::Integrity(format!(
                "working-tree edit failed ({original}); cleanup failed ({cleanup_error}); repository recovery is pending"
            )),
        }
    }

    fn restore_after_failure(
        &self,
        intent: &WorkingTreeEditIntent,
        original: LocalError,
    ) -> LocalError {
        match self.restore_quarantined_entry(intent) {
            Ok(()) => original,
            Err(recovery_error) => LocalError::Integrity(format!(
                "working-tree edit failed ({original}); restoration failed ({recovery_error}); repository recovery is pending"
            )),
        }
    }

    pub(crate) fn recover_pending_working_tree_edit(&self) -> Result<()> {
        let Some(mut intent) = self.read_working_tree_edit_intent()? else {
            return Ok(());
        };
        validate_working_tree_edit_intent(&intent)?;

        let stage_path = self.working_tree_edit_stage_path();
        let metadata = self.open_metadata_dir()?;
        let stage_exists =
            entry_exists_nofollow(&metadata, WORKING_TREE_EDIT_STAGE_NAME, &stage_path)?;
        if !stage_exists {
            return match intent.phase {
                WorkingTreeEditPhase::Prepared => {
                    let (_, source) = self.inspect_intent_entry(
                        &intent.source,
                        intent.source_parent_identity,
                        &intent.source,
                    )?;
                    require_expected_entry(source.as_ref(), &intent.expected, "source")?;
                    self.clear_working_tree_edit_intent()
                }
                WorkingTreeEditPhase::Published => match intent.operation {
                    WorkingTreeEditOperation::Delete => self.clear_working_tree_edit_intent(),
                    WorkingTreeEditOperation::Move => {
                        let destination = intent.destination.as_deref().expect("validated move");
                        let (destination_parent, destination_entry) = self.inspect_intent_entry(
                            destination,
                            intent.destination_parent_identity,
                            &intent.source,
                        )?;
                        require_expected_entry(
                            destination_entry.as_ref(),
                            &intent.expected,
                            "destination",
                        )?;
                        sync_open_directory(
                            &destination_parent,
                            self.root_path()
                                .join(display_path(destination))
                                .parent()
                                .expect("validated destination has a parent"),
                        )?;
                        self.clear_working_tree_edit_intent()
                    }
                },
                WorkingTreeEditPhase::Quarantined => {
                    let (_, source) = self.inspect_intent_entry(
                        &intent.source,
                        intent.source_parent_identity,
                        &intent.source,
                    )?;
                    if source
                        .as_ref()
                        .is_some_and(|entry| intent.expected.matches(entry))
                    {
                        self.clear_working_tree_edit_intent()
                    } else {
                        Err(LocalError::Integrity(
                            "working-tree edit recovery lost quarantined staging".to_string(),
                        ))
                    }
                }
            };
        }

        let stage = self.open_working_tree_edit_stage()?;
        let staged = observed_entry(&stage, WORKING_TREE_EDIT_STAGE_ENTRY, &intent.source)?;

        if let Some(staged) = staged {
            require_expected_entry(Some(&staged), &intent.expected, "staging")?;
            if intent.phase == WorkingTreeEditPhase::Published {
                return Err(LocalError::Integrity(
                    "published working-tree edit still contains quarantined data".to_string(),
                ));
            }
            let (_, source) = self.inspect_intent_entry(
                &intent.source,
                intent.source_parent_identity,
                &intent.source,
            )?;
            require_empty_entry(source.as_ref(), "source")?;
            drop(stage);
            return self.restore_quarantined_entry(&intent);
        }

        match intent.phase {
            WorkingTreeEditPhase::Prepared => {
                let (_, source) = self.inspect_intent_entry(
                    &intent.source,
                    intent.source_parent_identity,
                    &intent.source,
                )?;
                require_expected_entry(source.as_ref(), &intent.expected, "source")?;
                drop(stage);
                self.finish_working_tree_edit()
            }
            WorkingTreeEditPhase::Quarantined => {
                let (source_parent, source) = self.inspect_intent_entry(
                    &intent.source,
                    intent.source_parent_identity,
                    &intent.source,
                )?;
                if source
                    .as_ref()
                    .is_some_and(|entry| intent.expected.matches(entry))
                {
                    sync_rename_directories(
                        &source_parent,
                        self.root_path()
                            .join(display_path(&intent.source))
                            .parent()
                            .expect("validated source has a parent"),
                        &stage,
                        &stage_path,
                    )?;
                    drop(stage);
                    return self.finish_working_tree_edit();
                }
                match intent.operation {
                    WorkingTreeEditOperation::Delete => {
                        sync_open_directory(&stage, &stage_path)?;
                    }
                    WorkingTreeEditOperation::Move => {
                        let destination = intent.destination.as_deref().expect("validated move");
                        let (destination_parent, destination_entry) = self.inspect_intent_entry(
                            destination,
                            intent.destination_parent_identity,
                            &intent.source,
                        )?;
                        require_expected_entry(
                            destination_entry.as_ref(),
                            &intent.expected,
                            "destination",
                        )?;
                        sync_rename_directories(
                            &destination_parent,
                            self.root_path()
                                .join(display_path(destination))
                                .parent()
                                .expect("validated destination has a parent"),
                            &stage,
                            &stage_path,
                        )?;
                    }
                }
                drop(stage);
                self.mark_working_tree_edit_published(&mut intent)?;
                self.finish_working_tree_edit()
            }
            WorkingTreeEditPhase::Published => {
                match intent.operation {
                    WorkingTreeEditOperation::Delete => {
                        sync_open_directory(&stage, &stage_path)?;
                    }
                    WorkingTreeEditOperation::Move => {
                        let destination = intent.destination.as_deref().expect("validated move");
                        let (destination_parent, destination_entry) = self.inspect_intent_entry(
                            destination,
                            intent.destination_parent_identity,
                            &intent.source,
                        )?;
                        require_expected_entry(
                            destination_entry.as_ref(),
                            &intent.expected,
                            "destination",
                        )?;
                        sync_rename_directories(
                            &destination_parent,
                            self.root_path()
                                .join(display_path(destination))
                                .parent()
                                .expect("validated destination has a parent"),
                            &stage,
                            &stage_path,
                        )?;
                    }
                }
                drop(stage);
                self.finish_working_tree_edit()
            }
        }
    }

    fn inspect_intent_entry(
        &self,
        path: &str,
        parent_identity: Option<FileIdentity>,
        authority_path: &str,
    ) -> Result<(Dir, Option<WorkingTreeEntry>)> {
        let parent = open_intent_parent(self, parent_path(path), parent_identity)?;
        let entry = observed_entry(
            &parent,
            leaf_name(path),
            authority_path,
        )?;
        Ok((parent, entry))
    }

    async fn verified_working_tree<'a>(
        &self,
        entries: impl IntoIterator<Item = &'a WorkingTreeEntry>,
    ) -> Result<Vec<WorkingTreeEntry>> {
        let current = self.working_tree().await?;
        for expected in entries {
            let matches = current.iter().any(|entry| {
                entry.path == expected.path
                    && entry.kind == expected.kind
                    && entry.identity == expected.identity
                    && entry.size == expected.size
                    && entry.digest_hex == expected.digest_hex
                    && entry.subtree_digest == expected.subtree_digest
            });
            if !matches {
                return Err(WorkingTreeEditError::Stale.into());
            }
        }
        Ok(current)
    }

    async fn working_entry_at(&self, path: &str) -> Result<WorkingTreeEntry> {
        self.working_tree()
            .await?
            .into_iter()
            .find(|entry| entry.path == path)
            .ok_or_else(|| {
                LocalError::Integrity(format!(
                    "working item is missing after filesystem mutation: {path}"
                ))
            })
    }
}

fn node_kind(kind: NodeKind) -> WorkingTreeEntryKind {
    match kind {
        NodeKind::File => WorkingTreeEntryKind::File,
        NodeKind::Folder => WorkingTreeEntryKind::Folder,
    }
}

fn working_entry_from_node(
    path: String,
    node: WorkingNode,
    item_id: Option<String>,
    item_seq: Option<i64>,
    content_item_seq: Option<i64>,
    modified_at: Option<DateTime<Utc>>,
) -> WorkingTreeEntry {
    WorkingTreeEntry {
        path,
        kind: node_kind(node.kind),
        item_id,
        item_seq,
        content_item_seq,
        size: node.size,
        modified_at,
        identity: node.identity,
        digest_hex: node.digest_hex,
        subtree_digest: [0; 32],
    }
}

fn populate_subtree_digests(entries: &mut [WorkingTreeEntry]) {
    let mut children = BTreeMap::<String, Vec<(String, [u8; 32])>>::new();
    for entry in entries.iter_mut().rev() {
        let mut hasher = Sha256::new();
        hasher.update(entry.path.len().to_le_bytes());
        hasher.update(entry.path.as_bytes());
        hasher.update([match entry.kind {
            WorkingTreeEntryKind::File => 0,
            WorkingTreeEntryKind::Folder => 1,
        }]);
        let (device_id, file_id, incarnation) = entry.identity.durable_parts();
        hasher.update(device_id.to_le_bytes());
        hasher.update(file_id.to_le_bytes());
        match incarnation {
            Some(value) => {
                hasher.update([1]);
                hasher.update(value.seconds().to_le_bytes());
                hasher.update(value.nanos().to_le_bytes());
            }
            None => hasher.update([0]),
        }
        match entry.size {
            Some(value) => {
                hasher.update([1]);
                hasher.update(value.to_le_bytes());
            }
            None => hasher.update([0]),
        }
        if let Some(digest) = &entry.digest_hex {
            hasher.update(digest.len().to_le_bytes());
            hasher.update(digest.as_bytes());
        } else {
            hasher.update(0_usize.to_le_bytes());
        }
        if let Some(mut descendants) = children.remove(&entry.path) {
            descendants.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            for (path, digest) in descendants {
                hasher.update(path.len().to_le_bytes());
                hasher.update(path.as_bytes());
                hasher.update(digest);
            }
        }
        entry.subtree_digest = hasher.finalize().into();
        if let Some(parent) = parent_path(&entry.path) {
            children
                .entry(parent.to_string())
                .or_default()
                .push((entry.path.clone(), entry.subtree_digest));
        }
    }
}

fn filesystem_modified_at(path: &Path) -> Option<DateTime<Utc>> {
    fs::symlink_metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(DateTime::<Utc>::from)
}

fn display_path(path: &str) -> PathBuf {
    path.split('/').collect()
}

fn parent_path(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

fn leaf_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn is_descendant(path: &str, ancestor: &str) -> bool {
    path.strip_prefix(ancestor)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn validate_name(name: &str) -> Result<String> {
    if name.is_empty() || name.trim() != name {
        return Err(WorkingTreeEditError::InvalidName.into());
    }
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(WorkingTreeEditError::InvalidName.into());
    }
    if name == ".synck" || name.starts_with(".synck-tmp-") {
        return Err(WorkingTreeEditError::ReservedName.into());
    }
    compute_name_key(name).map_err(|_| WorkingTreeEditError::InvalidName.into())
}

fn validate_working_tree_edit_intent(intent: &WorkingTreeEditIntent) -> Result<()> {
    if intent.version != WORKING_TREE_EDIT_INTENT_VERSION {
        return Err(LocalError::Integrity(format!(
            "unsupported working-tree edit intent version: {}",
            intent.version
        )));
    }
    validate_intent_path(&intent.source)?;
    validate_intent_parent(&intent.source, intent.source_parent_identity, "source")?;
    validate_intent_identity(intent.expected.identity, "item")?;
    match intent.expected.kind {
        WorkingTreeEntryKind::File
            if intent.expected.size.is_some()
                && intent.expected.digest_hex.as_ref().is_some_and(|digest| {
                    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                }) => {}
        WorkingTreeEntryKind::Folder
            if intent.expected.size.is_none() && intent.expected.digest_hex.is_none() => {}
        _ => {
            return Err(LocalError::Integrity(
                "working-tree edit intent has invalid item authority".to_string(),
            ));
        }
    }
    match (intent.operation, intent.destination.as_deref()) {
        (WorkingTreeEditOperation::Delete, None)
            if intent.destination_parent_identity.is_none() =>
        {
            Ok(())
        }
        (WorkingTreeEditOperation::Move, Some(destination)) => {
            validate_intent_path(destination)?;
            validate_intent_parent(
                destination,
                intent.destination_parent_identity,
                "destination",
            )?;
            if destination == intent.source {
                return Err(LocalError::Integrity(
                    "working-tree move intent has identical paths".to_string(),
                ));
            }
            Ok(())
        }
        _ => Err(LocalError::Integrity(
            "working-tree edit intent has an invalid destination".to_string(),
        )),
    }
}

fn validate_intent_parent(path: &str, identity: Option<FileIdentity>, label: &str) -> Result<()> {
    if parent_path(path).is_some() != identity.is_some() {
        return Err(LocalError::Integrity(format!(
            "working-tree edit intent has invalid {label} parent authority"
        )));
    }
    if let Some(identity) = identity {
        validate_intent_identity(identity, label)?;
    }
    Ok(())
}

fn validate_intent_identity(identity: FileIdentity, label: &str) -> Result<()> {
    if identity
        .incarnation
        .is_some_and(|incarnation| !incarnation.is_valid())
    {
        return Err(LocalError::Integrity(format!(
            "working-tree edit intent has invalid {label} identity"
        )));
    }
    Ok(())
}

fn validate_intent_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(LocalError::Integrity(
            "working-tree edit intent has an empty path".to_string(),
        ));
    }
    for component in path.split('/') {
        validate_name(component).map_err(|_| {
            LocalError::Integrity(format!(
                "working-tree edit intent has an invalid path: {path}"
            ))
        })?;
    }
    Ok(())
}

fn open_import_source(path: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).at("open import source", path)?;
    let metadata = file.metadata().at("inspect import source", path)?;
    if !metadata.is_file() || crate::repository::is_link_or_reparse(&metadata) {
        return Err(WorkingTreeEditError::InvalidImportSource.into());
    }
    Ok(file)
}

fn destination_path(
    current: &[WorkingTreeEntry],
    parent: Option<&WorkingTreeEntry>,
    name: &str,
    source: Option<&WorkingTreeEntry>,
) -> Result<String> {
    if parent.is_some_and(|entry| entry.kind != WorkingTreeEntryKind::Folder) {
        return Err(WorkingTreeEditError::DestinationNotFolder.into());
    }
    destination_path_for_parent(
        current,
        parent.map(|entry| entry.path.as_str()),
        name,
        source,
    )
}

fn destination_path_for_parent(
    current: &[WorkingTreeEntry],
    parent: Option<&str>,
    name: &str,
    source: Option<&WorkingTreeEntry>,
) -> Result<String> {
    let name_key = validate_name(name)?;
    for entry in current {
        if source.is_some_and(|source| source.path == entry.path) {
            continue;
        }
        if parent_path(&entry.path) == parent
            && compute_name_key(leaf_name(&entry.path)).map_err(|error| {
                LocalError::Integrity(format!("invalid existing item name: {error}"))
            })? == name_key
        {
            return Err(WorkingTreeEditError::DestinationExists.into());
        }
    }
    Ok(parent.map_or_else(|| name.to_string(), |parent| format!("{parent}/{name}")))
}

fn working_parent<'a>(
    current: &'a [WorkingTreeEntry],
    path: &str,
) -> Result<Option<&'a WorkingTreeEntry>> {
    let Some(parent) = parent_path(path) else {
        return Ok(None);
    };
    current
        .iter()
        .find(|entry| entry.path == parent && entry.kind == WorkingTreeEntryKind::Folder)
        .map(Some)
        .ok_or_else(|| {
            LocalError::Integrity(format!("working parent is missing from snapshot: {parent}"))
        })
}

fn open_verified_parent(repository: &Repository, parent: Option<&WorkingTreeEntry>) -> Result<Dir> {
    let Some(parent) = parent else {
        return repository.root().try_clone().at("open working parent", ".");
    };
    if parent.kind != WorkingTreeEntryKind::Folder {
        return Err(WorkingTreeEditError::DestinationNotFolder.into());
    }
    let directory = repository
        .root()
        .open_dir_nofollow(display_path(&parent.path))
        .at("open working parent", &parent.path)?;
    let metadata = directory
        .dir_metadata()
        .at("inspect working parent", &parent.path)?;
    if directory_identity(&directory, &metadata)? != parent.identity {
        return Err(WorkingTreeEditError::Stale.into());
    }
    Ok(directory)
}

fn open_intent_parent(
    repository: &Repository,
    parent: Option<&str>,
    expected_identity: Option<FileIdentity>,
) -> Result<Dir> {
    let Some(parent) = parent else {
        if expected_identity.is_some() {
            return Err(LocalError::Integrity(
                "working-tree edit root parent has an identity".to_string(),
            ));
        }
        return repository.root().try_clone().at("open working parent", ".");
    };
    let expected_identity = expected_identity.ok_or_else(|| {
        LocalError::Integrity("working-tree edit parent identity is missing".to_string())
    })?;
    let directory = repository
        .root()
        .open_dir_nofollow(display_path(parent))
        .at("open working parent", parent)?;
    let metadata = directory
        .dir_metadata()
        .at("inspect working parent", parent)?;
    if directory_identity(&directory, &metadata)? != expected_identity {
        return Err(LocalError::Integrity(format!(
            "working-tree edit parent identity changed: {parent}"
        )));
    }
    Ok(directory)
}

fn observed_entry(
    parent: &Dir,
    name: &str,
    display: &str,
) -> Result<Option<WorkingTreeEntry>> {
    let nodes = scan_entry_without_ignore(parent, name)?;
    if nodes.is_empty() {
        return Ok(None);
    }
    let prefix = format!("{name}/");
    let mut entries = nodes
        .into_iter()
        .map(|(path, node)| {
            let path = if path == name {
                display.to_string()
            } else if let Some(suffix) = path.strip_prefix(&prefix) {
                format!("{display}/{suffix}")
            } else {
                return Err(LocalError::Integrity(format!(
                    "working-tree entry scan escaped its root: {path}"
                )));
            };
            Ok(working_entry_from_node(path, node, None, None, None, None))
        })
        .collect::<Result<Vec<_>>>()?;
    populate_subtree_digests(&mut entries);
    Ok(entries.into_iter().find(|entry| entry.path == display))
}

fn require_expected_entry(
    actual: Option<&WorkingTreeEntry>,
    expected: &WorkingTreeEditAuthority,
    location: &str,
) -> Result<()> {
    if actual.is_some_and(|entry| expected.matches(entry)) {
        return Ok(());
    }
    Err(LocalError::Integrity(format!(
        "working-tree edit {location} does not contain the expected item"
    )))
}

fn require_empty_entry(actual: Option<&WorkingTreeEntry>, location: &str) -> Result<()> {
    if actual.is_none() {
        return Ok(());
    }
    Err(LocalError::Integrity(format!(
        "working-tree edit recovery refuses to overwrite an occupied {location}"
    )))
}

fn entry_exists_nofollow(parent: &Dir, name: &str, path: &Path) -> Result<bool> {
    match parent.symlink_metadata(name) {
        Ok(metadata) => {
            if crate::repository::is_cap_link_or_reparse(&metadata) {
                return Err(LocalError::Integrity(format!(
                    "working-tree edit path became a link or reparse point: {}",
                    path.display()
                )));
            }
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(LocalError::Io {
            action: "inspect working-tree edit path",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn sync_open_directory(directory: &Dir, display: &Path) -> Result<()> {
    #[cfg(test)]
    DIRECTORY_SYNC_TRACE.with(|trace| {
        if let Some(trace) = trace.borrow_mut().as_mut() {
            trace.push(display.to_path_buf());
        }
    });
    sync_open_directory_handle(directory, display)
}

fn sync_rename_directories(
    destination: &Dir,
    destination_display: &Path,
    source: &Dir,
    source_display: &Path,
) -> Result<()> {
    sync_open_directory(destination, destination_display)?;
    sync_open_directory(source, source_display)
}

fn rename_between_noreplace(
    source_dir: &Dir,
    source_name: &str,
    destination_dir: &Dir,
    destination_name: &str,
    _source_path: &Path,
    _destination_path: &Path,
) -> io::Result<()> {
    crate::repository::rename_between_noreplace(
        source_dir,
        std::ffi::OsStr::new(source_name),
        destination_dir,
        std::ffi::OsStr::new(destination_name),
        _source_path,
        _destination_path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init;

    async fn repository_with_file() -> (tempfile::TempDir, Repository, WorkingTreeEntry) {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("draft.txt"), b"draft").unwrap();
        init(temporary.path(), "author").await.unwrap();
        let repository = Repository::discover(temporary.path()).await.unwrap();
        let entry = repository
            .working_tree()
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == "draft.txt")
            .unwrap();
        (temporary, repository, entry)
    }

    fn prepared_move_intent(entry: &WorkingTreeEntry, destination: &str) -> WorkingTreeEditIntent {
        WorkingTreeEditIntent {
            version: WORKING_TREE_EDIT_INTENT_VERSION,
            operation: WorkingTreeEditOperation::Move,
            phase: WorkingTreeEditPhase::Prepared,
            source: entry.path.clone(),
            destination: Some(destination.to_string()),
            expected: WorkingTreeEditAuthority::from_entry(entry),
            source_parent_identity: None,
            destination_parent_identity: None,
        }
    }

    #[tokio::test]
    async fn move_syncs_each_rename_destination_before_source() {
        let (_temporary, repository, entry) = repository_with_file().await;
        DIRECTORY_SYNC_TRACE.with(|trace| {
            *trace.borrow_mut() = Some(Vec::new());
        });

        repository
            .rename_working_entry(&entry, "moved.txt")
            .await
            .unwrap();

        let trace = DIRECTORY_SYNC_TRACE.with(|trace| trace.borrow_mut().take().unwrap());
        let stage = repository.working_tree_edit_stage_path();
        let root = repository.root_path().to_path_buf();
        let quarantine = trace
            .windows(2)
            .position(|pair| pair == [stage.clone(), root.clone()])
            .expect("quarantine must sync staging before the source parent");
        let publish = trace
            .windows(2)
            .position(|pair| pair == [root.clone(), stage.clone()])
            .expect("publication must sync the destination parent before staging");
        assert!(quarantine < publish);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn working_tree_journal_stays_with_a_retained_relocated_root() {
        let temporary = tempfile::tempdir().unwrap();
        let registered = temporary.path().join("registered");
        let relocated = temporary.path().join("relocated");
        fs::create_dir(&registered).unwrap();
        fs::write(registered.join("draft.txt"), b"draft").unwrap();
        init(&registered, "author").await.unwrap();
        let repository = Repository::discover(&registered).await.unwrap();
        let entry = repository
            .working_tree()
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == "draft.txt")
            .unwrap();

        fs::rename(&registered, &relocated).unwrap();
        fs::create_dir(&registered).unwrap();
        fs::write(registered.join("replacement.txt"), b"replacement").unwrap();
        init(&registered, "other-author").await.unwrap();

        repository
            .rename_working_entry(&entry, "renamed.txt")
            .await
            .unwrap();

        assert_eq!(fs::read(relocated.join("renamed.txt")).unwrap(), b"draft");
        assert!(!relocated.join("draft.txt").exists());
        assert_eq!(
            fs::read(registered.join("replacement.txt")).unwrap(),
            b"replacement"
        );
        for root in [&registered, &relocated] {
            assert!(!root.join(".synck/working-tree-edit.json").exists());
            assert!(!root.join(".synck/working-tree-edit-stage").exists());
        }
    }

    #[cfg(windows)]
    #[test]
    fn working_tree_sync_uses_retained_authority_not_the_display_path() {
        let temporary = tempfile::tempdir().unwrap();
        let authority_path = temporary.path().join("authority");
        let missing_display_path = temporary.path().join("missing-display-path");
        fs::create_dir(&authority_path).unwrap();
        let authority =
            Dir::open_ambient_dir(&authority_path, cap_std::ambient_authority()).unwrap();

        sync_open_directory(&authority, &missing_display_path).unwrap();

        assert!(!missing_display_path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn handle_relative_rename_does_not_replace() {
        let temporary = tempfile::tempdir().unwrap();
        let source_path = temporary.path().join("source");
        let destination_path = temporary.path().join("destination");
        fs::create_dir(&source_path).unwrap();
        fs::create_dir(&destination_path).unwrap();
        fs::write(source_path.join("first"), b"first").unwrap();
        fs::write(source_path.join("second"), b"second").unwrap();
        fs::write(destination_path.join("occupied"), b"occupied").unwrap();
        let source = Dir::open_ambient_dir(&source_path, cap_std::ambient_authority()).unwrap();
        let destination =
            Dir::open_ambient_dir(&destination_path, cap_std::ambient_authority()).unwrap();

        rename_between_noreplace(
            &source,
            "first",
            &destination,
            "published",
            &source_path.join("first"),
            &destination_path.join("published"),
        )
        .unwrap();
        assert!(!source_path.join("first").exists());
        assert_eq!(
            fs::read(destination_path.join("published")).unwrap(),
            b"first"
        );

        let error = rename_between_noreplace(
            &source,
            "second",
            &destination,
            "occupied",
            &source_path.join("second"),
            &destination_path.join("occupied"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(source_path.join("second")).unwrap(), b"second");
        assert_eq!(
            fs::read(destination_path.join("occupied")).unwrap(),
            b"occupied"
        );
    }

    #[tokio::test]
    async fn discovery_restores_an_exact_quarantined_entry() {
        let (temporary, repository, entry) = repository_with_file().await;
        repository
            .quarantine_working_entry(&entry, None, WorkingTreeEditOperation::Delete, None, None)
            .unwrap();
        drop(repository);

        let recovered = Repository::discover(temporary.path()).await.unwrap();
        assert_eq!(
            fs::read(temporary.path().join("draft.txt")).unwrap(),
            b"draft"
        );
        assert!(
            !temporary
                .path()
                .join(".synck/working-tree-edit-stage")
                .exists()
        );
        assert!(
            !temporary
                .path()
                .join(".synck/working-tree-edit.json")
                .exists()
        );
        drop(recovered);
    }

    #[tokio::test]
    async fn discovery_accepts_a_restored_entry_after_stage_cleanup() {
        let (temporary, repository, entry) = repository_with_file().await;
        repository
            .quarantine_working_entry(&entry, None, WorkingTreeEditOperation::Delete, None, None)
            .unwrap();
        let stage = repository.open_working_tree_edit_stage().unwrap();
        let root = repository.root().try_clone().unwrap();
        rename_between_noreplace(
            &stage,
            WORKING_TREE_EDIT_STAGE_ENTRY,
            &root,
            "draft.txt",
            &temporary
                .path()
                .join(".synck/working-tree-edit-stage/entry"),
            &temporary.path().join("draft.txt"),
        )
        .unwrap();
        sync_rename_directories(
            &root,
            temporary.path(),
            &stage,
            &repository.working_tree_edit_stage_path(),
        )
        .unwrap();
        drop(stage);
        let metadata = repository.open_metadata_dir().unwrap();
        metadata.remove_dir(WORKING_TREE_EDIT_STAGE_NAME).unwrap();
        sync_open_directory(&metadata, repository.metadata_path()).unwrap();
        drop(metadata);
        drop(root);
        drop(repository);

        let recovered = Repository::discover(temporary.path()).await.unwrap();
        assert_eq!(
            fs::read(temporary.path().join("draft.txt")).unwrap(),
            b"draft"
        );
        assert!(
            !temporary
                .path()
                .join(".synck/working-tree-edit.json")
                .exists()
        );
        drop(recovered);
    }

    #[tokio::test]
    async fn discovery_completes_a_move_published_before_its_phase_update() {
        let (temporary, repository, entry) = repository_with_file().await;
        repository
            .quarantine_working_entry(
                &entry,
                None,
                WorkingTreeEditOperation::Move,
                Some("moved.txt"),
                None,
            )
            .unwrap();
        let stage = repository.open_working_tree_edit_stage().unwrap();
        let root = repository.root().try_clone().unwrap();
        rename_between_noreplace(
            &stage,
            WORKING_TREE_EDIT_STAGE_ENTRY,
            &root,
            "moved.txt",
            &temporary
                .path()
                .join(".synck/working-tree-edit-stage/entry"),
            &temporary.path().join("moved.txt"),
        )
        .unwrap();
        drop(stage);
        drop(root);
        drop(repository);

        let recovered = Repository::discover(temporary.path()).await.unwrap();
        assert_eq!(
            fs::read(temporary.path().join("moved.txt")).unwrap(),
            b"draft"
        );
        assert!(!temporary.path().join("draft.txt").exists());
        assert!(
            !temporary
                .path()
                .join(".synck/working-tree-edit.json")
                .exists()
        );
        drop(recovered);
    }

    #[tokio::test]
    async fn discovery_validates_a_published_move_after_stage_cleanup() {
        let (temporary, repository, entry) = repository_with_file().await;
        let mut intent = repository
            .quarantine_working_entry(
                &entry,
                None,
                WorkingTreeEditOperation::Move,
                Some("moved.txt"),
                None,
            )
            .unwrap();
        let stage = repository.open_working_tree_edit_stage().unwrap();
        let root = repository.root().try_clone().unwrap();
        rename_between_noreplace(
            &stage,
            WORKING_TREE_EDIT_STAGE_ENTRY,
            &root,
            "moved.txt",
            &temporary
                .path()
                .join(".synck/working-tree-edit-stage/entry"),
            &temporary.path().join("moved.txt"),
        )
        .unwrap();
        sync_rename_directories(
            &root,
            temporary.path(),
            &stage,
            &repository.working_tree_edit_stage_path(),
        )
        .unwrap();
        repository
            .mark_working_tree_edit_published(&mut intent)
            .unwrap();
        drop(stage);
        repository
            .open_metadata_dir()
            .unwrap()
            .remove_dir(WORKING_TREE_EDIT_STAGE_NAME)
            .unwrap();
        drop(root);
        drop(repository);

        let recovered = Repository::discover(temporary.path()).await.unwrap();
        assert_eq!(
            fs::read(temporary.path().join("moved.txt")).unwrap(),
            b"draft"
        );
        assert!(
            !temporary
                .path()
                .join(".synck/working-tree-edit.json")
                .exists()
        );
        drop(recovered);
    }

    #[tokio::test]
    async fn discovery_rejects_a_mismatched_published_destination() {
        let (temporary, repository, entry) = repository_with_file().await;
        repository
            .quarantine_working_entry(
                &entry,
                None,
                WorkingTreeEditOperation::Move,
                Some("moved.txt"),
                None,
            )
            .unwrap();
        repository
            .open_working_tree_edit_stage()
            .unwrap()
            .remove_file(WORKING_TREE_EDIT_STAGE_ENTRY)
            .unwrap();
        fs::write(temporary.path().join("moved.txt"), b"unrelated").unwrap();
        drop(repository);

        let error = match Repository::discover(temporary.path()).await {
            Ok(_) => panic!("mismatched published destination was accepted"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("destination does not contain the expected item")
        );
        assert_eq!(
            fs::read(temporary.path().join("moved.txt")).unwrap(),
            b"unrelated"
        );
        assert!(
            temporary
                .path()
                .join(".synck/working-tree-edit.json")
                .exists()
        );
    }

    #[tokio::test]
    async fn discovery_clears_an_unstarted_prepared_move() {
        let (temporary, repository, entry) = repository_with_file().await;
        repository
            .write_working_tree_edit_intent(&prepared_move_intent(&entry, "moved.txt"))
            .unwrap();
        drop(repository);

        let recovered = Repository::discover(temporary.path()).await.unwrap();
        assert_eq!(
            fs::read(temporary.path().join("draft.txt")).unwrap(),
            b"draft"
        );
        assert!(!temporary.path().join("moved.txt").exists());
        assert!(
            !temporary
                .path()
                .join(".synck/working-tree-edit.json")
                .exists()
        );
        drop(recovered);
    }

    #[tokio::test]
    async fn opened_parent_must_match_the_snapshot_identity() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir(temporary.path().join("docs")).unwrap();
        init(temporary.path(), "author").await.unwrap();
        let repository = Repository::discover(temporary.path()).await.unwrap();
        let parent = repository
            .working_tree()
            .await
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == "docs")
            .unwrap();
        fs::rename(
            temporary.path().join("docs"),
            temporary.path().join("old-docs"),
        )
        .unwrap();
        fs::create_dir(temporary.path().join("docs")).unwrap();

        let error = open_verified_parent(&repository, Some(&parent)).unwrap_err();
        assert!(matches!(
            error,
            LocalError::WorkingTree(WorkingTreeEditError::Stale)
        ));
    }
}
