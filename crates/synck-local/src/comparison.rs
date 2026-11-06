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
use crate::objects::BlobReader;
use crate::ooxml::OoxmlKind;
use crate::repository::{Repository, is_cap_link_or_reparse};
use crate::worktree::{Change, ChangeKind, CommittedNode, NodeKind, WorkingNode};

pub const TEXT_DIFF_MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub enum TextDiffKind {
    Patch(String),
    /// A patch of the text of an Office document or a PDF, not of its bytes.
    Document(String),
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

pub(crate) enum FileBytes {
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
        let objects = self.blob_reader()?;
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
                .map(|(_, node)| self.read_object_bytes(&objects, node))
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
        let objects = self.blob_reader()?;
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
                .map(|(_, node)| self.read_object_bytes(&objects, node))
                .transpose()?;
            let new_bytes = new.map(|node| self.read_object_bytes(&objects, node)).transpose()?;
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

    pub(crate) fn read_object_bytes(
        &self,
        objects: &BlobReader,
        node: &CommittedNode,
    ) -> Result<FileBytes> {
        let size = u64::try_from(node.size.ok_or_else(|| {
            LocalError::Integrity("historical FILE has no content size".to_string())
        })?)
        .map_err(|_| LocalError::Integrity("historical FILE has negative size".to_string()))?;
        let blob_id = node
            .blob_id
            .as_ref()
            .ok_or_else(|| LocalError::Integrity("historical FILE has no object".to_string()))?;
        if size > TEXT_DIFF_MAX_BYTES {
            objects.copy_blob(blob_id, size, &mut std::io::sink())?;
            return Ok(FileBytes::TooLarge);
        }
        let mut bytes = Vec::with_capacity(usize::try_from(size).expect("bounded diff size"));
        objects.copy_blob(blob_id, size, &mut bytes)?;
        Ok(FileBytes::Bytes(bytes))
    }

    pub(crate) fn read_working_bytes(&self, display: &str, node: &WorkingNode) -> Result<FileBytes> {
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
    if let (Some((old_text, old_sections)), Some((new_text, new_sections))) = (
        document_text(old_path.as_deref(), &old),
        document_text(new_path.as_deref(), &new),
    ) {
        // Bytes differ but the text may not: a change only in formatting.
        let kind = if old_text == new_text {
            TextDiffKind::Binary
        } else {
            let patch = unified(old_path.as_deref(), new_path.as_deref(), &old_text, &new_text)?;
            if old_sections || new_sections {
                TextDiffKind::Document(section_headers(&patch, &old_text, &new_text))
            } else {
                TextDiffKind::Document(patch)
            }
        };
        return Ok(Some(TextDiffEntry { old_path, new_path, kind }));
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
    let patch = unified(old_path.as_deref(), new_path.as_deref(), old_text, new_text)?;
    Ok(Some(TextDiffEntry {
        old_path,
        new_path,
        kind: TextDiffKind::Patch(patch),
    }))
}

/// The text of an Office document or a PDF at `path`, or "" for an absent
/// side, and whether its lines fall under section lines: `Slide N`, `Sheet N`
/// or `Page N`. None when the present side is not a readable document.
fn document_text(path: Option<&str>, bytes: &[u8]) -> Option<(String, bool)> {
    let Some(path) = path.map(Path::new) else {
        return Some((String::new(), false));
    };
    if let Some(kind) = OoxmlKind::from_path(path) {
        let text = crate::office_text::document_text(kind, bytes)?;
        return Some((text, kind != OoxmlKind::Docx));
    }
    if !path.extension()?.eq_ignore_ascii_case("pdf") {
        return None;
    }
    Some((crate::pdf_text::document_text(bytes)?, true))
}

/// Names the section each hunk starts in after its `@@` line, as Git names
/// the enclosing function: the last `Page N`, `Slide N` or `Sheet N` line
/// before the hunk, in the old text, or in the new one for an added file.
fn section_headers(patch: &str, old_text: &str, new_text: &str) -> String {
    let added = old_text.is_empty();
    let lines: Vec<&str> = if added { new_text } else { old_text }.lines().collect();
    let mut out = String::with_capacity(patch.len());
    for line in patch.split_inclusive('\n') {
        let section = line
            .strip_prefix("@@ -")
            .and_then(|ranges| ranges.split(' ').nth(usize::from(added)))
            .and_then(|range| {
                let range = range.trim_start_matches('+');
                let (start, length) = range.split_once(',').unwrap_or((range, "1"));
                let (start, length): (usize, usize) = (start.parse().ok()?, length.parse().ok()?);
                // An empty range starts after its line, any other at it.
                let before = if length == 0 { start } else { start.saturating_sub(1) };
                lines[..before.min(lines.len())].iter().rev().find(|line| is_section(line))
            });
        match section {
            Some(section) => {
                out.push_str(line.trim_end_matches('\n'));
                out.push(' ');
                out.push_str(section);
                out.push('\n');
            }
            None => out.push_str(line),
        }
    }
    out
}

fn is_section(line: &str) -> bool {
    ["Page ", "Slide ", "Sheet "].iter().any(|name| {
        line.strip_prefix(name)
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

fn unified(old_path: Option<&str>, new_path: Option<&str>, old_text: &str, new_text: &str) -> Result<String> {
    let old_label = old_path.map_or_else(|| "/dev/null".to_string(), |path| format!("a/{path}"));
    let new_label = new_path.map_or_else(|| "/dev/null".to_string(), |path| format!("b/{path}"));
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
    Ok(patch)
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

    fn docx(body: &str) -> Vec<u8> {
        use std::io::Write;
        let mut output = std::io::Cursor::new(Vec::new());
        let mut archive = rawzip::ZipArchiveWriter::new(&mut output);
        let (mut entry, config) = archive.new_file("word/document.xml").start().unwrap();
        let mut writer = config.wrap(&mut entry);
        writer
            .write_all(format!("<w:document><w:body>{body}</w:body></w:document>").as_bytes())
            .unwrap();
        let (_, descriptor) = writer.finish().unwrap();
        entry.finish(descriptor).unwrap();
        archive.finish().unwrap();
        output.into_inner()
    }

    fn diff_kind(old: Vec<u8>, new: Vec<u8>) -> TextDiffKind {
        let path = Some("Contracts/lease.docx".to_string());
        text_diff_entry(path.clone(), path, Some(FileBytes::Bytes(old)), Some(FileBytes::Bytes(new)))
            .unwrap()
            .unwrap()
            .kind
    }

    #[test]
    fn office_documents_diff_by_their_text() {
        let old = docx("<w:p><w:r><w:t>Term: 12 months</w:t></w:r></w:p>");
        let new = docx("<w:p><w:r><w:t>Term: 24 months</w:t></w:r></w:p>");
        let TextDiffKind::Document(patch) = diff_kind(old.clone(), new) else {
            panic!("expected a document diff");
        };
        assert!(patch.starts_with("--- a/Contracts/lease.docx\n+++ b/Contracts/lease.docx\n"));
        assert!(patch.contains("-Term: 12 months\n+Term: 24 months\n"));

        let restyled = docx("<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>Term: 12 months</w:t></w:r></w:p>");
        assert_eq!(diff_kind(old.clone(), restyled), TextDiffKind::Binary);
        assert_eq!(diff_kind(old, b"not a zip".to_vec()), TextDiffKind::Binary);
    }

    #[test]
    fn pdf_documents_diff_by_their_text_under_page_headers() {
        let page = |paragraphs: &[&str]| {
            let lines: Vec<String> = paragraphs
                .iter()
                .enumerate()
                .map(|(index, text)| format!("1 0 0 1 72 {} Tm ({text}) Tj", 700 - 40 * index))
                .collect();
            format!("BT /F1 10 Tf {} ET", lines.join(" "))
        };
        let old = crate::pdf_text::tests::pages(&[&page(&["Intro"]), &page(&["One", "Two", "Three", "Four", "Five", "Six"])]);
        let new = crate::pdf_text::tests::pages(&[&page(&["Intro"]), &page(&["One", "Two", "Three", "Four", "5", "Six"])]);
        let path = Some("Reports/q3.pdf".to_string());
        let entry = text_diff_entry(path.clone(), path, Some(FileBytes::Bytes(old)), Some(FileBytes::Bytes(new)))
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.kind,
            TextDiffKind::Document(
                "--- a/Reports/q3.pdf\n+++ b/Reports/q3.pdf\n@@ -5,5 +5,5 @@ Page 2\n   Two\n   Three\n   Four\n-  Five\n+  5\n   Six\n"
                    .to_string()
            )
        );
    }

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
