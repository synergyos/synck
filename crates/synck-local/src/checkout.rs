//! Optional filesystem view of a complete repository. Selection is never history.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use synck_vcs::types::ItemId;

use crate::commands::{WorktreeRestoreEntry, worktree_restore_manifest};
use crate::error::{LocalError, Result};
use crate::repository::{Config, Repository};
use crate::worktree::{CommittedNode, NodeKind, WorkingNode};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checkout {
    pub paths: Vec<String>,
    #[serde(default)]
    pub hidden_files: crate::repository::HiddenFilesMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Transition>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Transition {
    /// None means first materialization; Some([]) means an earlier full view.
    pub previous_paths: Option<Vec<String>>,
}

const CHECKOUT_FILE: &str = "checkout.json";

/// A repository with no checkout state has no working tree.
pub(crate) fn read_checkout(metadata: &Path) -> Result<Option<Checkout>> {
    let path = metadata.join(CHECKOUT_FILE);
    match crate::repository::open_regular_file_nofollow(&path) {
        Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
        Err(LocalError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn write_checkout(metadata: &Path, checkout: &Checkout) -> Result<()> {
    use crate::error::IoContext;
    let bytes = serde_json::to_vec(checkout)?;
    if bytes.len() > 64 * 1024 {
        return Err(LocalError::Invalid(
            "checkout selection exceeds config size limit".into(),
        ));
    }
    let path = metadata.join(format!("checkout-{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&path, &bytes).at("write checkout state", &path)?;
    std::fs::File::open(&path)
        .and_then(|file| file.sync_all())
        .at("sync checkout state", &path)?;
    crate::repository::replace_file(&path, &metadata.join(CHECKOUT_FILE))
        .at("publish checkout state", &path)?;
    crate::repository::sync_directory(metadata)
}

pub(crate) fn normalize_paths(paths: &[String]) -> Result<Vec<String>> {
    let mut normalized = BTreeSet::new();
    for path in paths {
        if path.is_empty()
            || path.contains('\\')
            || path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == ".." || p.eq_ignore_ascii_case(".synck"))
            || Path::new(path)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(LocalError::Invalid(format!(
                "invalid checkout subtree: {path:?}"
            )));
        }
        normalized.insert(path.clone());
    }
    Ok(normalized.into_iter().collect())
}

pub(crate) fn selected(paths: &[String], path: &str) -> bool {
    paths.is_empty()
        || paths.iter().any(|root| {
            path == root
                || path.strip_prefix(root).is_some_and(|s| s.starts_with('/'))
                || root.strip_prefix(path).is_some_and(|s| s.starts_with('/'))
        })
}

pub(crate) fn validate_config(config: &Config) -> Result<()> {
    let checkout = config
        .checkout
        .as_ref()
        .ok_or_else(|| LocalError::Invalid("complete repository has no checkout".into()))?;
    if normalize_paths(&checkout.paths)? != checkout.paths {
        return Err(LocalError::Integrity(
            "checkout paths are not canonical".into(),
        ));
    }
    if let Some(transition) = &checkout.pending {
        if let Some(paths) = &transition.previous_paths {
            normalize_paths(paths)?;
        }
    }
    Ok(())
}

impl Repository {
    /// Warn about selected paths that legacy Windows applications may reject.
    pub async fn checkout_path_warnings(&self) -> Result<Vec<std::path::PathBuf>> {
        #[cfg(windows)]
        {
            let tree = self.complete_file_tree().await?;
            Ok(crate::path_warnings::long_paths(self.root_path(), tree.keys()
                .filter(|path| selected(self.checkout_paths().unwrap_or(&[]), path))
                .map(String::as_str)))
        }
        #[cfg(not(windows))]
        Ok(Vec::new())
    }

    pub fn checkout_paths(&self) -> Option<&[String]> {
        self.config().checkout.as_ref().map(|c| c.paths.as_slice())
    }

    pub(crate) fn checkout_observes(&self, path: &str) -> bool {
        let Some(checkout) = &self.config().checkout else {
            return true;
        };
        selected(&checkout.paths, path)
            || checkout
                .pending
                .as_ref()
                .and_then(|p| p.previous_paths.as_ref())
                .is_some_and(|paths| selected(paths, path))
    }

    pub(crate) fn filter_checkout_observation(&self, working: &mut BTreeMap<String, WorkingNode>) {
        if self.checkout_paths().is_some() {
            working.retain(|path, _| self.checkout_observes(path));
        }
    }

    pub(crate) async fn validate_checkout_selection(&self) -> Result<()> {
        let paths = self
            .checkout_paths()
            .ok_or_else(|| LocalError::Invalid("not a complete checkout".into()))?;
        let tree = self.complete_file_tree().await?;
        for path in paths {
            if !tree.contains_key(path) {
                return Err(LocalError::Invalid(format!(
                    "checkout subtree is not a live filesystem item: {path}"
                )));
            }
        }
        self.preflight_checkout_paths(&tree)?;
        let connection = self
            .storage()
            .open_readonly()
            .map_err(|e| LocalError::Integrity(e.to_string()))?;
        for (path, node) in tree.iter().filter(|(path, _)| selected(paths, path)) {
            let mode: String = connection.query_row(
                "SELECT sequencing_mode FROM vcs_items WHERE repo_id=?1 AND item_id=?2",
                rusqlite::params![self.repository_id().as_str(), node.item_id.as_str()],
                |r| r.get(0),
            )?;
            if mode != "LOCAL" {
                return Err(LocalError::Invalid(format!(
                    "externally sequenced item cannot be checked out for editing: {path}"
                )));
            }
        }
        Ok(())
    }

    fn preflight_checkout_paths(&self, tree: &BTreeMap<String, CommittedNode>) -> Result<()> {
        use crate::error::IoContext;
        let stage = self
            .metadata_path()
            .join(format!("checkout-preflight-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&stage).at("create checkout preflight", &stage)?;
        let result = (|| {
            for (path, node) in tree
                .iter()
                .filter(|(p, _)| selected(self.checkout_paths().unwrap_or(&[]), p))
            {
                normalize_paths(std::slice::from_ref(path))?;
                if path.split('/').any(|part| part.starts_with(".synck-tmp-")) {
                    return Err(LocalError::Invalid(format!(
                        "reserved checkout path: {path}"
                    )));
                }
                let destination = stage.join(path);
                if node.kind == NodeKind::Folder {
                    std::fs::create_dir(&destination)
                        .at("preflight checkout directory", &destination)?;
                } else {
                    std::fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&destination)
                        .at("preflight checkout file", &destination)?;
                }
            }
            Ok(())
        })();
        crate::repository::remove_tree_nofollow(&stage)?;
        result
    }

    /// Change literal subtree selection only after the present view is clean.
    /// A durable source manifest guards the existing restore journal.
    pub async fn set_checkout_paths(&mut self, paths: &[String]) -> Result<()> {
        self.guard_agent_all()?;
        self.finish_checkout_transition().await?;
        let paths = normalize_paths(paths)?;
        let previous = self
            .checkout_paths()
            .ok_or_else(|| LocalError::Invalid("not a complete checkout".into()))?
            .to_vec();
        if previous == paths {
            return Ok(());
        }
        if !self.status().await?.is_empty() {
            return Err(LocalError::Invalid(
                "commit or restore changes before changing checkout selection".into(),
            ));
        }
        let all = self.complete_file_tree().await?;
        for path in &paths {
            if !all.contains_key(path) {
                return Err(LocalError::Invalid(format!(
                    "checkout subtree is not a live filesystem item: {path}"
                )));
            }
        }
        // Inspect every newly selected path before changing persistent selection.
        // Unknown entries, symlinks and modified bytes are never overwritten.
        let mut source = crate::worktree::scan_canonical_without_ignore(self.root(), &all)?;
        let previous_tree = all
            .iter()
            .filter(|(p, _)| selected(&previous, p))
            .map(|(p, n)| (p.clone(), n.clone()))
            .collect::<BTreeMap<_, _>>();
        let previous_working = source
            .iter()
            .filter(|(p, _)| selected(&previous, p))
            .map(|(p, n)| (p.clone(), n.clone()))
            .collect::<BTreeMap<_, _>>();
        crate::tree_materialization::validate_materialized_tree(&previous_tree, &previous_working)?;
        let desired = all
            .iter()
            .filter(|(p, _)| selected(&paths, p))
            .map(|(p, n)| (p.clone(), n.clone()))
            .collect::<BTreeMap<_, _>>();
        for (path, observed) in &source {
            if !selected(&paths, path) {
                continue;
            }
            let Some(node) = desired.get(path) else {
                return Err(LocalError::Invalid(format!(
                    "untracked path blocks checkout selection: {path}"
                )));
            };
            if node.kind != observed.kind
                || (node.kind == NodeKind::File
                    && (node.plain_digest.as_deref()
                        != observed.digest_hex.as_deref()
                        || node.size.and_then(|s| u64::try_from(s).ok()) != observed.size))
            {
                return Err(LocalError::Invalid(format!(
                    "dirty path blocks checkout selection: {path}"
                )));
            }
        }
        source.retain(|p, _| selected(&previous, p) || selected(&paths, p));
        let source = worktree_restore_manifest(&source, &HashMap::new())?;
        let old_config = self.config().clone();
        let checkout = self.config_mut().checkout.as_mut().expect("checked");
        checkout.paths = paths;
        checkout.pending = Some(Transition {
            previous_paths: Some(previous),
        });
        if let Err(error) = self.validate_checkout_selection().await {
            *self.config_mut() = old_config;
            return Err(error);
        }
        if let Err(error) = self.write_checkout_source(&source) {
            *self.config_mut() = old_config;
            return Err(error);
        }
        self.publish_checkout_config()?;
        self.finish_checkout_transition().await
    }

    pub(crate) async fn finish_checkout_transition(&mut self) -> Result<()> {
        let Some(pending) = self
            .config()
            .checkout
            .as_ref()
            .and_then(|c| c.pending.clone())
        else {
            return Ok(());
        };
        let _ = pending;
        let source = crate::repository::open_regular_file_nofollow(
            &self.metadata_path().join("checkout-source.json"),
        )?;
        let source: Vec<WorktreeRestoreEntry> = serde_json::from_reader(source)?;
        self.restore_checkout_worktree(&source).await?;
        self.config_mut()
            .checkout
            .as_mut()
            .expect("checkout transition")
            .pending = None;
        self.publish_checkout_config()?;
        std::fs::remove_file(self.metadata_path().join("checkout-source.json"))
            .map_err(|e| LocalError::Invalid(e.to_string()))?;
        crate::repository::sync_directory(self.metadata_path())
    }

    pub(crate) fn write_checkout_source(&self, source: &[WorktreeRestoreEntry]) -> Result<()> {
        use std::io::Write;
        let target = self.metadata_path().join("checkout-source.json");
        let temp = self
            .metadata_path()
            .join(format!("checkout-source-{}.json", uuid::Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|e| LocalError::Invalid(e.to_string()))?;
        file.write_all(&serde_json::to_vec(source)?)
            .map_err(|e| LocalError::Invalid(e.to_string()))?;
        file.sync_all()
            .map_err(|e| LocalError::Invalid(e.to_string()))?;
        crate::repository::replace_file(&temp, &target)
            .map_err(|e| LocalError::Invalid(e.to_string()))?;
        crate::repository::sync_directory(self.metadata_path())
    }

    pub(crate) async fn complete_file_tree(&self) -> Result<BTreeMap<String, CommittedNode>> {
        // Use the ordinary tree builder with a flag, avoiding recursion through
        // the selection filter. Non-file families remain solely canonical data.
        self.build_committed_tree(false).await
    }

    pub(crate) fn checkout_visible_item_ids(
        &self,
        connection: &rusqlite::Connection,
    ) -> Result<BTreeSet<ItemId>> {
        // This is used solely for rejecting lifecycle closures that include
        // omitted items. Namespace paths follow canonical parents.
        let mut statement = connection.prepare("SELECT item_id,parent_folder_id,name,item_type FROM vcs_items WHERE repo_id=?1 AND lifecycle_state='LIVE' AND name IS NOT NULL")?;
        let rows = statement.query_map([self.repository_id().as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        let by_id = items
            .iter()
            .map(|r| (r.0.as_str(), r))
            .collect::<HashMap<_, _>>();
        let mut visible = BTreeSet::new();
        for (id, _, _, kind) in &items {
            if kind != "FILE" && kind != "FOLDER" {
                continue;
            }
            let mut cursor = id.as_str();
            let mut components = Vec::new();
            let mut visited = BTreeSet::new();
            let mut supported = true;
            loop {
                if !visited.insert(cursor) {
                    return Err(LocalError::Integrity("namespace cycle".into()));
                }
                let Some(item) = by_id.get(cursor) else {
                    supported = false;
                    break;
                };
                if item.3 != "FILE" && item.3 != "FOLDER" {
                    supported = false;
                    break;
                }
                components.push(item.2.as_str());
                let Some(parent) = item.1.as_deref() else {
                    break;
                };
                cursor = parent;
            }
            components.reverse();
            if supported && selected(self.checkout_paths().unwrap_or(&[]), &components.join("/")) {
                visible
                    .insert(ItemId::parse(id).map_err(|e| LocalError::Integrity(e.to_string()))?);
            }
        }
        Ok(visible)
    }
}
