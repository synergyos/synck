use crate::commands::display_to_path;
use crate::error::{IoContext, LocalError, Result};
use crate::repository::{Repository, is_link_or_reparse, sync_directory};
use crate::worktree::{CommittedNode, NodeKind, WorkingNode};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use synck_vcs::types::BlobId;

pub(crate) fn materialize_tree(
    repository: &Repository,
    root: &Path,
    tree: &BTreeMap<String, CommittedNode>,
) -> Result<()> {
    for (path, node) in tree {
        if node.kind == NodeKind::Folder {
            let destination = root.join(display_to_path(path));
            fs::create_dir(&destination).at("create tree folder", &destination)?;
        }
    }
    for (path, node) in tree {
        if node.kind != NodeKind::File {
            continue;
        }
        let blob_id = node
            .blob_id
            .as_ref()
            .ok_or_else(|| LocalError::Integrity(format!("tree FILE {path} has no object")))?;
        let size = node
            .size
            .ok_or_else(|| LocalError::Integrity(format!("tree FILE {path} has no size")))?;
        copy_blob_new(
            repository,
            blob_id,
            u64::try_from(size)
                .map_err(|_| LocalError::Integrity(format!("tree FILE {path} has invalid size")))?,
            &root.join(display_to_path(path)),
        )?;
    }
    sync_tree(root)
}

pub(crate) fn validate_materialized_tree(
    committed: &BTreeMap<String, CommittedNode>,
    working: &BTreeMap<String, WorkingNode>,
) -> Result<()> {
    if committed.len() != working.len() {
        return Err(LocalError::Integrity(
            "materialized tree entry count differs from baseline".to_string(),
        ));
    }
    for (path, old) in committed {
        let new = working
            .get(path)
            .ok_or_else(|| LocalError::Integrity(format!("materialized tree is missing {path}")))?;
        if old.kind != new.kind
            || (old.kind == NodeKind::File
                && (old.blob_id.as_ref().map(BlobId::as_str)
                    != new
                        .digest_hex
                        .as_ref()
                        .map(|digest| format!("sha256:{digest}"))
                        .as_deref()
                    || old.size.and_then(|size| u64::try_from(size).ok()) != new.size))
        {
            return Err(LocalError::Integrity(format!(
                "materialized tree differs at {path}"
            )));
        }
    }
    Ok(())
}

fn copy_blob_new(
    repository: &Repository,
    blob_id: &BlobId,
    size: u64,
    destination: &Path,
) -> Result<()> {
    let mut target = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .at("create tree file", destination)?;
    let result = repository.copy_blob(blob_id, size, &mut target);
    if result.is_ok() {
        target.sync_all().at("sync tree file", destination)?;
    }
    drop(target);
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn sync_tree(root: &Path) -> Result<()> {
    let mut directories = Vec::new();
    collect_directories(root, &mut directories)?;
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        sync_directory(&directory)?;
    }
    Ok(())
}

fn collect_directories(root: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    output.push(root.to_path_buf());
    for entry in fs::read_dir(root).at("read directory", root)? {
        let entry = entry.at("read directory entry", root)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).at("inspect directory entry", &path)?;
        if metadata.file_type().is_dir() && !is_link_or_reparse(&metadata) {
            collect_directories(&entry.path(), output)?;
        } else if !metadata.file_type().is_file() || is_link_or_reparse(&metadata) {
            return Err(LocalError::Integrity(format!(
                "tree staging contains an unsupported entry: {}",
                path.display()
            )));
        }
    }
    Ok(())
}
