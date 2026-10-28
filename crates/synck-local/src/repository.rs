use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(test)]
use std::sync::{LazyLock, Mutex};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};
use serde::{Deserialize, Serialize};
use synck_file_folder::{BlobRef, CurrentItem, Intent, Location, ProfileLifecycleState};
use synck_vcs::config::CoreConfig;
use synck_vcs::naming::compute_name_key;
use synck_vcs::repo::Repo;
use synck_vcs::state::{StateIndexStore, StateProjectorStore};
use synck_vcs::storage::Storage;
use synck_vcs::types::{ObjectId, CommitEventRow, CommitId, ItemId, RepoId};
use synck_vcs_sqlite::SqliteStorage;
use uuid::Uuid;

use crate::chunk_store::{ChunkStore, OBJECT_STORE_FILE, StorageMode};
use crate::error::{IoContext, LocalError, Result};

const FORMAT_VERSION: u32 = 6;
const LOCAL_SCHEMA_VERSION: i64 = 12;
pub(crate) const MONITOR_LOCK_FILE: &str = "monitor.lock";
const PAGE_SIZE: usize = 1_024;
const SQLITE_ITEM_ID_CHUNK_SIZE: usize = 900;

#[cfg(test)]
type RecordedIdLoad = (String, Vec<String>);

#[cfg(test)]
static CURRENT_ITEM_LOADS: LazyLock<Mutex<Vec<RecordedIdLoad>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
#[cfg(test)]
static STATE_INDEX_CATCH_UPS: LazyLock<Mutex<Vec<String>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
const DEFAULT_SYNCKIGNORE: &str = "\
# macOS archive metadata
__MACOSX/

# Windows metadata
Thumbs.db
ehthumbs.db
ehthumbs_vista.db
desktop.ini
/$RECYCLE.BIN/
/System Volume Information/

# Microsoft Office temporary files
~$*

# Editor temporary files
*~
*.swp
*.swo
*.tmp

# Build and dependency caches
node_modules/
__pycache__/
*.pyc
.pytest_cache/
.mypy_cache/
.venv/
.next/
";

const LOCAL_SCHEMA: &str = include_str!("local-schema.sql");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HiddenFilesMode {
    Ignore,
    Track,
}

impl HiddenFilesMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Track => "track",
        }
    }

    pub(crate) fn database_value(self) -> &'static str {
        match self {
            Self::Ignore => "IGNORE",
            Self::Track => "TRACK",
        }
    }

    pub(crate) fn from_database(value: &str) -> Result<Self> {
        match value {
            "IGNORE" => Ok(Self::Ignore),
            "TRACK" => Ok(Self::Track),
            _ => Err(LocalError::Integrity(format!(
                "invalid hidden-files setting {value:?}"
            ))),
        }
    }
}

/// Working-copy commands need a working tree. Name a bare complete repository
/// explicitly instead of reporting the first missing working-copy file.
fn reject_bare_complete_repository(metadata_path: &Path) -> Result<()> {
    let Ok(bytes) = fs::read(metadata_path.join("config.json")) else {
        return Ok(());
    };
    let Ok(config) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(());
    };
    match config.get("format_version").and_then(serde_json::Value::as_i64) {
        Some(6) if config.get("checkout").is_none() => Err(LocalError::Invalid(
            "this is a complete repository without a checkout; read it with synck item and synck repository commands (synck repository log lists its history), or enable file editing with synck repository checkout".to_string(),
        )),
        _ => Ok(()),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) storage_mode: Option<StorageMode>,
    format_version: u32,
    repo_id: RepoId,
    author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authority: Option<crate::portable::Authority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checkout: Option<crate::checkout::Checkout>,
}

impl Config {
    pub(crate) fn complete(
        repo_id: RepoId,
        author: String,
        authority: crate::portable::Authority,
        checkout: crate::checkout::Checkout,
        storage_mode: StorageMode,
    ) -> Self {
        Self {
            format_version: 6,
            storage_mode: Some(storage_mode),
            repo_id,
            author,
            authority: Some(authority),
            checkout: Some(checkout),
        }
    }

    pub(crate) fn format_version(&self) -> u32 {
        self.format_version
    }

    pub(crate) fn repo_id(&self) -> &RepoId {
        &self.repo_id
    }

    pub(crate) fn author(&self) -> &str {
        &self.author
    }

}

pub(crate) struct RepositoryLock {
    file: File,
}

impl RepositoryLock {
    pub(crate) fn acquire(file: File) -> std::result::Result<Arc<Self>, std::fs::TryLockError> {
        file.try_lock()?;
        Ok(Arc::new(Self { file }))
    }
}

impl Drop for RepositoryLock {
    fn drop(&mut self) {
        // Closing alone can leave a Unix flock held by a child between fork
        // and exec. Release it explicitly when the last owning view is gone.
        let _ = self.file.unlock();
    }
}

pub struct Repository {
    root_path: PathBuf,
    metadata_path: PathBuf,
    root: Dir,
    config: Config,
    _lock: Arc<RepositoryLock>,
    storage: SqliteStorage,
    repo: Repo,
    pub(crate) storage_mode: StorageMode,
    hidden_files_mode: HiddenFilesMode,
}

impl Repository {
    /// Open the checkout containing `start`, reporting any repository format
    /// other than 6 with one plain message.
    pub async fn discover_complete(start: &Path) -> Result<Self> {
        let root = discover_root(start)?;
        let file = open_regular_file_nofollow(&root.join(".synck/config.json"))?;
        let config: serde_json::Value = serde_json::from_reader(file)?;
        if config.get("format_version").and_then(|v| v.as_u64()) != Some(6) {
            return Err(LocalError::Invalid(
                "unsupported repository format; format 6 is required".into(),
            ));
        }
        Self::discover(start).await
    }

    pub async fn discover(start: &Path) -> Result<Self> {
        let root_path = discover_root(start)?;
        let metadata_path = root_path.join(".synck");
        reject_bare_complete_repository(&metadata_path)?;
        validate_metadata_layout(&metadata_path)?;
        let metadata_dir = Dir::open_ambient_dir(&metadata_path, ambient_authority())
            .at("open repository metadata", &metadata_path)?;
        #[cfg(test)]
        run_open_lock_hook();
        let lock_path = metadata_path.join("lock");
        let mut lock_options = CapOpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .follow(FollowSymlinks::No);
        let lock = metadata_dir
            .open_with("lock", &lock_options)
            .at("open", &lock_path)?
            .into_std();
        let lock = match RepositoryLock::acquire(lock) {
            Ok(lock) => lock,
            Err(std::fs::TryLockError::WouldBlock) => return Err(LocalError::Busy),
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(LocalError::Io {
                    action: "lock",
                    path: lock_path,
                    source,
                });
            }
        };

        let config_path = metadata_path.join("config.json");
        let mut read_options = CapOpenOptions::new();
        read_options.read(true).follow(FollowSymlinks::No);
        let mut config_file = metadata_dir
            .open_with("config.json", &read_options)
            .at("open", &config_path)?;
        let mut config_bytes = Vec::new();
        std::io::Read::read_to_end(&mut config_file, &mut config_bytes).at("read", &config_path)?;
        // Close the reader before recovery replaces config.json on Windows.
        drop(config_file);
        let config: Config = serde_json::from_slice(&config_bytes)?;
        if config.format_version != FORMAT_VERSION
            || config.checkout.is_none()
            || !config
                .authority
                .is_some_and(|authority| authority != crate::portable::Authority::Follower)
            || config.author.is_empty()
        {
            return Err(LocalError::Invalid(
                "expected a complete format-6 repository with an independent checkout".into(),
            ));
        }

        ensure_monitor_lock(&metadata_path)?;

        cleanup_tmp(&metadata_path.join("tmp"))?;

        let database_path = metadata_path.join("repository.sqlite3");
        let storage = SqliteStorage::new(database_path);
        storage.open_readonly().map_err(|error| {
            LocalError::Integrity(format!("cannot read Core VCS schema: {error}"))
        })?;
        validate_local_schema_version(&storage)?;
        crate::checkout::validate_config(&config)?;
        validate_local_schema(&crate::local_database::open_readonly(&storage)?)?;
        let storage_mode = config.storage_mode.unwrap_or_default();
        let hidden_files_mode = read_hidden_files_mode(&storage, &config.repo_id)?;
        let object_store = ChunkStore::new(metadata_path.join(OBJECT_STORE_FILE), &config.repo_id);
        match storage_mode {
            StorageMode::WholeFile => validate_optional_object_store_path(object_store.path())?,
            StorageMode::Chunked => {
                object_store.recover_incomplete()?;
            }
        }

        let root = Dir::open_ambient_dir(&root_path, ambient_authority())
            .at("open repository root", &root_path)?;
        let core_config = CoreConfig::default();
        let repo = Repo::new(Storage::new(Arc::new(storage.clone())), core_config);

        let mut repository = Self {
            root_path,
            metadata_path,
            root,
            config,
            _lock: lock,
            storage,
            repo,
            storage_mode,
            hidden_files_mode,
        };
        let _ = repository.validate_pending_operations()?;
        repository.recover_name_repairs()?;
        if repository
            .config
            .checkout
            .as_ref()
            .is_some_and(|c| c.pending.is_some())
        {
            repository.finish_checkout_transition().await?;
        }
        repository.recover_pending_working_tree_edit()?;
        repository.recover_pending_move().await?;
        repository.recover_pending_recover().await?;
        repository.recover_pending_revert().await?;
        repository.recover_pending_worktree_restore().await?;
        Ok(repository)
    }

    pub async fn verify_complete(&self) -> Result<crate::portable::VerificationReport> {
        let report = self.portable_view()?.verify().await?;
        self.verify_named_versions().await?;
        self.status().await?;
        Ok(report)
    }

    /// Named versions are labels in each FILE's ref ledger. Canonical
    /// verification checks their bytes; this also checks what they label.
    pub(crate) async fn verify_named_versions(&self) -> Result<()> {
        let mut findings = crate::integrity::Findings::default();
        self.version_findings(&mut findings).await?;
        if findings.is_empty() {
            Ok(())
        } else {
            Err(LocalError::Integrity(findings.summary()))
        }
    }

    pub async fn archive_complete(
        &self,
        destination: &Path,
        mode: Option<StorageMode>,
    ) -> Result<()> {
        self.recover_current().await?;
        let portable = self.portable_view()?;
        portable
            .archive_with_mode(destination, mode.unwrap_or(portable.storage_mode()))
            .await
    }

    pub(crate) fn portable_view(&self) -> Result<crate::portable::PortableRepository> {
        crate::portable::PortableRepository::from_locked_checkout(
            &self.root_path,
            self.storage.clone(),
            Arc::clone(&self._lock),
            &self.config,
        )
    }

    pub(crate) fn from_portable(
        root_path: &Path,
        storage: SqliteStorage,
        lock: Arc<RepositoryLock>,
        config: Config,
    ) -> Result<Self> {
        validate_local_schema_version(&storage)?;
        validate_local_schema(&crate::local_database::open_readonly(&storage)?)?;
        let storage_mode = config.storage_mode.unwrap_or_default();
        let hidden_files_mode = read_hidden_files_mode(&storage, &config.repo_id)?;
        let root_path = root_path
            .canonicalize()
            .at("resolve checkout root", root_path)?;
        let metadata_path = root_path.join(".synck");
        let root = Dir::open_ambient_dir(&root_path, ambient_authority())
            .at("open checkout root", &root_path)?;
        let repo = Repo::new(
            Storage::new(Arc::new(storage.clone())),
            CoreConfig::default(),
        );
        Ok(Self {
            root_path,
            metadata_path,
            root,
            config,
            _lock: lock,
            storage,
            repo,
            storage_mode,
            hidden_files_mode,
        })
    }

    pub(crate) fn publish_checkout_config(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.config)?;
        if bytes.len() > 64 * 1024 {
            return Err(LocalError::Invalid(
                "checkout selection exceeds config size limit".into(),
            ));
        }
        let path = self
            .metadata_path
            .join(format!("config-{}.json", Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .at("create checkout config", &path)?;
        file.write_all(&bytes).at("write checkout config", &path)?;
        file.sync_all().at("sync checkout config", &path)?;
        replace_file(&path, &self.metadata_path.join("config.json"))
            .at("publish checkout config", &path)?;
        sync_directory(&self.metadata_path)
    }

    pub(crate) fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub(crate) fn metadata_path(&self) -> &Path {
        &self.metadata_path
    }

    pub fn repository_root(&self) -> &Path {
        &self.root_path
    }

    pub fn repository_id(&self) -> &RepoId {
        &self.config.repo_id
    }

    pub(crate) fn root(&self) -> &Dir {
        &self.root
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    pub(crate) fn core(&self) -> &Repo {
        &self.repo
    }

    pub(crate) fn storage(&self) -> &SqliteStorage {
        &self.storage
    }

    pub(crate) fn storage_mode(&self) -> StorageMode {
        self.storage_mode
    }

    pub fn hidden_files_mode(&self) -> HiddenFilesMode {
        self.hidden_files_mode
    }

    pub fn set_hidden_files_mode(&mut self, mode: HiddenFilesMode) -> Result<()> {
        if mode == self.hidden_files_mode {
            return Ok(());
        }
        let connection = crate::local_database::open_writable(&self.storage).map_err(|error| {
            LocalError::Integrity(format!("cannot update hidden-files setting: {error}"))
        })?;
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let changed = connection.execute(
            "UPDATE local_repository_settings SET hidden_files = ?1
             WHERE repo_id = ?2 AND hidden_files = ?3",
            params![
                mode.database_value(),
                self.config.repo_id.as_str(),
                self.hidden_files_mode.database_value(),
            ],
        )?;
        if changed != 1 {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(LocalError::Integrity(
                "hidden-files setting changed unexpectedly".to_string(),
            ));
        }
        connection.execute(
            "DELETE FROM local_worktree_scan_state WHERE repo_id = ?1",
            [self.config.repo_id.as_str()],
        )?;
        connection.execute_batch("COMMIT")?;
        self.hidden_files_mode = mode;
        Ok(())
    }

    pub(crate) fn object_store(&self) -> ChunkStore {
        ChunkStore::new(
            self.metadata_path.join(OBJECT_STORE_FILE),
            self.config.repo_id(),
        )
    }

    pub fn set_author(&mut self, author: &str) -> Result<()> {
        if author.is_empty() {
            return Err(LocalError::Invalid("author must not be empty".to_string()));
        }
        if author == self.config.author {
            return Ok(());
        }

        let temporary = self
            .metadata_path
            .join("tmp")
            .join(format!("config-{}", Uuid::new_v4()));
        let destination = self.metadata_path.join("config.json");
        let next = Config {
            format_version: self.config.format_version,
            repo_id: self.config.repo_id.clone(),
            author: author.to_string(),
            ..self.config.clone()
        };
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .at("create repository config", &temporary)?;
            serde_json::to_writer_pretty(&mut file, &next)?;
            file.write_all(b"\n")
                .at("write repository config", &temporary)?;
            file.sync_all().at("sync repository config", &temporary)?;
            replace_file(&temporary, &destination).at("replace repository config", &destination)?;
            sync_directory(&self.metadata_path.join("tmp"))?;
            sync_directory(&self.metadata_path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        self.config = next;
        Ok(())
    }

    pub(crate) async fn recover_current(&self) -> Result<()> {
        loop {
            let count = self
                .repo
                .sequence_repo_activity(self.config.repo_id.clone(), PAGE_SIZE)
                .await?;
            if count < PAGE_SIZE {
                break;
            }
        }
        self.catch_up_projection().await?;
        Ok(())
    }

    pub(crate) async fn recover(&self) -> Result<()> {
        self.recover_current().await?;
        self.catch_up_state_index().await
    }

    fn load_current_file_folder_items(
        &self,
        connection: &Connection,
        item_ids: &BTreeSet<ItemId>,
    ) -> Result<BTreeMap<ItemId, CurrentItem>> {
        if item_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        #[cfg(test)]
        CURRENT_ITEM_LOADS.lock().unwrap().push((
            self.config.repo_id.to_string(),
            item_ids.iter().map(ToString::to_string).collect(),
        ));
        let mut current = BTreeMap::new();
        for chunk in item_ids
            .iter()
            .collect::<Vec<_>>()
            .chunks(SQLITE_ITEM_ID_CHUNK_SIZE)
        {
            let placeholders = (2..chunk.len() + 2)
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT i.item_id, i.item_type, i.parent_folder_id, i.name, i.name_key,
                    i.head_seq, i.lifecycle_state, lower(hex(c.commit_id)),
                    f.blob_id, f.size_bytes
             FROM vcs_items i
             JOIN vcs_events e
               ON e.repo_id = i.repo_id AND e.item_id = i.item_id
              AND e.item_seq = i.head_seq
             JOIN vcs_commits c
               ON c.repo_id = e.repo_id AND c.commit_pk = e.commit_pk
             LEFT JOIN local_file_heads f
               ON f.repo_id = i.repo_id AND f.item_id = i.item_id
             WHERE i.repo_id = ?1 AND i.item_type IN ('FILE', 'FOLDER')
               AND i.item_id IN ({placeholders})
             ORDER BY i.item_id"
            );
            let parameters = std::iter::once(Value::Text(self.config.repo_id.to_string()))
                .chain(chunk.iter().map(|item_id| Value::Text(item_id.to_string())));
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(parameters), current_item_columns)?;
            for row in rows {
                let item = parse_current_item(row?)?;
                current.insert(item.item_id.clone(), item);
            }
        }
        if current.len() != item_ids.len() {
            let missing = item_ids
                .iter()
                .find(|item_id| !current.contains_key(*item_id))
                .expect("different set sizes have a missing requested item");
            return Err(LocalError::Integrity(format!(
                "missing current FILE/FOLDER item {missing}"
            )));
        }
        Ok(current)
    }

    pub(crate) fn current_file_folder_context(
        &self,
        intents: &mut [Intent],
    ) -> Result<BTreeMap<ItemId, CurrentItem>> {
        let connection = crate::local_database::open_readonly(&self.storage).map_err(|error| {
            LocalError::Invalid(format!("cannot read current FILE/FOLDER context: {error}"))
        })?;
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
        let mut lifecycle_roots = Vec::new();
        for intent in intents.iter() {
            match intent {
                Intent::CreateFolder { location, .. } | Intent::CreateFile { location, .. } => {
                    if let Some(parent_id) = &location.parent_item_id
                        && !created.contains(parent_id)
                    {
                        required.insert(parent_id.clone());
                    }
                }
                Intent::UpdateFolder {
                    item_id, location, ..
                } => {
                    required.insert(item_id.clone());
                    if let Some(parent_id) = &location.parent_item_id
                        && !created.contains(parent_id)
                    {
                        required.insert(parent_id.clone());
                    }
                }
                Intent::UpdateFile {
                    item_id, location, ..
                } => {
                    required.insert(item_id.clone());
                    if let Some(parent_id) = location
                        .as_ref()
                        .and_then(|location| location.parent_item_id.as_ref())
                        && !created.contains(parent_id)
                    {
                        required.insert(parent_id.clone());
                    }
                }
                Intent::Trash { root, descendants }
                | Intent::Restore {
                    root, descendants, ..
                } => {
                    required.insert(root.item_id.clone());
                    required.extend(descendants.iter().map(|item| item.item_id.clone()));
                    lifecycle_roots.push(root.item_id.clone());
                }
            }
        }

        let mut current = self.load_current_file_folder_items(&connection, &required)?;
        let mut closures = BTreeMap::new();
        for root_id in lifecycle_roots {
            let root = current.get(&root_id).ok_or_else(|| {
                LocalError::Integrity(format!("missing lifecycle root {root_id}"))
            })?;
            let descendants = if root.item_type == "FOLDER" {
                self.descendant_ids(&connection, &root_id)?
            } else {
                Vec::new()
            };
            required.extend(descendants.iter().cloned());
            closures.insert(root_id, descendants);
        }
        let missing = required
            .iter()
            .filter(|item_id| !current.contains_key(*item_id))
            .cloned()
            .collect::<BTreeSet<_>>();
        if self.config.format_version == 6 {
            for id in &missing {
                let kind: String = connection.query_row(
                    "SELECT item_type FROM vcs_items WHERE repo_id=?1 AND item_id=?2",
                    params![self.config.repo_id.as_str(), id.as_str()],
                    |row| row.get(0),
                )?;
                if kind != "FILE" && kind != "FOLDER" {
                    return Err(LocalError::Invalid(
                        "operation would affect non-filesystem items".into(),
                    ));
                }
            }
        }
        current.extend(self.load_current_file_folder_items(&connection, &missing)?);

        for intent in intents.iter_mut() {
            let Intent::Trash {
                root, descendants, ..
            } = intent
            else {
                continue;
            };
            let mut expected = closures[&root.item_id]
                .iter()
                .filter_map(|item_id| {
                    let item = &current[item_id];
                    (item.lifecycle_state == ProfileLifecycleState::Live).then(|| {
                        synck_file_folder::ItemBase {
                            item_id: item_id.clone(),
                            base_seq: item.head_seq,
                        }
                    })
                })
                .collect::<Vec<_>>();
            expected.sort_by(|left, right| left.item_id.cmp(&right.item_id));
            if !descendants.is_empty() && descendants != &expected {
                return Err(LocalError::Invalid(
                    "LIFECYCLE_MISMATCH: trash descendants do not match current closure"
                        .to_string(),
                ));
            }
            *descendants = expected;
        }

        if self.config.format_version == 6 {
            let visible = self.checkout_visible_item_ids(&connection)?;
            for intent in intents.iter() {
                let target = match intent {
                    Intent::UpdateFile { item_id, .. } | Intent::UpdateFolder { item_id, .. } => {
                        Some(item_id)
                    }
                    Intent::Trash { root, descendants } => {
                        if descendants
                            .iter()
                            .any(|item| !visible.contains(&item.item_id))
                        {
                            return Err(LocalError::Invalid(
                                "folder deletion would affect omitted or non-filesystem items"
                                    .into(),
                            ));
                        }
                        Some(&root.item_id)
                    }
                    _ => None,
                };
                if let Some(id) = target {
                    let mode: String = connection.query_row(
                        "SELECT sequencing_mode FROM vcs_items WHERE repo_id=?1 AND item_id=?2",
                        params![self.config.repo_id.as_str(), id.as_str()],
                        |row| row.get(0),
                    )?;
                    if mode != "LOCAL" {
                        return Err(LocalError::Invalid(
                            "externally sequenced items cannot be edited in a local checkout"
                                .into(),
                        ));
                    }
                }
            }
        }

        let mut ancestor_frontier = BTreeSet::new();
        let mut namespace_keys = BTreeSet::new();
        for intent in intents.iter() {
            match intent {
                Intent::CreateFolder { location, .. }
                | Intent::CreateFile { location, .. }
                | Intent::UpdateFolder { location, .. } => {
                    record_final_location(
                        location,
                        &created,
                        &mut ancestor_frontier,
                        &mut namespace_keys,
                    )?;
                }
                Intent::UpdateFile {
                    location: Some(location),
                    ..
                } => {
                    record_final_location(
                        location,
                        &created,
                        &mut ancestor_frontier,
                        &mut namespace_keys,
                    )?;
                }
                Intent::Restore {
                    root,
                    trash_commit_id,
                    location,
                    ..
                } => {
                    let root_item = &current[&root.item_id];
                    let root_location = location.clone().unwrap_or_else(|| Location {
                        parent_item_id: root_item.parent_item_id.clone(),
                        name: root_item.name.clone(),
                    });
                    record_final_location(
                        &root_location,
                        &created,
                        &mut ancestor_frontier,
                        &mut namespace_keys,
                    )?;
                    for item_id in &closures[&root.item_id] {
                        let item = &current[item_id];
                        if item.lifecycle_state == ProfileLifecycleState::Trash
                            && item.trash_commit_id.as_ref() == Some(trash_commit_id)
                        {
                            namespace_keys
                                .insert((item.parent_item_id.clone(), item.name_key.clone()));
                        }
                    }
                }
                Intent::UpdateFile { location: None, .. } | Intent::Trash { .. } => {}
            }
        }

        let mut visited_ancestors = BTreeSet::new();
        while !ancestor_frontier.is_empty() {
            ancestor_frontier.retain(|item_id| {
                !created.contains(item_id) && visited_ancestors.insert(item_id.clone())
            });
            let missing = ancestor_frontier
                .iter()
                .filter(|item_id| !current.contains_key(*item_id))
                .cloned()
                .collect::<BTreeSet<_>>();
            current.extend(self.load_current_file_folder_items(&connection, &missing)?);
            let mut next = BTreeSet::new();
            for item_id in &ancestor_frontier {
                if let Some(parent_id) = current[item_id].parent_item_id.as_ref() {
                    next.insert(parent_id.clone());
                }
            }
            ancestor_frontier = next;
        }

        let occupants = self.namespace_occupant_ids(&connection, &namespace_keys)?;
        let current_ids = current.keys().cloned().collect::<BTreeSet<_>>();
        let missing = occupants
            .difference(&current_ids)
            .cloned()
            .collect::<BTreeSet<_>>();
        current.extend(self.load_current_file_folder_items(&connection, &missing)?);
        Ok(current)
    }

    fn descendant_ids(
        &self,
        connection: &Connection,
        root_item_id: &ItemId,
    ) -> Result<Vec<ItemId>> {
        let mut statement = connection.prepare(
            "WITH RECURSIVE descendants(item_id) AS (
                 SELECT item_id FROM vcs_items
                 WHERE repo_id = ?1 AND parent_folder_id = ?2
                   AND lifecycle_state <> 'TOMBSTONED'
                 UNION
                 SELECT child.item_id FROM vcs_items child
                 JOIN descendants parent
                   ON child.repo_id = ?1 AND child.parent_folder_id = parent.item_id
                 WHERE child.lifecycle_state <> 'TOMBSTONED'
             )
             SELECT item_id FROM descendants",
        )?;
        let rows = statement.query_map(
            params![self.config.repo_id.as_str(), root_item_id.as_str()],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| {
            let value = row?;
            ItemId::parse(&value).map_err(|error| {
                LocalError::Integrity(format!("invalid stored item ID {value:?}: {error}"))
            })
        })
        .collect()
    }

    fn namespace_occupant_ids(
        &self,
        connection: &Connection,
        keys: &BTreeSet<(Option<ItemId>, String)>,
    ) -> Result<BTreeSet<ItemId>> {
        let mut occupants = BTreeSet::new();
        for chunk in keys.iter().collect::<Vec<_>>().chunks(400) {
            if chunk.is_empty() {
                continue;
            }
            let predicates = (0..chunk.len())
                .map(|index| {
                    let parent = index * 2 + 2;
                    let name = parent + 1;
                    format!("(parent_folder_id IS ?{parent} AND name_key = ?{name})")
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            let sql = format!(
                "SELECT item_id FROM vcs_items
                 WHERE repo_id = ?1 AND lifecycle_state = 'LIVE'
                   AND item_type IN ('FILE', 'FOLDER') AND ({predicates})"
            );
            let mut parameters = vec![Value::Text(self.config.repo_id.to_string())];
            for (parent_id, name_key) in chunk {
                parameters.push(match parent_id {
                    Some(item_id) => Value::Text(item_id.to_string()),
                    None => Value::Null,
                });
                parameters.push(Value::Text(name_key.clone()));
            }
            let mut statement = connection.prepare(&sql)?;
            let rows =
                statement.query_map(params_from_iter(parameters), |row| row.get::<_, String>(0))?;
            for row in rows {
                let value = row?;
                occupants.insert(ItemId::parse(&value).map_err(|error| {
                    LocalError::Integrity(format!("invalid stored item ID {value:?}: {error}"))
                })?);
            }
        }
        Ok(occupants)
    }

    pub(crate) async fn catch_up_state_index(&self) -> Result<()> {
        let mut store = self
            .storage
            .state_index_store(self.config.repo_id.clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        self.catch_up_state_index_in(&mut store).await
    }

    pub(crate) async fn catch_up_state_index_in<S: StateProjectorStore>(
        &self,
        store: &mut S,
    ) -> Result<()> {
        #[cfg(test)]
        STATE_INDEX_CATCH_UPS
            .lock()
            .unwrap()
            .push(self.config.repo_id.to_string());
        synck_vcs::state::catch_up_state_index(&self.repo, &self.config.repo_id, store, None)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn state_index_catch_ups(&self) -> usize {
        STATE_INDEX_CATCH_UPS
            .lock()
            .unwrap()
            .iter()
            .filter(|repo_id| *repo_id == self.config.repo_id.as_str())
            .count()
    }

    pub async fn rebuild_state_index(&self) -> Result<()> {
        let mut store = self
            .storage
            .state_index_store(self.config.repo_id.clone())
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        store
            .remove_head()
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        synck_vcs::state::catch_up_state_index(&self.repo, &self.config.repo_id, &mut store, None)
            .await
            .map_err(|error| LocalError::Integrity(error.to_string()))?;
        Ok(())
    }

    async fn catch_up_projection(&self) -> Result<()> {
        let mut connection = crate::local_database::open_writable(&self.storage).map_err(|error| {
            LocalError::Invalid(format!("cannot open repository database: {error}"))
        })?;
        let transaction = connection.transaction()?;
        let applied: Option<i64> = transaction
            .query_row(
                "SELECT applied_repo_seq FROM local_projection_state WHERE repo_id = ?1",
                [self.config.repo_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let mut last_repo_seq = applied.unwrap_or(0);
        if applied.is_none() {
            transaction.execute(
                "DELETE FROM local_file_heads WHERE repo_id = ?1",
                [self.config.repo_id.as_str()],
            )?;
        }
        let mut next_repo_seq = last_repo_seq.saturating_add(1);
        loop {
            let commits = self
                .repo
                .list_commits_by_repo_seq(
                    self.config.repo_id.clone(),
                    next_repo_seq,
                    None,
                    PAGE_SIZE,
                )
                .await?;
            if commits.is_empty() {
                break;
            }
            for commit in &commits {
                let repo_seq = commit.repo_seq.ok_or_else(|| {
                    LocalError::Integrity("repository commit is not sequenced".to_string())
                })?;
                let event_limit = usize::try_from(commit.event_count)
                    .ok()
                    .and_then(|count| count.checked_add(1))
                    .ok_or_else(|| {
                        LocalError::Integrity("commit event count is invalid".to_string())
                    })?;
                let events = self
                    .repo
                    .list_commit_events(
                        self.config.repo_id.clone(),
                        commit.commit_id.clone(),
                        event_limit,
                    )
                    .await?;
                if !events.is_empty() {
                    self.apply_pending_projection_commit(&transaction, &events)
                        .await?;
                }
                last_repo_seq = repo_seq;
                next_repo_seq = repo_seq.checked_add(1).ok_or_else(|| {
                    LocalError::Integrity("repository sequence overflow".to_string())
                })?;
            }
            if commits.len() < PAGE_SIZE {
                break;
            }
        }
        transaction.execute(
            "INSERT INTO local_projection_state(repo_id, applied_repo_seq) VALUES (?1, ?2)
             ON CONFLICT(repo_id) DO UPDATE SET applied_repo_seq = excluded.applied_repo_seq",
            params![self.config.repo_id.as_str(), last_repo_seq],
        )?;
        transaction.commit()?;
        Ok(())
    }

    async fn apply_pending_projection_commit(
        &self,
        transaction: &rusqlite::Transaction<'_>,
        events: &[CommitEventRow],
    ) -> Result<()> {
        for event in events {
            let CommitEventRow::Item(event) = event else {
                continue;
            };
            if event.event_type != "FILE::SET_CONTENT" {
                continue;
            }
            let proposal: synck_vcs::types::EventProposal = event.proposal()?;
            let Some(blob_id) = proposal.set_content.as_ref().and_then(|content| content.only("bytes"))
            else {
                continue;
            };
            let size = proposal
                .set_metadata.as_ref().and_then(synck_vcs::metadata::MetadataRef::value)
                .and_then(|m| m.get("content"))
                .and_then(|m| m.get("size"))
                .and_then(serde_json::Value::as_i64)
                .ok_or_else(|| LocalError::Integrity("FILE content size is missing".into()))?;
            transaction.execute("INSERT INTO local_file_heads(repo_id,item_id,item_seq,blob_id,size_bytes) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(repo_id,item_id) DO UPDATE SET item_seq=excluded.item_seq,blob_id=excluded.blob_id,size_bytes=excluded.size_bytes",
                params![self.config.repo_id.as_str(),event.item_id.as_str(),event.item_seq,blob_id.as_str(),size])?;
        }
        Ok(())
    }
}

type CurrentItemColumns = (
    String,
    String,
    Option<String>,
    String,
    String,
    i64,
    String,
    String,
    Option<String>,
    Option<i64>,
);

fn current_item_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<CurrentItemColumns> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn parse_current_item(columns: CurrentItemColumns) -> Result<CurrentItem> {
    let (
        item_id,
        item_type,
        parent_item_id,
        name,
        name_key,
        head_seq,
        lifecycle,
        head_commit_digest,
        blob_id,
        size,
    ) = columns;
    let item_id = ItemId::parse(&item_id).map_err(|error| {
        LocalError::Integrity(format!("invalid stored item ID {item_id:?}: {error}"))
    })?;
    let parent_item_id = parent_item_id
        .map(|value| {
            ItemId::parse(&value).map_err(|error| {
                LocalError::Integrity(format!("invalid stored parent item ID {value:?}: {error}"))
            })
        })
        .transpose()?;
    let lifecycle_state = match lifecycle.as_str() {
        "LIVE" => ProfileLifecycleState::Live,
        "TRASH" => ProfileLifecycleState::Trash,
        "TOMBSTONED" => ProfileLifecycleState::Tombstoned,
        other => {
            return Err(LocalError::Integrity(format!(
                "invalid stored lifecycle state {other:?}"
            )));
        }
    };
    let trash_commit_id = (lifecycle_state == ProfileLifecycleState::Trash)
        .then(|| CommitId::parse(format!("sha256:{head_commit_digest}")))
        .transpose()
        .map_err(|error| LocalError::Integrity(format!("invalid trash commit ID: {error}")))?;
    let content = match item_type.as_str() {
        "FILE" => Some(BlobRef {
            blob_id: ObjectId::parse(blob_id.ok_or_else(|| {
                LocalError::Integrity(format!("FILE {item_id} has no projected blob"))
            })?)
            .map_err(|error| {
                LocalError::Integrity(format!("FILE {item_id} has invalid blob: {error}"))
            })?,
            size: u64::try_from(size.ok_or_else(|| {
                LocalError::Integrity(format!("FILE {item_id} has no projected size"))
            })?)
            .map_err(|_| LocalError::Integrity(format!("FILE {item_id} has negative size")))?,
        }),
        "FOLDER" => {
            if blob_id.is_some() || size.is_some() {
                return Err(LocalError::Integrity(format!(
                    "FOLDER {item_id} has projected content"
                )));
            }
            None
        }
        other => {
            return Err(LocalError::Integrity(format!(
                "requested item {item_id} has unsupported type {other}"
            )));
        }
    };
    Ok(CurrentItem {
        item_id,
        item_type,
        parent_item_id,
        name,
        name_key,
        head_seq,
        lifecycle_state,
        trash_commit_id,
        content,
    })
}

fn record_final_location(
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
    let name_key = compute_name_key(&location.name)
        .map_err(|error| LocalError::Invalid(format!("invalid item name: {error}")))?;
    namespace_keys.insert((location.parent_item_id.clone(), name_key));
    Ok(())
}

pub fn create_default_synckignore(root: &Path) -> Result<()> {
    let path = root.join(".synckignore");
    let mut file = match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return crate::worktree::validate_synckignore(root);
        }
        Err(error) => return Err(error).at("create", &path),
    };
    let result = file
        .write_all(DEFAULT_SYNCKIGNORE.as_bytes())
        .at("write", &path)
        .and_then(|()| file.sync_all().at("sync", &path));
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result?;
    sync_directory(root)?;
    crate::worktree::validate_synckignore(root)
}

pub(crate) fn discover_root(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .at("resolve current directory", start.to_path_buf())?;
    for directory in start.ancestors() {
        let candidate = directory.join(".synck");
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || is_link_or_reparse(&metadata) {
                    return Err(LocalError::Invalid(format!(
                        "invalid repository metadata entry {}",
                        candidate.display()
                    )));
                }
                return Ok(directory.to_path_buf());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect",
                    path: candidate,
                    source,
                });
            }
        }
    }
    Err(LocalError::Invalid(
        "not inside a Synck repository".to_string(),
    ))
}

fn validate_metadata_layout(metadata: &Path) -> Result<()> {
    for name in ["config.json", "repository.sqlite3", "lock"] {
        let path = metadata.join(name);
        let entry = fs::symlink_metadata(&path).at("inspect", &path)?;
        if !entry.file_type().is_file() || is_link_or_reparse(&entry) {
            return Err(LocalError::Invalid(format!(
                "repository entry is not a regular file: {}",
                path.display()
            )));
        }
    }
    for name in ["objects", "tmp"] {
        let path = metadata.join(name);
        let entry = fs::symlink_metadata(&path).at("inspect", &path)?;
        if !entry.file_type().is_dir() || is_link_or_reparse(&entry) {
            return Err(LocalError::Invalid(format!(
                "repository entry is not a directory: {}",
                path.display()
            )));
        }
    }
    validate_optional_object_store_path(&metadata.join(OBJECT_STORE_FILE))?;
    let monitor_lock = metadata.join(MONITOR_LOCK_FILE);
    match fs::symlink_metadata(&monitor_lock) {
        Ok(entry) if entry.file_type().is_file() && !is_link_or_reparse(&entry) => {}
        Ok(_) => {
            return Err(LocalError::Invalid(format!(
                "repository monitor lock is not a regular file: {}",
                monitor_lock.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect repository monitor lock",
                path: monitor_lock,
                source,
            });
        }
    }
    let monitor_socket = metadata.join("monitor.sock");
    match fs::symlink_metadata(&monitor_socket) {
        #[cfg(unix)]
        Ok(entry) => {
            use std::os::unix::fs::FileTypeExt;
            if !entry.file_type().is_socket() || is_link_or_reparse(&entry) {
                return Err(LocalError::Invalid(format!(
                    "repository monitor endpoint is not a socket: {}",
                    monitor_socket.display()
                )));
            }
        }
        #[cfg(not(unix))]
        Ok(_) => {
            return Err(LocalError::Invalid(format!(
                "unexpected repository monitor endpoint: {}",
                monitor_socket.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect repository monitor endpoint",
                path: monitor_socket,
                source,
            });
        }
    }
    Ok(())
}

fn ensure_monitor_lock(metadata: &Path) -> Result<()> {
    let path = metadata.join(MONITOR_LOCK_FILE);
    match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(file) => {
            file.sync_all().at("sync", &path)?;
            sync_directory(metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let entry = fs::symlink_metadata(&path).at("inspect", &path)?;
            if !entry.file_type().is_file() || is_link_or_reparse(&entry) {
                return Err(LocalError::Invalid(format!(
                    "repository monitor lock is not a regular file: {}",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(source) => Err(LocalError::Io {
            action: "create repository monitor lock",
            path,
            source,
        }),
    }
}

fn validate_optional_object_store_path(path: &Path) -> Result<()> {
    let present = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || is_link_or_reparse(&metadata) {
                return Err(LocalError::Invalid(format!(
                    "repository object database is not a regular file: {}",
                    path.display()
                )));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect object database",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    for suffix in ["-wal", "-shm"] {
        let mut sidecar_name = path.as_os_str().to_os_string();
        sidecar_name.push(suffix);
        let sidecar = PathBuf::from(sidecar_name);
        match fs::symlink_metadata(&sidecar) {
            Ok(metadata)
                if present && metadata.file_type().is_file() && !is_link_or_reparse(&metadata) => {}
            Ok(_) => {
                return Err(LocalError::Invalid(format!(
                    "repository object database sidecar is invalid: {}",
                    sidecar.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect object database sidecar",
                    path: sidecar,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn read_hidden_files_mode(storage: &SqliteStorage, repo_id: &RepoId) -> Result<HiddenFilesMode> {
    let connection = crate::local_database::open_readonly(storage).map_err(|error| {
        LocalError::Integrity(format!("cannot read hidden-files setting: {error}"))
    })?;
    let row: Option<(String, String)> = connection
        .query_row(
            "SELECT repo_id, hidden_files FROM local_repository_settings",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM local_repository_settings",
        [],
        |row| row.get(0),
    )?;
    let (stored_repo_id, value) = row.ok_or_else(|| {
        LocalError::Integrity("local repository settings row is missing".to_string())
    })?;
    if count != 1 || stored_repo_id != repo_id.as_str() {
        return Err(LocalError::Integrity(
            "local repository settings do not match repository".to_string(),
        ));
    }
    HiddenFilesMode::from_database(&value)
}

fn validate_local_schema(connection: &Connection) -> Result<()> {
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(LOCAL_SCHEMA)?;
    if local_schema_objects(connection)? != local_schema_objects(&expected)? {
        return Err(LocalError::Integrity(format!(
            "local SQLite schema definition does not match version {LOCAL_SCHEMA_VERSION}"
        )));
    }
    let unknown_objects: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema
         WHERE sql IS NOT NULL
           AND name NOT GLOB 'sqlite_*'
           AND NOT (
               name GLOB 'local_*' OR tbl_name GLOB 'local_*'
           )",
        [],
        |row| row.get(0),
    )?;
    if unknown_objects != 0 {
        return Err(LocalError::Integrity(
            "local SQLite schema contains unknown objects".to_string(),
        ));
    }
    Ok(())
}

fn validate_local_schema_version(storage: &SqliteStorage) -> Result<()> {
    let connection = crate::local_database::open_readonly(storage).map_err(|error| {
        LocalError::Integrity(format!("cannot read local repository schema: {error}"))
    })?;
    let version: i64 = connection.query_row(
        "SELECT version FROM local_schema_meta WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if version != LOCAL_SCHEMA_VERSION {
        return Err(LocalError::Invalid(format!(
            "unsupported local schema version {version}"
        )));
    }
    Ok(())
}

fn local_schema_objects(connection: &Connection) -> Result<synck_vcs_sqlite::schema::SchemaShape> {
    Ok(synck_vcs_sqlite::schema::inspect(connection, "local_")?)
}

fn cleanup_tmp(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).at("inspect", path)?;
    if !metadata.file_type().is_dir() || is_link_or_reparse(&metadata) {
        return Err(LocalError::Invalid(format!(
            "repository temporary entry is not a directory: {}",
            path.display()
        )));
    }
    let directory = Dir::open_ambient_dir(path, ambient_authority()).at("open", path)?;
    clear_directory(&directory, path)
}

pub(crate) fn remove_tree_nofollow(path: &Path) -> Result<()> {
    let parent_path = path
        .parent()
        .ok_or_else(|| LocalError::Invalid(format!("cannot remove {}", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| LocalError::Invalid(format!("cannot remove {}", path.display())))?;
    let parent = Dir::open_ambient_dir(parent_path, ambient_authority())
        .at("open directory", parent_path)?;
    remove_entry(&parent, name, path)
}

fn clear_directory(directory: &Dir, display: &Path) -> Result<()> {
    let entries = directory
        .entries()
        .at("read directory", display)?
        .collect::<std::io::Result<Vec<_>>>()
        .at("read directory", display)?;
    for entry in entries {
        let name = entry.file_name();
        remove_entry(directory, &name, &display.join(&name))?;
    }
    Ok(())
}

fn remove_entry(parent: &Dir, name: &std::ffi::OsStr, display: &Path) -> Result<()> {
    let metadata = parent.symlink_metadata(name).at("inspect", display)?;
    if metadata.file_type().is_dir() && !is_cap_link_or_reparse(&metadata) {
        let child = parent
            .open_dir_nofollow(name)
            .at("open directory", display)?;
        clear_directory(&child, display)?;
        child.remove_open_dir().at("remove directory", display)?;
    } else if metadata.file_type().is_dir() {
        parent.remove_dir(name).at("remove", display)?;
    } else {
        parent.remove_file_or_symlink(name).at("remove", display)?;
    }
    Ok(())
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use rustix::fs::{CWD, Mode, OFlags, fsync, openat};

        let directory = openat(
            CWD,
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)
        .at("open directory", path)?;
        fsync(directory)
            .map_err(std::io::Error::from)
            .at("sync directory", path)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };

        let directory = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .at("open directory", path)?;
        use std::os::windows::fs::MetadataExt;
        if directory
            .metadata()
            .at("inspect directory", path)?
            .file_attributes()
            & FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(LocalError::Invalid(format!(
                "cannot sync reparse-point directory: {}",
                path.display()
            )));
        }
        directory.sync_all().at("sync directory", path)
    }
    #[cfg(not(any(unix, windows)))]
    Err(LocalError::Invalid(
        "directory durability is unsupported on this platform".to_string(),
    ))
}

pub(crate) fn sync_open_directory(directory: &Dir, display: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        // cap-std opens directories with O_PATH on Linux, and fsync on an
        // O_PATH descriptor fails with EBADF. Sync a real read-only
        // descriptor for the same directory instead.
        use rustix::fs::{Mode, OFlags, openat};
        let handle = openat(
            directory,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)
        .at("open directory for sync", display)?;
        rustix::fs::fsync(handle)
            .map_err(io::Error::from)
            .at("sync directory", display)
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };

        let mut options = CapOpenOptions::new();
        options
            .access_mode(GENERIC_READ | GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .follow(FollowSymlinks::No);
        let handle = directory
            .open_with(".", &options)
            .at("open directory for sync", display)?;
        let metadata = handle
            .metadata()
            .at("inspect directory for sync", display)?;
        if !metadata.file_type().is_dir() || is_cap_link_or_reparse(&metadata) {
            return Err(LocalError::Integrity(format!(
                "directory sync authority is not a regular directory: {}",
                display.display()
            )));
        }
        handle.sync_all().at("sync directory", display)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = directory;
        Err(LocalError::Invalid(
            "directory durability is unsupported on this platform".to_string(),
        ))
    }
}

pub(crate) fn sync_file(path: &Path) -> Result<()> {
    open_regular_file_with_access_nofollow(path, true)?
        .sync_all()
        .at("sync", path)
}

pub(crate) fn open_regular_file_nofollow(path: &Path) -> Result<File> {
    open_regular_file_with_access_nofollow(path, false)
}

fn open_regular_file_with_access_nofollow(path: &Path, writable: bool) -> Result<File> {
    #[cfg(unix)]
    let file = {
        use rustix::fs::{CWD, Mode, OFlags, openat};

        let access = if writable {
            OFlags::RDWR
        } else {
            OFlags::RDONLY
        };
        let descriptor = openat(
            CWD,
            path,
            access | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)
        .at("open", path)?;
        File::from(descriptor)
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;

        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(writable)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        options.open(path).at("open", path)?
    };
    #[cfg(not(any(unix, windows)))]
    return Err(LocalError::Invalid(
        "no-follow file access is unsupported on this platform".to_string(),
    ));

    let metadata = file.metadata().at("inspect", path)?;
    if !metadata.file_type().is_file() || is_link_or_reparse(&metadata) {
        return Err(LocalError::Integrity(format!(
            "entry is not a no-follow regular file: {}",
            path.display()
        )));
    }
    Ok(file)
}

pub(crate) fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

pub(crate) fn is_cap_link_or_reparse(metadata: &cap_std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use cap_std::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

pub(crate) fn rename_between_noreplace(
    source_dir: &Dir,
    source_name: &OsStr,
    destination_dir: &Dir,
    destination_name: &OsStr,
    _source_path: &Path,
    _destination_path: &Path,
) -> io::Result<()> {
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    {
        rustix::fs::renameat_with(
            source_dir,
            source_name,
            destination_dir,
            destination_name,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(io::Error::from)
    }
    #[cfg(windows)]
    {
        rename_between_noreplace_windows(source_dir, source_name, destination_dir, destination_name)
    }
    #[cfg(not(any(
        windows,
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android"
    )))]
    {
        let _ = (source_dir, source_name, destination_dir, destination_name);
        rename_noreplace(_source_path, _destination_path)
    }
}

#[cfg(windows)]
fn rename_between_noreplace_windows(
    source_dir: &Dir,
    source_name: &OsStr,
    destination_dir: &Dir,
    destination_name: &OsStr,
) -> io::Result<()> {
    use cap_std::fs::OpenOptionsExt;
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
    };
    use windows_sys::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, RtlNtStatusToDosError,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let mut options = CapOpenOptions::new();
    options
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .follow(FollowSymlinks::No);
    let source = source_dir.open_with(source_name, &options)?;
    let destination_name = destination_name.encode_wide().collect::<Vec<_>>();
    let header_len = offset_of!(FILE_RENAME_INFORMATION, FileName);
    let byte_len = destination_name
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination name is too long")
        })?;
    let total_len = header_len.checked_add(byte_len).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination name is too long")
    })?;
    let word_len = total_len.div_ceil(size_of::<usize>());
    let mut buffer = vec![0_usize; word_len];
    let buffer_bytes = buffer.as_mut_ptr().cast::<u8>();
    let info = buffer_bytes.cast::<FILE_RENAME_INFORMATION>();
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY: `buffer` is pointer-aligned and includes the fixed fields plus every UTF-16 code
    // unit. The source and destination directory handles remain alive for the synchronous call.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = false;
        (*info).RootDirectory = destination_dir.as_raw_handle();
        (*info).FileNameLength = u32::try_from(byte_len).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination name is too long")
        })?;
        std::ptr::copy_nonoverlapping(
            destination_name.as_ptr().cast::<u8>(),
            buffer_bytes.add(header_len),
            byte_len,
        );
        let status = NtSetInformationFile(
            source.as_raw_handle(),
            &mut io_status,
            buffer.as_ptr().cast(),
            u32::try_from(total_len).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "destination name is too long")
            })?,
            FileRenameInformation,
        );
        if status < 0 {
            let error = io::Error::from_raw_os_error(RtlNtStatusToDosError(status) as i32);
            if error.raw_os_error().is_some_and(|code| {
                code == ERROR_FILE_EXISTS as i32 || code == ERROR_ALREADY_EXISTS as i32
            }) {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, error));
            }
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    {
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            source,
            rustix::fs::CWD,
            destination,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(std::io::Error::from)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

        let source = windows_extended_length_path_units(
            &source.as_os_str().encode_wide().collect::<Vec<_>>(),
        );
        let destination = windows_extended_length_path_units(
            &destination.as_os_str().encode_wide().collect::<Vec<_>>(),
        );
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        windows
    )))]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exclusive directory publication is unsupported on this platform",
    ))
}

pub(crate) fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(source, destination)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };

        let source = windows_extended_length_path_units(
            &source.as_os_str().encode_wide().collect::<Vec<_>>(),
        );
        let destination = windows_extended_length_path_units(
            &destination.as_os_str().encode_wide().collect::<Vec<_>>(),
        );
        if unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(any(windows, test))]
pub(crate) fn windows_extended_length_path_units(path: &[u16]) -> Vec<u16> {
    const BACKSLASH: u16 = b'\\' as u16;
    const SLASH: u16 = b'/' as u16;
    const QUESTION: u16 = b'?' as u16;
    const COLON: u16 = b':' as u16;

    let mut output = Vec::with_capacity(path.len() + 8);
    if path.starts_with(&[BACKSLASH, BACKSLASH, QUESTION, BACKSLASH]) {
        output.extend_from_slice(path);
    } else if path.starts_with(&[BACKSLASH, BACKSLASH]) {
        output.extend("\\\\?\\UNC\\".encode_utf16());
        output.extend_from_slice(&path[2..]);
    } else if path.len() >= 3 && path[1] == COLON && matches!(path[2], BACKSLASH | SLASH) {
        output.extend("\\\\?\\".encode_utf16());
        output.extend(
            path.iter()
                .map(|unit| if *unit == SLASH { BACKSLASH } else { *unit }),
        );
    } else {
        output.extend_from_slice(path);
    }
    output.push(0);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init;
    use synck_vcs::types::{CommitProposal, CreateItem, EventProposal};

    #[tokio::test]
    async fn repository_lock_is_held_until_the_last_view_drops() {
        let temporary = tempfile::tempdir().unwrap();
        init(temporary.path(), "author").await.unwrap();
        let repository = Repository::discover(temporary.path()).await.unwrap();
        let view = repository.portable_view().unwrap();
        drop(repository);
        assert!(matches!(
            Repository::discover(temporary.path()).await,
            Err(LocalError::Busy)
        ));
        drop(view);
        Repository::discover(temporary.path()).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repository_lock_is_released_while_a_forked_child_retains_its_descriptor() {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        struct Child {
            pid: libc::pid_t,
            release: UnixStream,
        }
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.release.shutdown(std::net::Shutdown::Write);
                unsafe {
                    while libc::waitpid(self.pid, std::ptr::null_mut(), 0) < 0
                        && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
                    {
                    }
                }
            }
        }

        let temporary = tempfile::tempdir().unwrap();
        init(temporary.path(), "author").await.unwrap();
        let repository = Repository::discover(temporary.path()).await.unwrap();
        let (release, inherited) = UnixStream::pair().unwrap();
        let release_fd = release.as_raw_fd();
        let inherited_fd = inherited.as_raw_fd();
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // Only async-signal-safe calls are allowed after a multithreaded
            // fork. Keep inherited descriptors alive until the parent is done.
            unsafe {
                libc::close(release_fd);
                let mut byte = 0_u8;
                while libc::read(inherited_fd, (&mut byte as *mut u8).cast(), 1) < 0 {}
                libc::_exit(0);
            }
        }
        drop(inherited);
        let child = Child { pid, release };
        drop(repository);
        let reopened = Repository::discover(temporary.path()).await;
        drop(child);
        reopened.unwrap();
    }

    #[test]
    fn directory_publication_never_replaces_an_existing_target() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("payload"), b"payload").unwrap();
        fs::create_dir(&destination).unwrap();

        let error = rename_noreplace(&source, &destination).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(source.join("payload").is_file());
        assert!(destination.read_dir().unwrap().next().is_none());
    }

    #[test]
    fn durability_helpers_flush_files_and_directories() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("file");
        fs::write(&file, b"content").unwrap();

        sync_file(&file).unwrap();
        sync_directory(temporary.path()).unwrap();
    }

    #[tokio::test]
    async fn content_commit_loads_current_context_for_changed_files_only() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        for index in 0..100 {
            fs::write(root.join(format!("file-{index}")), b"initial").unwrap();
        }
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        let target = repository.committed_tree().await.unwrap()["file-0"]
            .item_id
            .to_string();
        let repo_id = repository.config().repo_id().to_string();
        let before = CURRENT_ITEM_LOADS
            .lock()
            .unwrap()
            .iter()
            .filter(|(loaded_repo_id, _)| loaded_repo_id == &repo_id)
            .count();

        fs::write(root.join("file-0"), b"changed").unwrap();
        repository.commit("edit one").await.unwrap();

        let loads = CURRENT_ITEM_LOADS.lock().unwrap();
        let new_loads = loads
            .iter()
            .filter(|(loaded_repo_id, _)| loaded_repo_id == &repo_id)
            .skip(before)
            .map(|(_, item_ids)| item_ids.clone())
            .collect::<Vec<_>>();
        assert_eq!(new_loads, vec![vec![target]]);
    }

    #[tokio::test]
    async fn live_folder_move_does_not_load_descendants() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::create_dir(root.join("source")).unwrap();
        fs::create_dir(root.join("destination")).unwrap();
        for index in 0..100 {
            fs::write(root.join(format!("source/file-{index}")), b"content").unwrap();
        }
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        let committed = repository.committed_tree().await.unwrap();
        let source_id = committed["source"].item_id.to_string();
        let destination_id = committed["destination"].item_id.to_string();
        let descendant_ids = committed
            .iter()
            .filter(|(path, _)| path.starts_with("source/"))
            .map(|(_, node)| node.item_id.to_string())
            .collect::<BTreeSet<_>>();
        let repo_id = repository.config().repo_id().to_string();
        let before = CURRENT_ITEM_LOADS
            .lock()
            .unwrap()
            .iter()
            .filter(|(loaded_repo_id, _)| loaded_repo_id == &repo_id)
            .count();

        repository
            .move_path(
                root,
                Path::new("source"),
                Path::new("destination/source"),
                "move folder",
            )
            .await
            .unwrap();

        let loads = CURRENT_ITEM_LOADS.lock().unwrap();
        let loaded = loads
            .iter()
            .filter(|(loaded_repo_id, _)| loaded_repo_id == &repo_id)
            .skip(before)
            .flat_map(|(_, item_ids)| item_ids.iter().cloned())
            .collect::<BTreeSet<_>>();
        assert_eq!(loaded, BTreeSet::from([source_id, destination_id]));
        assert!(loaded.is_disjoint(&descendant_ids));
    }

    #[tokio::test]
    async fn projection_assembles_one_commit_across_event_pages() {
        let temporary = tempfile::tempdir().unwrap();
        init(temporary.path(), "author").await.unwrap();
        let repository = Repository::discover(temporary.path()).await.unwrap();
        let item_ids = (0..PAGE_SIZE + 1)
            .map(|_| ItemId::new())
            .collect::<Vec<_>>();
        let creates = item_ids
            .iter()
            .enumerate()
            .map(|(index, item_id)| EventProposal {
                item_id: Some(item_id.clone()),
                event_type: "FOLDER::CREATE".to_string(),
                set_metadata: None,
                set_content: None,
                create_item: Some(CreateItem {
                    item_type: "FOLDER".to_string(),
                    parent_folder_id: None,
                    name: format!("folder-{index}"),
                }),
                set_parent: None,
                set_name: None,
                set_tombstoned: None,
                set_in_trash: None,
                set_is_federated: None,
                set_federation_stream_id: None,
                base_seq: None,
            })
            .collect();
        repository
            .core()
            .append(
                repository.config().repo_id().clone(),
                CommitProposal {
                    author_id: "author".to_string(),
                    committer_id: "author".to_string(),
                    message: Some("create folders".to_string()),
                    occurred_at: None,
                    operation_id: Some(Uuid::new_v4().to_string()),
                    events: creates,
                },
            )
            .await
            .unwrap();
        repository.recover().await.unwrap();

        let repo_id = repository.config().repo_id().to_string();
        let renames = item_ids
            .iter()
            .enumerate()
            .map(|(index, item_id)| EventProposal {
                item_id: Some(item_id.clone()),
                event_type: "FOLDER::SET_NAME".to_string(),
                set_metadata: None,
                set_content: None,
                create_item: None,
                set_parent: None,
                set_name: Some(format!("renamed-{index}")),
                set_tombstoned: None,
                set_in_trash: None,
                set_is_federated: None,
                set_federation_stream_id: None,
                base_seq: Some(1),
            })
            .collect();
        repository
            .core()
            .append(
                repository.config().repo_id().clone(),
                CommitProposal {
                    author_id: "author".to_string(),
                    committer_id: "author".to_string(),
                    message: Some("rename folders".to_string()),
                    occurred_at: None,
                    operation_id: Some(Uuid::new_v4().to_string()),
                    events: renames,
                },
            )
            .await
            .unwrap();
        repository.recover().await.unwrap();

        let connection = crate::local_database::open_readonly(&repository.storage).unwrap();
        let applied: i64 = connection
            .query_row(
                "SELECT applied_repo_seq FROM local_projection_state WHERE repo_id = ?1",
                [repo_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied, 3);
    }

    #[cfg(unix)]
    #[test]
    fn durability_and_read_helpers_reject_symlinks_at_open() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("file");
        let file_link = temporary.path().join("file-link");
        let directory = temporary.path().join("directory");
        let directory_link = temporary.path().join("directory-link");
        fs::write(&file, b"content").unwrap();
        fs::create_dir(&directory).unwrap();
        symlink(&file, &file_link).unwrap();
        symlink(&directory, &directory_link).unwrap();

        assert!(open_regular_file_nofollow(&file_link).is_err());
        assert!(sync_file(&file_link).is_err());
        assert!(sync_directory(&directory_link).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"content");
        assert!(directory.is_dir());
    }

    #[tokio::test]
    async fn core_rejects_a_commit_above_the_shared_event_limit() {
        let temporary = tempfile::tempdir().unwrap();
        init(temporary.path(), "author").await.unwrap();
        let repository = Repository::discover(temporary.path()).await.unwrap();
        let events = (0..10_001)
            .map(|index| EventProposal {
                item_id: Some(synck_vcs::types::ItemId::new()),
                event_type: "FOLDER::CREATE".to_string(),
                set_metadata: None,
                set_content: None,
                create_item: Some(CreateItem {
                    item_type: "FOLDER".to_string(),
                    parent_folder_id: None,
                    name: format!("folder-{index}"),
                }),
                set_parent: None,
                set_name: None,
                set_tombstoned: None,
                set_in_trash: None,
                set_is_federated: None,
                set_federation_stream_id: None,
                base_seq: None,
            })
            .collect();
        let error = repository
            .core()
            .append(
                repository.config().repo_id().clone(),
                CommitProposal {
                    author_id: "author".to_string(),
                    committer_id: "author".to_string(),
                    message: Some("large".to_string()),
                    occurred_at: None,
                    operation_id: Some(Uuid::new_v4().to_string()),
                    events,
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("max_events_per_commit"));
    }

    #[tokio::test]
    async fn local_projection_replays_canonical_move_history() {
        let temporary = tempfile::tempdir().unwrap();
        init(temporary.path(), "author").await.unwrap();
        let repository = Repository::discover(temporary.path()).await.unwrap();
        let item_id = ItemId::new();
        repository
            .core()
            .append(
                repository.config().repo_id().clone(),
                CommitProposal {
                    author_id: "author".to_string(),
                    committer_id: "author".to_string(),
                    message: Some("create".to_string()),
                    occurred_at: None,
                    operation_id: Some(Uuid::new_v4().to_string()),
                    events: vec![EventProposal {
                        item_id: Some(item_id.clone()),
                        event_type: "FOLDER::CREATE".to_string(),
                        set_metadata: None,
                        set_content: None,
                        create_item: Some(CreateItem {
                            item_type: "FOLDER".to_string(),
                            parent_folder_id: None,
                            name: "before".to_string(),
                        }),
                        set_parent: None,
                        set_name: None,
                        set_tombstoned: None,
                        set_in_trash: None,
                        set_is_federated: None,
                        set_federation_stream_id: None,
                        base_seq: None,
                    }],
                },
            )
            .await
            .unwrap();
        repository.recover().await.unwrap();
        repository
            .core()
            .append(
                repository.config().repo_id().clone(),
                CommitProposal {
                    author_id: "author".to_string(),
                    committer_id: "author".to_string(),
                    message: Some("move".to_string()),
                    occurred_at: None,
                    operation_id: Some(Uuid::new_v4().to_string()),
                    events: vec![EventProposal {
                        item_id: Some(item_id),
                        event_type: "FOLDER::MOVE".to_string(),
                        set_metadata: None,
                        set_content: None,
                        create_item: None,
                        set_parent: Some(None),
                        set_name: Some("after".to_string()),
                        set_tombstoned: None,
                        set_in_trash: None,
                        set_is_federated: None,
                        set_federation_stream_id: None,
                        base_seq: Some(1),
                    }],
                },
            )
            .await
            .unwrap();
        repository.recover().await.unwrap();
        let tree = repository.committed_tree().await.unwrap();
        assert!(tree.contains_key("after"));
        assert!(!tree.contains_key("before"));
    }

    #[tokio::test]
    async fn fsck_rejects_a_live_reserved_component_but_permits_it_in_trash() {
        let temporary = tempfile::tempdir().unwrap();
        init(temporary.path(), "author").await.unwrap();
        let repository = Repository::discover(temporary.path()).await.unwrap();
        let item_id = ItemId::new();
        repository
            .core()
            .append(
                repository.config().repo_id().clone(),
                CommitProposal {
                    author_id: "author".to_string(),
                    committer_id: "author".to_string(),
                    message: Some("corrupt reserved name".to_string()),
                    occurred_at: None,
                    operation_id: Some(Uuid::new_v4().to_string()),
                    events: vec![EventProposal {
                        item_id: Some(item_id.clone()),
                        event_type: "FOLDER::CREATE".to_string(),
                        set_metadata: None,
                        set_content: None,
                        create_item: Some(CreateItem {
                            item_type: "FOLDER".to_string(),
                            parent_folder_id: None,
                            name: ".SyNcK".to_string(),
                        }),
                        set_parent: None,
                        set_name: None,
                        set_tombstoned: None,
                        set_in_trash: None,
                        set_is_federated: None,
                        set_federation_stream_id: None,
                        base_seq: None,
                    }],
                },
            )
            .await
            .unwrap();
        repository.recover().await.unwrap();
        let report = repository.fsck().await.unwrap();
        assert!(
            report
                .iter()
                .any(|finding| finding.contains("invalid checkout subtree")),
            "{report:?}"
        );

        repository
            .core()
            .append_file_folder_checked(
                repository.config().repo_id().clone(),
                CommitProposal {
                    author_id: "author".to_string(),
                    committer_id: "author".to_string(),
                    message: Some("trash corrupt item".to_string()),
                    occurred_at: None,
                    operation_id: Some(Uuid::new_v4().to_string()),
                    events: vec![EventProposal {
                        item_id: Some(item_id),
                        event_type: "FOLDER::TRASH".to_string(),
                        set_metadata: None,
                        set_content: None,
                        create_item: None,
                        set_parent: None,
                        set_name: None,
                        set_tombstoned: None,
                        set_in_trash: Some(true),
                        set_is_federated: None,
                        set_federation_stream_id: None,
                        base_seq: Some(1),
                    }],
                },
                &[],
                &[],
            )
            .await
            .unwrap();
        repository.recover().await.unwrap();
        assert!(repository.fsck().await.unwrap().is_empty());
    }
}

/// Initialize the separate checkout database without modifying canonical history.
pub(crate) fn initialize_checkout_schema(storage: &SqliteStorage, repo_id: &RepoId) -> Result<()> {
    let mut connection = crate::local_database::create(storage)?;
    let transaction = connection.transaction()?;
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='local_schema_meta')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        transaction.execute_batch(LOCAL_SCHEMA)?;
        transaction.execute(
            "INSERT INTO local_repository_settings VALUES(?1,'IGNORE')",
            [repo_id.as_str()],
        )?;
    }
    validate_local_schema(&transaction)?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    pub(crate) static OPEN_LOCK_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn run_open_lock_hook() {
    let hook = OPEN_LOCK_HOOK.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}
