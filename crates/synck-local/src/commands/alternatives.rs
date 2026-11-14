//! Alternatives: a copy of a file or folder inside the same history, and
//! taking one back.
//!
//! `duplicate` copies committed files to a new path and commits them as new
//! items, each recording the revision it began as (`derived_from`). `adopt`
//! makes an original match an alternative of it, path by path: a file at the
//! same relative path takes the alternative's content as a new version that
//! records where it came from (`merged_from`), a file only in the alternative
//! is created, and a file only in the original is deleted. The original keeps
//! its identity and history. Both write the working files, then commit them
//! as one ordinary commit; an interruption between leaves those files as
//! changes in the working tree.

use super::*;

/// `path` and everything under it, keyed by the path relative to it ("" for
/// `path` itself).
fn subtree<'a>(
    tree: &'a BTreeMap<String, CommittedNode>,
    path: &str,
) -> BTreeMap<String, &'a CommittedNode> {
    tree.iter()
        .filter_map(|(entry, node)| {
            if entry == path {
                Some((String::new(), node))
            } else {
                entry
                    .strip_prefix(path)
                    .and_then(|rest| rest.strip_prefix('/'))
                    .map(|rest| (rest.to_string(), node))
            }
        })
        .collect()
}

fn join(root: &str, relative: &str) -> String {
    if relative.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{relative}")
    }
}

fn revision(node: &CommittedNode) -> ItemRevision {
    ItemRevision {
        item_id: node.item_id.clone(),
        item_seq: node.head_seq,
    }
}

impl Repository {
    /// The committed tree at HEAD, with `paths` unchanged in the working tree.
    async fn clean_tree(&self, paths: &[&str]) -> Result<BTreeMap<String, CommittedNode>> {
        self.reject_unresolved()?;
        self.pull_if_behind().await?;
        let (committed, _, changes, _) = self.changes().await?;
        for change in &changes {
            if let Some(path) = paths.iter().find(|path| {
                path_in_scope(&change.path, path)
                    || change
                        .source_path
                        .as_deref()
                        .is_some_and(|source| path_in_scope(source, path))
            }) {
                return Err(LocalError::Invalid(format!(
                    "{path} has uncommitted changes; commit or restore them first"
                )));
            }
        }
        Ok(committed)
    }

    fn require_selected(&self, path: &str) -> Result<()> {
        if self
            .checkout_paths()
            .is_some_and(|paths| !crate::checkout::selected(paths, path))
        {
            return Err(LocalError::Invalid(format!(
                "{path} is outside this working tree's checkout; widen it first"
            )));
        }
        Ok(())
    }

    /// Copy a committed file or folder to `destination` as an alternative.
    pub async fn duplicate_path(
        &self,
        caller: &Path,
        source: &Path,
        destination: &Path,
        message: Option<&str>,
    ) -> Result<CommitOutcome> {
        let source = normalize_input_path(self.root_path(), caller, source)?;
        let destination = normalize_input_path(self.root_path(), caller, destination)?;
        if path_in_scope(&destination, &source) {
            return Err(LocalError::Invalid(
                "an alternative cannot be inside its source".into(),
            ));
        }
        self.require_selected(&destination)?;
        self.guard_agent_paths(&[&destination])?;
        let committed = self.clean_tree(&[&source]).await?;
        let entries = subtree(&committed, &source);
        if entries.is_empty() {
            return Err(LocalError::Invalid(format!("{source} is not committed")));
        }
        if std::fs::symlink_metadata(self.root_path().join(display_to_path(&destination))).is_ok() {
            return Err(LocalError::Invalid(format!("{destination} already exists")));
        }
        let mut sources = HashMap::new();
        for (relative, node) in &entries {
            let path = join(&destination, relative);
            self.write_alternative_entry(&path, node, false)?;
            sources.insert(path, revision(node));
        }
        let message = message.map_or_else(
            || format!("Duplicate {source} as {destination}"),
            str::to_string,
        );
        self.commit_alternative(&destination, message, &sources)
            .await
    }

    /// Make `original` match `alternative`, keeping the original's identity.
    pub async fn adopt(
        &self,
        caller: &Path,
        alternative: &Path,
        original: &Path,
        message: Option<&str>,
    ) -> Result<CommitOutcome> {
        let alternative = normalize_input_path(self.root_path(), caller, alternative)?;
        let original = normalize_input_path(self.root_path(), caller, original)?;
        if path_in_scope(&alternative, &original) || path_in_scope(&original, &alternative) {
            return Err(LocalError::Invalid(
                "an alternative and its original cannot contain each other".into(),
            ));
        }
        self.require_selected(&alternative)?;
        self.require_selected(&original)?;
        self.guard_agent_paths(&[&original])?;
        let committed = self.clean_tree(&[&alternative, &original]).await?;
        let theirs = subtree(&committed, &alternative);
        let ours = subtree(&committed, &original);
        match (theirs.get(""), ours.get("")) {
            (Some(left), Some(right)) if left.kind == right.kind => {}
            (Some(_), Some(_)) => {
                return Err(LocalError::Invalid(
                    "an alternative and its original must both be files or both folders".into(),
                ));
            }
            _ => {
                return Err(LocalError::Invalid(format!(
                    "{alternative} and {original} must both be committed"
                )));
            }
        }

        // Remove what the alternative lacks, deepest first, then write what
        // it has, shallowest first.
        for (relative, node) in ours.iter().rev() {
            if theirs
                .get(relative)
                .is_some_and(|other| other.kind == node.kind)
            {
                continue;
            }
            let path = self
                .root_path()
                .join(display_to_path(&join(&original, relative)));
            match node.kind {
                NodeKind::File => std::fs::remove_file(&path).at("remove", &path)?,
                NodeKind::Folder => std::fs::remove_dir(&path).at("remove", &path)?,
            }
        }
        let mut sources = HashMap::new();
        for (relative, node) in &theirs {
            let path = join(&original, relative);
            let current = ours.get(relative).filter(|other| other.kind == node.kind);
            if current.is_some_and(|current| {
                node.kind == NodeKind::Folder || current.blob_id == node.blob_id
            }) {
                continue;
            }
            self.write_alternative_entry(&path, node, current.is_some())?;
            sources.insert(path, revision(node));
        }
        let message = message.map_or_else(
            || format!("Adopt {alternative} into {original}"),
            str::to_string,
        );
        self.commit_alternative(&original, message, &sources).await
    }

    fn write_alternative_entry(
        &self,
        path: &str,
        node: &CommittedNode,
        overwrite: bool,
    ) -> Result<()> {
        match node.kind {
            NodeKind::Folder => {
                let path = self.root_path().join(display_to_path(path));
                std::fs::create_dir(&path).at("create folder", &path)
            }
            NodeKind::File => self.write_working_file(
                path,
                node.blob_id.as_ref().expect("FILE has an object"),
                node.size.expect("FILE has a size"),
                overwrite,
            ),
        }
    }

    async fn commit_alternative(
        &self,
        path: &str,
        message: String,
        sources: &HashMap<String, ItemRevision>,
    ) -> Result<CommitOutcome> {
        let options = crate::portable::WriteOptions {
            message: Some(message),
            ..Default::default()
        };
        self.commit_selected_with(
            self.root_path(),
            &[PathBuf::from(path)],
            &options,
            None,
            sources,
        )
        .await
    }
}
