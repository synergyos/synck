use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use synck_vcs::naming::compute_name_key;
use synck_vcs::types::{ObjectId, ItemCursor, ItemId, ItemRow, LifecycleState};
use synck_vcs_sqlite::SqliteStorage;
use uuid::Uuid;

use crate::error::{IoContext, LocalError, Result};
use crate::repository::{HiddenFilesMode, Repository, is_cap_link_or_reparse};

const PAGE_SIZE: usize = 1_024;

#[cfg(test)]
static FULL_WORKTREE_RECONCILIATIONS: LazyLock<Mutex<Vec<String>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
#[cfg(test)]
static EXACT_IDENTITY_ROWS: LazyLock<Mutex<Vec<(String, usize)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NodeKind {
    File,
    Folder,
}

impl NodeKind {
    pub(crate) fn as_item_type(self) -> &'static str {
        match self {
            Self::File => "FILE",
            Self::Folder => "FOLDER",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedNode {
    pub item_id: ItemId,
    pub kind: NodeKind,
    pub head_seq: i64,
    pub blob_id: Option<ObjectId>,
    pub plain_digest: Option<String>,
    pub size: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingNode {
    pub kind: NodeKind,
    pub digest_hex: Option<String>,
    pub size: Option<u64>,
    pub identity: FileIdentity,
    pub(crate) fingerprint: Option<FileFingerprint>,
    pub(crate) stat_valid: bool,
    pub(crate) native_hidden: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct FileIdentity {
    pub device_id: u64,
    pub file_id: u64,
    pub(crate) incarnation: Option<FileTime>,
}

impl FileIdentity {
    pub(crate) fn durable_parts(self) -> (u64, u64, Option<FileTime>) {
        (self.device_id, self.file_id, self.incarnation)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct FileTime {
    seconds: i64,
    nanos: i64,
}

impl FileTime {
    pub(crate) fn is_valid(self) -> bool {
        (0..1_000_000_000).contains(&self.nanos)
    }

    pub(crate) fn seconds(self) -> i64 {
        self.seconds
    }

    pub(crate) fn nanos(self) -> i64 {
        self.nanos
    }
}

impl Ord for FileTime {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.seconds, self.nanos).cmp(&(other.seconds, other.nanos))
    }
}

impl PartialOrd for FileTime {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileFingerprint {
    pub(crate) identity: FileIdentity,
    pub(crate) size: u64,
    pub(crate) modified: FileTime,
    pub(crate) changed: Option<FileTime>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MonitorToken {
    pub(crate) instance_id: Uuid,
    pub(crate) sequence: i64,
}

#[derive(Clone, Debug, Default)]
struct WorktreeCache {
    entries: BTreeMap<String, WorkingNode>,
    ignore_sha256: Option<String>,
    monitor_token: Option<MonitorToken>,
    initialized: bool,
}

struct ScanResult {
    working: BTreeMap<String, WorkingNode>,
    ignore_sha256: Option<String>,
    monitor_token: Option<MonitorToken>,
    incremental_scopes: Option<Vec<String>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum NativeHiddenMode {
    Skip,
    Observe,
    Ignore,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ChangeKind {
    Delete,
    Add,
    Modify,
    Rename,
    Move,
    MoveRename,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct Change {
    pub kind: ChangeKind,
    pub item_type: String,
    pub path: String,
    pub source_path: Option<String>,
    pub content_changed: bool,
}

impl Change {
    pub fn marker(&self) -> &'static str {
        match (self.kind, self.content_changed) {
            (ChangeKind::Add, _) => "A",
            (ChangeKind::Modify, _) => "M",
            (ChangeKind::Rename, false) => "RN",
            (ChangeKind::Rename, true) => "M+RN",
            (ChangeKind::Move, false) => "MV",
            (ChangeKind::Move, true) => "M+MV",
            (ChangeKind::MoveRename, false) => "MV+RN",
            (ChangeKind::MoveRename, true) => "M+MV+RN",
            (ChangeKind::Delete, _) => "D",
        }
    }
}

impl ChangeKind {
    pub fn is_relocation(self) -> bool {
        matches!(self, Self::Rename | Self::Move | Self::MoveRename)
    }

    pub(crate) fn relocation(parent_changed: bool, name_changed: bool) -> Option<Self> {
        match (parent_changed, name_changed) {
            (false, false) => None,
            (false, true) => Some(Self::Rename),
            (true, false) => Some(Self::Move),
            (true, true) => Some(Self::MoveRename),
        }
    }
}

impl Repository {
    pub(crate) async fn scan_for_name_repairs(&self) -> Result<BTreeMap<String, WorkingNode>> {
        self.recover_current().await?;
        let committed = self.committed_tree().await?;
        let (ignore, _) = load_ignore(self.root())?;
        scan_tree_unvalidated(
            self.root(), &committed, &ignore, self.discovery_hidden_mode(), true,
        )
    }

    fn discovery_hidden_mode(&self) -> NativeHiddenMode {
        match self.hidden_files_mode() {
            HiddenFilesMode::Ignore => NativeHiddenMode::Ignore,
            HiddenFilesMode::Track => NativeHiddenMode::Skip,
        }
    }

    pub(crate) async fn committed_tree(&self) -> Result<BTreeMap<String, CommittedNode>> {
        self.build_committed_tree(true).await
    }

    pub(crate) async fn build_committed_tree(
        &self,
        apply_selection: bool,
    ) -> Result<BTreeMap<String, CommittedNode>> {
        let mut items = Vec::new();
        let mut cursor: Option<ItemCursor> = None;
        loop {
            let page = self
                .core()
                .list_items(
                    self.config().repo_id().clone(),
                    cursor.clone(),
                    Some(LifecycleState::Live),
                    PAGE_SIZE,
                )
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map(|item| ItemCursor {
                created_at: item.created_at,
                item_id: item.item_id.clone(),
            });
            let done = page.len() < PAGE_SIZE;
            items.extend(page);
            if done {
                break;
            }
        }

        // Records have no path.
        items.retain(|item| item.name.is_some());
        if self.checkout_paths().is_some() {
            items.retain(|item| matches!(item.item_type.as_str(), "FILE" | "FOLDER"));
            // Files attached beneath non-folder application owners have no
            // faithful filesystem path and stay solely inside the repository.
            loop {
                let folders = items
                    .iter()
                    .filter(|i| i.item_type == "FOLDER")
                    .map(|i| i.item_id.clone())
                    .collect::<HashSet<_>>();
                let before = items.len();
                items.retain(|i| {
                    i.parent_folder_id
                        .as_ref()
                        .is_none_or(|p| folders.contains(p))
                });
                if before == items.len() {
                    break;
                }
            }
        }

        let plain_digests = self.cached_plain_digests()?;
        let heads = load_file_heads(self.storage(), self.config().repo_id().as_str())?;
        let by_id: HashMap<ItemId, &ItemRow> = items
            .iter()
            .map(|item| (item.item_id.clone(), item))
            .collect();
        let mut paths = HashMap::new();
        let mut visiting = HashSet::new();
        for item in &items {
            build_item_path(item, &by_id, &mut paths, &mut visiting)?;
        }

        let mut tree = BTreeMap::new();
        for item in items {
            let kind = match item.item_type.as_str() {
                "FILE" => NodeKind::File,
                "FOLDER" => NodeKind::Folder,
                other => {
                    return Err(LocalError::Integrity(format!(
                        "unsupported live item type {other} for {}",
                        item.item_id
                    )));
                }
            };
            let (blob_id, size) = if kind == NodeKind::File {
                let (item_seq, blob_id, size) = heads.get(&item.item_id).ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "FILE {} has no local content projection",
                        item.item_id
                    ))
                })?;
                if *item_seq > item.head_seq {
                    return Err(LocalError::Integrity(format!(
                        "FILE {} content projection is ahead of its item head",
                        item.item_id
                    )));
                }
                (Some(blob_id.clone()), Some(*size))
            } else {
                (None, None)
            };
            let path = paths
                .remove(&item.item_id)
                .expect("path built for every item");
            if tree
                .insert(
                    path.clone(),
                    CommittedNode {
                        plain_digest: blob_id.as_ref().map(|id| -> Result<String> {
                            match plain_digests.get(id) {
                                Some(digest) => Ok(digest.clone()),
                                None => self.plain_digest(id),
                            }
                        }).transpose()?,
                        item_id: item.item_id,
                        kind,
                        head_seq: item.head_seq,
                        blob_id,
                        size,
                    },
                )
                .is_some()
            {
                return Err(LocalError::Integrity(format!(
                    "multiple live items resolve to {path}"
                )));
            }
        }
        if apply_selection {
            if let Some(paths) = self.checkout_paths() {
                tree.retain(|path, _| crate::checkout::selected(paths, path));
                for path in tree.keys() {
                    crate::checkout::normalize_paths(std::slice::from_ref(path))?;
                    if path.split('/').any(|part| part.starts_with(".synck-tmp-")) {
                        return Err(LocalError::Integrity(format!(
                            "reserved checkout path: {path}"
                        )));
                    }
                }
            }
        }
        Ok(tree)
    }

    pub(crate) async fn changes(
        &self,
    ) -> Result<(
        BTreeMap<String, CommittedNode>,
        BTreeMap<String, WorkingNode>,
        Vec<Change>,
        HashMap<String, ItemId>,
    )> {
        self.recover_current().await?;
        #[cfg(test)]
        FULL_WORKTREE_RECONCILIATIONS
            .lock()
            .unwrap()
            .push(self.config().repo_id().to_string());
        let committed = self.committed_tree().await?;
        let cache = load_worktree_cache(self.storage(), self.config().repo_id().as_str())?;
        let mut scan = self.scan_observed(&committed, &cache)?;
        self.filter_imported_email_sources(&committed, &mut scan.working)
            .await?;
        let (changes, working_ids) = self.reconcile_observation(&committed, &scan)?;
        Ok((committed, scan.working, changes, working_ids))
    }

    pub(crate) async fn inspect_changes(
        &self,
    ) -> Result<(
        BTreeMap<String, CommittedNode>,
        BTreeMap<String, WorkingNode>,
        Vec<Change>,
        HashMap<String, ItemId>,
    )> {
        self.recover_current().await?;
        #[cfg(test)]
        FULL_WORKTREE_RECONCILIATIONS
            .lock()
            .unwrap()
            .push(self.config().repo_id().to_string());
        let committed = self.committed_tree().await?;
        let cache = load_worktree_cache(self.storage(), self.config().repo_id().as_str())?;
        let (mut complete, mut scan) = self.scan_inspection(&committed, &cache)?;
        self.filter_checkout_observation(&mut complete);
        self.filter_checkout_observation(&mut scan.working);
        self.filter_imported_email_sources(&committed, &mut scan.working)
            .await?;
        let (changes, working_ids) = self.reconcile_observation(&committed, &scan)?;
        Ok((committed, complete, changes, working_ids))
    }

    fn reconcile_observation(
        &self,
        committed: &BTreeMap<String, CommittedNode>,
        scan: &ScanResult,
    ) -> Result<(Vec<Change>, HashMap<String, ItemId>)> {
        let working = &scan.working;
        let records = load_identity_records(self.storage(), self.config().repo_id().as_str())?;
        let matched = reconcile_identities(committed, working, &records);
        let working_ids = matched
            .iter()
            .map(|(item_id, path)| (path.clone(), item_id.clone()))
            .collect();
        let changes = diff(committed, working, &matched, &working_ids);
        let identities = next_identity_records(committed, working, &records, &matched);
        publish_worktree_observation(
            self.storage(),
            self.config().repo_id().as_str(),
            scan,
            &identities,
            &records,
        )?;
        Ok((changes, working_ids))
    }

    pub(crate) async fn refresh_worktree_identities(&self) -> Result<()> {
        #[cfg(test)]
        FULL_WORKTREE_RECONCILIATIONS
            .lock()
            .unwrap()
            .push(self.config().repo_id().to_string());
        let committed = self.committed_tree().await?;
        let cache = load_worktree_cache(self.storage(), self.config().repo_id().as_str())?;
        let scan = self.scan_observed(&committed, &cache)?;
        let working = &scan.working;
        let records = load_identity_records(self.storage(), self.config().repo_id().as_str())?;
        let matched = reconcile_identities(&committed, working, &records);
        let identities = next_identity_records(&committed, working, &records, &matched);
        publish_worktree_observation(
            self.storage(),
            self.config().repo_id().as_str(),
            &scan,
            &identities,
            &records,
        )?;
        Ok(())
    }

    pub(crate) fn publish_committed_identities(
        &self,
        committed: &BTreeMap<String, CommittedNode>,
        working: &BTreeMap<String, WorkingNode>,
        changes: &[Change],
        working_ids: &HashMap<String, ItemId>,
        created_ids: &HashMap<String, ItemId>,
    ) -> Result<()> {
        let mut deleted = BTreeSet::new();
        let mut updated = BTreeMap::new();
        for change in changes {
            if change.kind == ChangeKind::Delete {
                let item = committed.get(&change.path).ok_or_else(|| {
                    LocalError::Integrity(format!(
                        "committed identity source is missing: {}",
                        change.path
                    ))
                })?;
                deleted.insert(item.item_id.clone());
            }
            if change.kind == ChangeKind::Delete {
                continue;
            }
            let node = working.get(&change.path).ok_or_else(|| {
                LocalError::Integrity(format!(
                    "committed identity target is missing: {}",
                    change.path
                ))
            })?;
            let item_id = match change.kind {
                ChangeKind::Add => created_ids.get(&change.path),
                ChangeKind::Modify
                | ChangeKind::Rename
                | ChangeKind::Move
                | ChangeKind::MoveRename => working_ids.get(&change.path),
                ChangeKind::Delete => unreachable!(),
            }
            .ok_or_else(|| {
                LocalError::Integrity(format!(
                    "committed identity item is missing: {}",
                    change.path
                ))
            })?;
            updated.insert(
                item_id.clone(),
                IdentityRecord {
                    item_id: item_id.clone(),
                    kind: node.kind,
                    identity: node.identity,
                    path: change.path.clone(),
                },
            );
        }

        self.publish_identity_records(&deleted, &updated)
    }

    pub(crate) fn publish_identity_records(
        &self,
        deleted: &BTreeSet<ItemId>,
        updated: &BTreeMap<ItemId, IdentityRecord>,
    ) -> Result<()> {
        let mut connection = crate::local_database::open_writable(self.storage()).map_err(|error| {
            LocalError::Invalid(format!("cannot update committed identities: {error}"))
        })?;
        let transaction = connection.transaction()?;
        for item_id in deleted {
            transaction.execute(
                "DELETE FROM local_worktree_identities WHERE repo_id = ?1 AND item_id = ?2",
                rusqlite::params![self.config().repo_id().as_str(), item_id.as_str()],
            )?;
        }
        for record in updated.values() {
            let incarnation = record.identity.incarnation;
            transaction.execute(
                "INSERT INTO local_worktree_identities(
                     repo_id, item_id, node_kind, device_id, file_id,
                     birth_seconds, birth_nanos, path
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(repo_id, item_id) DO UPDATE SET
                     node_kind = excluded.node_kind,
                     device_id = excluded.device_id,
                     file_id = excluded.file_id,
                     birth_seconds = excluded.birth_seconds,
                     birth_nanos = excluded.birth_nanos,
                     path = excluded.path",
                rusqlite::params![
                    self.config().repo_id().as_str(),
                    record.item_id.as_str(),
                    match record.kind {
                        NodeKind::File => "FILE",
                        NodeKind::Folder => "FOLDER",
                    },
                    record.identity.device_id.to_be_bytes().as_slice(),
                    record.identity.file_id.to_be_bytes().as_slice(),
                    incarnation.map(FileTime::seconds),
                    incarnation.map(FileTime::nanos),
                    &record.path,
                ],
            )?;
        }
        transaction.commit()?;
        #[cfg(test)]
        EXACT_IDENTITY_ROWS.lock().unwrap().push((
            self.config().repo_id().to_string(),
            deleted.len() + updated.len(),
        ));
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn commit_work_counters(&self) -> (usize, usize) {
        let repo_id = self.config().repo_id().as_str();
        (
            FULL_WORKTREE_RECONCILIATIONS
                .lock()
                .unwrap()
                .iter()
                .filter(|recorded| recorded.as_str() == repo_id)
                .count(),
            EXACT_IDENTITY_ROWS
                .lock()
                .unwrap()
                .iter()
                .filter(|(recorded, _)| recorded == repo_id)
                .map(|(_, count)| count)
                .sum(),
        )
    }

    fn scan_observed(
        &self,
        committed: &BTreeMap<String, CommittedNode>,
        cache: &WorktreeCache,
    ) -> Result<ScanResult> {
        let mut scan = self.scan_cached(committed, cache)?;
        if self.checkout_paths().is_some() {
            if self
                .config()
                .checkout
                .as_ref()
                .is_some_and(|c| c.pending.is_none())
            {
                let records =
                    load_identity_records(self.storage(), self.config().repo_id().as_str())?;
                let matched = reconcile_identities(committed, &scan.working, &records);
                if let Some((item_id, moved_to)) = matched
                    .iter()
                    .find(|(_, path)| !self.checkout_observes(path))
                {
                    let moved_from = committed
                        .iter()
                        .find(|(_, node)| &node.item_id == item_id)
                        .map(|(path, _)| path.as_str())
                        .unwrap_or("its committed path");
                    return Err(LocalError::Invalid(format!(
                        "{moved_from} was moved to {moved_to}, outside checkout selection; \
                         move it back, or widen the selection with \
                         synck repository checkout-selection before moving it"
                    )));
                }
            }
            self.filter_checkout_observation(&mut scan.working);
        }
        Ok(scan)
    }

    // Observe candidate paths before applying checkout selection so that moves
    // across its boundary are still rejected by scan_observed.
    fn scan_cached(
        &self,
        committed: &BTreeMap<String, CommittedNode>,
        cache: &WorktreeCache,
    ) -> Result<ScanResult> {
        let (ignore, ignore_sha256) = load_ignore(self.root())?;
        if cache.initialized
            && cache.ignore_sha256 == ignore_sha256
            && let Some(token) = cache.monitor_token.as_ref()
        {
            match self.query_monitor(Some(token)) {
                Ok(crate::monitor::QueryResult::Changes {
                    instance_id,
                    sequence,
                    paths,
                }) => {
                    if changes_ignore_rules(&paths) {
                        return self.scan_baseline(committed, cache);
                    }
                    let scopes = incremental_scopes(paths);
                    let working = self.scan_incremental(
                        committed,
                        cache,
                        &ignore,
                        self.discovery_hidden_mode(),
                        &scopes,
                    )?;
                    return Ok(ScanResult {
                        working,
                        ignore_sha256,
                        monitor_token: Some(MonitorToken {
                            instance_id,
                            sequence,
                        }),
                        incremental_scopes: Some(scopes),
                    });
                }
                Ok(crate::monitor::QueryResult::Reset { .. }) => {
                    return self.scan_baseline(committed, cache);
                }
                Err(_) => {
                    let (ignore, ignore_sha256) = load_ignore(self.root())?;
                    let working = self.scan_complete(
                        committed,
                        cache,
                        &ignore,
                        self.discovery_hidden_mode(),
                    )?;
                    return Ok(ScanResult {
                        working,
                        ignore_sha256,
                        monitor_token: None,
                        incremental_scopes: None,
                    });
                }
            }
        }
        self.scan_baseline(committed, cache)
    }

    fn scan_inspection(
        &self,
        committed: &BTreeMap<String, CommittedNode>,
        cache: &WorktreeCache,
    ) -> Result<(BTreeMap<String, WorkingNode>, ScanResult)> {
        let no_ignore = GitignoreBuilder::new(".").build().map_err(|error| {
            LocalError::Invalid(format!("cannot build worktree scanner: {error}"))
        })?;
        for attempt in 0..2 {
            let baseline = match self.query_monitor(None) {
                Ok(crate::monitor::QueryResult::Reset {
                    instance_id,
                    sequence,
                })
                | Ok(crate::monitor::QueryResult::Changes {
                    instance_id,
                    sequence,
                    ..
                }) => Some(MonitorToken {
                    instance_id,
                    sequence,
                }),
                Err(_) => None,
            };
            let complete =
                self.scan_complete(committed, cache, &no_ignore, NativeHiddenMode::Observe)?;
            let Some(baseline) = baseline else {
                let (ignore, ignore_sha256) = load_ignore(self.root())?;
                let working =
                    filter_ignored_working(&complete, committed, &ignore, self.hidden_files_mode());
                return Ok((
                    complete,
                    ScanResult {
                        working,
                        ignore_sha256,
                        monitor_token: None,
                        incremental_scopes: None,
                    },
                ));
            };
            match self.query_monitor(Some(&baseline)) {
                Ok(crate::monitor::QueryResult::Changes {
                    instance_id,
                    sequence,
                    paths,
                }) => {
                    let complete_cache = WorktreeCache {
                        entries: complete,
                        ignore_sha256: None,
                        monitor_token: Some(baseline),
                        initialized: true,
                    };
                    let scopes = incremental_scopes(paths);
                    let complete = self.scan_incremental(
                        committed,
                        &complete_cache,
                        &no_ignore,
                        NativeHiddenMode::Observe,
                        &scopes,
                    )?;
                    let (ignore, ignore_sha256) = load_ignore(self.root())?;
                    let working = filter_ignored_working(
                        &complete,
                        committed,
                        &ignore,
                        self.hidden_files_mode(),
                    );
                    return Ok((
                        complete,
                        ScanResult {
                            working,
                            ignore_sha256,
                            monitor_token: Some(MonitorToken {
                                instance_id,
                                sequence,
                            }),
                            incremental_scopes: None,
                        },
                    ));
                }
                Ok(crate::monitor::QueryResult::Reset { .. }) if attempt == 0 => continue,
                _ => {
                    let (ignore, ignore_sha256) = load_ignore(self.root())?;
                    let working = filter_ignored_working(
                        &complete,
                        committed,
                        &ignore,
                        self.hidden_files_mode(),
                    );
                    return Ok((
                        complete,
                        ScanResult {
                            working,
                            ignore_sha256,
                            monitor_token: None,
                            incremental_scopes: None,
                        },
                    ));
                }
            }
        }
        unreachable!("inspection scan has two bounded attempts")
    }

    fn scan_baseline(
        &self,
        committed: &BTreeMap<String, CommittedNode>,
        cache: &WorktreeCache,
    ) -> Result<ScanResult> {
        for attempt in 0..2 {
            let baseline = match self.query_monitor(None) {
                Ok(crate::monitor::QueryResult::Reset {
                    instance_id,
                    sequence,
                })
                | Ok(crate::monitor::QueryResult::Changes {
                    instance_id,
                    sequence,
                    ..
                }) => Some(MonitorToken {
                    instance_id,
                    sequence,
                }),
                Err(_) => None,
            };
            let (ignore, ignore_sha256) = load_ignore(self.root())?;
            let working =
                self.scan_complete(committed, cache, &ignore, self.discovery_hidden_mode())?;
            let Some(baseline) = baseline else {
                return Ok(ScanResult {
                    working,
                    ignore_sha256,
                    monitor_token: None,
                    incremental_scopes: None,
                });
            };
            match self.query_monitor(Some(&baseline)) {
                Ok(crate::monitor::QueryResult::Changes {
                    instance_id,
                    sequence,
                    paths,
                }) => {
                    if changes_ignore_rules(&paths) {
                        if attempt == 0 {
                            continue;
                        }
                        let (ignore, ignore_sha256) = load_ignore(self.root())?;
                        let working = self.scan_complete(
                            committed,
                            cache,
                            &ignore,
                            self.discovery_hidden_mode(),
                        )?;
                        return Ok(ScanResult {
                            working,
                            ignore_sha256,
                            monitor_token: None,
                            incremental_scopes: None,
                        });
                    }
                    let complete_cache = WorktreeCache {
                        entries: working,
                        ignore_sha256: ignore_sha256.clone(),
                        monitor_token: Some(baseline),
                        initialized: true,
                    };
                    let scopes = incremental_scopes(paths);
                    let working = self.scan_incremental(
                        committed,
                        &complete_cache,
                        &ignore,
                        self.discovery_hidden_mode(),
                        &scopes,
                    )?;
                    return Ok(ScanResult {
                        working,
                        ignore_sha256,
                        monitor_token: Some(MonitorToken {
                            instance_id,
                            sequence,
                        }),
                        incremental_scopes: None,
                    });
                }
                Ok(crate::monitor::QueryResult::Reset { .. }) if attempt == 0 => continue,
                _ => {
                    return Ok(ScanResult {
                        working,
                        ignore_sha256,
                        monitor_token: None,
                        incremental_scopes: None,
                    });
                }
            }
        }
        unreachable!("baseline scan has two bounded attempts")
    }

    fn scan_complete(
        &self,
        committed: &BTreeMap<String, CommittedNode>,
        cache: &WorktreeCache,
        ignore: &Gitignore,
        native_hidden_mode: NativeHiddenMode,
    ) -> Result<BTreeMap<String, WorkingNode>> {
        let tracked: BTreeSet<String> = committed.keys().cloned().collect();
        let scan_started_at = normalized_time(SystemTime::now())?;
        let cache_by_identity = unique_cache_entries_by_identity(&cache.entries);
        let mut working = BTreeMap::new();
        scan_dir(
            self.root(),
            Path::new("."),
            "",
            true,
            &tracked,
            ignore,
            native_hidden_mode,
            false,
            true,
            &cache.entries,
            &cache_by_identity,
            scan_started_at,
            &mut working,
        )?;
        crate::name_repairs::validate(self.root(), &working)?;
        Ok(working)
    }

    fn scan_incremental(
        &self,
        committed: &BTreeMap<String, CommittedNode>,
        cache: &WorktreeCache,
        ignore: &Gitignore,
        native_hidden_mode: NativeHiddenMode,
        scopes: &[String],
    ) -> Result<BTreeMap<String, WorkingNode>> {
        let tracked: BTreeSet<String> = committed.keys().cloned().collect();
        let scan_started_at = normalized_time(SystemTime::now())?;
        let cache_by_identity = unique_cache_entries_by_identity(&cache.entries);
        let mut working = cache.entries.clone();
        for scope in scopes {
            remove_scope(&mut working, scope);
            scan_scope(
                self.root(),
                scope,
                &tracked,
                ignore,
                native_hidden_mode,
                &cache.entries,
                &cache_by_identity,
                scan_started_at,
                &mut working,
            )?;
        }
        crate::name_repairs::validate(self.root(), &working)?;
        Ok(working)
    }
}

pub(crate) fn scan_entry_without_ignore(
    root: &Dir,
    path: &str,
) -> Result<BTreeMap<String, WorkingNode>> {
    let no_ignore = GitignoreBuilder::new(".")
        .build()
        .map_err(|error| LocalError::Invalid(format!("cannot build worktree scanner: {error}")))?;
    let mut working = BTreeMap::new();
    scan_scope(
        root,
        path,
        &BTreeSet::new(),
        &no_ignore,
        NativeHiddenMode::Skip,
        &BTreeMap::new(),
        &HashMap::new(),
        normalized_time(SystemTime::now())?,
        &mut working,
    )?;
    crate::name_repairs::validate(root, &working)?;
    Ok(working)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IdentityRecord {
    pub(crate) item_id: ItemId,
    pub(crate) kind: NodeKind,
    pub(crate) identity: FileIdentity,
    pub(crate) path: String,
}

pub(crate) fn scan_materialized_tree(
    root: &Dir,
    committed: &BTreeMap<String, CommittedNode>,
) -> Result<BTreeMap<String, WorkingNode>> {
    let ignore = GitignoreBuilder::new(".")
        .build()
        .map_err(|error| LocalError::Invalid(format!("cannot build workspace scanner: {error}")))?;
    scan_tree(
        root,
        committed,
        &ignore,
        NativeHiddenMode::Observe,
        false,
    )
}

pub(crate) fn scan_canonical_without_ignore(
    root: &Dir,
    committed: &BTreeMap<String, CommittedNode>,
) -> Result<BTreeMap<String, WorkingNode>> {
    scan_without_ignore(root, committed, true)
}

// Validate the destination with the ignore rules being initialized, before
// publishing either its metadata or its ignore file.
pub(crate) fn validate_initial_tree(
    root: &Dir, ignore_root: &Dir, fix_names: bool,
) -> Result<()> {
    let (ignore, _) = load_ignore(ignore_root)?;
    let working = scan_tree_unvalidated(root, &BTreeMap::new(), &ignore, NativeHiddenMode::Ignore, true)?;
    if !fix_names {
        crate::name_repairs::validate(root, &working)?;
    }
    Ok(())
}

fn scan_without_ignore(
    root: &Dir,
    committed: &BTreeMap<String, CommittedNode>,
    repository_root: bool,
) -> Result<BTreeMap<String, WorkingNode>> {
    let ignore = GitignoreBuilder::new(".")
        .build()
        .map_err(|error| LocalError::Invalid(format!("cannot build workspace scanner: {error}")))?;
    scan_tree(
        root,
        committed,
        &ignore,
        NativeHiddenMode::Skip,
        repository_root,
    )
}

fn scan_tree(
    root: &Dir,
    committed: &BTreeMap<String, CommittedNode>,
    ignore: &Gitignore,
    native_hidden_mode: NativeHiddenMode,
    repository_root: bool,
) -> Result<BTreeMap<String, WorkingNode>> {
    let working = scan_tree_unvalidated(
        root, committed, ignore, native_hidden_mode, repository_root,
    )?;
    crate::name_repairs::validate(root, &working)?;
    Ok(working)
}

fn scan_tree_unvalidated(
    root: &Dir,
    committed: &BTreeMap<String, CommittedNode>,
    ignore: &Gitignore,
    native_hidden_mode: NativeHiddenMode,
    repository_root: bool,
) -> Result<BTreeMap<String, WorkingNode>> {
    let tracked: BTreeSet<String> = committed.keys().cloned().collect();
    let mut working = BTreeMap::new();
    let cache = BTreeMap::new();
    let cache_by_identity = HashMap::new();
    scan_dir(
        root,
        Path::new("."),
        "",
        true,
        &tracked,
        ignore,
        native_hidden_mode,
        false,
        repository_root,
        &cache,
        &cache_by_identity,
        normalized_time(SystemTime::now())?,
        &mut working,
    )?;
    Ok(working)
}

pub(crate) fn reconcile_identities(
    committed: &BTreeMap<String, CommittedNode>,
    working: &BTreeMap<String, WorkingNode>,
    records: &[IdentityRecord],
) -> HashMap<ItemId, String> {
    let committed_by_id: HashMap<ItemId, (&String, &CommittedNode)> = committed
        .iter()
        .map(|(path, node)| (node.item_id.clone(), (path, node)))
        .collect();
    let mut records_by_identity: HashMap<FileIdentity, Vec<&IdentityRecord>> = HashMap::new();
    for record in records {
        if let Some((_, node)) = committed_by_id.get(&record.item_id)
            && node.kind == record.kind
            && record.identity.incarnation.is_some()
        {
            records_by_identity
                .entry(record.identity)
                .or_default()
                .push(record);
        }
    }
    let mut working_by_identity: HashMap<FileIdentity, Vec<(&String, &WorkingNode)>> =
        HashMap::new();
    for (path, node) in working {
        if node.identity.incarnation.is_some() {
            working_by_identity
                .entry(node.identity)
                .or_default()
                .push((path, node));
        }
    }

    let mut matched = HashMap::new();
    let mut claimed_paths = HashSet::new();
    #[cfg(windows)]
    let mut blocked_paths: HashSet<String> = HashSet::new();
    #[cfg(not(windows))]
    let blocked_paths: HashSet<String> = HashSet::new();
    for (identity, identity_records) in &records_by_identity {
        let Some(candidates) = working_by_identity.get(identity) else {
            continue;
        };
        let available_records = identity_records
            .iter()
            .filter(|record| !matched.contains_key(&record.item_id))
            .collect::<Vec<_>>();
        let available_paths = candidates
            .iter()
            .filter(|(path, _)| !claimed_paths.contains(*path))
            .collect::<Vec<_>>();
        if available_records.len() != 1 || available_paths.len() != 1 {
            continue;
        }
        let record = available_records[0];
        let (path, node) = available_paths[0];
        if record.kind == node.kind {
            matched.insert(record.item_id.clone(), (*path).clone());
            claimed_paths.insert((*path).clone());
        }
    }
    #[cfg(windows)]
    reconcile_windows_tunneled_relocations(
        &committed_by_id,
        working,
        records,
        &mut matched,
        &mut claimed_paths,
        &mut blocked_paths,
    );
    for record in records {
        if matched.contains_key(&record.item_id)
            || claimed_paths.contains(&record.path)
            || blocked_paths.contains(&record.path)
        {
            continue;
        }
        let Some((committed_path, node)) = committed_by_id.get(&record.item_id) else {
            continue;
        };
        if node.kind != record.kind || *committed_path == &record.path {
            continue;
        }
        if working
            .get(&record.path)
            .is_some_and(|working| working.kind == record.kind)
        {
            matched.insert(record.item_id.clone(), record.path.clone());
            claimed_paths.insert(record.path.clone());
        }
    }
    for (path, node) in committed {
        if matched.contains_key(&node.item_id)
            || claimed_paths.contains(path)
            || blocked_paths.contains(path)
        {
            continue;
        }
        if working
            .get(path)
            .is_some_and(|working| working.kind == node.kind)
        {
            matched.insert(node.item_id.clone(), path.clone());
            claimed_paths.insert(path.clone());
        }
    }

    let mut moved_folders = matched
        .iter()
        .filter_map(|(item_id, new_path)| {
            let (old_path, node) = committed_by_id.get(item_id)?;
            (node.kind == NodeKind::Folder && *old_path != new_path)
                .then_some(((*old_path).clone(), new_path.clone()))
        })
        .collect::<Vec<_>>();
    moved_folders.sort_by(|left, right| {
        path_depth(&left.0)
            .cmp(&path_depth(&right.0))
            .then_with(|| left.cmp(right))
    });
    for (old_root, new_root) in moved_folders {
        let prefix = format!("{old_root}/");
        for (old_path, node) in committed.range(prefix.clone()..) {
            let Some(suffix) = old_path.strip_prefix(&prefix) else {
                break;
            };
            if matched.contains_key(&node.item_id) {
                continue;
            }
            let new_path = format!("{new_root}/{suffix}");
            if claimed_paths.contains(&new_path)
                || blocked_paths.contains(&new_path)
                || !working
                    .get(&new_path)
                    .is_some_and(|working| working.kind == node.kind)
            {
                continue;
            }
            matched.insert(node.item_id.clone(), new_path.clone());
            claimed_paths.insert(new_path);
        }
    }
    matched
}

#[cfg(any(windows, test))]
fn reconcile_windows_tunneled_relocations(
    committed_by_id: &HashMap<ItemId, (&String, &CommittedNode)>,
    working: &BTreeMap<String, WorkingNode>,
    records: &[IdentityRecord],
    matched: &mut HashMap<ItemId, String>,
    claimed_paths: &mut HashSet<String>,
    blocked_paths: &mut HashSet<String>,
) {
    let mut records_by_path: HashMap<&str, Vec<&IdentityRecord>> = HashMap::new();
    for record in records {
        if let Some((committed_path, committed)) = committed_by_id.get(&record.item_id)
            && committed.kind == record.kind
            && *committed_path == &record.path
        {
            records_by_path
                .entry(record.path.as_str())
                .or_default()
                .push(record);
        }
    }
    let mut working_by_file_id: HashMap<(u64, u64), Vec<(&String, &WorkingNode)>> = HashMap::new();
    for (path, node) in working {
        if node.kind == NodeKind::File && node.identity.incarnation.is_some() {
            working_by_file_id
                .entry((node.identity.device_id, node.identity.file_id))
                .or_default()
                .push((path, node));
        }
    }

    for record in records {
        if record.kind != NodeKind::File
            || record.identity.incarnation.is_none()
            || matched.contains_key(&record.item_id)
            || working.contains_key(&record.path)
        {
            continue;
        }
        if !committed_by_id.contains_key(&record.item_id) {
            continue;
        }
        let Some(allocation_candidates) =
            working_by_file_id.get(&(record.identity.device_id, record.identity.file_id))
        else {
            continue;
        };
        let available_candidates = allocation_candidates
            .iter()
            .filter(|(path, node)| {
                !claimed_paths.contains(*path)
                    && node.identity.incarnation != record.identity.incarnation
            })
            .collect::<Vec<_>>();
        blocked_paths.extend(available_candidates.iter().map(|(path, _)| (*path).clone()));
        let candidates = available_candidates
            .into_iter()
            .filter(|(path, node)| {
                let Some(destination_records) = records_by_path.get(path.as_str()) else {
                    return false;
                };
                destination_records.len() == 1
                    && destination_records[0].item_id != record.item_id
                    && destination_records[0].identity.device_id == node.identity.device_id
                    && destination_records[0].identity.file_id != node.identity.file_id
                    && destination_records[0].identity.incarnation == node.identity.incarnation
            })
            .map(|(path, _)| *path)
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            continue;
        }
        let path = candidates[0].clone();
        matched.insert(record.item_id.clone(), path.clone());
        claimed_paths.insert(path);
    }
}

pub(crate) fn load_identity_records(
    storage: &SqliteStorage,
    repo_id: &str,
) -> Result<Vec<IdentityRecord>> {
    let connection = crate::local_database::open_readonly(storage).map_err(|error| {
        LocalError::Integrity(format!("cannot read worktree identities: {error}"))
    })?;
    let mut statement = connection.prepare(
        "SELECT item_id, node_kind, device_id, file_id, birth_seconds, birth_nanos, path
         FROM local_worktree_identities WHERE repo_id = ?1 ORDER BY item_id",
    )?;
    let rows = statement.query_map([repo_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let mut records = Vec::new();
    for row in rows {
        let (item_id, kind, device_id, file_id, birth_seconds, birth_nanos, path) = row?;
        let item_id = ItemId::parse(item_id).map_err(|error| {
            LocalError::Integrity(format!("invalid worktree identity item ID: {error}"))
        })?;
        let kind = match kind.as_str() {
            "FILE" => NodeKind::File,
            "FOLDER" => NodeKind::Folder,
            _ => {
                return Err(LocalError::Integrity(format!(
                    "invalid worktree identity kind {kind:?}"
                )));
            }
        };
        records.push(IdentityRecord {
            item_id,
            kind,
            identity: FileIdentity {
                device_id: decode_identity(&device_id)?,
                file_id: decode_identity(&file_id)?,
                incarnation: decode_incarnation(birth_seconds, birth_nanos)?,
            },
            path,
        });
    }
    Ok(records)
}

fn decode_identity(value: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = value
        .try_into()
        .map_err(|_| LocalError::Integrity("invalid worktree filesystem identity".to_string()))?;
    Ok(u64::from_be_bytes(bytes))
}

pub(crate) fn decode_incarnation(
    seconds: Option<i64>,
    nanos: Option<i64>,
) -> Result<Option<FileTime>> {
    match (seconds, nanos) {
        (None, None) => Ok(None),
        (Some(seconds), Some(nanos)) if (0..1_000_000_000).contains(&nanos) => {
            Ok(Some(FileTime { seconds, nanos }))
        }
        _ => Err(LocalError::Integrity(
            "invalid worktree filesystem incarnation".to_string(),
        )),
    }
}

fn validate_plain_digest(digest: &str) -> Result<()> {
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(LocalError::Integrity("invalid cached plaintext SHA-256 digest".into()));
    }
    Ok(())
}

fn load_worktree_cache(storage: &SqliteStorage, repo_id: &str) -> Result<WorktreeCache> {
    let connection = crate::local_database::open_readonly(storage)
        .map_err(|error| LocalError::Integrity(format!("cannot read worktree cache: {error}")))?;
    let mut statement = connection.prepare(
        "SELECT path, node_kind, device_id, file_id, birth_seconds, birth_nanos, size_bytes,
                mtime_seconds, mtime_nanos, change_seconds, change_nanos,
                plain_sha256, stat_valid
         FROM local_worktree_entries WHERE repo_id = ?1 ORDER BY path",
    )?;
    let rows = statement.query_map([repo_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, Option<i64>>(10)?,
            row.get::<_, Option<String>>(11)?,
            row.get::<_, Option<i64>>(12)?,
        ))
    })?;
    let mut entries = BTreeMap::new();
    for row in rows {
        let (
            path,
            kind,
            device_id,
            file_id,
            birth_seconds,
            birth_nanos,
            size,
            mtime_seconds,
            mtime_nanos,
            change_seconds,
            change_nanos,
            plain_digest,
            stat_valid,
        ) = row?;
        let identity = FileIdentity {
            device_id: decode_identity(&device_id)?,
            file_id: decode_identity(&file_id)?,
            incarnation: decode_incarnation(birth_seconds, birth_nanos)?,
        };
        let node = match kind.as_str() {
            "FILE" => {
                let size = u64::try_from(size.ok_or_else(|| {
                    LocalError::Integrity(format!("cached FILE {path} has no size"))
                })?)
                .map_err(|_| {
                    LocalError::Integrity(format!("cached FILE {path} has invalid size"))
                })?;
                let plain_digest = plain_digest.ok_or_else(|| {
                    LocalError::Integrity(format!("cached FILE {path} has no plaintext digest"))
                })?;
                validate_plain_digest(&plain_digest)?;
                WorkingNode {
                    kind: NodeKind::File,
                    digest_hex: Some(plain_digest),
                    size: Some(size),
                    identity,
                    fingerprint: Some(FileFingerprint {
                        identity,
                        size,
                        modified: FileTime {
                            seconds: mtime_seconds.ok_or_else(|| {
                                LocalError::Integrity(format!(
                                    "cached FILE {path} has no modification time"
                                ))
                            })?,
                            nanos: mtime_nanos.ok_or_else(|| {
                                LocalError::Integrity(format!(
                                    "cached FILE {path} has no modification time"
                                ))
                            })?,
                        },
                        changed: match (change_seconds, change_nanos) {
                            (Some(seconds), Some(nanos)) => Some(FileTime { seconds, nanos }),
                            (None, None) => None,
                            _ => {
                                return Err(LocalError::Integrity(format!(
                                    "cached FILE {path} has an incomplete change time"
                                )));
                            }
                        },
                    }),
                    stat_valid: stat_valid == Some(1),
                    native_hidden: false,
                }
            }
            "FOLDER" => WorkingNode {
                kind: NodeKind::Folder,
                digest_hex: None,
                size: None,
                identity,
                fingerprint: None,
                stat_valid: false,
                native_hidden: false,
            },
            _ => {
                return Err(LocalError::Integrity(format!(
                    "cached entry {path} has invalid kind {kind:?}"
                )));
            }
        };
        if entries.insert(path.clone(), node).is_some() {
            return Err(LocalError::Integrity(format!(
                "duplicate cached worktree path {path}"
            )));
        }
    }

    let state: Option<(Option<String>, Option<String>, Option<i64>)> = connection
        .query_row(
            "SELECT ignore_sha256, monitor_instance_id, monitor_sequence
             FROM local_worktree_scan_state WHERE repo_id = ?1",
            [repo_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((ignore_sha256, monitor_instance_id, monitor_sequence)) = state else {
        return Ok(WorktreeCache {
            entries,
            ..WorktreeCache::default()
        });
    };
    if let Some(ignore_sha256) = ignore_sha256.as_deref() {
        validate_plain_digest(ignore_sha256)?;
    }
    let monitor_token = match (monitor_instance_id, monitor_sequence) {
        (Some(instance_id), Some(sequence)) => {
            let parsed = Uuid::parse_str(&instance_id).map_err(|error| {
                LocalError::Integrity(format!("invalid cached monitor instance ID: {error}"))
            })?;
            if parsed.to_string() != instance_id || sequence < 0 {
                return Err(LocalError::Integrity(
                    "cached monitor token is noncanonical".to_string(),
                ));
            }
            Some(MonitorToken {
                instance_id: parsed,
                sequence,
            })
        }
        (None, None) => None,
        _ => {
            return Err(LocalError::Integrity(
                "cached monitor token is incomplete".to_string(),
            ));
        }
    };
    Ok(WorktreeCache {
        entries,
        ignore_sha256,
        monitor_token,
        initialized: true,
    })
}

pub(crate) fn next_identity_records(
    committed: &BTreeMap<String, CommittedNode>,
    working: &BTreeMap<String, WorkingNode>,
    previous: &[IdentityRecord],
    matched: &HashMap<ItemId, String>,
) -> Vec<IdentityRecord> {
    let previous_by_item: HashMap<&ItemId, &IdentityRecord> = previous
        .iter()
        .map(|record| (&record.item_id, record))
        .collect();
    let mut records = Vec::new();
    let mut occupied_paths = HashSet::new();
    for node in committed.values() {
        let Some(path) = matched.get(&node.item_id) else {
            continue;
        };
        let working = working
            .get(path)
            .expect("matched worktree identity has a working node");
        records.push(IdentityRecord {
            item_id: node.item_id.clone(),
            kind: node.kind,
            identity: working.identity,
            path: path.clone(),
        });
        occupied_paths.insert(path.clone());
    }
    for node in committed.values() {
        if matched.contains_key(&node.item_id) {
            continue;
        }
        let Some(previous) = previous_by_item.get(&node.item_id) else {
            continue;
        };
        if previous.kind == node.kind && occupied_paths.insert(previous.path.clone()) {
            records.push((*previous).clone());
        }
    }
    records.sort_by(|left, right| left.item_id.cmp(&right.item_id));
    records
}

fn publish_worktree_observation(
    storage: &SqliteStorage,
    repo_id: &str,
    scan: &ScanResult,
    identities: &[IdentityRecord],
    previous_identities: &[IdentityRecord],
) -> Result<()> {
    let working = &scan.working;
    let incremental_scopes = scan.incremental_scopes.as_deref();
    let mut connection = crate::local_database::open_writable(storage)
        .map_err(|error| LocalError::Invalid(format!("cannot update worktree cache: {error}")))?;
    let transaction = connection.transaction()?;
    if let Some(scopes) = incremental_scopes {
        for scope in scopes {
            transaction.execute(
                "DELETE FROM local_worktree_entries
                 WHERE repo_id = ?1 AND (path = ?2 OR substr(path, 1, length(?2) + 1) = ?2 || '/')",
                rusqlite::params![repo_id, scope],
            )?;
        }
    } else {
        transaction.execute(
            "DELETE FROM local_worktree_entries WHERE repo_id = ?1",
            [repo_id],
        )?;
    }
    for (path, node) in working.iter().filter(|(path, _)| {
        incremental_scopes
            .is_none_or(|scopes| scopes.iter().any(|scope| path_in_scope(path, scope)))
    }) {
        let incarnation = node.identity.incarnation;
        let (size, modified, changed, plain_digest, stat_valid) = match node.kind {
            NodeKind::File => {
                let fingerprint = node
                    .fingerprint
                    .expect("scanned FILE has a complete fingerprint");
                let digest = node.digest_hex.clone().expect("scanned FILE has a digest");
                (
                    Some(i64::try_from(fingerprint.size).map_err(|_| {
                        LocalError::Invalid(format!("file is too large for local storage: {path}"))
                    })?),
                    Some(fingerprint.modified),
                    fingerprint.changed,
                    Some(digest),
                    Some(i64::from(node.stat_valid)),
                )
            }
            NodeKind::Folder => (None, None, None, None, None),
        };
        transaction.execute(
            "INSERT INTO local_worktree_entries(
                 repo_id, path, node_kind, device_id, file_id, birth_seconds, birth_nanos,
                 size_bytes,
                 mtime_seconds, mtime_nanos, change_seconds, change_nanos,
                 plain_sha256, stat_valid
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                repo_id,
                path,
                match node.kind {
                    NodeKind::File => "FILE",
                    NodeKind::Folder => "FOLDER",
                },
                node.identity.device_id.to_be_bytes().as_slice(),
                node.identity.file_id.to_be_bytes().as_slice(),
                incarnation.map(FileTime::seconds),
                incarnation.map(FileTime::nanos),
                size,
                modified.map(|time| time.seconds),
                modified.map(|time| time.nanos),
                changed.map(|time| time.seconds),
                changed.map(|time| time.nanos),
                plain_digest,
                stat_valid,
            ],
        )?;
    }
    transaction.execute(
        "INSERT INTO local_worktree_scan_state(
             repo_id, ignore_sha256, monitor_instance_id, monitor_sequence
         ) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(repo_id) DO UPDATE SET
             ignore_sha256 = excluded.ignore_sha256,
             monitor_instance_id = excluded.monitor_instance_id,
             monitor_sequence = excluded.monitor_sequence",
        rusqlite::params![
            repo_id,
            scan.ignore_sha256.as_deref(),
            scan.monitor_token
                .as_ref()
                .map(|token| token.instance_id.to_string()),
            scan.monitor_token.as_ref().map(|token| token.sequence),
        ],
    )?;
    let next_by_item = identities
        .iter()
        .map(|record| (&record.item_id, record))
        .collect::<HashMap<_, _>>();
    let previous_by_item = previous_identities
        .iter()
        .map(|record| (&record.item_id, record))
        .collect::<HashMap<_, _>>();
    for previous in previous_identities {
        if next_by_item.get(&previous.item_id).copied() != Some(previous) {
            transaction.execute(
                "DELETE FROM local_worktree_identities WHERE repo_id = ?1 AND item_id = ?2",
                rusqlite::params![repo_id, previous.item_id.as_str()],
            )?;
        }
    }
    for record in identities {
        if previous_by_item.get(&record.item_id).copied() == Some(record) {
            continue;
        }
        let incarnation = record.identity.incarnation;
        transaction.execute(
            "INSERT INTO local_worktree_identities(
                 repo_id, item_id, node_kind, device_id, file_id,
                 birth_seconds, birth_nanos, path
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                repo_id,
                record.item_id.as_str(),
                match record.kind {
                    NodeKind::File => "FILE",
                    NodeKind::Folder => "FOLDER",
                },
                record.identity.device_id.to_be_bytes().as_slice(),
                record.identity.file_id.to_be_bytes().as_slice(),
                incarnation.map(FileTime::seconds),
                incarnation.map(FileTime::nanos),
                &record.path,
            ],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn build_item_path(
    item: &ItemRow,
    by_id: &HashMap<ItemId, &ItemRow>,
    paths: &mut HashMap<ItemId, String>,
    visiting: &mut HashSet<ItemId>,
) -> Result<String> {
    if let Some(path) = paths.get(&item.item_id) {
        return Ok(path.clone());
    }
    if !visiting.insert(item.item_id.clone()) {
        return Err(LocalError::Integrity(format!(
            "folder cycle at {}",
            item.item_id
        )));
    }
    let name = item.name.as_deref().ok_or_else(|| {
        LocalError::Integrity(format!("tree item {} has no name", item.item_id))
    })?;
    compute_name_key(name).map_err(|error| {
        LocalError::Integrity(format!("invalid stored item name {name:?}: {error}"))
    })?;
    let path = match item.parent_folder_id.as_ref() {
        None => name.to_string(),
        Some(parent_id) => {
            let parent = by_id.get(parent_id).ok_or_else(|| {
                LocalError::Integrity(format!(
                    "item {} has missing live parent {parent_id}",
                    item.item_id
                ))
            })?;
            if parent.item_type != "FOLDER" {
                return Err(LocalError::Integrity(format!(
                    "item {} has non-folder parent {parent_id}",
                    item.item_id
                )));
            }
            format!(
                "{}/{}",
                build_item_path(parent, by_id, paths, visiting)?,
                name
            )
        }
    };
    visiting.remove(&item.item_id);
    paths.insert(item.item_id.clone(), path.clone());
    Ok(path)
}

fn load_file_heads(
    storage: &SqliteStorage,
    repo_id: &str,
) -> Result<HashMap<ItemId, (i64, ObjectId, i64)>> {
    let connection = crate::local_database::open_readonly(storage)
        .map_err(|error| LocalError::Integrity(format!("cannot read file heads: {error}")))?;
    let mut statement = connection.prepare(
        "SELECT item_id, item_seq, blob_id, size_bytes
         FROM local_file_heads WHERE repo_id = ?1",
    )?;
    let rows = statement.query_map([repo_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut heads = HashMap::new();
    for row in rows {
        let (item_id, item_seq, blob_id, size) = row?;
        let item_id = ItemId::parse(item_id).map_err(|error| {
            LocalError::Integrity(format!("invalid projected item ID: {error}"))
        })?;
        let blob_id = ObjectId::parse(blob_id).map_err(|error| {
            LocalError::Integrity(format!("invalid projected blob ID: {error}"))
        })?;
        heads.insert(item_id, (item_seq, blob_id, size));
    }
    Ok(heads)
}

pub(crate) fn load_ignore(root: &Dir) -> Result<(Gitignore, Option<String>)> {
    let mut builder = GitignoreBuilder::new(".");
    let mut digest = None;
    match root.symlink_metadata(".synckignore") {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || is_cap_link_or_reparse(&metadata) {
                return Err(LocalError::Invalid(
                    "unsupported entry .synckignore".to_string(),
                ));
            }
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let file = root
                .open_with(".synckignore", &options)
                .at("open", ".synckignore")?;
            let mut contents = Vec::new();
            BufReader::new(file)
                .read_to_end(&mut contents)
                .at("read", ".synckignore")?;
            let text = std::str::from_utf8(&contents).map_err(|error| {
                LocalError::Invalid(format!("invalid UTF-8 in .synckignore: {error}"))
            })?;
            digest = Some(hex::encode(Sha256::digest(&contents)));
            for line in text.lines() {
                builder
                    .add_line(Some(PathBuf::from(".synckignore")), line)
                    .map_err(|error| {
                        LocalError::Invalid(format!("invalid .synckignore: {error}"))
                    })?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect",
                path: PathBuf::from(".synckignore"),
                source,
            });
        }
    }
    let ignore = builder
        .build()
        .map_err(|error| LocalError::Invalid(format!("invalid .synckignore: {error}")))?;
    Ok((ignore, digest))
}

pub(crate) fn validate_synckignore(root_path: &Path) -> Result<()> {
    let root = Dir::open_ambient_dir(root_path, ambient_authority())
        .at("open repository root", root_path)?;
    let (_, digest) = load_ignore(&root)?;
    if digest.is_none() {
        return Err(LocalError::Invalid(".synckignore is missing".to_string()));
    }
    Ok(())
}

#[cfg(windows)]
fn native_hidden(
    directory: &Dir,
    name: &OsStr,
    metadata: &cap_std::fs::Metadata,
    display: &str,
) -> Result<bool> {
    use cap_std::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM};

    let _ = (directory, name, display);
    let attributes = metadata.file_attributes();
    Ok(attributes & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0)
}

#[cfg(target_os = "macos")]
fn native_hidden(
    directory: &Dir,
    name: &OsStr,
    metadata: &cap_std::fs::Metadata,
    display: &str,
) -> Result<bool> {
    let _ = metadata;
    let stat = rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(std::io::Error::from)
        .at("inspect native hidden flag", display)?;
    Ok(stat.st_flags & libc::UF_HIDDEN != 0)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn native_hidden(
    directory: &Dir,
    name: &OsStr,
    metadata: &cap_std::fs::Metadata,
    display: &str,
) -> Result<bool> {
    let _ = (directory, name, metadata, display);
    Ok(false)
}

fn native_hidden_path(root: &Dir, relative: &Path) -> Result<bool> {
    let mut ancestors = relative
        .ancestors()
        .take_while(|path| !path.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for path in ancestors {
        if path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with('.'))
        {
            return Ok(true);
        }
        let metadata = root.symlink_metadata(path).at("inspect", path)?;
        if native_hidden(
            root,
            path.as_os_str(),
            &metadata,
            &path.display().to_string(),
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn validate_new_checkout_file(
    root: &Dir,
    path: &str,
    mode: HiddenFilesMode,
) -> Result<()> {
    let relative = Path::new(path);
    let (ignore, _) = load_ignore(root)?;
    let hidden = relative
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with('.'))
        || match relative.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(parent) => native_hidden_path(root, parent)?,
            None => false,
        };
    if ignored_untracked_entry(&ignore, relative, false, hidden, mode) {
        return Err(LocalError::Invalid(format!(
            "checkout ignores {path}; change the ignore rules before creating it"
        )));
    }
    Ok(())
}

fn ignored_untracked_entry(
    ignore: &Gitignore,
    relative: &Path,
    is_dir: bool,
    native_hidden: bool,
    hidden_files_mode: HiddenFilesMode,
) -> bool {
    if hidden_files_mode == HiddenFilesMode::Ignore
        && relative != Path::new(".synckignore")
        && native_hidden
    {
        return true;
    }
    let matched = ignore.matched_path_or_any_parents(relative, is_dir);
    matched.is_ignore()
}

#[allow(clippy::too_many_arguments)]
fn scan_dir(
    root: &Dir,
    relative_dir: &Path,
    display_dir: &str,
    is_root: bool,
    tracked: &BTreeSet<String>,
    ignore: &Gitignore,
    native_hidden_mode: NativeHiddenMode,
    native_hidden_ancestor: bool,
    repository_root: bool,
    cache: &BTreeMap<String, WorkingNode>,
    cache_by_identity: &HashMap<FileIdentity, &WorkingNode>,
    scan_started_at: FileTime,
    output: &mut BTreeMap<String, WorkingNode>,
) -> Result<()> {
    let directory = if is_root {
        root.try_clone().at("open", ".")?
    } else {
        root.open_dir_nofollow(relative_dir)
            .at("open directory", relative_dir)?
    };
    let mut entries = directory
        .entries()
        .at("read directory", relative_dir)?
        .collect::<std::io::Result<Vec<_>>>()
        .at("read directory", relative_dir)?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let name_os: OsString = entry.file_name();
        let name = name_os.to_str().ok_or_else(|| {
            LocalError::Invalid(format!(
                "non-Unicode name below {}",
                if display_dir.is_empty() {
                    "."
                } else {
                    display_dir
                }
            ))
        })?;
        let display = if display_dir.is_empty() {
            name.to_string()
        } else {
            format!("{display_dir}/{name}")
        };
        let relative = if is_root {
            PathBuf::from(name)
        } else {
            relative_dir.join(name)
        };
        let metadata = directory.symlink_metadata(name).at("inspect", &display)?;
        let file_type = metadata.file_type();

        if is_root && repository_root && name == ".synck" {
            continue;
        }
        if is_root && repository_root && is_reserved(name, ".synck-init-") {
            continue;
        }
        if is_reserved(name, ".synck-tmp-") {
            if file_type.is_file() && !is_cap_link_or_reparse(&metadata) {
                directory
                    .remove_file(name)
                    .at("remove stale temporary file", &display)?;
                continue;
            }
            return Err(LocalError::Invalid(format!(
                "reserved temporary entry is not a regular file: {display}"
            )));
        }

        if name == ".synck" {
            return Err(LocalError::Invalid(format!(
                "nested .synck entry is not supported: {display}"
            )));
        }
        let exact = tracked.contains(&display);
        let descendant = has_tracked_descendant(tracked, &display);
        let native_hidden = native_hidden_mode != NativeHiddenMode::Skip
            && (native_hidden_ancestor
                || name.starts_with('.')
                || native_hidden(&directory, &name_os, &metadata, &display)?);
        let ignored = !exact
            && !descendant
            && ignored_untracked_entry(
                ignore,
                &relative,
                file_type.is_dir(),
                native_hidden_mode == NativeHiddenMode::Ignore && native_hidden,
                if native_hidden_mode == NativeHiddenMode::Ignore {
                    HiddenFilesMode::Ignore
                } else {
                    HiddenFilesMode::Track
                },
            );
        if ignored {
            continue;
        }

        if file_type.is_dir() && !is_cap_link_or_reparse(&metadata) {
            let opened = directory
                .open_dir_nofollow(name)
                .at("open directory", &display)?;
            let opened_metadata = opened.dir_metadata().at("inspect", &display)?;
            let identity = directory_identity(&opened, &opened_metadata)?;
            output.insert(
                display.clone(),
                WorkingNode {
                    kind: NodeKind::Folder,
                    digest_hex: None,
                    size: None,
                    identity,
                    fingerprint: None,
                    stat_valid: false,
                    native_hidden,
                },
            );
            scan_dir(
                root,
                &relative,
                &display,
                false,
                tracked,
                ignore,
                native_hidden_mode,
                native_hidden,
                repository_root,
                cache,
                cache_by_identity,
                scan_started_at,
                output,
            )?;
        } else if file_type.is_file() && !is_cap_link_or_reparse(&metadata) {
            let mut node = inspect_file(
                root,
                &relative,
                &display,
                cache.get(&display),
                cache_by_identity,
                scan_started_at,
            )?;
            node.native_hidden = native_hidden;
            output.insert(display, node);
        } else {
            return Err(LocalError::Invalid(format!(
                "unsupported filesystem entry: {display}"
            )));
        }
    }
    Ok(())
}

fn unique_cache_entries_by_identity(
    cache: &BTreeMap<String, WorkingNode>,
) -> HashMap<FileIdentity, &WorkingNode> {
    let mut candidates = HashMap::new();
    let mut ambiguous = HashSet::new();
    for node in cache.values() {
        if node.identity.incarnation.is_none() {
            continue;
        }
        if ambiguous.contains(&node.identity) {
            continue;
        }
        if candidates.insert(node.identity, node).is_some() {
            candidates.remove(&node.identity);
            ambiguous.insert(node.identity);
        }
    }
    candidates
}

fn incremental_scopes(paths: Vec<String>) -> Vec<String> {
    let mut candidates = BTreeSet::new();
    for path in paths {
        candidates.insert(path.clone());
        if let Some((parent, _)) = path.rsplit_once('/') {
            candidates.insert(parent.to_string());
        }
    }
    let mut scopes = Vec::new();
    for candidate in candidates {
        if scopes
            .iter()
            .any(|scope: &String| path_in_scope(&candidate, scope))
        {
            continue;
        }
        scopes.retain(|scope| !path_in_scope(scope, &candidate));
        scopes.push(candidate);
    }
    scopes.sort();
    scopes
}

fn changes_ignore_rules(paths: &[String]) -> bool {
    paths.iter().any(|path| path == ".synckignore")
}

fn filter_ignored_working(
    complete: &BTreeMap<String, WorkingNode>,
    committed: &BTreeMap<String, CommittedNode>,
    ignore: &Gitignore,
    hidden_files_mode: HiddenFilesMode,
) -> BTreeMap<String, WorkingNode> {
    let tracked = committed.keys().cloned().collect::<BTreeSet<_>>();
    complete
        .iter()
        .filter(|(path, node)| {
            tracked.contains(*path)
                || has_tracked_descendant(&tracked, path)
                || !ignored_untracked_entry(
                    ignore,
                    Path::new(path.as_str()),
                    node.kind == NodeKind::Folder,
                    node.native_hidden,
                    hidden_files_mode,
                )
        })
        .map(|(path, node)| (path.clone(), node.clone()))
        .collect()
}

fn path_in_scope(path: &str, scope: &str) -> bool {
    path == scope
        || path
            .strip_prefix(scope)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn remove_scope(working: &mut BTreeMap<String, WorkingNode>, scope: &str) {
    let paths = working
        .range(scope.to_string()..)
        .take_while(|(path, _)| path_in_scope(path, scope))
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    for path in paths {
        working.remove(&path);
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_scope(
    root: &Dir,
    scope: &str,
    tracked: &BTreeSet<String>,
    ignore: &Gitignore,
    native_hidden_mode: NativeHiddenMode,
    cache: &BTreeMap<String, WorkingNode>,
    cache_by_identity: &HashMap<FileIdentity, &WorkingNode>,
    scan_started_at: FileTime,
    output: &mut BTreeMap<String, WorkingNode>,
) -> Result<()> {
    let relative = Path::new(scope);
    let metadata = match root.symlink_metadata(relative) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect",
                path: relative.to_path_buf(),
                source,
            });
        }
    };
    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| LocalError::Invalid(format!("invalid monitor path {scope:?}")))?;
    let file_type = metadata.file_type();
    if !scope.contains('/') && is_reserved(name, ".synck-init-") {
        return Ok(());
    }
    if is_reserved(name, ".synck-tmp-") {
        if file_type.is_file() && !is_cap_link_or_reparse(&metadata) {
            root.remove_file(relative)
                .at("remove stale temporary file", scope)?;
            return Ok(());
        }
        return Err(LocalError::Invalid(format!(
            "reserved temporary entry is not a regular file: {scope}"
        )));
    }
    if name == ".synck" || scope.starts_with(".synck/") {
        return Err(LocalError::Invalid(format!(
            "nested .synck entry is not supported: {scope}"
        )));
    }
    let exact = tracked.contains(scope);
    let descendant = has_tracked_descendant(tracked, scope);
    let native_hidden =
        native_hidden_mode != NativeHiddenMode::Skip && native_hidden_path(root, relative)?;
    if !exact
        && !descendant
        && ignored_untracked_entry(
            ignore,
            relative,
            file_type.is_dir(),
            native_hidden_mode == NativeHiddenMode::Ignore && native_hidden,
            if native_hidden_mode == NativeHiddenMode::Ignore {
                HiddenFilesMode::Ignore
            } else {
                HiddenFilesMode::Track
            },
        )
    {
        return Ok(());
    }
    if file_type.is_dir() && !is_cap_link_or_reparse(&metadata) {
        let opened = root
            .open_dir_nofollow(relative)
            .at("open directory", scope)?;
        let opened_metadata = opened.dir_metadata().at("inspect", scope)?;
        let identity = directory_identity(&opened, &opened_metadata)?;
        output.insert(
            scope.to_string(),
            WorkingNode {
                kind: NodeKind::Folder,
                digest_hex: None,
                size: None,
                identity,
                fingerprint: None,
                stat_valid: false,
                native_hidden,
            },
        );
        scan_dir(
            root,
            relative,
            scope,
            false,
            tracked,
            ignore,
            native_hidden_mode,
            native_hidden,
            true,
            cache,
            cache_by_identity,
            scan_started_at,
            output,
        )
    } else if file_type.is_file() && !is_cap_link_or_reparse(&metadata) {
        let mut node = inspect_file(
            root,
            relative,
            scope,
            cache.get(scope),
            cache_by_identity,
            scan_started_at,
        )?;
        node.native_hidden = native_hidden;
        output.insert(scope.to_string(), node);
        Ok(())
    } else {
        Err(LocalError::Invalid(format!(
            "unsupported filesystem entry: {scope}"
        )))
    }
}

fn inspect_file(
    root: &Dir,
    relative: &Path,
    display: &str,
    same_path_cache: Option<&WorkingNode>,
    cache_by_identity: &HashMap<FileIdentity, &WorkingNode>,
    scan_started_at: FileTime,
) -> Result<WorkingNode> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = root.open_with(relative, &options).at("open", display)?;
    let before = file.metadata().at("inspect", display)?;
    if !before.file_type().is_file() || is_cap_link_or_reparse(&before) {
        return Err(LocalError::Invalid(format!(
            "unsupported filesystem entry: {display}"
        )));
    }
    let fingerprint = file_fingerprint(&file, &before, display)?;
    let cached = same_path_cache
        .filter(|cached| cached.kind == NodeKind::File && cached.identity == fingerprint.identity)
        .or_else(|| cache_by_identity.get(&fingerprint.identity).copied());
    if let Some(cached) = cached
        && cached.stat_valid
        && cached.fingerprint == Some(fingerprint)
        && fingerprint_is_old(fingerprint, scan_started_at)
    {
        return Ok(WorkingNode {
            kind: NodeKind::File,
            digest_hex: cached.digest_hex.clone(),
            size: Some(fingerprint.size),
            identity: fingerprint.identity,
            fingerprint: Some(fingerprint),
            stat_valid: true,
            native_hidden: false,
        });
    }

    hash_stable_file(file, fingerprint, display, scan_started_at)
}

fn hash_stable_file(
    file: cap_std::fs::File,
    before: FileFingerprint,
    display: &str,
    scan_started_at: FileTime,
) -> Result<WorkingNode> {
    #[cfg(test)]
    HASH_STABLE_FILE_CALLS.with(|count| count.set(count.get() + 1));
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 128 * 1_024];
    let mut size = 0_u64;
    loop {
        let count = reader.read(&mut buffer).at("read", display)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size = size.checked_add(count as u64).ok_or_else(|| {
            LocalError::Invalid(format!("file size overflow while reading {display}"))
        })?;
    }
    let after = reader.get_ref().metadata().at("inspect", display)?;
    if !after.file_type().is_file() || is_cap_link_or_reparse(&after) {
        return Err(LocalError::Invalid(format!(
            "unsupported filesystem entry: {display}"
        )));
    }
    let after = file_fingerprint(reader.get_ref(), &after, display)?;
    if before != after || size != after.size {
        return Err(LocalError::Invalid(format!(
            "file changed while being read: {display}"
        )));
    }
    Ok(WorkingNode {
        kind: NodeKind::File,
        digest_hex: Some(hex::encode(hasher.finalize())),
        size: Some(size),
        identity: after.identity,
        fingerprint: Some(after),
        stat_valid: fingerprint_is_old(after, scan_started_at),
        native_hidden: false,
    })
}

pub(crate) fn file_fingerprint(
    file: &cap_std::fs::File,
    metadata: &cap_std::fs::Metadata,
    display: &str,
) -> Result<FileFingerprint> {
    let (modified, changed) = file_times(file, metadata, display)?;
    Ok(FileFingerprint {
        identity: file_identity(file, metadata)?,
        size: metadata.len(),
        modified,
        changed,
    })
}

pub(crate) fn file_identity(
    file: &cap_std::fs::File,
    metadata: &cap_std::fs::Metadata,
) -> Result<FileIdentity> {
    Ok(FileIdentity {
        device_id: metadata.dev(),
        file_id: metadata.ino(),
        incarnation: filesystem_incarnation(file, metadata)?,
    })
}

pub(crate) fn directory_identity(
    directory: &Dir,
    metadata: &cap_std::fs::Metadata,
) -> Result<FileIdentity> {
    Ok(FileIdentity {
        device_id: metadata.dev(),
        file_id: metadata.ino(),
        incarnation: filesystem_incarnation(directory, metadata)?,
    })
}

#[cfg(target_os = "linux")]
fn filesystem_incarnation<T: std::os::fd::AsFd>(
    handle: &T,
    _metadata: &cap_std::fs::Metadata,
) -> Result<Option<FileTime>> {
    use rustix::fs::{AtFlags, StatxFlags, statx};

    let stat = match statx(handle.as_fd(), "", AtFlags::EMPTY_PATH, StatxFlags::BTIME) {
        Ok(stat) => stat,
        Err(_) => return Ok(None),
    };
    if stat.stx_mask & StatxFlags::BTIME.bits() == 0 {
        return Ok(None);
    }
    Ok(Some(FileTime {
        seconds: stat.stx_btime.tv_sec,
        nanos: i64::from(stat.stx_btime.tv_nsec),
    }))
}

#[cfg(not(target_os = "linux"))]
fn filesystem_incarnation<T>(
    _handle: &T,
    metadata: &cap_std::fs::Metadata,
) -> Result<Option<FileTime>> {
    metadata
        .created()
        .ok()
        .map(|created| normalized_time(created.into_std()))
        .transpose()
}

#[cfg(unix)]
fn file_times(
    _file: &cap_std::fs::File,
    metadata: &cap_std::fs::Metadata,
    _display: &str,
) -> Result<(FileTime, Option<FileTime>)> {
    Ok((
        FileTime {
            seconds: cap_fs_ext::OsMetadataExt::mtime(metadata),
            nanos: cap_fs_ext::OsMetadataExt::mtime_nsec(metadata),
        },
        Some(FileTime {
            seconds: cap_fs_ext::OsMetadataExt::ctime(metadata),
            nanos: cap_fs_ext::OsMetadataExt::ctime_nsec(metadata),
        }),
    ))
}

#[cfg(windows)]
fn file_times(
    file: &cap_std::fs::File,
    metadata: &cap_std::fs::Metadata,
    display: &str,
) -> Result<(FileTime, Option<FileTime>)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
    };

    let mut info: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileBasicInfo,
            std::ptr::addr_of_mut!(info).cast(),
            u32::try_from(std::mem::size_of::<FILE_BASIC_INFO>())
                .expect("FILE_BASIC_INFO fits u32"),
        )
    };
    if result == 0 {
        return Ok((
            normalized_time(
                metadata
                    .modified()
                    .at("read modification time", display)?
                    .into_std(),
            )?,
            None,
        ));
    }
    Ok((
        windows_time(info.LastWriteTime)?,
        windows_time(info.ChangeTime).ok(),
    ))
}

#[cfg(windows)]
fn windows_time(ticks: i64) -> Result<FileTime> {
    const TICKS_PER_SECOND: i64 = 10_000_000;
    const WINDOWS_TO_UNIX_SECONDS: i64 = 11_644_473_600;
    let seconds = ticks
        .div_euclid(TICKS_PER_SECOND)
        .checked_sub(WINDOWS_TO_UNIX_SECONDS)
        .ok_or_else(|| LocalError::Invalid("file timestamp is out of range".to_string()))?;
    Ok(FileTime {
        seconds,
        nanos: ticks.rem_euclid(TICKS_PER_SECOND) * 100,
    })
}

#[cfg(not(any(unix, windows)))]
fn file_times(
    _file: &cap_std::fs::File,
    metadata: &cap_std::fs::Metadata,
    display: &str,
) -> Result<(FileTime, Option<FileTime>)> {
    Ok((
        normalized_time(
            metadata
                .modified()
                .at("read modification time", display)?
                .into_std(),
        )?,
        None,
    ))
}

fn normalized_time(time: SystemTime) -> Result<FileTime> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => Ok(FileTime {
            seconds: i64::try_from(duration.as_secs())
                .map_err(|_| LocalError::Invalid("file timestamp is out of range".to_string()))?,
            nanos: i64::from(duration.subsec_nanos()),
        }),
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs())
                .map_err(|_| LocalError::Invalid("file timestamp is out of range".to_string()))?;
            if duration.subsec_nanos() == 0 {
                Ok(FileTime {
                    seconds: -seconds,
                    nanos: 0,
                })
            } else {
                Ok(FileTime {
                    seconds: seconds
                        .checked_add(1)
                        .and_then(|value| value.checked_neg())
                        .ok_or_else(|| {
                            LocalError::Invalid("file timestamp is out of range".to_string())
                        })?,
                    nanos: i64::from(1_000_000_000 - duration.subsec_nanos()),
                })
            }
        }
    }
}

fn fingerprint_is_old(fingerprint: FileFingerprint, scan_started_at: FileTime) -> bool {
    let threshold = FileTime {
        seconds: scan_started_at.seconds.saturating_sub(2),
        nanos: scan_started_at.nanos,
    };
    fingerprint.modified < threshold && fingerprint.changed.is_some_and(|time| time < threshold)
}

#[cfg(test)]
thread_local! {
    static HASH_STABLE_FILE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn has_tracked_descendant(tracked: &BTreeSet<String>, path: &str) -> bool {
    let prefix = format!("{path}/");
    tracked
        .range(prefix.clone()..)
        .next()
        .is_some_and(|candidate| candidate.starts_with(&prefix))
}

fn is_reserved(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|suffix| Uuid::parse_str(suffix).ok())
        .is_some()
}

pub(crate) fn diff(
    committed: &BTreeMap<String, CommittedNode>,
    working: &BTreeMap<String, WorkingNode>,
    matched: &HashMap<ItemId, String>,
    working_ids: &HashMap<String, ItemId>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    let mut claimed_working = HashSet::new();
    for (old_path, old) in committed {
        let Some(new_path) = matched.get(&old.item_id) else {
            changes.push(Change {
                kind: ChangeKind::Delete,
                item_type: old.kind.as_item_type().to_owned(),
                path: old_path.clone(),
                source_path: None,
                content_changed: false,
            });
            continue;
        };
        claimed_working.insert(new_path.clone());
        let new = &working[new_path];
        let old_parent = committed_parent_id(old_path, committed);
        let new_parent = working_parent_id(new_path, working_ids);
        let old_name = old_path
            .rsplit('/')
            .next()
            .expect("committed path has a name");
        let new_name = new_path
            .rsplit('/')
            .next()
            .expect("working path has a name");
        let name_changed = new_name != old_name;
        let parent_changed = new_parent.as_ref() != Some(&old_parent);
        let content_changed = old.kind == NodeKind::File && old.plain_digest != new.digest_hex;
        if let Some(kind) = ChangeKind::relocation(parent_changed, name_changed) {
            changes.push(Change {
                kind,
                item_type: new.kind.as_item_type().to_owned(),
                path: new_path.clone(),
                source_path: Some(old_path.clone()),
                content_changed,
            });
        } else if content_changed {
            changes.push(Change {
                kind: ChangeKind::Modify,
                item_type: new.kind.as_item_type().to_owned(),
                path: new_path.clone(),
                source_path: (new_path != old_path).then(|| old_path.clone()),
                content_changed: true,
            });
        }
    }
    for path in working.keys() {
        if !claimed_working.contains(path) {
            changes.push(Change {
                kind: ChangeKind::Add,
                item_type: working[path].kind.as_item_type().to_owned(),
                path: path.clone(),
                source_path: None,
                content_changed: false,
            });
        }
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path).then(left.kind.cmp(&right.kind)));
    changes
}

fn committed_parent_id(path: &str, committed: &BTreeMap<String, CommittedNode>) -> Option<ItemId> {
    path.rsplit_once('/')
        .and_then(|(parent, _)| committed.get(parent))
        .map(|node| node.item_id.clone())
}

fn working_parent_id(path: &str, working_ids: &HashMap<String, ItemId>) -> Option<Option<ItemId>> {
    match path.rsplit_once('/') {
        None => Some(None),
        Some((parent, _)) => working_ids.get(parent).cloned().map(Some),
    }
}

fn path_depth(path: &str) -> usize {
    path.bytes().filter(|byte| *byte == b'/').count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_std::ambient_authority;

    fn identity(incarnation: Option<(i64, i64)>) -> FileIdentity {
        FileIdentity {
            device_id: 7,
            file_id: 11,
            incarnation: incarnation.map(|(seconds, nanos)| FileTime { seconds, nanos }),
        }
    }

    fn working_file(identity: FileIdentity) -> WorkingNode {
        WorkingNode {
            kind: NodeKind::File,
            digest_hex: Some("0".repeat(64)),
            size: Some(0),
            identity,
            fingerprint: None,
            stat_valid: false,
            native_hidden: false,
        }
    }

    #[test]
    fn relocation_kinds_and_markers_are_exact() {
        assert_eq!(ChangeKind::relocation(false, false), None);
        assert_eq!(
            ChangeKind::relocation(false, true),
            Some(ChangeKind::Rename)
        );
        assert_eq!(ChangeKind::relocation(true, false), Some(ChangeKind::Move));
        assert_eq!(
            ChangeKind::relocation(true, true),
            Some(ChangeKind::MoveRename)
        );

        for (kind, content_changed, marker) in [
            (ChangeKind::Rename, false, "RN"),
            (ChangeKind::Rename, true, "M+RN"),
            (ChangeKind::Move, false, "MV"),
            (ChangeKind::Move, true, "M+MV"),
            (ChangeKind::MoveRename, false, "MV+RN"),
            (ChangeKind::MoveRename, true, "M+MV+RN"),
        ] {
            assert_eq!(
                Change {
                    kind,
                    item_type: "FILE".to_string(),
                    path: "new".to_string(),
                    source_path: Some("old".to_string()),
                    content_changed,
                }
                .marker(),
                marker
            );
        }
    }

    #[test]
    fn off_path_identity_requires_the_same_incarnation() {
        let item_id = ItemId::new();
        let committed = BTreeMap::from([(
            "old".to_string(),
            CommittedNode {
                plain_digest: None,
                item_id: item_id.clone(),
                kind: NodeKind::File,
                head_seq: 1,
                blob_id: None,
                size: None,
            },
        )]);
        let record = IdentityRecord {
            item_id: item_id.clone(),
            kind: NodeKind::File,
            identity: identity(Some((1, 0))),
            path: "old".to_string(),
        };

        let reused = BTreeMap::from([("new".to_string(), working_file(identity(Some((2, 0)))))]);
        assert!(
            reconcile_identities(&committed, &reused, std::slice::from_ref(&record)).is_empty()
        );

        let moved = BTreeMap::from([("new".to_string(), working_file(identity(Some((1, 0)))))]);
        assert_eq!(
            reconcile_identities(&committed, &moved, &[record])
                .get(&item_id)
                .map(String::as_str),
            Some("new")
        );
    }

    #[test]
    fn windows_tunneled_creation_time_preserves_source_identity() {
        let displaced_id = ItemId::new();
        let moved_id = ItemId::new();
        let displaced_identity = FileIdentity {
            device_id: 7,
            file_id: 10,
            incarnation: Some(FileTime {
                seconds: 1,
                nanos: 0,
            }),
        };
        let moved_identity = FileIdentity {
            device_id: 7,
            file_id: 11,
            incarnation: Some(FileTime {
                seconds: 2,
                nanos: 0,
            }),
        };
        let tunneled_identity = FileIdentity {
            incarnation: displaced_identity.incarnation,
            ..moved_identity
        };
        let moved_digest = "2".repeat(64);
        let committed = BTreeMap::from([
            (
                "A.txt".to_string(),
                CommittedNode {
                    plain_digest: Some("1".repeat(64)),
                    item_id: displaced_id.clone(),
                    kind: NodeKind::File,
                    head_seq: 1,
                    blob_id: Some(ObjectId::parse(format!("sha256:{}", "1".repeat(64))).unwrap()),
                    size: Some(5),
                },
            ),
            (
                "B.txt".to_string(),
                CommittedNode {
                    plain_digest: Some(moved_digest.clone()),
                    item_id: moved_id.clone(),
                    kind: NodeKind::File,
                    head_seq: 2,
                    blob_id: Some(ObjectId::parse(format!("sha256:{moved_digest}")).unwrap()),
                    size: Some(6),
                },
            ),
        ]);
        let records = vec![
            IdentityRecord {
                item_id: displaced_id.clone(),
                kind: NodeKind::File,
                identity: displaced_identity,
                path: "A.txt".to_string(),
            },
            IdentityRecord {
                item_id: moved_id.clone(),
                kind: NodeKind::File,
                identity: moved_identity,
                path: "B.txt".to_string(),
            },
        ];
        let committed_by_id = committed
            .iter()
            .map(|(path, node)| (node.item_id.clone(), (path, node)))
            .collect::<HashMap<_, _>>();
        let mut working = BTreeMap::from([(
            "A.txt".to_string(),
            WorkingNode {
                kind: NodeKind::File,
                digest_hex: Some(moved_digest),
                size: Some(6),
                identity: tunneled_identity,
                fingerprint: None,
                stat_valid: false,
                native_hidden: false,
            },
        )]);
        let mut matched = HashMap::new();
        let mut claimed_paths = HashSet::new();
        let mut blocked_paths = HashSet::new();
        reconcile_windows_tunneled_relocations(
            &committed_by_id,
            &working,
            &records,
            &mut matched,
            &mut claimed_paths,
            &mut blocked_paths,
        );
        assert_eq!(matched.get(&moved_id).map(String::as_str), Some("A.txt"));
        assert!(!matched.contains_key(&displaced_id));
        assert_eq!(blocked_paths, HashSet::from(["A.txt".to_string()]));

        working.get_mut("A.txt").unwrap().digest_hex = Some("3".repeat(64));
        matched.clear();
        claimed_paths.clear();
        blocked_paths.clear();
        reconcile_windows_tunneled_relocations(
            &committed_by_id,
            &working,
            &records,
            &mut matched,
            &mut claimed_paths,
            &mut blocked_paths,
        );
        assert_eq!(matched.get(&moved_id).map(String::as_str), Some("A.txt"));
        assert!(!matched.contains_key(&displaced_id));
        assert_eq!(blocked_paths, HashSet::from(["A.txt".to_string()]));

        // An untracked destination has no stored incarnation to authenticate
        // tunneling. Its creation time cannot establish the source's identity.
        matched.clear();
        claimed_paths.clear();
        blocked_paths.clear();
        reconcile_windows_tunneled_relocations(
            &committed_by_id,
            &working,
            &records[1..],
            &mut matched,
            &mut claimed_paths,
            &mut blocked_paths,
        );
        assert!(matched.is_empty());
        assert!(claimed_paths.is_empty());
        assert_eq!(blocked_paths, HashSet::from(["A.txt".to_string()]));
    }

    #[test]
    fn unavailable_incarnation_never_establishes_an_off_path_match() {
        let item_id = ItemId::new();
        let committed = BTreeMap::from([(
            "old".to_string(),
            CommittedNode {
                plain_digest: None,
                item_id: item_id.clone(),
                kind: NodeKind::File,
                head_seq: 1,
                blob_id: None,
                size: None,
            },
        )]);
        let record = IdentityRecord {
            item_id,
            kind: NodeKind::File,
            identity: identity(None),
            path: "old".to_string(),
        };
        let working = BTreeMap::from([("new".to_string(), working_file(identity(None)))]);

        assert!(reconcile_identities(&committed, &working, &[record]).is_empty());
    }

    #[test]
    fn exact_previously_observed_path_survives_atomic_replacement() {
        let item_id = ItemId::new();
        let committed = BTreeMap::from([(
            "old".to_string(),
            CommittedNode {
                plain_digest: None,
                item_id: item_id.clone(),
                kind: NodeKind::File,
                head_seq: 1,
                blob_id: None,
                size: None,
            },
        )]);
        let record = IdentityRecord {
            item_id: item_id.clone(),
            kind: NodeKind::File,
            identity: identity(Some((1, 0))),
            path: "new".to_string(),
        };
        let working = BTreeMap::from([("new".to_string(), working_file(identity(Some((2, 0)))))]);

        assert_eq!(
            reconcile_identities(&committed, &working, &[record])
                .get(&item_id)
                .map(String::as_str),
            Some("new")
        );
    }

    #[test]
    fn same_path_same_kind_preserves_ownership_without_an_incarnation() {
        let item_id = ItemId::new();
        let committed = BTreeMap::from([(
            "same".to_string(),
            CommittedNode {
                plain_digest: None,
                item_id: item_id.clone(),
                kind: NodeKind::File,
                head_seq: 1,
                blob_id: None,
                size: None,
            },
        )]);
        let working = BTreeMap::from([("same".to_string(), working_file(identity(None)))]);

        assert_eq!(
            reconcile_identities(&committed, &working, &[])
                .get(&item_id)
                .map(String::as_str),
            Some("same")
        );
    }

    #[tokio::test]
    async fn complete_checkout_status_and_clean_commit_reuse_cached_digests() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        crate::portable::init_repository(root, "author")
            .await
            .unwrap();
        std::fs::write(root.join("document.bin"), b"one").unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("add file").await.unwrap();
        drop(repository);
        // Let both mtime and ctime leave the scanner's racy timestamp window.
        tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
        let repository = Repository::discover(root).await.unwrap();
        assert!(repository.status().await.unwrap().is_empty());
        drop(repository);

        HASH_STABLE_FILE_CALLS.with(|count| count.set(0));
        let repository = Repository::discover(root).await.unwrap();
        assert!(repository.status().await.unwrap().is_empty());
        assert!(matches!(
            repository.commit("unchanged").await.unwrap(),
            crate::commands::CommitOutcome::Clean
        ));
        HASH_STABLE_FILE_CALLS.with(|count| assert_eq!(count.get(), 0));

        std::fs::write(root.join("document.bin"), b"two").unwrap();
        assert_eq!(repository.status().await.unwrap().len(), 1);
        HASH_STABLE_FILE_CALLS.with(|count| assert!(count.get() > 0));
    }

    #[test]
    fn unchanged_clean_and_dirty_files_reuse_the_cached_digest() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(temporary.path().join("document.bin"), b"one").unwrap();
        let root = Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
        let scan_time =
            normalized_time(SystemTime::now() + std::time::Duration::from_secs(10)).unwrap();
        HASH_STABLE_FILE_CALLS.with(|count| count.set(0));

        let first = inspect_file(
            &root,
            Path::new("document.bin"),
            "document.bin",
            None,
            &HashMap::new(),
            scan_time,
        )
        .unwrap();
        HASH_STABLE_FILE_CALLS.with(|count| assert_eq!(count.get(), 1));
        let second = inspect_file(
            &root,
            Path::new("document.bin"),
            "document.bin",
            Some(&first),
            &HashMap::new(),
            scan_time,
        )
        .unwrap();
        assert_eq!(second.digest_hex, first.digest_hex);
        HASH_STABLE_FILE_CALLS.with(|count| assert_eq!(count.get(), 1));

        std::fs::write(temporary.path().join("document.bin"), b"changed").unwrap();
        let dirty = inspect_file(
            &root,
            Path::new("document.bin"),
            "document.bin",
            Some(&second),
            &HashMap::new(),
            scan_time,
        )
        .unwrap();
        assert_ne!(dirty.digest_hex, first.digest_hex);
        HASH_STABLE_FILE_CALLS.with(|count| assert_eq!(count.get(), 2));
        let repeated = inspect_file(
            &root,
            Path::new("document.bin"),
            "document.bin",
            Some(&dirty),
            &HashMap::new(),
            scan_time,
        )
        .unwrap();
        assert_eq!(repeated.digest_hex, dirty.digest_hex);
        HASH_STABLE_FILE_CALLS.with(|count| assert_eq!(count.get(), 2));
    }

    #[test]
    fn candidate_scopes_include_parents_and_remove_descendant_duplicates() {
        assert_eq!(
            incremental_scopes(vec![
                "Folder/a.txt".to_string(),
                "Folder/Nested/b.txt".to_string(),
                "root.txt".to_string(),
            ]),
            vec!["Folder".to_string(), "root.txt".to_string()]
        );
    }

    #[test]
    fn racy_fingerprints_are_never_reused() {
        let identity = FileIdentity {
            device_id: 1,
            file_id: 2,
            incarnation: Some(FileTime {
                seconds: 3,
                nanos: 4,
            }),
        };
        let scan = FileTime {
            seconds: 100,
            nanos: 0,
        };
        let fingerprint = |seconds| FileFingerprint {
            identity,
            size: 3,
            modified: FileTime { seconds, nanos: 0 },
            changed: Some(FileTime { seconds, nanos: 0 }),
        };
        assert!(fingerprint_is_old(fingerprint(97), scan));
        assert!(!fingerprint_is_old(fingerprint(98), scan));
        assert!(!fingerprint_is_old(fingerprint(100), scan));
        assert!(!fingerprint_is_old(fingerprint(101), scan));
    }
}
