//! Format 7 repositories: canonical data and content without a working checkout.
//!
//! Opening and reading never repairs history or touches a working tree. Imported
//! copies are followers. Local activation records an operator's source-retirement
//! acknowledgment; it is not a distributed fencing or federation handoff protocol.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions as CapOptions};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use synck_file_folder::{
    BlobRef, CurrentItem, Intent, IntentProposal, Location, ProfileLifecycleState,
};
use synck_vcs::object::{Codec, Object, HEADER_BYTES};
use synck_items::reader::{ItemReader, ReadContext, ReadableItem, RefReadContext};
use synck_vcs::bundle::{
    validate_bundle_event_payload_json_and_digest,
    validate_bundle_ref_event_payload_json_and_digest,
};
use synck_vcs::config::CoreConfig;
use synck_vcs::limits::{MAX_ACTOR_ID_BYTES, MAX_MESSAGE_BYTES};
use synck_vcs::repo::Repo;
use synck_vcs::state::{NamespaceKey, StateIndexStore, StateTree};
use synck_vcs::storage::{Storage, StorageBackend};
use synck_vcs::types::{
    AcceptedCommit, ObjectId, CommitEventRow, CommitId, CommitProposal, EventContent,
    EventProposal, ItemCursor, ItemId, ItemRow, LifecycleState, RefEventRow, RepoEventCursor,
    RepoId, SequencingMode, Timestamp,
};
use synck_vcs::verify::{
    SemanticVerifier, VerifyCommit, VerifyCommitEvent, VerifyEvent, VerifyItem, VerifyRefEvent,
};
use synck_vcs_sqlite::SqliteStorage;
use uuid::Uuid;

use crate::chunk_store::{ChunkStore, OBJECT_STORE_FILE, StorageMode};
use crate::ooxml::{OoxmlKind, chunk_region_lengths};
use crate::error::{IoContext, LocalError, Result};
use crate::repository::{
    RepositoryLock, is_cap_link_or_reparse, is_link_or_reparse, open_regular_file_nofollow,
    remove_tree_nofollow, rename_between_noreplace, rename_noreplace, replace_file, sync_directory,
    sync_file, sync_open_directory,
};

pub const FORMAT_VERSION: u32 = 7;
const MAX_PAYLOAD_BYTES: usize = HEADER_BYTES + synck_items::body_events::MAX_BODY_BYTES;
pub(crate) fn unsupported_format(version: Option<u64>) -> LocalError {
    let reader = match version {
        Some(6) => "Open format 6 with Synck 0.1.x, export its working files, and initialize a new repository with 0.2.0.",
        _ => "Open this repository with the release that created it; this release has no migration reader.",
    };
    let version = version.map_or_else(|| "missing".to_string(), |value| value.to_string());
    invalid(format!("unsupported repository format {version}; Synck 0.2.0 requires format-7. {reader}"))
}

const PAGE: usize = 512;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Authority {
    Follower,
    Local,
    /// Independent continuation with no synchronization or rejoin contract.
    IndependentLocal,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    storage_mode: StorageMode,
    format_version: u32,
    repo_id: RepoId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    authority: Authority,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkout: Option<crate::checkout::Checkout>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSnapshot {
    pub repo_id: RepoId,
    pub terminal_repo_seq: i64,
    pub history_sha256: String,
    pub history_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamespaceCursor {
    pub repo_id: RepoId,
    pub parent_item_id: Option<ItemId>,
    pub through_repo_seq: Option<i64>,
    pub name_key: String,
    pub item_id: ItemId,
}

#[derive(Debug, Serialize)]
pub struct NamespacePage {
    pub items: Vec<ItemRow>,
    pub next_cursor: Option<NamespaceCursor>,
}

/// One commit of a complete repository's history, newest-first listing.
#[derive(Debug, Serialize)]
pub struct PortableLogEntry {
    pub repo_seq: i64,
    pub commit_id: CommitId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub author: String,
    pub committer: String,
    pub message: Option<String>,
    pub events: Vec<PortableLogEvent>,
}

/// One accepted event: its item or ref ledger, position and type.
#[derive(Debug, Serialize)]
pub struct PortableLogEvent {
    pub ledger: &'static str,
    pub item_id: ItemId,
    pub seq: i64,
    pub event_type: String,
}

#[derive(Debug, Serialize)]
pub struct PortableLogPage {
    pub commits: Vec<PortableLogEntry>,
    /// Pass as `before` to read the next, older page. Null at the start.
    pub next_before: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct PortableItem {
    pub item: ReadableItem,
    /// Exact attached-ref history, retained independently from item_seq.
    pub refs: Vec<RefEventRow>,
}

#[derive(Debug, Serialize)]
pub struct VerificationReport {
    pub repo_id: RepoId,
    pub terminal_repo_seq: i64,
    pub commits: u64,
    pub items: u64,
    pub objects: usize,
    pub metadata: usize,
    pub missing_objects: Vec<ObjectId>,
    pub missing_metadata: Vec<ObjectId>,
}

/// The OS lock excludes other opens. Canonical writers require exclusive
/// mutable access, so safe callers cannot overlap a write with an archive or
/// composed read on this same handle. Direct external database writes are not
/// supported and bypass that consistency contract.
pub struct PortableRepository {
    agent_lock_tokens: Vec<String>,
    root: PathBuf,
    metadata: Dir,
    config: Config,
    storage: SqliteStorage,
    _lock: Arc<RepositoryLock>,
}

struct BoundedEmailSource<'a> { bytes: &'a mut Vec<u8>, limit: u64 }
impl Write for BoundedEmailSource<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.limit.saturating_sub(self.bytes.len() as u64) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "EMAIL source exceeds parser limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn invalid(error: impl std::fmt::Display) -> LocalError {
    LocalError::Invalid(error.to_string())
}
fn corrupt(error: impl std::fmt::Display) -> LocalError {
    LocalError::Integrity(error.to_string())
}

fn regular(dir: &Dir, name: &str, display: &Path) -> Result<File> {
    regular_with_access(dir, name, display, false)
}

fn regular_with_access(dir: &Dir, name: &str, display: &Path, writable: bool) -> Result<File> {
    let mut options = CapOptions::new();
    options
        .read(true)
        .write(writable)
        .follow(FollowSymlinks::No);
    let file = dir
        .open_with(name, &options)
        .at("open portable file", display)?;
    let info = file.metadata().at("inspect portable file", display)?;
    if !info.is_file() || is_cap_link_or_reparse(&info) {
        return Err(corrupt("portable entry is not a regular file"));
    }
    Ok(file.into_std())
}

fn read_small(dir: &Dir, name: &str, max: u64, display: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    regular(dir, name, display)?
        .take(max + 1)
        .read_to_end(&mut bytes)
        .at("read portable file", display)?;
    if bytes.len() as u64 > max {
        return Err(corrupt("portable metadata exceeds its size bound"));
    }
    Ok(bytes)
}

fn directory(path: &Path) -> Result<Dir> {
    let info = fs::symlink_metadata(path).at("inspect portable directory", path)?;
    if !info.is_dir() || is_link_or_reparse(&info) {
        return Err(corrupt("portable directory is a link or not a directory"));
    }
    Dir::open_ambient_dir(path, ambient_authority()).at("open portable directory", path)
}

fn entries(dir: &Dir) -> Result<BTreeSet<String>> {
    dir.entries()
        .map_err(invalid)?
        .map(|entry| {
            let entry = entry.map_err(invalid)?;
            if entry.file_type().map_err(invalid)?.is_symlink() {
                return Err(corrupt("linked portable entry"));
            }
            entry
                .file_name()
                .into_string()
                .map_err(|_| corrupt("non-UTF-8 portable entry name"))
        })
        .collect()
}

fn digest_name(value: &str) -> Result<&str> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(corrupt("noncanonical object filename"));
    }
    Ok(value)
}

fn hash(file: &mut File) -> Result<(String, u64)> {
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let size = file.read(&mut buffer).map_err(invalid)?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
        count = count
            .checked_add(size as u64)
            .ok_or_else(|| corrupt("object size overflow"))?;
    }
    Ok((hex::encode(digest.finalize()), count))
}

fn create_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).at("create private portable file", path)
}

fn private_directory(path: &Path) -> Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .at("create private portable directory", path)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = create_file(path)?;
    file.write_all(bytes).at("write portable file", path)?;
    file.sync_all().at("sync portable file", path)
}

fn copy_new(mut source: File, destination: &Path) -> Result<(String, u64)> {
    let mut target = create_file(destination)?;
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = source
            .read(&mut buffer)
            .at("read portable copy", destination)?;
        if n == 0 {
            break;
        }
        target
            .write_all(&buffer[..n])
            .at("write portable copy", destination)?;
        hasher.update(&buffer[..n]);
        bytes = bytes
            .checked_add(n as u64)
            .ok_or_else(|| corrupt("object size overflow"))?;
    }
    target.sync_all().at("sync portable copy", destination)?;
    Ok((hex::encode(hasher.finalize()), bytes))
}

fn copy_named(source: File, destination: &Path, name: &str) -> Result<()> {
    if copy_new(source, destination)?.0 != name {
        return Err(corrupt("content changed while copying verified object"));
    }
    Ok(())
}

fn author_valid(author: &str) -> Result<()> {
    if author.trim().is_empty() || author.len() > MAX_ACTOR_ID_BYTES {
        return Err(invalid(
            "author must be nonblank and within the Core author limit",
        ));
    }
    Ok(())
}

/// Commit attribution and idempotency for one direct FILE write.
///
/// The repository author always remains the committer. A repeated
/// `operation_id` returns the write already accepted under it.
#[derive(Clone, Debug, Default)]
pub struct WriteOptions {
    pub message: Option<String>,
    pub author: Option<String>,
    pub occurred_at: Option<Timestamp>,
    pub operation_id: Option<Uuid>,
}

impl WriteOptions {
    fn validate(&self) -> Result<()> {
        if let Some(message) = &self.message
            && (message.is_empty() || message.len() > MAX_MESSAGE_BYTES)
        {
            return Err(invalid(format!(
                "message must be 1 to {MAX_MESSAGE_BYTES} UTF-8 bytes"
            )));
        }
        if let Some(author) = &self.author {
            author_valid(author)?;
        }
        Ok(())
    }
}

/// Caller operation IDs share the namespace of other direct repository writes.
pub(crate) fn stored_operation_id(id: &Uuid) -> String {
    format!("portable:{id}")
}

impl PortableRepository {
    /// Open exactly this format-7 repository. No discovery, migration, recovery,
    /// checkout scan, indexing or authoritative database writes occur here.
    pub fn open(root: &Path) -> Result<Self> {
        let root_dir = directory(root)?;
        let metadata = root_dir
            .open_dir_nofollow(".synck")
            .at("open portable metadata", root.join(".synck"))?;
        #[cfg(test)]
        crate::repository::run_open_lock_hook();
        let lock = regular_with_access(&metadata, "lock", &root.join(".synck/lock"), true)?;
        let lock = match RepositoryLock::acquire(lock) {
            Ok(lock) => lock,
            Err(std::fs::TryLockError::WouldBlock) => return Err(LocalError::Busy),
            Err(std::fs::TryLockError::Error(error)) => return Err(invalid(error)),
        };
        let bytes = read_small(
            &metadata,
            "config.json",
            64 * 1024,
            &root.join(".synck/config.json"),
        )?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let version = value.get("format_version").and_then(serde_json::Value::as_u64);
        if version != Some(u64::from(FORMAT_VERSION)) {
            return Err(unsupported_format(version));
        }
        let config: Config = serde_json::from_value(value)?;
        if let Some(author) = &config.author {
            author_valid(author)?;
        } else if config.authority != Authority::Follower {
            return Err(corrupt("local writer has no author"));
        }
        for name in ["objects"] {
            metadata.open_dir_nofollow(name).at(
                "open portable content directory",
                root.join(".synck").join(name),
            )?;
        }
        regular(
            &metadata,
            "repository.sqlite3",
            &root.join(".synck/repository.sqlite3"),
        )?;
        let storage = SqliteStorage::new(root.join(".synck/repository.sqlite3"));
        storage.open_readonly().map_err(corrupt)?;
        if config.storage_mode == StorageMode::Chunked {
            drop(
                ChunkStore::new(root.join(".synck").join(OBJECT_STORE_FILE), &config.repo_id)
                    .open_readonly()?,
            );
        }
        Ok(Self {
            agent_lock_tokens: Vec::new(),
            root: root.to_path_buf(),
            metadata,
            config,
            storage,
            _lock: lock,
        })
    }

    pub(crate) fn from_locked_checkout(
        root: &Path,
        storage: SqliteStorage,
        lock: Arc<RepositoryLock>,
        config: &crate::repository::Config,
        tokens: &[String],
    ) -> Result<Self> {
        Ok(Self {
            agent_lock_tokens: tokens.to_vec(),
            root: root.to_path_buf(),
            metadata: directory(&root.join(".synck"))?,
            config: serde_json::from_value(serde_json::to_value(config)?)?,
            storage,
            _lock: lock,
        })
    }

    fn checkout_adapter(&self) -> Result<crate::Repository> {
        let checkout = self
            .config
            .checkout
            .clone()
            .ok_or_else(|| invalid("repository has no checkout"))?;
        let config = crate::repository::Config::complete(
            self.repo_id().clone(),
            self.config
                .author
                .clone()
                .ok_or_else(|| corrupt("checkout has no author"))?,
            self.authority(),
            checkout,
            self.storage_mode(),
        );
        crate::checkout::validate_config(&config)?;
        let mut repository = crate::Repository::from_portable(
            &self.root,
            self.storage.clone(),
            Arc::clone(&self._lock),
            config,
        )?;
        repository.set_agent_lock_tokens(&self.agent_lock_tokens)?;
        Ok(repository)
    }

    pub fn set_agent_lock_tokens(&mut self, tokens: &[String]) -> Result<()> {
        if !tokens.is_empty() && self.config.checkout.is_none() {
            return Err(invalid("agent reservations require a local checkout"));
        }
        crate::agent_locks::validate_tokens(tokens)?;
        self.agent_lock_tokens = tokens.to_vec();
        Ok(())
    }

    pub fn storage_mode(&self) -> StorageMode {
        self.config.storage_mode
    }

    fn object_store(&self) -> ChunkStore {
        ChunkStore::new(
            self.root.join(".synck").join(OBJECT_STORE_FILE),
            self.repo_id(),
        )
    }

    fn stored_objects(&self) -> Result<BTreeMap<String, u64>> {
        if self.storage_mode() == StorageMode::Chunked {
            return self.object_store().inventory();
        }
        let objects = self
            .metadata
            .open_dir_nofollow("objects")
            .map_err(corrupt)?;
        let mut stored = BTreeMap::new();
        for name in entries(&objects)? {
            digest_name(&name)?;
            let mut file = regular(
                &objects,
                &name,
                &self.root.join(".synck/objects").join(&name),
            )?;
            let (digest, size) = hash(&mut file)?;
            if digest != name {
                return Err(corrupt("stored object hash mismatch"));
            }
            stored.insert(name, size);
        }
        Ok(stored)
    }

    pub async fn storage_report(&self) -> Result<crate::StorageReport> {
        self.verify().await?;
        let (referenced, _) = self.references().await?;
        let stored = self.stored_objects()?;
        let mut report = crate::StorageReport {
            referenced_objects: 0,
            referenced_bytes: 0,
            unreachable_objects: 0,
            unreachable_bytes: 0,
        };
        for (name, size) in stored {
            if referenced.contains(&name) {
                report.referenced_objects += 1;
                report.referenced_bytes += size;
            } else {
                report.unreachable_objects += 1;
                report.unreachable_bytes += size;
            }
        }
        Ok(report)
    }

    pub async fn gc(&mut self) -> Result<crate::GcReport> {
        if entries(&self.metadata)?
            .iter()
            .any(|name| name.ends_with(".json") && name != "config.json")
        {
            return Err(invalid(
                "finish checkout recovery before garbage collection",
            ));
        }
        self.verify().await?;
        let (referenced, _) = self.references().await?;
        if self.storage_mode() == StorageMode::Chunked {
            self.object_store().recover_incomplete()?;
            // Verify and remove optimization leftovers before collecting their chunks.
            self.remove_whole_file_objects()?;
            let (removed_objects, removed_bytes) = self.object_store().gc(&referenced)?;
            return Ok(crate::GcReport {
                removed_objects,
                removed_bytes,
            });
        }
        let stored = self.stored_objects()?;
        let objects = self
            .metadata
            .open_dir_nofollow("objects")
            .map_err(corrupt)?;
        let mut report = crate::GcReport {
            removed_objects: 0,
            removed_bytes: 0,
        };
        for (name, size) in stored {
            if !referenced.contains(&name) {
                objects.remove_file(&name).map_err(corrupt)?;
                report.removed_objects += 1;
                report.removed_bytes += size;
            }
        }
        sync_directory(&self.root.join(".synck/objects"))?;
        Ok(report)
    }

    /// Convert physical storage only. Canonical history and all IDs stay intact.
    pub async fn optimize_storage(&mut self) -> Result<bool> {
        if self
            .config
            .checkout
            .as_ref()
            .is_some_and(|c| c.pending.is_some())
        {
            return Err(invalid(
                "finish checkout selection before optimizing storage",
            ));
        }
        self.verify().await?;
        if self.storage_mode() == StorageMode::Chunked {
            self.remove_whole_file_objects()?;
            return Ok(false);
        }
        if self.config.format_version != FORMAT_VERSION {
            return Err(invalid(
                "import older repositories before optimizing storage",
            ));
        }
        let store = self.object_store();
        // A failed earlier attempt remains non-authoritative and can be resumed.
        if !store
            .path()
            .try_exists()
            .at("inspect object store", store.path())?
        {
            let temporary = self
                .root
                .join(".synck")
                .join(format!("object-store-{}.sqlite3", Uuid::new_v4()));
            ChunkStore::create(&temporary, self.repo_id())?;
            rename_noreplace(&temporary, store.path()).at("publish object store", store.path())?;
            sync_open_directory(&self.metadata, &self.root.join(".synck"))?;
        }
        store.recover_incomplete()?;
        let objects = self
            .metadata
            .open_dir_nofollow("objects")
            .map_err(corrupt)?;
        for name in entries(&objects)? {
            digest_name(&name)?;
            let (digest, _) =
                store.store_object_file(&self.root.join(".synck/objects").join(&name))?;
            if digest != name {
                return Err(corrupt("object digest changed during optimization"));
            }
        }
        store.verify_all()?;
        store.checkpoint()?;
        let mut updated = self.config.clone();
        updated.storage_mode = StorageMode::Chunked;
        let path = self
            .root
            .join(".synck")
            .join(format!("config-{}.json", Uuid::new_v4()));
        write_new(&path, &serde_json::to_vec(&updated)?)?;
        replace_file(&path, &self.root.join(".synck/config.json"))
            .at("publish storage mode", &path)?;
        self.config = updated;
        sync_open_directory(&self.metadata, &self.root.join(".synck"))?;
        self.remove_whole_file_objects()?;
        Ok(true)
    }

    fn remove_whole_file_objects(&self) -> Result<()> {
        let objects = self
            .metadata
            .open_dir_nofollow("objects")
            .map_err(corrupt)?;
        for name in entries(&objects)? {
            digest_name(&name)?;
            // Never remove an unexpected entry or the last unverified copy.
            let mut file = regular(
                &objects,
                &name,
                &self.root.join(".synck/objects").join(&name),
            )?;
            let (digest, size) = hash(&mut file)?;
            if digest != name {
                return Err(corrupt("invalid whole-file object"));
            }
            let id = ObjectId::parse(format!("sha256:{name}")).map_err(corrupt)?;
            self.object_store().verify_blob(&id, size)?;
            objects.remove_file(&name).map_err(corrupt)?;
        }
        sync_directory(&self.root.join(".synck/objects"))
    }

    /// The filesystem view is explicit; bare opening never scans or repairs it.
    pub fn checkout_paths(&self) -> Option<&[String]> {
        self.config
            .checkout
            .as_ref()
            .map(|checkout| checkout.paths.as_slice())
    }

    pub async fn checkout_path_warnings(&self) -> Result<Vec<PathBuf>> {
        self.checkout_adapter()?.checkout_path_warnings().await
    }

    /// Enable an independent filesystem checkout in this complete repository.
    /// The existing source may remain live. There is no synchronization or rejoin.
    pub async fn enable_checkout(&mut self, author: &str, paths: &[String]) -> Result<()> {
        author_valid(author)?;
        if self.config.checkout.is_some() {
            return Err(invalid("checkout already enabled; use checkout selection"));
        }
        let paths = crate::checkout::normalize_paths(paths)?;
        self.verify().await?;
        let root = directory(&self.root)?;
        if entries(&root)? != BTreeSet::from([".synck".to_string()]) {
            return Err(invalid(
                "checkout requires a directory containing only .synck; existing files are never overwritten",
            ));
        }
        for name in ["tmp"] {
            match self.metadata.create_dir(name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    self.metadata.open_dir_nofollow(name).map_err(corrupt)?;
                }
                Err(error) => return Err(invalid(error)),
            }
        }
        crate::repository::initialize_checkout_schema(&self.storage)?;
        let checkout = crate::checkout::Checkout {
            hidden_files: Default::default(),
            paths,
            pending: Some(crate::checkout::Transition {
                previous_paths: None,
            }),
        };
        // Share this exact lock with the temporary adapter. No second opener
        // or lock gap is involved.
        let config = crate::repository::Config::complete(
            self.repo_id().clone(),
            author.to_string(),
            Authority::IndependentLocal,
            checkout.clone(),
            self.storage_mode(),
        );
        let mut working = crate::repository::Repository::from_portable(
            &self.root,
            self.storage.clone(),
            Arc::clone(&self._lock),
            config,
        )?;
        working.recover_current().await?;
        working.validate_checkout_selection().await?;
        working.write_checkout_source(&[])?;
        working.publish_checkout_config()?;
        self.config.author = Some(author.to_string());
        self.config.authority = Authority::IndependentLocal;
        self.config.checkout = Some(checkout);
        working.finish_checkout_transition().await?;
        self.config.checkout = working.config().checkout.clone();
        Ok(())
    }

    pub fn repo_id(&self) -> &RepoId {
        &self.config.repo_id
    }
    pub fn authority(&self) -> Authority {
        self.config.authority
    }

    /// The caller is explicitly retiring the former local writer. This records
    /// that acknowledgment; it cannot fence an inaccessible remote authority.
    pub async fn activate(&mut self, author: &str, source_retired: bool) -> Result<()> {
        if !source_retired {
            return Err(invalid(
                "activation requires explicit acknowledgment that the source writer is retired",
            ));
        }
        author_valid(author)?;
        self.verify().await?;
        let updated = Config {
            format_version: FORMAT_VERSION,
            storage_mode: self.storage_mode(),
            repo_id: self.config.repo_id.clone(),
            author: Some(author.into()),
            authority: Authority::Local,
            checkout: self.config.checkout.clone(),
        };
        let name = format!("config-{}.json", Uuid::new_v4());
        let path = self.root.join(".synck").join(&name);
        write_new(&path, &serde_json::to_vec(&updated)?)?;
        replace_file(&path, &self.root.join(".synck/config.json"))
            .at("activate portable repository", &path)?;
        self.config = updated;
        sync_open_directory(&self.metadata, &self.root.join(".synck"))
    }

    fn core(&self) -> Repo {
        Repo::new(
            Storage::new(Arc::new(self.storage.clone())),
            CoreConfig::default(),
        )
    }
    fn writable(&self) -> Result<()> {
        if self.config.format_version != FORMAT_VERSION
            || self.config.authority == Authority::Follower
        {
            Err(invalid(
                "portable repository is a read-only follower; explicit activation is required",
            ))
        } else {
            Ok(())
        }
    }

    async fn local_owner(&self, id: &ItemId, kind: &str) -> Result<ItemRow> {
        self.writable()?;
        let item = self
            .storage
            .get_item_readonly(self.repo_id(), id)
            .await
            .map_err(corrupt)?
            .ok_or_else(|| invalid("item not found"))?;
        if item.item_type != kind
            || item.lifecycle_state != LifecycleState::Live
            || item.sequencing_mode != SequencingMode::Local
        {
            return Err(invalid(
                "operation requires a live local owner of the supported item type; federated authority is retained",
            ));
        }
        Ok(item)
    }

    /// Accepted live namespace, ordered by canonical name then item ID. Current
    /// pages are not a frozen multi-request snapshot; pin a historical revision
    /// when stable pagination is required.
    pub async fn list_items(
        &self,
        parent: Option<&ItemId>,
        after: Option<&NamespaceCursor>,
        limit: usize,
    ) -> Result<NamespacePage> {
        if limit == 0 || limit > PAGE {
            return Err(invalid("namespace page limit must be 1..=512"));
        }
        if after.is_some_and(|cursor| {
            &cursor.repo_id != self.repo_id()
                || cursor.parent_item_id.as_ref() != parent
                || cursor.through_repo_seq.is_some()
        }) {
            return Err(invalid(
                "namespace cursor belongs to another repository or parent",
            ));
        }
        if let Some(parent) = parent {
            let item = self
                .storage
                .get_item_readonly(self.repo_id(), parent)
                .await
                .map_err(corrupt)?
                .ok_or_else(|| invalid("parent not found"))?;
            if item.item_type != "FOLDER" || item.lifecycle_state != LifecycleState::Live {
                return Err(invalid("namespace parent must be a live folder"));
            }
        }
        let connection = self.storage.open_readonly().map_err(corrupt)?;
        let mut query=connection.prepare("SELECT item_id FROM vcs_items WHERE repo_id=?1 AND parent_folder_id IS ?2 AND lifecycle_state='LIVE' AND item_type<>'CHAT_ORGANIZATION' AND (?3 IS NULL OR (name_key,item_id)>(?3,?4)) ORDER BY name_key,item_id LIMIT ?5")?;
        let ids = query
            .query_map(
                params![
                    self.repo_id().as_str(),
                    parent.map(ItemId::as_str),
                    after.map(|c| c.name_key.as_str()),
                    after.map(|c| c.item_id.as_str()),
                    limit as i64 + 1
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = ids.len() > limit;
        let ids = ids
            .into_iter()
            .take(limit)
            .map(|id| ItemId::parse(id).map_err(corrupt))
            .collect::<Result<Vec<_>>>()?;
        let mut fetched = self
            .storage
            .get_items_readonly(self.repo_id(), &ids)
            .await
            .map_err(corrupt)?
            .into_iter()
            .map(|item| (item.item_id.clone(), item))
            .collect::<BTreeMap<_, _>>();
        let items = ids
            .iter()
            .map(|id| {
                fetched
                    .remove(id)
                    .ok_or_else(|| corrupt("namespace item disappeared"))
            })
            .collect::<Result<Vec<_>>>()?;
        let next_cursor = if more {
            items.last().map(|item| NamespaceCursor {
                repo_id: self.repo_id().clone(),
                parent_item_id: parent.cloned(),
                through_repo_seq: None,
                name_key: item.name_key.clone(),
                item_id: item.item_id.clone(),
            })
        } else {
            None
        };
        Ok(NamespacePage { items, next_cursor })
    }

    /// Stable historical namespace page, rebuilding the disposable cache as needed.
    pub async fn list_historical_items(
        &self,
        through: i64,
        parent: Option<&ItemId>,
        after: Option<&NamespaceCursor>,
        limit: usize,
    ) -> Result<NamespacePage> {
        self.check_revision(Some(through))?;
        if limit == 0 || limit > synck_vcs::state::CHILD_PAGE_SIZE {
            return Err(invalid("historical page limit must be 1..=256"));
        }
        if after.is_some_and(|cursor| {
            &cursor.repo_id != self.repo_id()
                || cursor.parent_item_id.as_ref() != parent
                || cursor.through_repo_seq != Some(through)
        }) {
            return Err(invalid(
                "historical cursor belongs to another checkpoint or parent",
            ));
        }
        let mut store = self.storage.state_index_store(self.repo_id().clone()).map_err(corrupt)?;
        synck_vcs::state::catch_up_state_index(&self.core(), self.repo_id(), &mut store, Some(through))
            .await.map_err(corrupt)?;
        let root_id = store.root_at(through).await.map_err(corrupt)?
            .ok_or_else(|| corrupt("historical checkpoint is missing after replay"))?.state_root;
        let mut tree = StateTree::new(&mut store);
        let root = tree.read_root(root_id).await.map_err(corrupt)?;
        if root.repo_id != *self.repo_id() || root.repo_seq != through {
            return Err(corrupt("historical root checkpoint mismatch"));
        }
        if let Some(parent) = parent {
            let item = tree
                .get_item(root.item_map_root, parent)
                .await
                .map_err(corrupt)?
                .ok_or_else(|| invalid("historical parent not found"))?;
            if item.item_type != "FOLDER" || item.lifecycle_state != LifecycleState::Live {
                return Err(invalid("historical parent must be a live folder"));
            }
        }
        let mut cursor =
            after.map(|cursor| NamespaceKey::new(parent.cloned(), cursor.name_key.clone()));
        let mut entries = Vec::new();
        loop {
            let page = tree
                .child_page(
                    root.item_map_root,
                    root.namespace_root,
                    parent,
                    cursor.as_ref(),
                )
                .await
                .map_err(corrupt)?;
            entries.extend(
                page.entries
                    .into_iter()
                    .filter(|(_, item)| item.item_type != "CHAT_ORGANIZATION"),
            );
            if entries.len() > limit || page.next_cursor.is_none() {
                break;
            }
            cursor = page.next_cursor;
        }
        let more = entries.len() > limit;
        let items = entries
            .into_iter()
            .take(limit)
            .map(|(_, item)| ItemRow {
                repo_id: self.repo_id().clone(),
                item_id: item.item_id,
                item_type: item.item_type,
                parent_folder_id: item.parent_folder_id,
                name: item.name,
                name_key: item.name_key,
                head_seq: item.head_seq,
                ref_head_seq: item.ref_head_seq,
                lifecycle_state: item.lifecycle_state,
                created_at: item.created_at,
                created_by: item.created_by,
                sequencing_mode: item.sequencing_mode,
                federation_stream_id: item.federation_stream_id,
            })
            .collect::<Vec<_>>();
        let next_cursor = if more {
            items.last().map(|item| NamespaceCursor {
                repo_id: self.repo_id().clone(),
                parent_item_id: parent.cloned(),
                through_repo_seq: Some(through),
                name_key: item.name_key.clone(),
                item_id: item.item_id.clone(),
            })
        } else {
            None
        };
        Ok(NamespacePage { items, next_cursor })
    }

    fn check_revision(&self, through: Option<i64>) -> Result<()> {
        if let Some(seq) = through {
            let found:bool=self.storage.open_readonly().map_err(corrupt)?.query_row("SELECT EXISTS(SELECT 1 FROM vcs_commits WHERE repo_id=?1 AND repo_seq=?2 AND commit_id IS NOT NULL)",params![self.repo_id().as_str(),seq],|row|row.get(0))?;
            if !found {
                return Err(invalid(
                    "historical sequence is not a sealed repository checkpoint",
                ));
            }
        }
        Ok(())
    }

    /// Replay only the requested owner's item history. Referenced bodies are
    /// restored in an in-memory view; stored canonical bytes remain unchanged.
    pub async fn show_item(&self, id: &ItemId, through: Option<i64>) -> Result<PortableItem> {
        self.check_revision(through)?;
        let mut reader = self.read_owner(id, through).await?;
        let mut refs = Vec::new();
        let mut ref_from = 1;
        'refs: loop {
            let page = self
                .storage
                .list_ref_events_for_owner_readonly(self.repo_id(), id, ref_from, PAGE)
                .await
                .map_err(corrupt)?;
            if page.is_empty() {
                break;
            }
            let done = page.len() < PAGE;
            for row in page {
                if through.is_some_and(|seq| row.repo_seq.is_none_or(|position| position > seq)) {
                    break 'refs;
                }
                ref_from = row
                    .ref_seq
                    .checked_add(1)
                    .ok_or_else(|| corrupt("ref sequence exhausted"))?;
                validate_bundle_ref_event_payload_json_and_digest(
                    std::str::from_utf8(&row.payload).map_err(corrupt)?,
                    row.event_payload_digest.as_str(),
                )
                .map_err(corrupt)?;
                let event = row.proposal()?;
                let commit = self
                    .storage
                    .get_commit_readonly(self.repo_id(), &row.commit_id)
                    .await
                    .map_err(corrupt)?
                    .ok_or_else(|| corrupt("ref event has no commit"))?;
                reader
                    .apply_ref(
                        &event,
                        RefReadContext {
                            ref_seq: row.ref_seq,
                            created_at: row.created_at,
                            author: &commit.author_id,
                            committer: &commit.committer_id,
                        },
                    )
                    .map_err(corrupt)?;

                refs.push(row);
            }
            if done {
                break;
            }
        }
        let item = reader.snapshot().map_err(corrupt)?;
        Ok(PortableItem { item, refs })
    }

    async fn read_owner(
        &self,
        id: &ItemId,
        through: Option<i64>,
    ) -> Result<ItemReader> {
        let mut reader = ItemReader::for_repository(self.repo_id().clone(), id.clone());
        let mut from = 1;
        'history: loop {
            let page = self
                .storage
                .list_events_for_item_readonly(self.repo_id(), id, from, PAGE)
                .await
                .map_err(corrupt)?;
            if page.is_empty() {
                break;
            }
            let done = page.len() < PAGE;
            for row in page {
                if through.is_some_and(|seq| row.repo_seq.is_none_or(|event_seq| event_seq > seq)) {
                    break 'history;
                }
                validate_bundle_event_payload_json_and_digest(
                    std::str::from_utf8(&row.payload).map_err(corrupt)?,
                    row.event_payload_digest.as_str(),
                )
                .map_err(corrupt)?;
                let mut event: EventProposal = row.proposal()?;
                if let Some(content_id) = event.set_content.as_ref().and_then(|content| content.get("body")) {
                    let objects = BTreeMap::from([(content_id.clone(), self.read_body(content_id.clone())?)]);
                    synck_items::body_events::restore(
                        std::slice::from_mut(&mut event),
                        &objects,
                    )
                    .map_err(corrupt)?;
                }
                let commit = self
                    .storage
                    .get_commit_readonly(self.repo_id(), &row.commit_id)
                    .await
                    .map_err(corrupt)?
                    .ok_or_else(|| corrupt("item event has no commit"))?;
                reader
                    .apply(
                        &event,
                        ReadContext {
                            item_seq: row.item_seq,
                            created_at: row.created_at,
                            occurred_at: commit.occurred_at,
                            author: &commit.author_id,
                            committer: &commit.committer_id,
                        },
                    )
                    .map_err(corrupt)?;
                from = row
                    .item_seq
                    .checked_add(1)
                    .ok_or_else(|| corrupt("item sequence exhausted"))?;
            }
            if done {
                break;
            }
            // The next event may lie beyond a pinned checkpoint.
            if let Some(seq) = through {
                let next = self
                    .storage
                    .list_events_for_item_readonly(self.repo_id(), id, from, 1)
                    .await
                    .map_err(corrupt)?;
                if next
                    .first()
                    .is_none_or(|event| event.repo_seq.is_none_or(|position| position > seq))
                {
                    break;
                }
            }
        }
        Ok(reader)
    }

    pub fn read_body(&self, id: ObjectId) -> Result<Object> {
        let bytes = if self.storage_mode() == StorageMode::Chunked {
            let size = self.object_store().blob_size(&id)?;
            if size > MAX_PAYLOAD_BYTES as u64 {
                return Err(corrupt("body exceeds size bound"));
            }
            let mut bytes = Vec::with_capacity(size as usize);
            self.object_store().copy_blob(&id, size, &mut bytes)?;
            bytes
        } else {
            let dir = self.metadata.open_dir_nofollow("objects").map_err(corrupt)?;
            let name = &id.as_str()[7..];
            read_small(
                &dir,
                name,
                MAX_PAYLOAD_BYTES as u64,
                &self.root.join(".synck/objects").join(name),
            )?
        };
        let object = Object::verify(&id, bytes).map_err(corrupt)?;
        object.utf8().map_err(corrupt)?;
        Ok(object)
    }

    /// Stream exact blob bytes. On error the caller must discard any partial
    /// output; publish extracted files only after this returns success.
    pub fn copy_blob(&self, id: &ObjectId, out: &mut dyn Write) -> Result<u64> {
        let mut decoder = synck_vcs::object::Decoder::new(id.clone(), out);
        self.copy_stored_object(id, &mut decoder)?;
        decoder.finish().map(|(size, _)| size).map_err(corrupt)
    }

    pub(crate) fn copy_stored_object(&self, id: &ObjectId, mut out: &mut dyn Write) -> Result<u64> {
        if self.storage_mode() == StorageMode::Chunked {
            let size = self.object_store().blob_size(id)?;
            self.object_store().copy_blob(id, size, &mut out)?;
            return Ok(size);
        }
        let name = id
            .as_str()
            .strip_prefix("sha256:")
            .ok_or_else(|| corrupt("blob ID has no digest"))?;
        let dir = self
            .metadata
            .open_dir_nofollow("objects")
            .map_err(corrupt)?;
        let mut file = regular(&dir, name, &self.root.join(".synck/objects").join(name))?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).map_err(invalid)?;
            if count == 0 {
                break;
            }
            out.write_all(&buffer[..count]).map_err(invalid)?;
            hasher.update(&buffer[..count]);
            size = size
                .checked_add(count as u64)
                .ok_or_else(|| corrupt("blob size overflow"))?;
        }
        if hex::encode(hasher.finalize()) != name {
            return Err(corrupt("blob digest mismatch"));
        }
        Ok(size)
    }

    /// Export FILE primary bytes or an imported EMAIL's exact original source.
    /// The destination must be absent. The temporary
    /// output is never published until its content identity and size pass.
    /// Recompute presentation from the preserved original with the current parser.
    /// This view is never written back into canonical history.
    pub async fn render_email(&self, id: &ItemId, through: Option<i64>) -> Result<synck_items::imported_email::ParsedImportedEmail> {
        let item = self.show_item(id, through).await?.item.projection;
        if item.item_type != "EMAIL" { return Err(invalid("email rendering requires an EMAIL item")); }
        let source = item.content_roles.get("source").ok_or_else(|| corrupt("EMAIL has no source role"))?;
        let format = match item.metadata_json["mailcal"]["source"]["format"].as_str() {
            Some("eml") => synck_items::imported_email::ImportedEmailFormat::Eml,
            Some("msg") => synck_items::imported_email::ImportedEmailFormat::Msg,
            _ => return Err(invalid("unsupported EMAIL source format")),
        };
        let mut bytes = Vec::new();
        // Imported sources have the profile's size bound; refuse a forged size
        // before allocating an unbounded presentation buffer.
        let limit = crate::email_import::SOURCE_BYTES;
        self.copy_blob(source, &mut BoundedEmailSource { bytes: &mut bytes, limit })?;
        let limits = synck_items::imported_email::ImportedEmailLimits::for_source_limit_and_parser_concurrency(limit, 1);
        synck_items::imported_email::parse_imported_email_owned(format, bytes, &limits).map_err(invalid)
    }

    pub async fn export_file(
        &self,
        id: &ItemId,
        destination: &Path,
        through: Option<i64>,
    ) -> Result<u64> {
        self.check_revision(through)?;
        // Ref presentation can be unsupported while ordinary FILE content is
        // still readable. Its exact ref ledger remains available in the archive.
        let reader = self.read_owner(id, through).await?;
        let item = reader.snapshot().map_err(corrupt)?.projection;
        let (blob, expected_size) = match item.item_type.as_str() {
            "FILE" => (
                item.content_blob_id
                    .ok_or_else(|| invalid("FILE has no primary content"))?,
                item.size,
            ),
            "EMAIL" => {
                let source = &item.metadata_json["mailcal"]["source"];
                let blob = item.content_roles.get("source")
                    .map(|id| id.as_str())
                    .ok_or_else(|| invalid("EMAIL has no exact imported source"))?
                    .to_string();
                let size = source["size_bytes"]
                    .as_i64()
                    .filter(|size| *size >= 0)
                    .ok_or_else(|| corrupt("EMAIL source descriptor has no valid size"))?;
                (blob, Some(size))
            }
            _ => return Err(invalid("item export requires a FILE or imported EMAIL")),
        };
        let blob = ObjectId::parse(blob).map_err(corrupt)?;
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("export destination must name a new file"))?;
        let parent_path = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()
            .at("resolve file export parent", destination)?;
        crate::agent_locks::guard_output(&self.root, self.repo_id().as_str(), &self.agent_lock_tokens, &parent_path.join(name))?;
        let parent = directory(&parent_path)?;
        match parent.symlink_metadata(name) {
            Ok(_) => return Err(invalid("export destination already exists")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(invalid(error)),
        }
        let temporary = format!(".synck-file-export-{}", Uuid::new_v4());
        let mut options = CapOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = parent
            .open_with(&temporary, &options)
            .at("create verified file export", destination)?
            .into_std();
        let result = (|| {
            let size = self.copy_blob(&blob, &mut output)?;
            if expected_size.is_some_and(|expected| u64::try_from(expected).ok() != Some(size)) {
                return Err(corrupt("source descriptor size differs from verified blob"));
            }
            output
                .sync_all()
                .at("sync verified file export", destination)?;
            Ok(size)
        })();
        drop(output);
        let size = match result {
            Ok(size) => size,
            Err(error) => {
                let _ = parent.remove_file(&temporary);
                return Err(error);
            }
        };
        if let Err(error) = rename_between_noreplace(
            &parent,
            std::ffi::OsStr::new(&temporary),
            &parent,
            std::ffi::OsStr::new(name),
            &parent_path.join(&temporary),
            destination,
        )
        .at("publish verified file export", destination)
        {
            let _ = parent.remove_file(&temporary);
            return Err(error);
        }
        sync_open_directory(&parent, &parent_path).map_err(|error| {
            LocalError::PublishedNeedsDurability {
                operation: "portable file export",
                path: destination.to_path_buf(),
                detail: error.to_string(),
            }
        })?;
        Ok(size)
    }

    async fn references(&self) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
        let mut blobs = BTreeSet::new();
        let mut metadata = BTreeSet::new();
        let mut cursor = None;
        loop {
            let page = self
                .storage
                .list_repo_commit_events_readonly(self.repo_id(), cursor.as_ref(), PAGE)
                .await
                .map_err(corrupt)?;
            if page.is_empty() {
                break;
            }
            let done = page.len() < PAGE;
            for row in page {
                let refs =
                    synck_items::references::commit_event_references(&row).map_err(corrupt)?;
                blobs.extend(
                    refs.objects
                        .into_iter()
                        .map(|id| id.as_str()[7..].to_string()),
                );
                if let Some(id) = refs.metadata { metadata.insert(id.as_str()[7..].to_string()); }
                cursor = Some(RepoEventCursor {
                    repo_seq: row
                        .repo_seq()
                        .ok_or_else(|| corrupt("unsequenced canonical history"))?,
                    commit_event_idx: row.commit_event_idx(),
                });
            }
            if done {
                break;
            }
        }
        Ok((blobs, metadata))
    }

    /// Full explicit integrity/closure verification; bare open never runs it.
    /// List sealed commits newest first, with each commit's events. `before`
    /// is an exclusive repository sequence for paging toward older history.
    pub async fn log(&self, before: Option<i64>, limit: usize) -> Result<PortableLogPage> {
        if !(1..=512).contains(&limit) {
            return Err(invalid("limit must be between 1 and 512"));
        }
        let connection = self.storage.open_readonly().map_err(corrupt)?;
        let mut statement = connection.prepare(
            "SELECT repo_seq FROM vcs_commits WHERE repo_id=?1 AND repo_seq IS NOT NULL \
             AND (?2 IS NULL OR repo_seq<?2) ORDER BY repo_seq DESC LIMIT ?3",
        )?;
        let sequences = statement
            .query_map(
                params![
                    self.repo_id().as_str(),
                    before,
                    i64::try_from(limit).map_err(invalid)?
                ],
                |row| row.get::<_, i64>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let (Some(&newest), Some(&oldest)) = (sequences.first(), sequences.last()) else {
            return Ok(PortableLogPage {
                commits: Vec::new(),
                next_before: None,
            });
        };
        let older: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM vcs_commits WHERE repo_id=?1 AND repo_seq<?2)",
            params![self.repo_id().as_str(), oldest],
            |row| row.get(0),
        )?;
        let rows = self
            .storage
            .list_commits_by_repo_seq_readonly(self.repo_id(), oldest, Some(newest), limit)
            .await
            .map_err(corrupt)?;
        let mut commits = Vec::with_capacity(rows.len());
        for commit in rows.into_iter().rev() {
            let event_count = usize::try_from(commit.event_count).map_err(corrupt)?;
            let events = self
                .storage
                .list_commit_events_readonly(self.repo_id(), &commit.commit_id, event_count + 1)
                .await
                .map_err(corrupt)?
                .into_iter()
                .map(|row| match row {
                    CommitEventRow::Item(event) => PortableLogEvent {
                        ledger: "ITEM",
                        item_id: event.item_id,
                        seq: event.item_seq,
                        event_type: event.event_type,
                    },
                    CommitEventRow::Ref(event) => PortableLogEvent {
                        ledger: "REF",
                        item_id: event.owner_item_id,
                        seq: event.ref_seq,
                        event_type: event.event_type,
                    },
                })
                .collect();
            commits.push(PortableLogEntry {
                repo_seq: commit
                    .repo_seq
                    .ok_or_else(|| corrupt("unsequenced commit"))?,
                commit_id: commit.commit_id,
                created_at: commit.created_at,
                author: commit.author_id.to_string(),
                committer: commit.committer_id.to_string(),
                message: commit.message,
                events,
            });
        }
        Ok(PortableLogPage {
            commits,
            next_before: older.then_some(oldest),
        })
    }

    pub async fn verify(&self) -> Result<VerificationReport> {
        let findings = crate::integrity::physical_core_findings(&self.storage, self.repo_id())?;
        if !findings.is_empty() {
            return Err(corrupt(findings.summary()));
        }
        self.storage.verify_metadata_objects(self.repo_id()).map_err(corrupt)?;
        self.storage.verify_checkpoints(self.repo_id()).map_err(corrupt)?;
        let connection = self.storage.open_readonly().map_err(corrupt)?;
        for table in [
            "vcs_metadata_objects",
            "vcs_commits",
            "vcs_checkpoints",
            "vcs_history_generations",
            "vcs_events",
            "vcs_ref_events",
            "vcs_items",
            "vcs_federation_domains",
            "vcs_repo_activity_heads",
        ] {
            let foreign: bool = connection.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE repo_id<>?1)"),
                [self.repo_id().as_str()],
                |row| row.get(0),
            )?;
            if foreign {
                return Err(corrupt("portable database contains another repository"));
            }
        }
        let pending: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM vcs_commits WHERE commit_id IS NULL OR repo_seq IS NULL)",
            [],
            |row| row.get(0),
        )?;
        if pending {
            return Err(corrupt(
                "repository has incomplete canonical sequencing; explicit finalize is required",
            ));
        }
        let mut expected_heads = HashMap::new();
        let mut verifier = SemanticVerifier::new();
        let mut from = 1;
        let mut count = 0;
        let mut terminal = 0;
        loop {
            let page = self
                .storage
                .list_commits_by_repo_seq_readonly(self.repo_id(), from, None, PAGE)
                .await
                .map_err(corrupt)?;
            if page.is_empty() {
                break;
            }
            let done = page.len() < PAGE;
            for commit in page {
                let rows = self
                    .storage
                    .list_commit_events_readonly(
                        self.repo_id(),
                        &commit.commit_id,
                        commit.event_count as usize + 1,
                    )
                    .await
                    .map_err(corrupt)?;
                if self.config.checkout.is_some() {
                    for row in &rows {
                        if let CommitEventRow::Item(event) = row {
                            if event.event_type == "FILE::SET_CONTENT" {
                                let proposal: EventProposal =
                                    event.proposal()?;
                                let (blob, size) = crate::event_profile::parse_content(&proposal)
                                    .map_err(corrupt)?;
                                expected_heads.insert(
                                    event.item_id.to_string(),
                                    (event.item_seq, blob.to_string(), size),
                                );
                            }
                        }
                    }
                }
                let owned = rows.into_iter().map(VerifyOwned::from).collect::<Vec<_>>();
                let findings = verifier.verify_commit_events(
                    &VerifyCommit {
                        commit_id: Some(commit.commit_id.clone()),
                        repo_seq: commit.repo_seq,
                        format_version: commit.format_version,
                        import_job_id: commit.import_job_id,
                        committer_id: commit.committer_id,
                        occurred_at: commit.occurred_at,
                        author_id: commit.author_id,
                        message: commit.message,
                        event_count: Some(commit.event_count),
                        events_digest: Some(commit.events_digest),
                    },
                    owned.iter().map(VerifyOwned::borrowed),
                );
                if let Some(finding) = findings.into_iter().next() {
                    return Err(corrupt(finding));
                }
                terminal = commit
                    .repo_seq
                    .ok_or_else(|| corrupt("unsequenced commit"))?;
                count += 1;
            }
            from = terminal
                .checked_add(1)
                .ok_or_else(|| corrupt("repository sequence exhausted"))?;
            if done {
                break;
            }
        }
        if self.config.checkout.is_some() {
            let mut findings = crate::integrity::Findings::default();
            crate::integrity::verify_projection(
                &crate::local_database::open_readonly(&self.storage)?,
                self.repo_id().as_str(),
                &expected_heads,
                &mut findings,
            )?;
            if !findings.is_empty() {
                return Err(corrupt(findings.summary()));
            }
        }
        let mut items = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .storage
                .list_items_readonly(self.repo_id(), cursor.as_ref(), None, PAGE)
                .await
                .map_err(corrupt)?;
            if page.is_empty() {
                break;
            }
            let done = page.len() < PAGE;
            for item in page {
                cursor = Some(ItemCursor {
                    created_at: item.created_at,
                    item_id: item.item_id.clone(),
                });
                items.push(VerifyItem {
                    item_id: item.item_id,
                    item_type: item.item_type,
                    parent_folder_id: item.parent_folder_id,
                    name: item.name,
                    name_key: item.name_key,
                    head_seq: item.head_seq,
                    ref_head_seq: item.ref_head_seq,
                    lifecycle_state: item.lifecycle_state,
                    sequencing_mode: item.sequencing_mode,
                    federation_stream_id: item.federation_stream_id,
                });
            }
            if done {
                break;
            }
        }
        let item_count = items.len() as u64;
        if let Some(finding) = verifier.finish(&items).into_iter().next() {
            return Err(corrupt(finding));
        }
        let next: i64 = connection.query_row(
            "SELECT next_repo_seq FROM vcs_repo_activity_heads WHERE repo_id=?1",
            [self.repo_id().as_str()],
            |row| row.get(0),
        )?;
        if terminal.checked_add(1) != Some(next) {
            return Err(corrupt(
                "repository allocator does not follow canonical history",
            ));
        }
        let (objects, metadata) = self.references().await?;
        let inventory = if self.storage_mode() == StorageMode::Chunked {
            Some(self.object_store().verify_all()?)
        } else { None };
        let mut missing_objects = Vec::new();
        for name in &objects {
            let id = ObjectId::parse(format!("sha256:{name}")).map_err(corrupt)?;
            let present = match &inventory {
                Some(inventory) => inventory.contains_key(name),
                None => match self.metadata.symlink_metadata(format!("objects/{name}")) {
                    Ok(_) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(corrupt(error)),
                },
            };
            if !present { missing_objects.push(id); continue; }
            // ChunkStore::verify_all already checked the frame and every chunk.
            if inventory.is_none() {
                self.copy_blob(&id, &mut std::io::sink())?;
            }
        }
        let mut missing_metadata = Vec::new();
        for name in &metadata {
            let id = ObjectId::parse(format!("sha256:{name}")).map_err(corrupt)?;
            if self.storage.get_metadata(self.repo_id(), &id).map_err(corrupt)?.is_none() {
                missing_metadata.push(id);
            }
        }
        Ok(VerificationReport {
            repo_id: self.repo_id().clone(), terminal_repo_seq: terminal,
            commits: count, items: item_count, objects: objects.len(), metadata: metadata.len(),
            missing_objects, missing_metadata,
        })
    }

    /// Resume explicitly accepted local mutations after a sequencing/indexing
    /// failure. Never called by open/read, and never allowed for followers.
    pub async fn finalize(&mut self) -> Result<()> {
        self.writable()?;
        self.storage
            .repair_publication_counter(self.repo_id())
            .map_err(corrupt)?;
        self.finish_acceptance().await
    }

    async fn finish_acceptance(&mut self) -> Result<()> {
        while self
            .core()
            .sequence_repo_activity(self.repo_id().clone(), PAGE)
            .await?
            != 0
        {}
        self.storage
            .project_imported_history(self.repo_id())
            .await
            .map_err(corrupt)?;
        if self.config.checkout.is_some() {
            self.checkout_adapter()?.recover_current().await?;
        }
        let connection = self.storage.open_writable().map_err(corrupt)?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(connection);
        sync_file(&self.root.join(".synck/repository.sqlite3"))?;
        sync_open_directory(&self.metadata, &self.root.join(".synck"))
    }

    async fn accepted(&mut self, commit: AcceptedCommit) -> Result<CommitId> {
        let id = commit.commit_id;
        self.finish_acceptance()
            .await
            .map_err(|error| LocalError::AcceptedNeedsRecovery {
                commit_id: id.to_string(),
                detail: error.to_string(),
            })?;
        Ok(id)
    }

    fn proposal(&self, events: Vec<EventProposal>) -> CommitProposal {
        self.proposal_with(events, &WriteOptions::default())
    }

    fn proposal_with(&self, events: Vec<EventProposal>, options: &WriteOptions) -> CommitProposal {
        let committer = self.config.author.clone().expect("validated local author");
        CommitProposal {
            author_id: options.author.clone().unwrap_or_else(|| committer.clone()),
            committer_id: committer,
            message: options.message.clone(),
            occurred_at: options.occurred_at,
            operation_id: Some(stored_operation_id(
                &options.operation_id.unwrap_or_else(Uuid::now_v7),
            )),
            events,
        }
    }

    /// Find the FILE write already accepted under `operation_id`.
    ///
    /// A reused ID must name the same FILE (or, for a creation, the same
    /// location) and the same bytes; anything else is a caller error.
    async fn replayed_file_write(
        &self,
        operation_id: Option<Uuid>,
        item: Option<&ItemId>,
        location: Option<&Location>,
        content: &BlobRef,
    ) -> Result<Option<(ItemId, CommitId)>> {
        let Some(operation_id) = operation_id else {
            return Ok(None);
        };
        let core = self.core();
        let Some(commit) = core
            .get_commit_by_operation_id(self.repo_id().clone(), &stored_operation_id(&operation_id))
            .await?
        else {
            return Ok(None);
        };
        let events = core
            .list_events_for_commit(
                self.repo_id().clone(),
                commit.commit_id.clone(),
                usize::try_from(commit.event_count).map_err(corrupt)?,
            )
            .await?
            .iter()
            .map(|row| row.proposal())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut written = None;
        for event in &events {
            let Some(blob) = synck_file_folder::file_blob_reference(event).map_err(corrupt)? else { continue; };
            let id = event.item_id.as_ref().ok_or_else(|| corrupt("accepted FILE event has no item ID"))?;
            let same_item = match (item, location) {
                (Some(item), _) => id == item,
                (None, Some(location)) => events.iter().any(|create| {
                    create.item_id.as_ref() == Some(id)
                        && create.create_item.as_ref().is_some_and(|created| {
                            created.item_type == "FILE"
                                && created.name == location.name
                                && created.parent_folder_id == location.parent_item_id
                        })
                }),
                (None, None) => false,
            };
            if same_item && blob.size == content.size {
                let mut previous = crate::objects::PlainDigest::default();
                let mut proposed = crate::objects::PlainDigest::default();
                self.copy_blob(&blob.blob_id, &mut previous)?;
                self.copy_blob(&content.blob_id, &mut proposed)?;
                if previous.finish() == proposed.finish() { written = Some(id.clone()); break; }
            }
        }
        match written {
            Some(id) => Ok(Some((id, commit.commit_id))),
            None => Err(invalid(
                "operation ID was already used for a different write",
            )),
        }
    }

    fn store_bytes(&self, bytes: &[u8]) -> Result<String> {
        if self.storage_mode() == StorageMode::Chunked {
            return self.object_store().store_bytes(bytes);
        }
        let folder = "objects";
        let name = hex::encode(Sha256::digest(bytes));
        let path = self.root.join(".synck").join(folder).join(&name);
        if path.try_exists().at("inspect immutable object", &path)? {
            let dir = self.metadata.open_dir_nofollow(folder).map_err(corrupt)?;
            let mut existing = regular(&dir, &name, &path)?;
            if hash(&mut existing)?.0 != name {
                return Err(corrupt("existing immutable object is corrupt"));
            }
            // A prior attempt may have published this object but failed to
            // sync its directory before returning an error.
            sync_directory(&self.root.join(".synck").join(folder))?;
            return Ok(name);
        }
        let temporary = self
            .root
            .join(".synck/objects")
            .join(format!(".pending-{}", Uuid::new_v4()));
        let result = (|| {
            write_new(&temporary, bytes)?;
            rename_noreplace(&temporary, &path).at("publish immutable object", &path)
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        sync_directory(&self.root.join(".synck").join(folder))?;
        Ok(name)
    }

    fn store_file(&self, source: &Path) -> Result<BlobRef> {
        let file = open_regular_file_nofollow(source)?;
        let before = file.metadata().at("inspect capture source", source)?;
        let regions = OoxmlKind::from_path(source).and_then(|kind| {
            chunk_region_lengths(file.try_clone().ok()?, before.len(), kind)
        });
        let mut file = file;
        std::io::Seek::rewind(&mut file).at("rewind capture source", source)?;
        let store = self.object_store();
        let captured = crate::raw_object::capture(file, before.len(), regions, &self.root.join(".synck/objects"),
            (self.storage_mode() == StorageMode::Chunked).then_some(&store), true)?;
        let after = fs::symlink_metadata(source).at("inspect capture source", source)?;
        if !after.is_file() || before.len() != after.len() || before.modified().ok() != after.modified().ok() {
            return Err(invalid("file changed while being captured"));
        }
        Ok(BlobRef { blob_id:captured.id, size:captured.size })
    }

    async fn profile_current(&self, seeds: Vec<ItemId>) -> Result<BTreeMap<ItemId, CurrentItem>> {
        let mut current = BTreeMap::new();
        let mut pending = seeds;
        while let Some(id) = pending.pop() {
            if current.contains_key(&id) {
                continue;
            }
            let item = self
                .storage
                .get_item_readonly(self.repo_id(), &id)
                .await
                .map_err(corrupt)?
                .ok_or_else(|| invalid("item not found"))?;
            if let Some(parent) = item.parent_folder_id.clone() {
                pending.push(parent);
            }
            current.insert(
                id,
                CurrentItem {
                    item_id: item.item_id,
                    item_type: item.item_type,
                    parent_item_id: item.parent_folder_id,
                    name: item.name,
                    name_key: item.name_key,
                    head_seq: item.head_seq,
                    lifecycle_state: match item.lifecycle_state {
                        LifecycleState::Live => ProfileLifecycleState::Live,
                        LifecycleState::Trash => ProfileLifecycleState::Trash,
                        LifecycleState::Tombstoned => ProfileLifecycleState::Tombstoned,
                    },
                    trash_commit_id: None,
                    content: None,
                },
            );
        }
        Ok(current)
    }

    // Filesystem writes use the normal selected commit path. If acceptance
    // fails, the supplied bytes remain visible as an uncommitted edit; they
    // must never be replaced by stale checkout bytes on the next commit.
    async fn write_checkout_file(
        &self,
        item: Option<&ItemId>,
        parent: Option<&ItemId>,
        name: &str,
        content: &BlobRef,
        options: &WriteOptions,
    ) -> Result<(ItemId, CommitId)> {
        if self
            .config
            .checkout
            .as_ref()
            .is_some_and(|checkout| checkout.pending.is_some())
            || entries(&self.metadata)?
                .iter()
                .any(|name| name.ends_with(".json") && name != "config.json")
        {
            return Err(invalid(
                "finish checkout recovery with synck status before writing files",
            ));
        }
        let working = self.checkout_adapter()?;
        working.recover_current().await?;
        let tree = working.complete_file_tree().await?;
        let path = if let Some(id) = item {
            tree.iter()
                .find(|(_, node)| &node.item_id == id)
                .map(|(path, _)| path.clone())
                .ok_or_else(|| invalid("file has no filesystem checkout path"))?
        } else if let Some(parent) = parent {
            let path = tree
                .iter()
                .find(|(_, node)| {
                    &node.item_id == parent && node.kind == crate::worktree::NodeKind::Folder
                })
                .map(|(path, _)| path)
                .ok_or_else(|| invalid("parent has no filesystem checkout path"))?;
            format!("{path}/{name}")
        } else {
            name.to_owned()
        };
        working.guard_agent_paths(&[&path])?;
        if !working.checkout_observes(&path) {
            return Err(invalid(
                "file is outside checkout selection; widen it first",
            ));
        }
        let changes = working.status().await?;
        if changes.iter().any(|change| {
            change.path == path
                || change.source_path.as_deref() == Some(&path)
                || path.starts_with(&format!("{}/", change.path))
        }) {
            return Err(invalid(
                "commit or restore changes to the target before writing it",
            ));
        }
        if item.is_none() {
            crate::worktree::validate_new_checkout_file(
                working.root(),
                &path,
                working.hidden_files_mode(),
            )?;
        } else {
            let expected = &tree[&path];
            crate::commands::verify_working_file(
                &working,
                &path,
                expected
                    .blob_id
                    .as_ref()
                    .ok_or_else(|| corrupt("file has no blob"))?,
                expected.size.ok_or_else(|| corrupt("file has no size"))?,
            )?;
        }
        working.write_working_file(
            &path,
            &content.blob_id,
            i64::try_from(content.size).map_err(invalid)?,
            item.is_some(),
        )?;
        #[cfg(test)]
        if review_tests::FAIL_CHECKOUT_FILE_COMMIT.with(|fail| fail.replace(false)) {
            return Err(invalid("injected failure before file acceptance"));
        }
        let outcome = working
            .commit_selected_with(working.root_path(), &[PathBuf::from(&path)], options, Some((&path, content)))
            .await?;
        let commit = match outcome {
            crate::CommitOutcome::Committed(id) => CommitId::parse(id).map_err(corrupt)?,
            crate::CommitOutcome::Clean => {
                let head = working
                    .log(Some(1))
                    .await?
                    .into_iter()
                    .next()
                    .ok_or_else(|| corrupt("file write has no history"))?;
                CommitId::parse(head.commit_id).map_err(corrupt)?
            }
        };
        let tree = working.committed_tree().await?;
        let node = tree
            .get(&path)
            .ok_or_else(|| corrupt("file write was not committed"))?;
        if node.plain_digest.as_deref() != Some(working.plain_digest(&content.blob_id)?.as_str()) {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id: commit.to_string(),
                detail: "file changed during write; inspect status and history before retrying"
                    .into(),
            });
        }
        Ok((node.item_id.clone(), commit))
    }

    pub async fn create_file(
        &mut self,
        parent: Option<ItemId>,
        name: &str,
        source: &Path,
    ) -> Result<(ItemId, CommitId)> {
        self.create_file_with(parent, name, source, &WriteOptions::default())
            .await
    }

    /// Create a FILE, or import an EML/MSG source as an EMAIL, with caller
    /// attribution. Email imports do not accept an operation ID.
    pub async fn create_file_with(
        &mut self,
        parent: Option<ItemId>,
        name: &str,
        source: &Path,
        options: &WriteOptions,
    ) -> Result<(ItemId, CommitId)> {
        self.writable()?;
        options.validate()?;
        let filename = source.file_name();
        let original_name = filename.map(|name| name.to_string_lossy());
        if original_name.as_deref().is_some_and(|name| {
            synck_items::imported_email::ImportedEmailFormat::from_name(name).is_some()
        }) || synck_items::imported_email::ImportedEmailFormat::from_name(name).is_some()
        {
            let original_name = filename.and_then(|name| name.to_str()).ok_or_else(|| {
                invalid("EMAIL_IMPORT_INVALID_FORMAT: email source filename must be UTF-8")
            })?;
            if options.operation_id.is_some() {
                return Err(invalid("email imports do not accept an operation ID"));
            }
            return self
                .import_email_with(parent, original_name, name, source, options)
                .await;
        }
        let location = Location {
            parent_item_id: parent.clone(),
            name: name.into(),
        };
        let content = self.store_file(source)?;
        if let Some(replayed) = self
            .replayed_file_write(options.operation_id, None, Some(&location), &content)
            .await?
        {
            return Ok(replayed);
        }
        if let Some(parent) = parent.as_ref() {
            self.local_owner(parent, "FOLDER").await?;
        }
        let name_key = synck_vcs::naming::compute_name_key(name).map_err(invalid)?;
        let connection = self.storage.open_readonly().map_err(corrupt)?;
        let occupied:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM vcs_items WHERE repo_id=?1 AND parent_folder_id IS ?2 AND name_key=?3 AND lifecycle_state='LIVE')",params![self.repo_id().as_str(),parent.as_ref().map(ItemId::as_str),name_key],|row|row.get(0))?;
        if occupied {
            return Err(invalid("an item already occupies the requested name"));
        }
        let current = self
            .profile_current(parent.iter().cloned().collect())
            .await?;
        let id = ItemId::new();
        let materialized = synck_file_folder::validate_and_materialize(
            self.repo_id(),
            &IntentProposal {
                message: None,
                intents: vec![Intent::CreateFile {
                    item_id: id.clone(),
                    location,
                    content: content.clone(),
                }],
            },
            &current,
        )?;
        if self.config.checkout.is_some() {
            return self
                .write_checkout_file(None, parent.as_ref(), name, &content, options)
                .await;
        }
        let commit = self
            .core()
            .append_file_folder_checked(
                self.repo_id().clone(),
                self.proposal_with(materialized.events, options),
                &[],
                &materialized.subtree_guards,
            )
            .await?;
        Ok((id, self.accepted(commit).await?))
    }

    /// Import exact EML/MSG source bytes as one EMAIL and its attachment subitems.
    /// Source header identities and dates never replace the local commit author/time.
    pub async fn import_email(
        &mut self,
        parent: Option<ItemId>,
        filename: &str,
        source: &Path,
    ) -> Result<(ItemId, CommitId)> {
        self.import_email_named(parent, filename, filename, source)
            .await
    }

    /// Preserve the selected filename independently from the requested item name.
    pub async fn import_email_named(
        &mut self,
        parent: Option<ItemId>,
        source_filename: &str,
        requested_name: &str,
        source: &Path,
    ) -> Result<(ItemId, CommitId)> {
        self.import_email_with(
            parent,
            source_filename,
            requested_name,
            source,
            &WriteOptions::default(),
        )
        .await
    }

    async fn import_email_with(
        &mut self,
        parent: Option<ItemId>,
        source_filename: &str,
        requested_name: &str,
        source: &Path,
        options: &WriteOptions,
    ) -> Result<(ItemId, CommitId)> {
        self.writable()?;
        if let Some(parent) = parent.as_ref() {
            self.local_owner(parent, "FOLDER").await?;
        }
        let id = ItemId::new();
        let prepared = crate::email_import::prepare(
            &id,
            parent.clone(),
            source_filename,
            requested_name,
            source,
        )?;
        let name = &prepared.events[0]
            .create_item
            .as_ref()
            .expect("prepared EMAIL creation")
            .name;
        let key = synck_vcs::naming::compute_name_key(name).map_err(invalid)?;
        let occupied: bool=self.storage.open_readonly().map_err(corrupt)?.query_row(
            "SELECT EXISTS(SELECT 1 FROM vcs_items WHERE repo_id=?1 AND parent_folder_id IS ?2 AND name_key=?3 AND lifecycle_state='LIVE')",
            params![self.repo_id().as_str(),parent.as_ref().map(ItemId::as_str),key],|row|row.get(0))?;
        if occupied {
            return Err(invalid("an item already occupies the requested email name"));
        }
        for (_, bytes) in prepared.objects {
            self.store_bytes(&bytes)?;
        }
        let commit = self
            .core()
            .append(
                self.repo_id().clone(),
                self.proposal_with(prepared.events, options),
            )
            .await?;
        Ok((id, self.accepted(commit).await?))
    }

    pub async fn save_file(&mut self, id: &ItemId, source: &Path) -> Result<CommitId> {
        self.save_file_with(id, source, None, &WriteOptions::default())
            .await
    }

    /// Record new FILE content with caller attribution.
    ///
    /// With `base_content`, the save is accepted only while the FILE's current
    /// content is exactly that blob; otherwise it fails with
    /// `LocalError::Conflict` carrying the current head. A replayed operation
    /// ID returns its original commit before any base comparison.
    pub async fn save_file_with(
        &mut self,
        id: &ItemId,
        source: &Path,
        base_content: Option<&ObjectId>,
        options: &WriteOptions,
    ) -> Result<CommitId> {
        self.writable()?;
        options.validate()?;
        if let Some(filename) = source.file_name() {
            crate::email_import::reject_file_email_update(&filename.to_string_lossy())?;
        }
        let content = self.store_file(source)?;
        if let Some((_, commit)) = self
            .replayed_file_write(options.operation_id, Some(id), None, &content)
            .await?
        {
            return Ok(commit);
        }
        let item = self.local_owner(id, "FILE").await?;
        crate::email_import::reject_file_email_update(&item.name)?;
        let readable = self.show_item(id, None).await?;
        let head = match (
            readable.item.projection.content_blob_id,
            readable.item.projection.size,
        ) {
            (Some(blob), Some(size)) => Some(BlobRef {
                blob_id: ObjectId::parse(blob).map_err(corrupt)?,
                size: u64::try_from(size).map_err(corrupt)?,
            }),
            _ => None,
        };
        if let Some(expected) = base_content {
            let head = head
                .as_ref()
                .ok_or_else(|| corrupt("FILE has no current content"))?;
            if *expected != head.blob_id {
                return Err(LocalError::Conflict {
                    item_id: id.to_string(),
                    head_seq: item.head_seq,
                    blob_id: head.blob_id.to_string(),
                    size: head.size,
                    expected: expected.to_string(),
                });
            }
        }
        if self.config.checkout.is_some() {
            return self
                .write_checkout_file(Some(id), None, &item.name, &content, options)
                .await
                .map(|(_, commit)| commit);
        }
        let mut current = self.profile_current(vec![id.clone()]).await?;
        current.get_mut(id).expect("target loaded").content = head;
        let materialized = synck_file_folder::validate_and_materialize(
            self.repo_id(),
            &IntentProposal {
                message: None,
                intents: vec![Intent::UpdateFile {
                    item_id: id.clone(),
                    base_seq: item.head_seq,
                    location: None,
                    content: Some(content.clone()),
                }],
            },
            &current,
        )?;
        let commit = self
            .core()
            .append_file_folder_checked(
                self.repo_id().clone(),
                self.proposal_with(materialized.events, options),
                &[],
                &materialized.subtree_guards,
            )
            .await?;
        self.accepted(commit).await
    }

    pub async fn create_chat(&mut self, title: &str) -> Result<(ItemId, CommitId)> {
        self.writable()?;
        if title.trim().is_empty() {
            return Err(invalid("chat title must not be empty"));
        }
        let id = ItemId::new();
        let profile = json!({"chat_kind":"ROOM"});
        synck_items::subitems::parse_chat_profile_event(&profile).map_err(invalid)?;
        let events = vec![
            EventProposal::from_fields(
                json!({"item_id":id,"event_type":"CHAT::CREATE","create_item":{"item_type":"CHAT","name":format!("chat-{id}"),"parent_folder_id":null},"set_metadata":{"non_tree":true,"chat_title":title}}),
            )?,
            EventProposal::from_fields(
                json!({"item_id":id,"event_type":"CHAT::SET_PROFILE","set_metadata":profile}),
            )?,
        ];
        let accepted = self
            .core()
            .append(self.repo_id().clone(), self.proposal(events))
            .await?;
        Ok((id, self.accepted(accepted).await?))
    }

    pub async fn append_chat_message(&mut self, chat: &ItemId, body: &str) -> Result<CommitId> {
        self.append_chat_message_with_attachments(chat, body, &[])
            .await
    }

    pub async fn append_chat_message_with_attachments(
        &mut self,
        chat: &ItemId,
        body: &str,
        attachments: &[PathBuf],
    ) -> Result<CommitId> {
        let owner = self.local_owner(chat, "CHAT").await?;
        synck_items::body_events::validate_body("CHAT::MESSAGE::CREATE", body).map_err(invalid)?;
        let object = Object::new(Codec::Utf8, body.as_bytes()).map_err(invalid)?;
        let content_id = object.id();
        self.store_bytes(object.payload())?;
        let event: EventProposal = EventProposal::from_fields(
            json!({"item_id":chat,"event_type":"CHAT::MESSAGE::CREATE","set_content":EventContent::body(content_id)}),
        )?;
        synck_items::subitems::extract_chat_message_fields(
            self.config
                .author
                .as_deref()
                .expect("validated local author"),
            self.config
                .author
                .as_deref()
                .expect("validated local author"),
            &json!({}),
            &json!({"body":body}),
        )
        .map_err(invalid)?;
        let mut events = vec![event];
        for source in attachments {
            let blob = self.store_file(source)?;
            let name = source
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid("attachment filename must be UTF-8"))?;
            let meta = json!({"kind":"EMBEDDED","name":name,"size":blob.size});
            let content = json!({"blob_id":blob.blob_id});
            synck_items::subitems::extract_chat_attachment_fields(&meta, &content)
                .map_err(invalid)?;
            events.push(EventProposal::from_fields(json!({"item_id":chat,"event_type":"CHAT::MESSAGE.ATTACHMENT::CREATE","set_metadata":{"parent_item_seq":owner.head_seq + 1,"meta":meta},"set_content":EventContent::source(blob.blob_id)}))?);
        }
        let commit = self
            .core()
            .append_materializing_base_seq(self.repo_id().clone(), self.proposal(events))
            .await?;
        self.accepted(commit).await
    }

    /// Copy a complete verified repository to an absent destination. The copy
    /// starts as a follower and needs no identity until explicit activation.
    pub async fn archive(&self, destination: &Path) -> Result<()> {
        self.archive_with_mode(destination, self.storage_mode())
            .await
    }

    pub async fn archive_with_mode(&self, destination: &Path, mode: StorageMode) -> Result<()> {
        if let Some(checkout) = &self.config.checkout {
            if checkout.pending.is_some()
                || !entries(&self.metadata.open_dir_nofollow("tmp").map_err(corrupt)?)?.is_empty()
            {
                return Err(invalid(
                    "finish checkout recovery before copying the repository",
                ));
            }
        }
        let report = self.verify().await?;
        if !report.missing_objects.is_empty() || !report.missing_metadata.is_empty() {
            return Err(invalid("complete export requires all referenced content and metadata objects"));
        }
        let (objects_to_copy, _) = self.references().await?;
        crate::agent_locks::guard_output(&self.root, self.repo_id().as_str(), &self.agent_lock_tokens, destination)?;
        let (parent, parent_path, name, staging) = staging(destination)?;
        let result = async {
            let metadata = staging.join(".synck");
            private_directory(&metadata)?;
            let objects = metadata.join("objects");
            private_directory(&objects)?;
            write_new(&metadata.join("lock"), &[])?;
            let database = metadata.join("repository.sqlite3");
            create_file(&database)?;
            // SQLite's backup reads the complete committed snapshot, including
            // pages still in WAL. Copying the main database file would lose them.
            self.storage.open_readonly().map_err(corrupt)?.backup(
                rusqlite::MAIN_DB,
                &database,
                None,
            )?;
            let storage = SqliteStorage::new(database.clone());
            // Establish the ordinary local WAL profile on the private copy.
            drop(storage.open_writable().map_err(corrupt)?);
            let preserve_chunks =
                self.storage_mode() == StorageMode::Chunked && mode == StorageMode::Chunked;
            write_new(
                &metadata.join("config.json"),
                &serde_json::to_vec(&Config {
                    format_version: FORMAT_VERSION,
                    storage_mode: if preserve_chunks {
                        StorageMode::Chunked
                    } else {
                        StorageMode::WholeFile
                    },
                    repo_id: self.repo_id().clone(),
                    author: None,
                    authority: Authority::Follower,
                    checkout: None,
                })?,
            )?;
            if preserve_chunks {
                let target = metadata.join(OBJECT_STORE_FILE);
                self.object_store().backup_to(&target)?;
                let store = ChunkStore::new(target, self.repo_id());
                store.recover_incomplete()?;
                store.prune_to(&objects_to_copy)?;
                store.verify_archive_closure()?;
            } else {
                for (folder, names) in [("objects", objects_to_copy)] {
                    let source = self.metadata.open_dir_nofollow(folder).map_err(corrupt)?;
                    for name in names {
                        let destination = objects.join(&name);
                        if !destination
                            .try_exists()
                            .at("inspect copied object", &destination)?
                        {
                            if self.storage_mode() == StorageMode::Chunked {
                                let mut file = create_file(&destination)?;
                                self.copy_stored_object(
                                    &ObjectId::parse(format!("sha256:{name}")).map_err(corrupt)?,
                                    &mut file,
                                )?;
                                file.sync_all().at("sync copied object", &destination)?;
                            } else {
                                copy_named(
                                    regular(
                                        &source,
                                        &name,
                                        &self.root.join(".synck").join(folder).join(&name),
                                    )?,
                                    &destination,
                                    &name,
                                )?;
                            }
                        }
                    }
                }
            }
            sync_directory(&objects)?;
            let mut repository = PortableRepository::open(&staging)?;
            if mode == StorageMode::Chunked && !preserve_chunks {
                repository.optimize_storage().await?;
            }
            repository.verify().await?;
            drop(repository);
            let connection = storage.open_writable().map_err(corrupt)?;
            connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            drop(connection);
            sync_file(&database)?;
            sync_directory(&metadata)?;
            sync_directory(&staging)
        }
        .await;
        if let Err(error) = result {
            let _ = remove_tree_nofollow(&staging);
            return Err(error);
        }
        publish(
            &parent,
            &parent_path,
            &name,
            &staging,
            destination,
            "portable repository copy",
        )
    }
}

enum VerifyOwned {
    Item(VerifyEvent),
    Ref(VerifyRefEvent),
}
impl From<CommitEventRow> for VerifyOwned {
    fn from(row: CommitEventRow) -> Self {
        match row {
            CommitEventRow::Item(e) => Self::Item(VerifyEvent {
                commit_event_idx: e.commit_event_idx,
                item_id: e.item_id,
                item_seq: e.item_seq,
                event_type: e.event_type,
                payload: e.payload,
                event_payload_digest: synck_vcs::types::sha256_digest_to_digest_bytes(
                    &e.event_payload_digest,
                )
                .to_vec(),
                parent_folder_id: e.parent_folder_id,
                parent_folder_id_before: e.parent_folder_id_before,
            }),
            CommitEventRow::Ref(e) => Self::Ref(VerifyRefEvent {
                commit_event_idx: e.commit_event_idx,
                owner_item_id: e.owner_item_id,
                ref_seq: e.ref_seq,
                event_type: e.event_type,
                payload: e.payload,
                event_payload_digest: synck_vcs::types::sha256_digest_to_digest_bytes(
                    &e.event_payload_digest,
                )
                .to_vec(),
            }),
        }
    }
}
impl VerifyOwned {
    fn borrowed(&self) -> VerifyCommitEvent<'_> {
        match self {
            Self::Item(e) => VerifyCommitEvent::Item(e),
            Self::Ref(e) => VerifyCommitEvent::Ref(e),
        }
    }
}

fn staging(destination: &Path) -> Result<(Dir, PathBuf, String, PathBuf)> {
    let name = destination
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("destination must name a new directory"))?
        .to_string();
    let parent_path = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .at("resolve destination parent", destination)?;
    let parent = directory(&parent_path)?;
    match parent.symlink_metadata(&name) {
        Ok(_) => return Err(invalid("destination already exists")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(invalid(error)),
    }
    let staging_name = format!(".synck-portable-{}", Uuid::new_v4());
    parent
        .create_dir(&staging_name)
        .at("create portable staging", &parent_path)?;
    let staging = parent_path.join(staging_name);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))
            .at("protect portable staging", &staging)?;
    }
    Ok((parent, parent_path.clone(), name, staging))
}

fn publish(
    parent: &Dir,
    parent_path: &Path,
    name: &str,
    staging: &Path,
    destination: &Path,
    operation: &'static str,
) -> Result<()> {
    if let Err(error) = rename_between_noreplace(
        parent,
        staging.file_name().expect("staging name"),
        parent,
        std::ffi::OsStr::new(name),
        staging,
        destination,
    )
    .at("publish portable directory", destination)
    {
        let _ = remove_tree_nofollow(staging);
        return Err(error);
    }
    sync_open_directory(parent, parent_path).map_err(|error| LocalError::PublishedNeedsDurability {
        operation,
        path: destination.to_path_buf(),
        detail: error.to_string(),
    })
}

/// Handle complete repositories before working-copy CLI discovery (bare or checked out).
pub async fn optimize_complete_repository(start: &Path) -> Result<Option<bool>> {
    let root = crate::repository::discover_root(start)?;
    let metadata = directory(&root.join(".synck"))?;
    let bytes = read_small(
        &metadata,
        "config.json",
        64 * 1024,
        &root.join(".synck/config.json"),
    )?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value.get("format_version").and_then(|v| v.as_u64()) != Some(u64::from(FORMAT_VERSION)) {
        return Ok(None);
    }
    Ok(Some(
        PortableRepository::open(&root)?.optimize_storage().await?,
    ))
}

/// Verify canonical history before opening the mutable checkout adapter.
pub async fn verify_complete_repository(start: &Path) -> Result<Option<VerificationReport>> {
    let root = crate::repository::discover_root(start)?;
    let metadata = directory(&root.join(".synck"))?;
    let bytes = read_small(
        &metadata,
        "config.json",
        64 * 1024,
        &root.join(".synck/config.json"),
    )?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value.get("format_version").and_then(|v| v.as_u64()) != Some(u64::from(FORMAT_VERSION)) {
        return Ok(None);
    }
    let repository = PortableRepository::open(&root)?;
    let report = repository.verify().await?;
    Ok(Some(report))
}

/// Verified complete-repository copy into an absent destination; source is unchanged.
pub async fn import_repository(source: &Path, destination: &Path) -> Result<()> {
    PortableRepository::open(source)?.archive(destination).await
}

/// Verified copy with an explicit physical layout; source is unchanged.
pub async fn import_repository_with_mode(
    source: &Path,
    destination: &Path,
    mode: StorageMode,
) -> Result<()> {
    PortableRepository::open(source)?
        .archive_with_mode(destination, mode)
        .await
}

/// Materialize an internal canonical bundle and its exact unified object closure
/// as a normal local repository. Bundle provenance is not a target migration.
/// The snapshot covers final item/domain records as well as canonical commits.
pub async fn materialize_repository(
    history_path: &Path,
    objects: &Path,
    snapshot: &BundleSnapshot,
    destination: &Path,
) -> Result<VerificationReport> {
    if snapshot.terminal_repo_seq < 0 {
        return Err(invalid("invalid bundle checkpoint"));
    }
    let source_objects = directory(objects)?;
    let history = open_regular_file_nofollow(history_path)?;
    let (parent, parent_path, name, staging) = staging(destination)?;
    let result = async {
        let metadata = staging.join(".synck");
        private_directory(&metadata)?;
        private_directory(&metadata.join("objects"))?;
        write_new(&metadata.join("lock"), &[])?;
        let private_history = metadata.join("history.ndjson");
        let (digest, size) = copy_new(history, &private_history)?;
        if size != snapshot.history_bytes || format!("sha256:{digest}") != snapshot.history_sha256 {
            return Err(corrupt(
                "history size or SHA-256 differs from complete bundle snapshot",
            ));
        }
        let history = open_regular_file_nofollow(&private_history)?;
        let storage = SqliteStorage::new(metadata.join("repository.sqlite3"));
        let report = storage
            .import_bundle(&mut BufReader::new(history))
            .await
            .map_err(corrupt)?;
        fs::remove_file(&private_history).at("remove private history staging", &private_history)?;
        if report.repo_id != snapshot.repo_id
            || report.terminal_repo_seq != snapshot.terminal_repo_seq
        {
            return Err(corrupt(
                "imported repository/checkpoint differs from bundle snapshot",
            ));
        }
        write_new(
            &metadata.join("config.json"),
            &serde_json::to_vec(&Config {
                format_version: FORMAT_VERSION,
                storage_mode: StorageMode::WholeFile,
                repo_id: report.repo_id,
                author: None,
                authority: Authority::Follower,
                checkout: None,
            })?,
        )?;
        let repository = PortableRepository::open(&staging)?;
        let (required, _) = repository.references().await?;
        if entries(&source_objects)? != required {
            return Err(corrupt(
                "objects inventory is not the exact retained-history closure",
            ));
        }
        for name in required {
            digest_name(&name)?;
            copy_named(
                regular(&source_objects, &name, &objects.join(&name))?,
                &metadata.join("objects").join(&name),
                &name,
            )?;
        }
        sync_directory(&metadata.join("objects"))?;
        let verification = repository.verify().await?;
        drop(repository);
        let connection = storage.open_writable().map_err(corrupt)?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(connection);
        sync_file(&metadata.join("repository.sqlite3"))?;
        sync_directory(&metadata)?;
        sync_directory(&staging)?;
        Ok(verification)
    }
    .await;
    let verification = match result {
        Ok(report) => report,
        Err(error) => {
            let _ = remove_tree_nofollow(&staging);
            return Err(error);
        }
    };
    publish(
        &parent,
        &parent_path,
        &name,
        &staging,
        destination,
        "portable repository materialization",
    )?;
    Ok(verification)
}

/// Create an independent complete repository and checkout. Existing ordinary
/// files remain uncommitted; existing metadata and nested repositories are refused.
pub async fn init_repository(destination: &Path, author: &str) -> Result<RepoId> {
    init_repository_with_mode(destination, author, StorageMode::Chunked).await
}

pub async fn init_repository_with_mode(
    destination: &Path,
    author: &str,
    storage_mode: StorageMode,
) -> Result<RepoId> {
    Ok(init_repository_with_name_repair(destination, author, storage_mode, false)
        .await?
        .0)
}

/// Initialize a checkout and optionally repair its existing filesystem names.
/// Repairs run after publishing metadata, so an interruption leaves a recoverable
/// journal in the initialized repository rather than untracked temporary files.
pub async fn init_repository_with_name_repair(
    destination: &Path,
    author: &str,
    storage_mode: StorageMode,
    fix_names: bool,
) -> Result<(RepoId, Vec<crate::NameRepair>)> {
    author_valid(author)?;
    let existing = match fs::symlink_metadata(destination) {
        Ok(_) => Some(
            destination
                .canonicalize()
                .at("resolve new repository", destination)?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(invalid(error)),
    };
    let ancestor_root = match &existing {
        Some(root) => {
            directory(root)?;
            root.clone()
        }
        None => destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()
            .at("resolve initialization parent", destination)?,
    };
    for ancestor in ancestor_root.ancestors() {
        match fs::symlink_metadata(ancestor.join(".synck")) {
            Ok(_) => return Err(invalid("new repository is inside an existing repository")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(invalid(error)),
        }
    }
    let ignore_bytes = if let Some(root) = &existing {
        match fs::symlink_metadata(root.join(".synckignore")) {
            Ok(_) => {
                crate::worktree::validate_synckignore(root)?;
                let mut bytes = Vec::new();
                open_regular_file_nofollow(&root.join(".synckignore"))?
                    .read_to_end(&mut bytes)
                    .map_err(invalid)?;
                Some(bytes)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(invalid(error)),
        }
    } else {
        None
    };
    let (parent, parent_path, name, staging) = if let Some(root) = existing.as_ref() {
        let name = format!(".synck-init-{}", Uuid::new_v4());
        let staging = root.join(&name);
        private_directory(&staging)?;
        (directory(root)?, root.clone(), name, staging)
    } else {
        staging(destination)?
    };
    let repo_id = RepoId::new();
    let result = async {
        let metadata = staging.join(".synck");
        private_directory(&metadata)?;
        for directory_name in ["objects", "tmp"] {
            private_directory(&metadata.join(directory_name))?;
        }
        write_new(&metadata.join("lock"), &[])?;
        write_new(&metadata.join(crate::repository::MONITOR_LOCK_FILE), &[])?;
        let database = metadata.join("repository.sqlite3");
        let storage = SqliteStorage::new(database.clone());
        storage.init_schema().await.map_err(corrupt)?;
        let core = Repo::new(
            Storage::new(Arc::new(storage.clone())),
            CoreConfig::default(),
        );
        core.sequence_repo_activity(repo_id.clone(), 1).await?;
        storage
            .project_imported_history(&repo_id)
            .await
            .map_err(corrupt)?;
        crate::repository::initialize_checkout_schema(&storage)?;
        write_new(
            &metadata.join("config.json"),
            &serde_json::to_vec(&Config {
                format_version: FORMAT_VERSION,
                storage_mode,
                repo_id: repo_id.clone(),
                author: Some(author.into()),
                authority: Authority::IndependentLocal,
                checkout: Some(crate::checkout::Checkout {
                    hidden_files: Default::default(),
                    paths: Vec::new(),
                    pending: None,
                }),
            })?,
        )?;
        if storage_mode == StorageMode::Chunked {
            ChunkStore::create(&metadata.join(OBJECT_STORE_FILE), &repo_id)?;
        }
        if let Some(bytes) = &ignore_bytes {
            write_new(&staging.join(".synckignore"), bytes)?;
        } else {
            crate::repository::create_default_synckignore(&staging)?;
        }
        if let Some(root) = &existing {
            crate::worktree::validate_initial_tree(&directory(root)?, &directory(&staging)?, fix_names)?;
        }
        let working = crate::Repository::discover(&staging).await?;
        if !matches!(
            working
                .commit_selected(
                    &staging,
                    &[PathBuf::from(".synckignore")],
                    Some("Initialize Synck repository")
                )
                .await?,
            crate::CommitOutcome::Committed(_)
        ) {
            return Err(corrupt("initial ignore commit produced no changes"));
        }
        drop(working);
        let candidate = PortableRepository::open(&staging)?;
        candidate.verify().await?;
        drop(candidate);
        let connection = storage.open_writable().map_err(corrupt)?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(connection);
        sync_file(&database)?;
        sync_directory(&metadata)?;
        sync_directory(&staging)
    }
    .await;
    if let Err(error) = result {
        let _ = remove_tree_nofollow(&staging);
        return Err(error);
    }
    if let Some(root) = existing {
        let publication = (|| {
            let target = root.join(".synck");
            if let Some(bytes) = &ignore_bytes {
                let mut current = Vec::new();
                open_regular_file_nofollow(&root.join(".synckignore"))?
                    .read_to_end(&mut current)
                    .map_err(invalid)?;
                if &current != bytes {
                    return Err(invalid(".synckignore changed during initialization"));
                }
                fs::remove_file(staging.join(".synckignore")).map_err(invalid)?;
            } else {
                rename_noreplace(&staging.join(".synckignore"), &root.join(".synckignore"))
                    .at("publish ignore file", &root)?;
            }
            if let Err(error) = rename_noreplace(&staging.join(".synck"), &target) {
                if ignore_bytes.is_none() {
                    let _ = fs::remove_file(root.join(".synckignore"));
                }
                let _ = remove_tree_nofollow(&staging);
                return Err(invalid(error));
            }
            fs::remove_dir(&staging).at("remove initialization staging", &staging)?;
            sync_directory(&root).map_err(|error| LocalError::PublishedNeedsDurability {
                operation: "complete repository initialization",
                path: root,
                detail: error.to_string(),
            })?;
            Ok(())
        })();
        if publication.is_err() {
            let _ = remove_tree_nofollow(&staging);
        }
        publication?;
    } else {
        publish(
            &parent,
            &parent_path,
            &name,
            &staging,
            destination,
            "complete repository initialization",
        )?;
    }
    let repairs = if fix_names {
        crate::Repository::discover(destination).await?.fix_names().await?
    } else {
        Vec::new()
    };
    Ok((repo_id, repairs))
}

#[cfg(test)]
mod review_tests {
    use super::*;

    thread_local! {
        pub(super) static FAIL_CHECKOUT_FILE_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[tokio::test]
    async fn failed_checkout_file_acceptance_leaves_a_recoverable_working_edit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        init_repository(&root, "tester").await.unwrap();
        let source = temp.path().join("source.txt");
        fs::write(&source, b"preserve on failure").unwrap();
        let mut repo = PortableRepository::open(&root).unwrap();
        FAIL_CHECKOUT_FILE_COMMIT.with(|fail| fail.set(true));
        let error = repo
            .create_file(None, "new.txt", &source)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        assert_eq!(
            fs::read(root.join("new.txt")).unwrap(),
            b"preserve on failure"
        );
        assert_eq!(repo.verify().await.unwrap().commits, 1);
        drop(repo);
        let working = crate::Repository::discover(&root).await.unwrap();
        assert_eq!(working.status().await.unwrap().len(), 1);
        working.commit("retry preserved edit").await.unwrap();
        assert!(working.status().await.unwrap().is_empty());
        working.verify_complete().await.unwrap();
    }

    #[tokio::test]
    async fn open_reads_storage_authority_after_a_concurrent_optimization() {
        for checkout_opener in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("repo");
            init_repository_with_mode(&root, "tester", StorageMode::WholeFile)
                .await
                .unwrap();
            let optimize_root = root.clone();
            crate::repository::OPEN_LOCK_HOOK.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    // Pause this opener immediately before it acquires the lock;
                    // another writer completes conversion before it resumes.
                    std::thread::spawn(move || {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap()
                            .block_on(async {
                                PortableRepository::open(&optimize_root)
                                    .unwrap()
                                    .optimize_storage()
                                    .await
                                    .unwrap();
                            });
                    })
                    .join()
                    .unwrap();
                }))
            });
            if checkout_opener {
                let repo = crate::Repository::discover(&root).await.unwrap();
                assert_eq!(repo.storage_mode(), StorageMode::Chunked);
                fs::write(root.join("file.txt"), b"after optimize").unwrap();
                repo.commit("use current storage authority").await.unwrap();
                repo.verify_complete().await.unwrap();
            } else {
                let mut repo = PortableRepository::open(&root).unwrap();
                assert_eq!(repo.storage_mode(), StorageMode::Chunked);
                let source = temp.path().join("source.txt");
                fs::write(&source, b"after optimize").unwrap();
                repo.create_file(None, "file.txt", &source).await.unwrap();
                repo.verify().await.unwrap();
            }
        }
    }
}
