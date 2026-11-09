//! A repository published by a sync server. Reads take no lock and write
//! nothing: published history is immutable and each read is one SQLite
//! snapshot, so the repository's own working tree keeps working. Writes wait for
//! the repository lock, as a clone's commands wait for their origin's, and the
//! working tree takes in a commit accepted here at its next `pull`, as it takes
//! in a clone's. Only a repository that orders its own history can be served.

use std::collections::{BTreeMap, BTreeSet};

use synck_sync::{CommitMetadata, Cursor, HistoryPage, HistoryRequest, ItemHead};
use synck_vcs::types::{CommitRow, RefEventProposal};

use crate::agent_locks::{self, AgentLease, AgentLock};

use super::*;

/// Commits accepted here are found again by this operation ID; the prefix keeps
/// a client's IDs apart from the repository's other writes.
fn sync_operation_id(operation_id: &str) -> String {
    format!("sync:{}", hex::encode(Sha256::digest(operation_id.as_bytes())))
}

/// Core sequences in pages of this many commits.
const SEQUENCE_PAGE: usize = 256;
/// The working tree recorded for reservations taken through a server: never
/// a local tree, so a local `pull` does not answer to them.
const REMOTE_TREE: &str = "sync";
/// Bounds a walk up the folder tree.
const MAX_DEPTH: usize = 4096;

pub struct ServedRepository {
    root: PathBuf,
    metadata: Dir,
    repo_id: RepoId,
    encrypted: bool,
    storage_mode: StorageMode,
    storage: SqliteStorage,
    /// Idle connections to the history and object databases, held so their
    /// WAL indexes survive between requests. The databases keep their WAL
    /// when a connection closes, and the first connection to open one
    /// rebuilds its index from the whole WAL. Object uploads leave their
    /// frames in the WAL until `accept` checkpoints.
    _wal_indexes: std::sync::Mutex<Vec<rusqlite::Connection>>,
}

/// A verified protocol-1 upload, in stream order.
/// One object of a pack.
pub enum PackEntry {
    Metadata(Vec<u8>),
    Content(u64),
}

pub struct Proposal {
    pub repository_head: Option<u64>,
    pub operation_id: String,
    /// What the submission's digests say the commit is.
    pub commit_id: CommitId,
    pub metadata: CommitMetadata,
    pub events: Vec<EventProposal>,
    pub ref_events: Vec<RefEventProposal>,
    pub guards: Vec<ItemHead>,
    pub subtree_guards: Vec<synck_vcs::types::SubtreeClosureGuard>,
    /// Reservation tokens the writer holds, as `--lock-token` gives them.
    pub lock_tokens: Vec<String>,
}

#[derive(Debug)]
pub enum Acceptance {
    Accepted(Accepted),
    /// The heads of every item the proposal touches; zero for an absent item.
    Conflict(Vec<ItemHead>),
    /// Content the proposal names that the store does not hold.
    MissingObjects(Vec<ObjectId>),
}

#[derive(Debug)]
pub struct Accepted {
    pub commit_id: CommitId,
    pub accepted_at: Timestamp,
    /// `None` while the commit awaits sequencing.
    pub published: Option<Cursor>,
}

impl ServedRepository {
    pub fn open(root: &Path) -> Result<Self> {
        let root_dir = directory(root)?;
        let metadata = root_dir
            .open_dir_nofollow(".synck")
            .at("open portable metadata", root.join(".synck"))?;
        let config = read_config(&metadata, root)?;
        if config.authority == Authority::Follower {
            return Err(invalid(
                "a follower does not order its history; serve the repository it follows",
            ));
        }
        regular(
            &metadata,
            "repository.sqlite3",
            &root.join(".synck/repository.sqlite3"),
        )?;
        let storage = SqliteStorage::new(root.join(".synck/repository.sqlite3"))
            .with_encrypted(config.encrypted);
        let mut wal_indexes = vec![storage.open_readonly().map_err(corrupt)?];
        if config.storage_mode == StorageMode::Chunked {
            let store = ChunkStore::new(root.join(".synck").join(OBJECT_STORE_FILE), &config.repo_id);
            wal_indexes.push(store.open_readonly()?);
        }
        Ok(Self {
            root: root.to_path_buf(),
            metadata,
            repo_id: config.repo_id,
            encrypted: config.encrypted,
            storage_mode: config.storage_mode,
            storage,
            _wal_indexes: std::sync::Mutex::new(wal_indexes),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repository_id(&self) -> &RepoId {
        &self.repo_id
    }

    pub fn identity(&self) -> Result<synck_sync::RepositoryIdentity> {
        self.storage
            .history_identity(&self.repo_id)
            .map_err(corrupt)?
            .ok_or_else(|| corrupt("repository has no history generation"))
    }

    /// Blocking SQLite reads: call it off async worker threads.
    pub fn capabilities(&self) -> Result<synck_sync::Capabilities> {
        let repository = self.identity()?;
        let latest = self.storage.history_head(&repository).map_err(corrupt)?;
        Ok(synck_sync::Capabilities {
            protocol_version: synck_sync::PROTOCOL_VERSION,
            repository,
            encrypted: self.encrypted,
            latest,
            policy: synck_sync::ServerPolicy::default(),
            object_packs: true,
        })
    }

    fn object_store(&self) -> ChunkStore {
        ChunkStore::new(self.root.join(".synck").join(OBJECT_STORE_FILE), &self.repo_id)
    }

    pub fn has_object(&self, id: &ObjectId) -> Result<bool> {
        self.holds_object(&self.object_store(), id)
    }

    fn holds_object(&self, store: &ChunkStore, id: &ObjectId) -> Result<bool> {
        if self.storage_mode == StorageMode::Chunked {
            return store.holds_blob(id);
        }
        let name = id
            .as_str()
            .strip_prefix("sha256:")
            .ok_or_else(|| corrupt("object ID has no digest"))?;
        let objects = self.metadata.open_dir_nofollow("objects").map_err(corrupt)?;
        match objects.symlink_metadata(name) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).at("inspect object", self.root.join(".synck/objects").join(name)),
        }
    }

    /// A metadata object's exact bytes from history, which protocol 1 serves
    /// at the same route as content. Blocking.
    pub fn metadata_object(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        self.storage.metadata_object(&self.repo_id, id).map_err(corrupt)
    }

    /// What an object pack of `ids` holds, read through one connection to each
    /// database: a metadata object's bytes, or a content object's size. `None`
    /// when one of them is not stored. Blocking.
    pub fn pack_entries(&self, ids: &[ObjectId]) -> Result<Option<Vec<PackEntry>>> {
        let metadata = self.storage.metadata_objects(&self.repo_id, ids).map_err(corrupt)?;
        let chunks = (self.storage_mode == StorageMode::Chunked)
            .then(|| self.object_store().reader())
            .transpose()?;
        let objects = self.metadata.open_dir_nofollow("objects").map_err(corrupt)?;
        let mut entries = Vec::with_capacity(ids.len());
        for (id, metadata) in ids.iter().zip(metadata) {
            if let Some(bytes) = metadata {
                entries.push(PackEntry::Metadata(bytes));
                continue;
            }
            let size = match &chunks {
                Some(chunks) => chunks.blob_size(id)?,
                None => {
                    let name = id.as_str().strip_prefix("sha256:").ok_or_else(|| corrupt("object ID has no digest"))?;
                    match objects.symlink_metadata(name) {
                        Ok(entry) => Some(entry.len()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error).at("inspect object", self.root.join(".synck/objects").join(name)),
                    }
                }
            };
            match size {
                Some(size) => entries.push(PackEntry::Content(size)),
                None => return Ok(None),
            }
        }
        Ok(Some(entries))
    }

    /// Write the object pack of `ids`, as `pack_entries` found them, ending
    /// after the object that brings it to `budget` bytes. Blocking.
    pub fn write_pack(&self, ids: &[ObjectId], entries: Vec<PackEntry>, budget: u64, out: &mut dyn Write) -> Result<()> {
        let chunks = (self.storage_mode == StorageMode::Chunked)
            .then(|| self.object_store().reader())
            .transpose()?;
        let mut sent = 0;
        for (id, entry) in ids.iter().zip(entries) {
            if sent >= budget {
                break;
            }
            let size = match &entry {
                PackEntry::Metadata(bytes) => bytes.len() as u64,
                PackEntry::Content(size) => *size,
            };
            let mut header = id.as_str().as_bytes().to_vec();
            header.extend(size.to_be_bytes());
            out.write_all(&header).at("send object pack", &self.root)?;
            match (entry, &chunks) {
                (PackEntry::Metadata(bytes), _) => out.write_all(&bytes).at("send object pack", &self.root)?,
                (PackEntry::Content(_), Some(chunks)) => chunks.copy_blob(id, size, &mut &mut *out)?,
                (PackEntry::Content(_), None) => {
                    if copy_stored(&self.root, &self.metadata, None, id, out)? != size {
                        return Err(corrupt(format!("object {id} changed size while sent")));
                    }
                }
            }
            sent += size;
        }
        Ok(())
    }

    /// Stream an object's exact stored bytes, refusing any other digest. Blocking.
    pub fn copy_object(&self, id: &ObjectId, out: &mut dyn Write) -> Result<u64> {
        self.copy_from(&self.object_store(), id, out)
    }

    fn copy_from(&self, store: &ChunkStore, id: &ObjectId, out: &mut dyn Write) -> Result<u64> {
        let chunked = (self.storage_mode == StorageMode::Chunked).then_some(store);
        copy_stored(&self.root, &self.metadata, chunked, id, out)
    }

    /// Store each source's bytes as its ID, under one repository lock. An
    /// object's form is checked when a commit names it, since only the commit
    /// gives it a role. Blocking: waits for the lock.
    pub fn install_objects<'a>(&self, objects: impl IntoIterator<Item = (&'a ObjectId, &'a Path)>) -> Result<()> {
        let metadata = self.root.join(".synck");
        let _lock = crate::repository::acquire_lock(&metadata, true)?;
        match self.metadata.create_dir("tmp") {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(error).at("create staging directory", metadata.join("tmp"));
            }
            _ => {}
        }
        let store = self.object_store();
        for (id, source) in objects {
            let file = open_regular_file_nofollow(source)?;
            let size = file.metadata().at("inspect object upload", source)?.len();
            let captured = crate::raw_object::capture(
                file,
                size,
                None,
                &metadata.join("objects"),
                &metadata.join("tmp"),
                (self.storage_mode == StorageMode::Chunked).then_some(&store),
                // Durable at the checkpoint `accept` takes before it appends.
                false,
                None,
            )?;
            if captured.id != *id {
                return Err(invalid(format!("object bytes are {}, not {id}", captured.id)));
            }
        }
        Ok(())
    }

    /// The commit accepted for `operation_id`, if any. Takes no lock.
    pub async fn accepted(&self, operation_id: &str) -> Result<Option<Accepted>> {
        let commit = self
            .storage
            .get_commit_by_operation_id_readonly(&self.repo_id, &sync_operation_id(operation_id))
            .await
            .map_err(corrupt)?;
        commit.map(|commit| self.receipt(commit)).transpose()
    }

    fn receipt(&self, commit: CommitRow) -> Result<Accepted> {
        let published = match commit.repo_seq {
            Some(repo_seq) => self
                .storage
                .history_cursor(&self.identity()?, repo_seq)
                .map_err(corrupt)?,
            None => None,
        };
        Ok(Accepted {
            commit_id: commit.commit_id,
            accepted_at: commit.created_at,
            published,
        })
    }

    /// Accept a proposal with every Core check a local commit gets, then
    /// sequence it. A retry of an accepted operation returns its commit; one
    /// that names another commit is refused. Waits for the repository lock.
    pub async fn accept(&self, proposal: Proposal) -> Result<Acceptance> {
        let metadata = self.root.join(".synck");
        let _lock = crate::repository::acquire_lock(&metadata, true)?;
        crate::repository::reject_unfinished_origin_operation(&metadata)?;
        let core = Repo::new(
            Storage::new(Arc::new(self.storage.clone())),
            CoreConfig {
                encrypted: self.encrypted,
                follows_domains: false,
                ..CoreConfig::default()
            },
        );
        let operation_id = sync_operation_id(&proposal.operation_id);
        if let Some(commit) = core
            .get_commit_by_operation_id(self.repo_id.clone(), &operation_id)
            .await?
        {
            if commit.commit_id != proposal.commit_id {
                return Err(invalid("operation ID was already used for a different commit"));
            }
            return Ok(Acceptance::Accepted(self.receipt(commit)?));
        }
        if let Some(head) = proposal.repository_head
            && !self.storage.repository_head_matches(&self.repo_id,head).map_err(corrupt)?
        {
            // A cursor fence can fail without any touched item changing (for
            // example, a new child elsewhere). The client must pull full history.
            return Ok(Acceptance::Conflict(Vec::new()));
        }
        let tokens = &proposal.lock_tokens;
        agent_locks::validate_tokens(tokens)?;
        let repo_id = self.repo_id.as_str();
        if !tokens.is_empty() || agent_locks::any(&metadata, repo_id)? {
            let paths = self.proposal_paths(&proposal).await?;
            let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
            let scope = agent_locks::Scope { metadata: &metadata, tree: None };
            agent_locks::guard(&scope, repo_id, tokens, Some(&paths))?;
        }
        let mut missing = self.unusable_objects(&core, &proposal).await?;
        missing.extend(self.install_metadata(&proposal)?);
        if !missing.is_empty() {
            return Ok(Acceptance::MissingObjects(missing));
        }
        // Uploads lost to a crash before this point answer OBJECTS_MISSING above.
        if self.storage_mode == StorageMode::Chunked {
            self.object_store().checkpoint()?;
        } else {
            crate::repository::sync_directory(&self.root.join(".synck/objects"))?;
        }
        let mut items: Vec<ItemId> = proposal
            .events
            .iter()
            .filter_map(|event| event.item_id.clone())
            .chain(proposal.ref_events.iter().map(|event| event.owner_item_id.clone()))
            .chain(proposal.guards.iter().map(|guard| guard.item_id.clone()))
            .collect();
        items.sort();
        items.dedup();
        let guards: Vec<(ItemId, i64)> = proposal
            .guards
            .iter()
            .map(|guard| (guard.item_id.clone(), guard.head_seq))
            .collect();
        let accepted = core
            .append_file_folder_checked_with_refs(
                self.repo_id.clone(),
                CommitProposal {
                    author_id: proposal.metadata.author_id,
                    committer_id: proposal.metadata.committer_id,
                    message: proposal.metadata.message,
                    occurred_at: proposal.metadata.occurred_at,
                    operation_id: Some(operation_id),
                    events: proposal.events,
                },
                proposal.ref_events,
                &guards,
                &proposal.subtree_guards,
            )
            .await;
        let accepted = match accepted {
            Ok(accepted) => accepted,
            // An absent item or parent is a head of zero.
            Err(error)
                if crate::commands::is_conflict(&error)
                    || matches!(error, synck_vcs::errors::CoreError::ItemNotFound { .. }) =>
            {
                let heads: HashMap<ItemId, i64> = core
                    .get_items(self.repo_id.clone(), items.clone())
                    .await?
                    .into_iter()
                    .map(|row| (row.item_id, row.head_seq))
                    .collect();
                return Ok(Acceptance::Conflict(
                    items
                        .into_iter()
                        .map(|item_id| ItemHead {
                            head_seq: heads.get(&item_id).copied().unwrap_or(0),
                            item_id,
                        })
                        .collect(),
                ));
            }
            Err(error) => return Err(error.into()),
        };
        if accepted.commit_id != proposal.commit_id {
            return Err(LocalError::Integrity(format!(
                "proposal for commit {} was accepted as {}",
                proposal.commit_id, accepted.commit_id
            )));
        }
        while core
            .sequence_repo_activity(self.repo_id.clone(), SEQUENCE_PAGE)
            .await?
            == SEQUENCE_PAGE
        {}
        let commit = core
            .get_commit(self.repo_id.clone(), accepted.commit_id)
            .await?
            .ok_or_else(|| corrupt("accepted commit is missing"))?;
        Ok(Acceptance::Accepted(self.receipt(commit)?))
    }

    /// Objects the proposal names that are absent. A present object whose
    /// role selects a frame must hold one.
    async fn unusable_objects(&self, core: &Repo, proposal: &Proposal) -> Result<Vec<ObjectId>> {
        let mut item_types: HashMap<ItemId, String> = proposal
            .events
            .iter()
            .filter_map(|event| Some((event.item_id.clone()?, event.create_item.as_ref()?.item_type.clone())))
            .collect();
        let stored: Vec<ItemId> = proposal
            .events
            .iter()
            .filter(|event| event.set_content.is_some())
            .filter_map(|event| event.item_id.clone())
            .filter(|item_id| !item_types.contains_key(item_id))
            .collect();
        for row in core.get_items(self.repo_id.clone(), stored).await? {
            item_types.insert(row.item_id, row.item_type);
        }
        let mut forms: BTreeMap<ObjectId, Form> = BTreeMap::new();
        for event in &proposal.events {
            let (Some(content), Some(item_id)) = (&event.set_content, &event.item_id) else {
                continue;
            };
            let item_type = item_types
                .get(item_id)
                .ok_or_else(|| invalid(format!("content for unknown item {item_id}")))?;
            for (role, id) in content.roles() {
                forms.insert(id.clone(), content_form(self.encrypted, item_type, role));
            }
        }
        for event in &proposal.ref_events {
            for id in event.set_content.iter().flat_map(|content| content.values()) {
                forms.entry(id.clone()).or_insert(Form::Opaque);
            }
        }
        let store = self.object_store();
        let mut missing = Vec::new();
        for (id, form) in forms {
            if !self.holds_object(&store, &id)? {
                missing.push(id);
                continue;
            }
            match form {
                Form::Plain | Form::Opaque => {}
                Form::Salted => {
                    let mut frame = SaltedFrame::default();
                    self.copy_from(&store, &id, &mut frame)?;
                    frame
                        .finish()
                        .map_err(|error| invalid(format!("object {id}: {error}")))?;
                }
                Form::Encrypted => {
                    let mut frame = EncryptedFrameCheck::default();
                    self.copy_from(&store, &id, &mut frame)?;
                    frame
                        .finish()
                        .map_err(|error| invalid(format!("object {id}: {error}")))?;
                }
            }
        }
        Ok(missing)
    }

    /// Metadata a proposal names arrives as an uploaded object and is
    /// installed through Core's metadata storage before the commit that
    /// points to it. Returns the IDs neither stored nor uploaded.
    fn install_metadata(&self, proposal: &Proposal) -> Result<Vec<ObjectId>> {
        let ids: BTreeSet<ObjectId> = proposal
            .events
            .iter()
            .filter_map(|event| event.set_metadata.as_ref())
            .chain(proposal.ref_events.iter().filter_map(|event| event.set_metadata.as_ref()))
            .map(|metadata| metadata.id())
            .collect();
        let mut missing = Vec::new();
        let mut uploaded = Vec::new();
        let store = self.object_store();
        for id in self.storage.missing_metadata(&self.repo_id, ids).map_err(corrupt)? {
            if !self.holds_object(&store, &id)? {
                missing.push(id);
                continue;
            }
            let mut bytes = Bounded(Vec::new());
            self.copy_from(&store, &id, &mut bytes)?;
            uploaded.push((id, bytes.0));
        }
        if !uploaded.is_empty() {
            self.storage
                .put_metadata_objects(&self.repo_id, &uploaded)
                .map_err(|error| invalid(error.to_string()))?;
        }
        Ok(missing)
    }

    /// The repository paths a proposal writes: each item it touches where it
    /// is now and where the proposal puts it. Records outside the tree have
    /// none.
    async fn proposal_paths(&self, proposal: &Proposal) -> Result<Vec<String>> {
        let mut stored: HashMap<ItemId, Option<(Option<ItemId>, Option<String>)>> = HashMap::new();
        let mut placed: HashMap<ItemId, (Option<ItemId>, Option<String>)> = HashMap::new();
        for event in &proposal.events {
            let Some(item_id) = &event.item_id else { continue };
            let mut place = match &event.create_item {
                Some(create) => (create.parent_folder_id.clone(), create.name.clone()),
                None => match placed.get(item_id) {
                    Some(place) => place.clone(),
                    None => match self.stored_place(&mut stored, item_id).await? {
                        Some(place) => place,
                        None => continue,
                    },
                },
            };
            if let Some(parent) = &event.set_parent {
                place.0 = parent.clone();
            }
            if let Some(name) = &event.set_name {
                place.1 = Some(name.clone());
            }
            placed.insert(item_id.clone(), place);
        }
        let empty = HashMap::new();
        let mut paths = BTreeSet::new();
        for item_id in proposal.events.iter().filter_map(|event| event.item_id.as_ref()) {
            paths.extend(self.path(&mut stored, &empty, item_id).await?);
            paths.extend(self.path(&mut stored, &placed, item_id).await?);
        }
        for event in &proposal.ref_events {
            paths.extend(self.path(&mut stored, &empty, &event.owner_item_id).await?);
        }
        Ok(paths.into_iter().collect())
    }

    async fn stored_place(
        &self,
        stored: &mut HashMap<ItemId, Option<(Option<ItemId>, Option<String>)>>,
        item_id: &ItemId,
    ) -> Result<Option<(Option<ItemId>, Option<String>)>> {
        if let Some(place) = stored.get(item_id) {
            return Ok(place.clone());
        }
        let place = self
            .storage
            .get_item_readonly(&self.repo_id, item_id)
            .await
            .map_err(corrupt)?
            .map(|row| (row.parent_folder_id, row.name));
        stored.insert(item_id.clone(), place.clone());
        Ok(place)
    }

    /// `item_id`'s path with `placed` over the stored tree; `None` for an
    /// absent item, a record, or an item under one.
    async fn path(
        &self,
        stored: &mut HashMap<ItemId, Option<(Option<ItemId>, Option<String>)>>,
        placed: &HashMap<ItemId, (Option<ItemId>, Option<String>)>,
        item_id: &ItemId,
    ) -> Result<Option<String>> {
        let mut names = Vec::new();
        let mut current = Some(item_id.clone());
        while let Some(id) = current {
            if names.len() == MAX_DEPTH {
                return Err(corrupt("folder tree is deeper than a path can be"));
            }
            let place = match placed.get(&id) {
                Some(place) => Some(place.clone()),
                None => self.stored_place(stored, &id).await?,
            };
            let Some((parent, Some(name))) = place else {
                return Ok(None);
            };
            names.push(name);
            current = parent;
        }
        names.reverse();
        Ok(Some(names.join("/")))
    }

    /// Every reservation, live or expired. Waits for the repository lock.
    pub fn reservations(&self) -> Result<Vec<AgentLock>> {
        let metadata = self.root.join(".synck");
        let _lock = crate::repository::acquire_lock(&metadata, true)?;
        agent_locks::list(&metadata, self.repo_id.as_str())
    }

    /// Reserve repository paths for a remote writer, as `synck lock acquire`
    /// does for a local one. The writer has checked its own working tree;
    /// this one is not consulted. Waits for the repository lock.
    pub fn reserve(&self, paths: Vec<String>, owner: &str, ttl: u64) -> Result<AgentLease> {
        let metadata = self.root.join(".synck");
        let _lock = crate::repository::acquire_lock(&metadata, true)?;
        agent_locks::reserve(&metadata, self.repo_id.as_str(), paths, owner, ttl, REMOTE_TREE)
    }

    pub fn renew_reservation(&self, token: &str, ttl: u64) -> Result<()> {
        let metadata = self.root.join(".synck");
        let _lock = crate::repository::acquire_lock(&metadata, true)?;
        agent_locks::renew(&metadata, self.repo_id.as_str(), token, ttl)
    }

    pub fn release_reservation(&self, token: &str) -> Result<usize> {
        let metadata = self.root.join(".synck");
        let _lock = crate::repository::acquire_lock(&metadata, true)?;
        agent_locks::release(&metadata, self.repo_id.as_str(), token)
    }

    pub fn break_reservations(&self, paths: &[String]) -> Result<usize> {
        let metadata = self.root.join(".synck");
        let _lock = crate::repository::acquire_lock(&metadata, true)?;
        agent_locks::break_paths(&metadata, self.repo_id.as_str(), paths)
    }

    /// One page of published history; `None` when the request's position is
    /// not on this history. Takes no lock.
    pub async fn history(&self, request: &HistoryRequest) -> Result<Option<HistoryPage>> {
        request
            .validate()
            .map_err(|error| invalid(error.to_string()))?;
        self.storage.history_page(request).await.map_err(corrupt)
    }
}

/// Collects an uploaded metadata object, refusing anything larger than one.
struct Bounded(Vec<u8>);

impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        // An encrypted object's frame around the largest plain payload.
        if self.0.len() + bytes.len() > synck_vcs::metadata::MAX_METADATA_BYTES + 1024 {
            return Err(std::io::Error::other("too large for a metadata object"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
