use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use synck_vcs::state::{NamespaceKey, RootHeader, StateIndexStore, StateRootId, StateTree};
use synck_vcs::types::{ObjectId, CommitId, ItemId, LifecycleState, RepoId, Sha256Digest};
use synck_vcs_sqlite::parse_timestamp;
use uuid::Uuid;

use crate::commands::{LogEntry, checked_commit_event_count};
use crate::error::{IoContext, LocalError, Result};
use crate::repository::{
    Repository, is_link_or_reparse, open_regular_file_nofollow, remove_tree_nofollow,
    sync_directory,
};
use crate::worktree::NodeKind;

const MAX_LOG_PAGE_SIZE: usize = 1_000;
const HISTORICAL_PAGE_SIZE: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryInfo {
    pub root: PathBuf,
    pub format_version: u32,
    pub repo_id: String,
    pub author: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackedItemKind {
    File,
    Folder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackedItem {
    pub item_id: String,
    pub path: String,
    pub kind: TrackedItemKind,
    pub item_seq: i64,
    pub content_item_seq: Option<i64>,
    pub size: Option<i64>,
    pub modified_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct LogPage {
    pub entries: Vec<LogEntry>,
    pub next_cursor: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoricalItemKind {
    File,
    Folder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoricalItem {
    pub item_id: String,
    pub name: String,
    pub kind: HistoricalItemKind,
    pub item_seq: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoricalCursor {
    root_id: StateRootId,
    key: NamespaceKey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoricalPage {
    pub entries: Vec<HistoricalItem>,
    pub next_cursor: Option<HistoricalCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoricalView {
    pub repo_seq: i64,
    pub commit_id: String,
    pub state_root: String,
    repo_id: RepoId,
    root_id: StateRootId,
    root: RootHeader,
}

#[derive(Debug)]
pub struct HistoricalFile {
    pub path: PathBuf,
    cache_root: PathBuf,
    publication: String,
    manifest: HistoricalFileManifest,
    lease: Option<fs::File>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct HistoricalFileManifest {
    version: u32,
    publication: String,
    repo_id: String,
    repo_seq: i64,
    commit_id: String,
    state_root: String,
    item_id: String,
    blob_id: String,
    file_name: String,
}

const HISTORICAL_FILE_MANIFEST: &str = "manifest.json";
const HISTORICAL_FILE_LEASE: &str = "lease";
const HISTORICAL_FILE_CONTENT: &str = "content";

impl Repository {
    pub fn info(&self) -> RepositoryInfo {
        RepositoryInfo {
            root: self.root_path().to_path_buf(),
            format_version: self.config().format_version(),
            repo_id: self.config().repo_id().to_string(),
            author: self.config().author().to_string(),
        }
    }

    pub async fn tracked_items(&self) -> Result<Vec<TrackedItem>> {
        self.recover().await?;
        let connection = crate::local_database::open_readonly(self.storage()).map_err(|error| {
            LocalError::Invalid(format!("cannot read repository database: {error}"))
        })?;
        let mut statement = connection.prepare(
            "SELECT i.item_id, e.created_at, h.item_seq
             FROM vcs_items i
             JOIN vcs_events e
               ON e.repo_id = i.repo_id
              AND e.item_id = i.item_id
              AND e.item_seq = i.head_seq
             LEFT JOIN local_file_heads h
               ON h.repo_id = i.repo_id
              AND h.item_id = i.item_id
             WHERE i.repo_id = ?1 AND i.lifecycle_state = 'LIVE'",
        )?;
        let rows = statement.query_map(params![self.config().repo_id().as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?;
        let mut metadata = HashMap::new();
        for row in rows {
            let (item_id, modified_at, content_item_seq) = row?;
            let modified_at = parse_timestamp(&modified_at).map_err(|error| {
                LocalError::Integrity(format!(
                    "item {item_id} has an invalid head-event timestamp: {error}"
                ))
            })?;
            metadata.insert(item_id, (content_item_seq, modified_at));
        }
        drop(statement);
        drop(connection);

        let mut items = Vec::new();
        for (path, node) in self.committed_tree().await? {
            let item_id = node.item_id.to_string();
            let (content_item_seq, modified_at) = metadata.remove(&item_id).ok_or_else(|| {
                LocalError::Integrity(format!("live item {item_id} has no head-event metadata"))
            })?;
            items.push(TrackedItem {
                item_id,
                path,
                kind: match node.kind {
                    NodeKind::File => TrackedItemKind::File,
                    NodeKind::Folder => TrackedItemKind::Folder,
                },
                item_seq: node.head_seq,
                content_item_seq,
                size: node.size,
                modified_at,
            });
        }
        Ok(items)
    }

    pub async fn open_historical_view(&self, revision: &str) -> Result<HistoricalView> {
        self.recover().await?;
        let commit = self.resolve_revision(revision).await?;
        let repo_seq = commit.repo_seq.ok_or_else(|| {
            LocalError::Integrity(format!(
                "commit {} has no repository sequence",
                commit.commit_id
            ))
        })?;
        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        let (root_id, root) = self
            .historical_root_at_commit(&mut store, repo_seq, &commit.commit_id)
            .await?;
        Ok(HistoricalView {
            repo_seq,
            commit_id: commit.commit_id.to_string(),
            state_root: root_id.to_string(),
            repo_id: self.config().repo_id().clone(),
            root_id,
            root,
        })
    }

    pub async fn historical_children(
        &self,
        view: &HistoricalView,
        parent_item_id: Option<&str>,
        cursor: Option<&HistoricalCursor>,
    ) -> Result<HistoricalPage> {
        self.recover().await?;
        let mut store = self.open_historical_view_store(view).await?;
        let parent = parent_item_id
            .map(|item_id| {
                ItemId::parse(item_id).map_err(|error| {
                    LocalError::Invalid(format!("invalid historical parent item ID: {error}"))
                })
            })
            .transpose()?;
        let mut state = StateTree::new(&mut store);
        if cursor.is_some_and(|cursor| cursor.root_id != view.root_id) {
            return Err(LocalError::Invalid(
                "historical cursor belongs to another state root".to_string(),
            ));
        }
        if let Some(parent) = parent.as_ref() {
            let item = state
                .get_item(view.root.item_map_root, parent)
                .await
                .map_err(|error| LocalError::Integrity(error.to_string()))?
                .ok_or_else(|| {
                    LocalError::Invalid(format!("historical folder does not exist: {parent}"))
                })?;
            if item.lifecycle_state != LifecycleState::Live || item.item_type != "FOLDER" {
                return Err(LocalError::Invalid(format!(
                    "historical item is not a live folder: {parent}"
                )));
            }
        }
        let mut entries = Vec::with_capacity(HISTORICAL_PAGE_SIZE);
        let continuation_cursor;
        let cursor_key = cursor.map(|cursor| cursor.key.clone());

        if self.checkout_paths().is_some() {
            // A complete-repository checkout browses the same filesystem view
            // as its working tree: files and folders only. Other item families
            // remain canonical repository data and are read through items.
            continuation_cursor = scan_visible_children(
                &mut state,
                view,
                parent.as_ref(),
                cursor_key,
                &mut entries,
                |item| !matches!(item.item_type.as_str(), "FILE" | "FOLDER"),
            )
            .await?;
        } else {
            let page = state
                .child_page(
                    view.root.item_map_root,
                    view.root.namespace_root,
                    parent.as_ref(),
                    cursor_key.as_ref(),
                )
                .await
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
            append_historical_items(&mut entries, page.entries)?;
            continuation_cursor = page.next_cursor;
        }
        Ok(HistoricalPage {
            entries,
            next_cursor: continuation_cursor.map(|key| HistoricalCursor {
                root_id: view.root_id,
                key,
            }),
        })
    }

    pub async fn materialize_historical_file(
        &self,
        view: &HistoricalView,
        item_id: &str,
        cache_root: &Path,
    ) -> Result<HistoricalFile> {
        self.recover().await?;
        let mut store = self.open_historical_view_store(view).await?;
        let item_id = ItemId::parse(item_id).map_err(|error| {
            LocalError::Invalid(format!("invalid historical file item ID: {error}"))
        })?;
        let item = StateTree::new(&mut store)
            .get_item(view.root.item_map_root, &item_id)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?
            .ok_or_else(|| {
                LocalError::Invalid(format!("historical file does not exist: {item_id}"))
            })?;
        if item.lifecycle_state != LifecycleState::Live || item.item_type != "FILE" {
            return Err(LocalError::Invalid(format!(
                "historical item is not a live file: {item_id}"
            )));
        }
        drop(store);
        let file_name = item.name.clone().ok_or_else(|| {
            LocalError::Integrity(format!("historical FILE {item_id} has no name"))
        })?;
        let mut items = BTreeMap::new();
        items.insert(item_id.clone(), item);
        let (blob_id, size) = self
            .historical_file_contents(&items, true)?
            .remove(&item_id)
            .ok_or_else(|| {
                LocalError::Integrity(format!(
                    "historical FILE {item_id} has no authenticated content"
                ))
            })?;
        let cache_root = prepare_historical_cache(cache_root)?;
        let root = Dir::open_ambient_dir(&cache_root, ambient_authority())
            .at("open historical cache root", &cache_root)?;
        let publication = Uuid::new_v4().to_string();
        root.create_dir(&publication).at(
            "create historical file publication",
            cache_root.join(&publication),
        )?;
        let publication_path = cache_root.join(&publication);
        let result = (|| -> Result<HistoricalFile> {
            let publication_dir = root
                .open_dir_nofollow(&publication)
                .at("open historical file publication", &publication_path)?;
            let manifest = HistoricalFileManifest {
                version: 1,
                publication: publication.clone(),
                repo_id: self.config().repo_id().to_string(),
                repo_seq: view.repo_seq,
                commit_id: view.commit_id.clone(),
                state_root: view.state_root.clone(),
                item_id: item_id.to_string(),
                blob_id: blob_id.to_string(),
                file_name: file_name.clone(),
            };
            let mut lease_options = CapOpenOptions::new();
            lease_options
                .create_new(true)
                .write(true)
                .follow(FollowSymlinks::No);
            let lease_path = publication_path.join(HISTORICAL_FILE_LEASE);
            let mut lease = publication_dir
                .open_with(HISTORICAL_FILE_LEASE, &lease_options)
                .at("create historical file lease", &lease_path)?
                .into_std();
            lease
                .write_all(b"1")
                .at("write historical file lease", &lease_path)?;
            lease
                .sync_all()
                .at("sync historical file lease", &lease_path)?;
            if !try_lock_historical_lease(&lease)? {
                return Err(LocalError::Integrity(
                    "new historical file lease is already locked".to_string(),
                ));
            }
            write_historical_manifest(&publication_dir, &publication_path, &manifest)?;
            publication_dir.create_dir(HISTORICAL_FILE_CONTENT).at(
                "create historical file content directory",
                &publication_path,
            )?;
            let content_dir = publication_dir
                .open_dir_nofollow(HISTORICAL_FILE_CONTENT)
                .at("open historical file content directory", &publication_path)?;
            let mut options = CapOpenOptions::new();
            options
                .create_new(true)
                .write(true)
                .follow(FollowSymlinks::No);
            let file_path = publication_path
                .join(HISTORICAL_FILE_CONTENT)
                .join(&file_name);
            let mut file = content_dir
                .open_with(&file_name, &options)
                .at("create historical file", &file_path)?;
            let size = u64::try_from(size).map_err(|_| {
                LocalError::Integrity(format!("negative object size for {blob_id}"))
            })?;
            self.copy_blob(&blob_id, size, &mut file)?;
            file.sync_all().at("sync historical file", &file_path)?;
            sync_directory(&publication_path.join(HISTORICAL_FILE_CONTENT))?;
            sync_directory(&publication_path)?;
            sync_directory(&cache_root)?;
            Ok(HistoricalFile {
                path: file_path,
                cache_root: cache_root.clone(),
                publication,
                manifest,
                lease: Some(lease),
            })
        })();
        if result.is_err() {
            let _ = remove_tree_nofollow(&publication_path);
            let _ = sync_directory(&cache_root);
        }
        result
    }

    async fn open_historical_view_store(
        &self,
        view: &HistoricalView,
    ) -> Result<synck_vcs_sqlite::SqliteStateStore> {
        if view.repo_id != *self.config().repo_id() {
            return Err(LocalError::Invalid(
                "historical view belongs to another repository".to_string(),
            ));
        }
        let mut store = self
            .storage()
            .state_index_store(self.config().repo_id().clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        let root = StateTree::new(&mut store)
            .read_root(view.root_id)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        if root != view.root {
            return Err(LocalError::Integrity(
                "historical view state root changed".to_string(),
            ));
        }
        Ok(store)
    }

    /// Returns commits newest first. `before_repo_seq` is an exclusive cursor.
    pub async fn log_page(&self, before_repo_seq: Option<i64>, limit: usize) -> Result<LogPage> {
        self.recover().await?;
        if limit == 0 || limit > MAX_LOG_PAGE_SIZE {
            return Err(LocalError::Invalid(format!(
                "log page size must be between 1 and {MAX_LOG_PAGE_SIZE}"
            )));
        }

        let connection = self.storage().open_readonly().map_err(|error| {
            LocalError::Invalid(format!("cannot read repository database: {error}"))
        })?;
        let repo_id = self.config().repo_id().as_str();
        let maximum: Option<i64> = connection.query_row(
            "SELECT max(repo_seq) FROM vcs_commits
             WHERE repo_id = ?1 AND (?2 IS NULL OR repo_seq < ?2)",
            params![repo_id, before_repo_seq],
            |row| row.get(0),
        )?;
        let Some(maximum) = maximum else {
            return Ok(LogPage {
                entries: Vec::new(),
                next_cursor: None,
            });
        };

        let offset = i64::try_from(limit - 1).expect("validated page size fits SQLite INTEGER");
        let start = connection
            .query_row(
                "SELECT repo_seq FROM vcs_commits
                 WHERE repo_id = ?1 AND repo_seq <= ?2
                 ORDER BY repo_seq DESC LIMIT 1 OFFSET ?3",
                params![repo_id, maximum, offset],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(1);
        let has_older: bool = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM vcs_commits
                 WHERE repo_id = ?1 AND repo_seq < ?2
             )",
            params![repo_id, start],
            |row| row.get(0),
        )?;

        let mut commits = self
            .core()
            .list_commits_by_repo_seq(self.config().repo_id().clone(), start, Some(maximum), limit)
            .await?;
        commits.reverse();
        let entries = commits
            .into_iter()
            .map(|commit| {
                Ok(LogEntry {
                    repo_seq: commit.repo_seq.expect("sequenced query returns repo_seq"),
                    commit_id: commit.commit_id.to_string(),
                    created_at: commit.created_at,
                    author: commit.author_id,
                    message: commit.message,
                    event_count: checked_commit_event_count(commit.event_count, &commit.commit_id)?,
                    events_digest: commit.events_digest.to_string(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(LogPage {
            entries,
            next_cursor: has_older.then_some(start),
        })
    }
}

/// Fill `entries` with the next visible children after `cursor`, skipping
/// hidden items without ever returning an empty or underfilled page while
/// visible children remain. Returns the continuation key.
async fn scan_visible_children<S: StateIndexStore>(
    state: &mut StateTree<'_, S>,
    view: &HistoricalView,
    parent: Option<&ItemId>,
    cursor: Option<NamespaceKey>,
    entries: &mut Vec<HistoricalItem>,
    hidden: impl Fn(&synck_vcs::state::ItemValue) -> bool,
) -> Result<Option<NamespaceKey>> {
    let mut continuation_cursor = None;
    let mut scan_cursor = cursor;
    'pages: loop {
        let page = state
            .child_page(
                view.root.item_map_root,
                view.root.namespace_root,
                parent,
                scan_cursor.as_ref(),
            )
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        for (key, item) in page.entries {
            if hidden(&item) {
                if entries.len() == HISTORICAL_PAGE_SIZE {
                    continuation_cursor = Some(key);
                }
                continue;
            }
            if entries.len() == HISTORICAL_PAGE_SIZE {
                break 'pages;
            }
            entries.push(historical_item(item)?);
            if entries.len() == HISTORICAL_PAGE_SIZE {
                continuation_cursor = Some(key);
            }
        }
        let Some(next_cursor) = page.next_cursor else {
            return Ok(None);
        };
        scan_cursor = Some(next_cursor);
    }
    Ok(continuation_cursor)
}

fn append_historical_items(
    output: &mut Vec<HistoricalItem>,
    items: Vec<(NamespaceKey, synck_vcs::state::ItemValue)>,
) -> Result<()> {
    for (_, item) in items {
        output.push(historical_item(item)?);
    }
    Ok(())
}

fn historical_item(item: synck_vcs::state::ItemValue) -> Result<HistoricalItem> {
    let kind = match item.item_type.as_str() {
        "FILE" => HistoricalItemKind::File,
        "FOLDER" => HistoricalItemKind::Folder,
        other => {
            return Err(LocalError::Integrity(format!(
                "unsupported historical item type {other} for {}",
                item.item_id
            )));
        }
    };
    let name = item.name.ok_or_else(|| {
        LocalError::Integrity(format!("historical item {} has no name", item.item_id))
    })?;
    Ok(HistoricalItem {
        item_id: item.item_id.to_string(),
        name,
        kind,
        item_seq: item.head_seq,
    })
}

impl HistoricalFile {
    pub fn cleanup(&mut self) -> Result<bool> {
        self.lease.take();
        remove_historical_publication(&self.cache_root, &self.publication, Some(&self.manifest))
    }
}

impl Drop for HistoricalFile {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

pub fn cleanup_historical_cache(cache_root: &Path) -> Result<usize> {
    let cache_root = match fs::symlink_metadata(cache_root) {
        Ok(metadata) => {
            if !metadata.is_dir() || is_link_or_reparse(&metadata) {
                return Err(LocalError::Invalid(format!(
                    "historical cache root is not a real directory: {}",
                    cache_root.display()
                )));
            }
            cache_root
                .canonicalize()
                .at("resolve historical cache root", cache_root)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect historical cache root",
                path: cache_root.to_path_buf(),
                source,
            });
        }
    };
    let mut removed = 0;
    for entry in fs::read_dir(&cache_root).at("read historical cache", &cache_root)? {
        let entry = entry.at("read historical cache entry", &cache_root)?;
        let Some(publication) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if remove_historical_publication(&cache_root, &publication, None)? {
            removed += 1;
        }
    }
    Ok(removed)
}

fn prepare_historical_cache(cache_root: &Path) -> Result<PathBuf> {
    if !cache_root.is_absolute() {
        return Err(LocalError::Invalid(
            "historical cache root must be absolute".to_string(),
        ));
    }
    let missing = cache_root
        .ancestors()
        .take_while(|path| !path.exists())
        .collect::<Vec<_>>();
    for path in missing.into_iter().rev() {
        fs::create_dir(path).at("create historical cache directory", path)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
    }
    let metadata =
        fs::symlink_metadata(cache_root).at("inspect historical cache root", cache_root)?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Err(LocalError::Invalid(format!(
            "historical cache root is not a real directory: {}",
            cache_root.display()
        )));
    }
    cache_root
        .canonicalize()
        .at("resolve historical cache root", cache_root)
}

fn write_historical_manifest(
    directory: &Dir,
    publication_path: &Path,
    manifest: &HistoricalFileManifest,
) -> Result<()> {
    let manifest_path = publication_path.join(HISTORICAL_FILE_MANIFEST);
    let mut options = CapOpenOptions::new();
    options
        .create_new(true)
        .write(true)
        .follow(FollowSymlinks::No);
    let mut file = directory
        .open_with(HISTORICAL_FILE_MANIFEST, &options)
        .at("create historical file manifest", &manifest_path)?;
    serde_json::to_writer(&mut file, manifest)?;
    file.write_all(b"\n")
        .at("write historical file manifest", &manifest_path)?;
    file.sync_all()
        .at("sync historical file manifest", &manifest_path)
}

fn remove_historical_publication(
    cache_root: &Path,
    publication: &str,
    expected: Option<&HistoricalFileManifest>,
) -> Result<bool> {
    if publication.is_empty()
        || Path::new(publication).components().count() != 1
        || Path::new(publication).is_absolute()
    {
        return Err(LocalError::Invalid(
            "invalid historical cache publication name".to_string(),
        ));
    }
    let cache_root = cache_root
        .canonicalize()
        .at("resolve historical cache root", cache_root)?;
    let publication_path = cache_root.join(publication);
    let metadata = match fs::symlink_metadata(&publication_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect historical cache publication",
                path: publication_path,
                source,
            });
        }
    };
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Ok(false);
    }
    let canonical = publication_path
        .canonicalize()
        .at("resolve historical cache publication", &publication_path)?;
    if canonical.parent() != Some(cache_root.as_path()) {
        return Ok(false);
    }
    let manifest_path = canonical.join(HISTORICAL_FILE_MANIFEST);
    let manifest_metadata = match fs::symlink_metadata(&manifest_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect historical file manifest",
                path: manifest_path,
                source,
            });
        }
    };
    if !manifest_metadata.is_file() || is_link_or_reparse(&manifest_metadata) {
        return Ok(false);
    }
    let manifest: HistoricalFileManifest = match serde_json::from_slice(
        &fs::read(&manifest_path).at("read historical file manifest", &manifest_path)?,
    ) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    if !valid_historical_manifest(&manifest)
        || manifest.publication != publication
        || expected.is_some_and(|expected| expected != &manifest)
    {
        return Ok(false);
    }
    if expected.is_none() {
        let lease_path = canonical.join(HISTORICAL_FILE_LEASE);
        let lease = match open_regular_file_nofollow(&lease_path) {
            Ok(lease) => lease,
            Err(LocalError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        if !try_lock_historical_lease(&lease)? {
            return Ok(false);
        }
        drop(lease);
    }
    match remove_tree_nofollow(&canonical) {
        Ok(()) => {
            sync_directory(&cache_root)?;
            Ok(true)
        }
        Err(LocalError::Io { source, .. }) if cleanup_is_blocked(&source) => Ok(false),
        Err(error) => Err(error),
    }
}

fn try_lock_historical_lease(file: &fs::File) -> Result<bool> {
    #[cfg(unix)]
    {
        use rustix::fs::{FlockOperation, flock};
        match flock(file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(true),
            Err(error) if error == rustix::io::Errno::WOULDBLOCK => Ok(false),
            Err(error) => Err(LocalError::Io {
                action: "lock historical file lease",
                path: PathBuf::from(HISTORICAL_FILE_LEASE),
                source: std::io::Error::from(error),
            }),
        }
    }
    #[cfg(windows)]
    {
        use std::mem::zeroed;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, GetLastError};
        use windows_sys::Win32::Storage::FileSystem::{
            LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
        };
        use windows_sys::Win32::System::IO::OVERLAPPED;

        // SAFETY: an all-zero OVERLAPPED selects a synchronous byte-range lock.
        let mut overlapped: OVERLAPPED = unsafe { zeroed() };
        // SAFETY: the file owns a valid handle and `overlapped` remains alive
        // for this synchronous, nonblocking call.
        let locked = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        };
        if locked != 0 {
            return Ok(true);
        }
        // SAFETY: this immediately reads the calling thread's error from the
        // failed LockFileEx call above.
        let error = unsafe { GetLastError() };
        if error == ERROR_LOCK_VIOLATION {
            return Ok(false);
        }
        Err(LocalError::Io {
            action: "lock historical file lease",
            path: PathBuf::from(HISTORICAL_FILE_LEASE),
            source: std::io::Error::from_raw_os_error(error as i32),
        })
    }
    #[cfg(not(any(unix, windows)))]
    Err(LocalError::Invalid(
        "historical file leases are unsupported on this platform".to_string(),
    ))
}

fn valid_historical_manifest(manifest: &HistoricalFileManifest) -> bool {
    manifest.version == 1
        && manifest.repo_seq > 0
        && Uuid::parse_str(&manifest.publication)
            .ok()
            .is_some_and(|id| id.get_version_num() == 4)
        && RepoId::parse(&manifest.repo_id).is_ok()
        && CommitId::parse(&manifest.commit_id).is_ok()
        && Sha256Digest::parse(&manifest.state_root).is_ok()
        && ItemId::parse(&manifest.item_id).is_ok()
        && ObjectId::parse(&manifest.blob_id).is_ok()
        && matches!(
            Path::new(&manifest.file_name)
                .components()
                .collect::<Vec<_>>()
                .as_slice(),
            [std::path::Component::Normal(_)]
        )
}

fn cleanup_is_blocked(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(windows)]
    {
        return matches!(error.raw_os_error(), Some(32 | 33));
    }
    #[cfg(not(windows))]
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_cleanup_removes_an_unlocked_owned_publication() {
        let temporary = tempfile::tempdir().unwrap();
        let cache = temporary.path().join("cache");
        fs::create_dir(&cache).unwrap();
        let publication = Uuid::new_v4().to_string();
        let publication_path = cache.join(&publication);
        fs::create_dir(&publication_path).unwrap();
        fs::write(publication_path.join(HISTORICAL_FILE_LEASE), b"1").unwrap();
        let manifest = HistoricalFileManifest {
            version: 1,
            publication: publication.clone(),
            repo_id: RepoId::new().to_string(),
            repo_seq: 1,
            commit_id: format!("sha256:{}", "11".repeat(32)),
            state_root: format!("sha256:{}", "22".repeat(32)),
            item_id: ItemId::new().to_string(),
            blob_id: format!("sha256:{}", "33".repeat(32)),
            file_name: "document.bin".to_string(),
        };
        fs::write(
            publication_path.join(HISTORICAL_FILE_MANIFEST),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        assert_eq!(cleanup_historical_cache(&cache).unwrap(), 1);
        assert!(!publication_path.exists());
    }
}
