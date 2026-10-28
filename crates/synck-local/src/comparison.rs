use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::OpenOptions;
use diffy::DiffOptions;
use sha2::{Digest, Sha256};
use synck_vcs::types::ItemId;

use crate::commands::{display_to_path, select_changes, sort_changes};
use crate::error::{IoContext, LocalError, Result};
use crate::repository::{Repository, is_cap_link_or_reparse};
use crate::worktree::{Change, ChangeKind, CommittedNode, NodeKind, WorkingNode};

pub const TEXT_DIFF_MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub enum TextDiffKind {
    Patch(String),
    Binary,
    TooLarge,
}

#[derive(Debug, Eq, PartialEq)]
pub struct TextDiffEntry {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub kind: TextDiffKind,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ComparedRevision {
    pub repo_seq: i64,
    pub commit_id: String,
}

#[derive(Debug)]
pub struct StructuralComparison {
    pub from: ComparedRevision,
    pub to: ComparedRevision,
    pub changes: Vec<Change>,
}

enum FileBytes {
    Bytes(Vec<u8>),
    TooLarge,
}

impl Repository {
    pub async fn compare(&self, from: &str, to: &str) -> Result<StructuralComparison> {
        self.recover().await?;
        let delta = self.historical_changed_trees(from, to).await?;
        Ok(StructuralComparison {
            from: compared_revision(&delta.from_commit)?,
            to: compared_revision(&delta.to_commit)?,
            changes: compare_trees(&delta.from_tree, &delta.to_tree, &delta.changed_item_ids),
        })
    }

    pub async fn diff_worktree(&self) -> Result<Vec<TextDiffEntry>> {
        self.diff_worktree_selected(self.root_path(), &[]).await
    }

    pub async fn diff_worktree_selected(
        &self,
        caller: &Path,
        inputs: &[PathBuf],
    ) -> Result<Vec<TextDiffEntry>> {
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
        let mut output = Vec::new();
        for change in changes {
            let old = historical_file_for_change(&committed, &change);
            let new = working
                .get(&change.path)
                .filter(|node| node.kind == NodeKind::File);
            if old.is_none() && new.is_none() {
                continue;
            }
            let old_bytes = old
                .map(|(_, node)| self.read_object_bytes(node))
                .transpose()?;
            let new_bytes = new
                .map(|node| self.read_working_bytes(&change.path, node))
                .transpose()?;
            if let Some(entry) = text_diff_entry(
                old.map(|(path, _)| path.to_string()),
                new.map(|_| change.path.clone()),
                old_bytes,
                new_bytes,
            )? {
                output.push(entry);
            }
        }
        Ok(output)
    }

    pub async fn diff_revisions(&self, from: &str, to: &str) -> Result<Vec<TextDiffEntry>> {
        self.recover().await?;
        let delta = self.historical_changed_trees(from, to).await?;
        let mut output = Vec::new();
        for change in compare_trees(&delta.from_tree, &delta.to_tree, &delta.changed_item_ids) {
            let old = historical_file_for_change(&delta.from_tree, &change);
            let new = delta
                .to_tree
                .get(&change.path)
                .filter(|node| node.kind == NodeKind::File);
            if old.is_none() && new.is_none() {
                continue;
            }
            let old_bytes = old
                .map(|(_, node)| self.read_object_bytes(node))
                .transpose()?;
            let new_bytes = new.map(|node| self.read_object_bytes(node)).transpose()?;
            if let Some(entry) = text_diff_entry(
                old.map(|(path, _)| path.to_string()),
                new.map(|_| change.path.clone()),
                old_bytes,
                new_bytes,
            )? {
                output.push(entry);
            }
        }
        Ok(output)
    }

    fn read_object_bytes(&self, node: &CommittedNode) -> Result<FileBytes> {
        let size = u64::try_from(node.size.ok_or_else(|| {
            LocalError::Integrity("historical FILE has no content size".to_string())
        })?)
        .map_err(|_| LocalError::Integrity("historical FILE has negative size".to_string()))?;
        let blob_id = node
            .blob_id
            .as_ref()
            .ok_or_else(|| LocalError::Integrity("historical FILE has no object".to_string()))?;
        if size > TEXT_DIFF_MAX_BYTES {
            self.verify_blob(blob_id, size)?;
            return Ok(FileBytes::TooLarge);
        }
        let mut bytes = Vec::with_capacity(usize::try_from(size).expect("bounded diff size"));
        self.copy_blob(blob_id, size, &mut bytes)?;
        Ok(FileBytes::Bytes(bytes))
    }

    fn read_working_bytes(&self, display: &str, node: &WorkingNode) -> Result<FileBytes> {
        let size = node
            .size
            .ok_or_else(|| LocalError::Integrity(format!("working FILE has no size: {display}")))?;
        if size > TEXT_DIFF_MAX_BYTES {
            return Ok(FileBytes::TooLarge);
        }
        let relative = display_to_path(display);
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = self
            .root()
            .open_with(&relative, &options)
            .at("open diff file", &relative)?;
        let metadata = file.metadata().at("inspect diff file", &relative)?;
        if !metadata.file_type().is_file() || is_cap_link_or_reparse(&metadata) {
            return Err(LocalError::Invalid(format!(
                "diff target is not a regular file: {display}"
            )));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(size).expect("bounded diff size"));
        file.take(TEXT_DIFF_MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .at("read diff file", &relative)?;
        let digest = hex::encode(Sha256::digest(&bytes));
        if u64::try_from(bytes.len()).expect("diff buffer length fits u64") != size
            || node.digest_hex.as_deref() != Some(&digest)
        {
            return Err(LocalError::Invalid(format!(
                "file changed while being read for diff: {display}"
            )));
        }
        Ok(FileBytes::Bytes(bytes))
    }
}

fn compared_revision(commit: &synck_vcs::types::CommitRow) -> Result<ComparedRevision> {
    Ok(ComparedRevision {
        repo_seq: commit.repo_seq.ok_or_else(|| {
            LocalError::Integrity(format!(
                "commit {} has no repository sequence",
                commit.commit_id
            ))
        })?,
        commit_id: commit.commit_id.to_string(),
    })
}

fn historical_file_for_change<'a>(
    tree: &'a BTreeMap<String, CommittedNode>,
    change: &Change,
) -> Option<(&'a str, &'a CommittedNode)> {
    let path = change.source_path.as_deref().unwrap_or(&change.path);
    tree.get_key_value(path)
        .filter(|(_, node)| node.kind == NodeKind::File)
        .map(|(path, node)| (path.as_str(), node))
}

fn text_diff_entry(
    old_path: Option<String>,
    new_path: Option<String>,
    old: Option<FileBytes>,
    new: Option<FileBytes>,
) -> Result<Option<TextDiffEntry>> {
    if matches!(old, Some(FileBytes::TooLarge)) || matches!(new, Some(FileBytes::TooLarge)) {
        return Ok(Some(TextDiffEntry {
            old_path,
            new_path,
            kind: TextDiffKind::TooLarge,
        }));
    }
    let old = match old {
        Some(FileBytes::Bytes(bytes)) => bytes,
        None => Vec::new(),
        Some(FileBytes::TooLarge) => unreachable!(),
    };
    let new = match new {
        Some(FileBytes::Bytes(bytes)) => bytes,
        None => Vec::new(),
        Some(FileBytes::TooLarge) => unreachable!(),
    };
    if old == new {
        return Ok(None);
    }
    let old_text = std::str::from_utf8(&old)
        .ok()
        .filter(|text| !text.contains('\0'));
    let new_text = std::str::from_utf8(&new)
        .ok()
        .filter(|text| !text.contains('\0'));
    let (Some(old_text), Some(new_text)) = (old_text, new_text) else {
        return Ok(Some(TextDiffEntry {
            old_path,
            new_path,
            kind: TextDiffKind::Binary,
        }));
    };
    let old_label = old_path
        .as_deref()
        .map_or_else(|| "/dev/null".to_string(), |path| format!("a/{path}"));
    let new_label = new_path
        .as_deref()
        .map_or_else(|| "/dev/null".to_string(), |path| format!("b/{path}"));
    let mut options = DiffOptions::new();
    options
        .set_original_filename(old_label)
        .set_modified_filename(new_label);
    let patch = options.create_patch(old_text, new_text).to_string();
    if patch.is_empty() {
        return Err(LocalError::Integrity(
            "text diff produced no patch for different content".to_string(),
        ));
    }
    Ok(Some(TextDiffEntry {
        old_path,
        new_path,
        kind: TextDiffKind::Patch(patch),
    }))
}

pub(crate) fn compare_trees(
    from: &BTreeMap<String, CommittedNode>,
    to: &BTreeMap<String, CommittedNode>,
    changed_item_ids: &BTreeSet<ItemId>,
) -> Vec<Change> {
    let to_by_id: HashMap<&ItemId, (&String, &CommittedNode)> = to
        .iter()
        .map(|(path, node)| (&node.item_id, (path, node)))
        .collect();
    let mut changes = Vec::new();
    let mut seen = HashSet::new();
    for (old_path, old) in from {
        if !changed_item_ids.contains(&old.item_id) {
            continue;
        }
        seen.insert(old.item_id.clone());
        let Some((new_path, new)) = to_by_id.get(&old.item_id) else {
            changes.push(Change {
                kind: ChangeKind::Delete,
                item_type: old.kind.as_item_type().to_owned(),
                path: old_path.clone(),
                source_path: None,
                content_changed: false,
            });
            continue;
        };
        let old_parent = parent_id(old_path, from);
        let new_parent = parent_id(new_path, to);
        let old_name = final_name(old_path);
        let new_name = final_name(new_path);
        let name_changed = old_name != new_name;
        let parent_changed = old_parent != new_parent;
        let content_changed =
            old.kind == NodeKind::File && (old.blob_id != new.blob_id || old.size != new.size);
        if let Some(kind) = ChangeKind::relocation(parent_changed, name_changed) {
            changes.push(Change {
                kind,
                item_type: new.kind.as_item_type().to_owned(),
                path: (*new_path).clone(),
                source_path: Some(old_path.clone()),
                content_changed,
            });
        } else if content_changed {
            changes.push(Change {
                kind: ChangeKind::Modify,
                item_type: new.kind.as_item_type().to_owned(),
                path: (*new_path).clone(),
                source_path: (old_path != *new_path).then(|| old_path.clone()),
                content_changed: true,
            });
        }
    }
    for (new_path, new) in to {
        if changed_item_ids.contains(&new.item_id) && !seen.contains(&new.item_id) {
            changes.push(Change {
                kind: ChangeKind::Add,
                item_type: new.kind.as_item_type().to_owned(),
                path: new_path.clone(),
                source_path: None,
                content_changed: false,
            });
        }
    }
    sort_changes(changes)
}

fn parent_id<'a>(path: &str, tree: &'a BTreeMap<String, CommittedNode>) -> Option<&'a ItemId> {
    path.rsplit_once('/')
        .and_then(|(parent, _)| tree.get(parent))
        .map(|node| &node.item_id)
}

fn final_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .expect("validated display path has a Unicode name")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use synck_vcs::types::ObjectId;

    use crate::init;

    fn node(id: &str, kind: NodeKind, blob: Option<&str>) -> CommittedNode {
        CommittedNode {
            plain_digest: None,
            item_id: ItemId::parse(id).unwrap(),
            kind,
            head_seq: 1,
            blob_id: blob.map(|value| ObjectId::parse(value).unwrap()),
            size: blob.map(|_| 1),
        }
    }

    #[test]
    fn structural_comparison_collapses_descendants_of_a_folder_move() {
        let folder = "0198d958-0f3a-7000-8000-000000000001";
        let file = "0198d958-0f3a-7000-8000-000000000002";
        let blob = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let from = BTreeMap::from([
            ("Old".to_string(), node(folder, NodeKind::Folder, None)),
            (
                "Old/file.txt".to_string(),
                node(file, NodeKind::File, Some(blob)),
            ),
        ]);
        let to = BTreeMap::from([
            ("New".to_string(), node(folder, NodeKind::Folder, None)),
            (
                "New/file.txt".to_string(),
                node(file, NodeKind::File, Some(blob)),
            ),
        ]);

        let changes = compare_trees(
            &from,
            &to,
            &BTreeSet::from([ItemId::parse(folder).unwrap()]),
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Rename);
        assert_eq!(changes[0].source_path.as_deref(), Some("Old"));
        assert_eq!(changes[0].path, "New");
    }

    #[test]
    fn structural_comparison_uses_unchanged_parents_as_context_only() {
        let source = "0198d958-0f3a-7000-8000-000000000001";
        let destination = "0198d958-0f3a-7000-8000-000000000002";
        let file = "0198d958-0f3a-7000-8000-000000000003";
        let blob = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let from = BTreeMap::from([
            ("Src".to_string(), node(source, NodeKind::Folder, None)),
            ("Dst".to_string(), node(destination, NodeKind::Folder, None)),
            (
                "Src/file.txt".to_string(),
                node(file, NodeKind::File, Some(blob)),
            ),
        ]);
        let to = BTreeMap::from([
            ("Src".to_string(), node(source, NodeKind::Folder, None)),
            ("Dst".to_string(), node(destination, NodeKind::Folder, None)),
            (
                "Dst/file.txt".to_string(),
                node(file, NodeKind::File, Some(blob)),
            ),
        ]);

        let changes = compare_trees(&from, &to, &BTreeSet::from([ItemId::parse(file).unwrap()]));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Move);
        assert_eq!(changes[0].source_path.as_deref(), Some("Src/file.txt"));
        assert_eq!(changes[0].path, "Dst/file.txt");
    }

    #[tokio::test]
    async fn selected_worktree_diff_returns_only_the_requested_change() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("first.txt"), b"first\n").unwrap();
        fs::write(root.join("second.txt"), b"second\n").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        fs::write(root.join("first.txt"), b"changed first\n").unwrap();
        fs::write(root.join("second.txt"), b"changed second\n").unwrap();

        let diff = repository
            .diff_worktree_selected(root, &[PathBuf::from("first.txt")])
            .await
            .unwrap();

        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].old_path.as_deref(), Some("first.txt"));
        assert_eq!(diff[0].new_path.as_deref(), Some("first.txt"));
        let TextDiffKind::Patch(patch) = &diff[0].kind else {
            panic!("text change must produce a patch");
        };
        assert!(patch.contains("-first\n+changed first\n"), "{patch}");
        assert!(!patch.contains("second"), "{patch}");
    }

    #[tokio::test]
    async fn selected_worktree_diff_supports_a_deleted_file() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("deleted.txt"), b"deleted\n").unwrap();
        fs::write(root.join("unrelated.txt"), b"unchanged\n").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        fs::remove_file(root.join("deleted.txt")).unwrap();
        fs::write(root.join("unrelated.txt"), b"changed\n").unwrap();

        let diff = repository
            .diff_worktree_selected(root, &[PathBuf::from("deleted.txt")])
            .await
            .unwrap();

        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].old_path.as_deref(), Some("deleted.txt"));
        assert_eq!(diff[0].new_path, None);
    }
}
