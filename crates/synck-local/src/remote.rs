//! HTTPS clones: a held clone whose origin is a protocol-1 sync server
//! (`docs/spec/protocol.md`).
//!
//! The clone keeps its own history and content store. `pull` verifies the
//! server's history pages from the cursor in `held.json`, downloads the
//! content they name and replays each commit with its ID. `push` uploads each
//! held commit as one proposal whose operation ID is its commit ID, so a
//! retry after a lost receipt finds the accepted commit. The token lives in
//! `.synck/remote-token`; it never appears in a URL.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use synck_sync::{
    Capabilities, CommitMetadata, Cursor, HistoryPage, HistoryRequest, HistoryVerifier, Lease,
    LeasePaths, LeaseRenewal, LeaseRequest, LeaseToken, Leases, MAX_PAGE_BYTES, MAX_PAGE_RECORDS,
    PageAck, ProposalPage, ProposalRecord, ProposalVerifier, Receipt, Released,
    RepositoryIdentity, SubmitRequest, SubmitResult,
};
use synck_vcs::config::CoreConfig;
use synck_vcs::metadata::Metadata;
use synck_vcs::repo::Repo;
use synck_vcs::storage::{Storage, StorageBackend as _};
use synck_vcs::types::{CommitEventRow, CommitId, CommitRow, ItemId, ObjectId, RepoId};
use synck_vcs_sqlite::SqliteStorage;
use ureq::http::{Response, StatusCode, Uri};
use ureq::tls::{RootCerts, TlsConfig};

use crate::chunk_store::{ChunkStore, OBJECT_STORE_FILE, StorageMode};
use crate::commands::{Held, PushOutcome, apply, proposal_from_rows, read_held, stored_proposal, write_held};
use crate::error::{IoContext, LocalError, Result};
use crate::repository::{Config, Repository};
use crate::{AgentLease, AgentLock};

/// In an HTTPS clone's `.synck`: the bearer token, mode 0600.
pub(crate) const TOKEN_FILE: &str = "remote-token";
const LOCK_TOKEN: &str = "synck-lock-token";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Acceptance waits for the served repository's lock.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);
/// Room for a page's envelope around its records.
const PAGE_RECORD_BYTES: usize = MAX_PAGE_BYTES - 64 * 1024;
/// Refusals and small replies; history pages are bounded by the protocol.
const MAX_REPLY_BYTES: u64 = 2 * MAX_PAGE_BYTES as u64;

fn refused(status: &str) -> LocalError {
    LocalError::Server { status: status.to_string() }
}

fn unreachable(error: ureq::Error) -> LocalError {
    LocalError::Unreachable(error.to_string())
}

fn integrity(error: impl std::fmt::Display) -> LocalError {
    LocalError::Integrity(format!("sync server: {error}"))
}

/// `https://HOST/v1/repositories/REPO_ID`, or `http://` to a loopback host.
/// Returns the URL without a trailing slash and the repository ID it names.
pub(crate) fn parse_url(url: &str) -> Result<(String, RepoId)> {
    let invalid = |reason: &str| LocalError::Invalid(format!("{url} is not a sync server repository URL: {reason}"));
    let url = url.trim_end_matches('/');
    let uri: Uri = url.parse().map_err(|_| invalid("it does not parse"))?;
    let host = uri.host().ok_or_else(|| invalid("it has no host"))?;
    match uri.scheme_str() {
        Some("https") => {}
        Some("http") if matches!(host, "localhost" | "127.0.0.1" | "[::1]") => {}
        Some("http") => return Err(invalid("protocol 1 needs https except to this machine")),
        _ => return Err(invalid("it is not https")),
    }
    if uri.authority().is_some_and(|authority| authority.as_str().contains('@')) || uri.query().is_some() {
        return Err(invalid("it carries credentials or a query; give the token in SYNCK_TOKEN"));
    }
    let repo_id = uri
        .path()
        .strip_prefix("/v1/repositories/")
        .filter(|id| !id.contains('/'))
        .ok_or_else(|| invalid("its path is not /v1/repositories/REPO_ID"))?;
    let repo_id = RepoId::parse(repo_id).map_err(|_| invalid("its repository ID is not valid"))?;
    Ok((url.to_string(), repo_id))
}

/// One repository on a sync server.
pub(crate) struct Remote {
    agent: ureq::Agent,
    sync: String,
    authorization: String,
}

/// What a page upload or submission met besides its answer.
enum Submitted {
    Result(SubmitResult),
    Missing(Vec<ObjectId>),
    Expired,
}

impl Remote {
    pub(crate) fn new(url: &str, token: &str) -> Result<Self> {
        let (url, _) = parse_url(url)?;
        let tls = TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build();
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .tls_config(tls)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .build()
            .new_agent();
        Ok(Self {
            agent,
            sync: format!("{url}/sync"),
            authorization: format!("Bearer {}", token.trim()),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.sync)
    }

    fn get(&self, path: &str) -> Result<Response<ureq::Body>> {
        self.agent
            .get(self.url(path))
            .header("authorization", &self.authorization)
            .call()
            .map_err(unreachable)
    }

    fn post(&self, path: &str, body: &impl Serialize, lock_tokens: &[String]) -> Result<Response<ureq::Body>> {
        let mut request = self
            .agent
            .post(self.url(path))
            .header("authorization", &self.authorization)
            .header("content-type", "application/json");
        for token in lock_tokens {
            request = request.header(LOCK_TOKEN, token);
        }
        request.send(&serde_json::to_vec(body)?[..]).map_err(unreachable)
    }

    fn put(&self, path: &str, body: ureq::SendBody) -> Result<Response<ureq::Body>> {
        self.agent
            .put(self.url(path))
            .header("authorization", &self.authorization)
            .send(body)
            .map_err(unreachable)
    }

    /// A success's JSON, or the refusal as an error.
    fn json<T: DeserializeOwned>(response: Response<ureq::Body>) -> Result<T> {
        let (status, body) = Self::read(response)?;
        if !status.is_success() {
            return Err(Self::refusal(status, &body));
        }
        serde_json::from_slice(&body).map_err(integrity)
    }

    fn read(response: Response<ureq::Body>) -> Result<(StatusCode, Vec<u8>)> {
        let status = response.status();
        let body = response
            .into_body()
            .into_with_config()
            .limit(MAX_REPLY_BYTES)
            .read_to_vec()
            .map_err(unreachable)?;
        Ok((status, body))
    }

    fn status(body: &[u8]) -> Option<serde_json::Value> {
        let value: serde_json::Value = serde_json::from_slice(body).ok()?;
        value["status"].is_string().then_some(value)
    }

    fn refusal(status: StatusCode, body: &[u8]) -> LocalError {
        let Some(value) = Self::status(body) else {
            return refused(&status.to_string());
        };
        match value["status"].as_str() {
            Some("LOCKED") => LocalError::Locked {
                path: value["path"].as_str().unwrap_or_default().to_string(),
                owner: value["owner"].as_str().unwrap_or_default().to_string(),
            },
            Some("LOCK_LOST") => LocalError::LockLost,
            Some(code) => refused(code),
            None => refused(&status.to_string()),
        }
    }

    pub(crate) fn capabilities(&self) -> Result<Capabilities> {
        let capabilities: Capabilities = Self::json(self.get("capabilities")?)?;
        capabilities.validate().map_err(integrity)?;
        Ok(capabilities)
    }

    fn history(&self, request: &HistoryRequest) -> Result<HistoryPage> {
        Self::json(self.post("history", request, &[])?)
    }

    fn receipt(&self, operation_id: &str) -> Result<Option<Receipt>> {
        let response = self.get(&format!("receipts/{operation_id}"))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Self::json(response).map(Some)
    }

    fn put_page(&self, page: &ProposalPage) -> Result<Option<PageAck>> {
        let body = serde_json::to_vec(page)?;
        let response = self.put(
            &format!("uploads/{}/pages/{}", page.operation_id, page.offset),
            ureq::SendBody::from_reader(&mut &body[..]),
        )?;
        let (status, body) = Self::read(response)?;
        if status.is_success() {
            return serde_json::from_slice(&body).map(Some).map_err(integrity);
        }
        match Self::status(&body).and_then(|value| value["status"].as_str().map(str::to_string)).as_deref() {
            Some("UPLOAD_EXPIRED") => Ok(None),
            _ => Err(Self::refusal(status, &body)),
        }
    }

    fn submit(&self, request: &SubmitRequest, lock_tokens: &[String]) -> Result<Submitted> {
        let (status, body) = Self::read(self.post("proposals", request, lock_tokens)?)?;
        if status.is_success() {
            return serde_json::from_slice(&body).map(Submitted::Result).map_err(integrity);
        }
        let value = Self::status(&body);
        match value.as_ref().and_then(|value| value["status"].as_str()) {
            Some("UPLOAD_EXPIRED") => Ok(Submitted::Expired),
            Some("OBJECTS_MISSING") => {
                let objects = serde_json::from_value(value.expect("a status")["objects"].take())
                    .map_err(integrity)?;
                Ok(Submitted::Missing(objects))
            }
            _ => Err(Self::refusal(status, &body)),
        }
    }

    fn put_object(&self, id: &ObjectId, source: &Path) -> Result<()> {
        let mut file = std::fs::File::open(source).at("open", source)?;
        let response = self.put(&format!("objects/{}", id.as_str()), ureq::SendBody::from_reader(&mut file))?;
        let (status, body) = Self::read(response)?;
        if !status.is_success() {
            return Err(Self::refusal(status, &body));
        }
        Ok(())
    }

    /// A metadata object's bytes, which the caller checks against its ID.
    fn get_metadata(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let (status, body) = Self::read(self.get(&format!("objects/{}", id.as_str()))?)?;
        if !status.is_success() {
            return Err(Self::refusal(status, &body));
        }
        Ok(body)
    }

    /// Stream an object into `out`, which the caller checks against its ID.
    fn get_object(&self, id: &ObjectId, out: &mut std::fs::File) -> Result<()> {
        let response = self.get(&format!("objects/{}", id.as_str()))?;
        if !response.status().is_success() {
            let (status, body) = Self::read(response)?;
            return Err(Self::refusal(status, &body));
        }
        let mut reader = response.into_body().into_reader();
        std::io::copy(&mut reader, out).map_err(|error| LocalError::Unreachable(error.to_string()))?;
        Ok(())
    }

    fn leases(&self) -> Result<Leases> {
        Self::json(self.get("leases")?)
    }

    fn reserve(&self, request: &LeaseRequest) -> Result<Lease> {
        Self::json(self.post("leases", request, &[])?)
    }

    fn renew(&self, renewal: &LeaseRenewal) -> Result<()> {
        let (status, body) = Self::read(self.post("leases/renew", renewal, &[])?)?;
        if !status.is_success() {
            return Err(Self::refusal(status, &body));
        }
        Ok(())
    }

    fn release(&self, token: &LeaseToken) -> Result<Released> {
        Self::json(self.post("leases/release", token, &[])?)
    }

    fn break_paths(&self, paths: &LeasePaths) -> Result<Released> {
        Self::json(self.post("leases/break", paths, &[])?)
    }
}

/// An HTTPS clone's own history and content store.
struct Store<'a> {
    root: &'a Path,
    storage: SqliteStorage,
    core: &'a Repo,
    repo_id: &'a RepoId,
    chunks: Option<ChunkStore>,
}

impl Store<'_> {
    fn metadata(&self) -> PathBuf {
        self.root.join(".synck")
    }

    fn has_object(&self, id: &ObjectId) -> Result<bool> {
        if let Some(store) = &self.chunks {
            return store.holds_blob(id);
        }
        let path = self.metadata().join("objects").join(&id.as_str()["sha256:".len()..]);
        path.try_exists().at("inspect object", &path)
    }

    fn temporary(&self, kind: &str) -> Result<(PathBuf, std::fs::File)> {
        let path = self.metadata().join("tmp").join(format!("{kind}-{}.tmp", uuid::Uuid::new_v4()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .at("create", &path)?;
        Ok((path, file))
    }

    /// Download `id`, then store it only if its bytes hash to it.
    fn download(&self, remote: &Remote, id: &ObjectId) -> Result<()> {
        let (path, mut file) = self.temporary("download")?;
        let result = (|| {
            remote.get_object(id, &mut file)?;
            file.flush().at("write", &path)?;
            let size = file.metadata().at("inspect", &path)?.len();
            let source = std::fs::File::open(&path).at("open", &path)?;
            let metadata = self.metadata();
            let captured = crate::raw_object::capture(
                source,
                size,
                None,
                &metadata.join("objects"),
                &metadata.join("tmp"),
                self.chunks.as_ref(),
                // Durable at the checkpoint `install` takes before it applies.
                false,
                None,
            )?;
            if captured.id != *id {
                return Err(integrity(format!("object {id} arrived as {}", captured.id)));
            }
            Ok(())
        })();
        let _ = std::fs::remove_file(&path);
        result
    }

    fn upload(&self, remote: &Remote, id: &ObjectId) -> Result<()> {
        let (path, mut file) = self.temporary("upload")?;
        let result = (|| {
            // Metadata lives in history, content in the object store.
            if let Some(bytes) = self.storage.metadata_object(self.repo_id, id).map_err(integrity)? {
                file.write_all(&bytes).at("write", &path)?;
                return remote.put_object(id, &path);
            }
            let metadata = cap_std::fs::Dir::open_ambient_dir(self.metadata(), cap_std::ambient_authority())
                .at("open", self.metadata())?;
            crate::portable::copy_stored(self.root, &metadata, self.chunks.as_ref(), id, &mut file)?;
            file.flush().at("write", &path)?;
            remote.put_object(id, &path)
        })();
        let _ = std::fs::remove_file(&path);
        result
    }

    /// The metadata objects `ids`, downloading those this history lacks and
    /// storing them through Core's metadata storage in one transaction.
    fn metadata_objects(&self, remote: &Remote, ids: BTreeSet<ObjectId>) -> Result<BTreeMap<ObjectId, Metadata>> {
        let missing: BTreeSet<ObjectId> = self
            .storage
            .missing_metadata(self.repo_id, ids.iter().cloned())
            .map_err(integrity)?
            .into_iter()
            .collect();
        let mut found = BTreeMap::new();
        let mut downloaded = Vec::new();
        for id in ids {
            let metadata = if missing.contains(&id) {
                let bytes = remote.get_metadata(&id)?;
                let metadata = Metadata::verify(&id, bytes.clone()).map_err(integrity)?;
                downloaded.push((id.clone(), bytes));
                metadata
            } else {
                self.storage
                    .get_metadata(self.repo_id, &id)
                    .map_err(integrity)?
                    .ok_or_else(|| integrity(format!("metadata object {id} disappeared")))?
            };
            found.insert(id, metadata);
        }
        if !downloaded.is_empty() {
            self.storage.put_metadata_objects(self.repo_id, &downloaded).map_err(integrity)?;
        }
        Ok(found)
    }

    /// Install one verified server commit unless it is here already.
    async fn install(
        &self,
        remote: &Remote,
        commit: &CommitRow,
        mut events: Vec<CommitEventRow>,
    ) -> Result<std::result::Result<(), Vec<ItemId>>> {
        if self.core.get_commit(self.repo_id.clone(), commit.commit_id.clone()).await?.is_some() {
            return Ok(Ok(()));
        }
        // Rows on the wire carry metadata by ID only.
        let mut references = Vec::with_capacity(events.len());
        let mut downloaded = false;
        for row in &events {
            let found = synck_items::references::commit_event_references(row).map_err(integrity)?;
            for id in &found.objects {
                if !self.has_object(id)? {
                    self.download(remote, id)?;
                    downloaded = true;
                }
            }
            references.push(found.metadata);
        }
        let metadata = self.metadata_objects(remote, references.iter().flatten().cloned().collect())?;
        for (row, id) in events.iter_mut().zip(&references) {
            let metadata = id.as_ref().map(|id| metadata[id].clone());
            match row {
                CommitEventRow::Item(row) => row.metadata = metadata,
                CommitEventRow::Ref(row) => row.metadata = metadata,
            }
        }
        if let Some(chunks) = self.chunks.as_ref().filter(|_| downloaded) {
            chunks.checkpoint()?;
        }
        let (proposal, ref_events) = proposal_from_rows(commit, events)?;
        apply(self.core, self.repo_id, commit, proposal, ref_events).await
    }

    /// Take in the server's commits after `held.remote`, in order and with
    /// their IDs, advancing `held` past each one installed. Returns the items
    /// of the first commit this history cannot take.
    async fn fetch(&self, remote: &Remote, held: &mut Held) -> Result<std::result::Result<(), Vec<ItemId>>> {
        let start = held.remote.clone().ok_or_else(|| integrity("held.json has no server cursor"))?;
        let repository = start.repository.clone();
        // Idle connections keep each database's WAL index from being rebuilt
        // from the whole WAL at every open. Downloads leave their frames in
        // the object store's WAL until `install` checkpoints.
        let _history = self.storage.open_readonly().map_err(integrity)?;
        let _objects = self.chunks.as_ref().map(ChunkStore::open_readonly).transpose()?;
        let mut verifier = HistoryVerifier::new(start).map_err(integrity)?;
        let mut partial: Option<(CommitRow, synck_vcs::types::Sha256Digest, Vec<CommitEventRow>)> = None;
        loop {
            let request = HistoryRequest {
                after: verifier.position().clone(),
                limit: MAX_PAGE_RECORDS,
            };
            let page = remote.history(&request)?;
            verifier.push(&page).map_err(integrity)?;
            let has_more = page.has_more;
            for fragment in page.fragments {
                let (commit, _, events) = partial.get_or_insert_with(|| {
                    (fragment.header.commit.clone(), fragment.header.checkpoint.clone(), Vec::new())
                });
                events.extend(fragment.events);
                if events.len() < commit.event_count as usize {
                    continue;
                }
                let (commit, checkpoint, events) = partial.take().expect("a commit in progress");
                if let Err(items) = self.install(remote, &commit, events).await? {
                    return Ok(Err(items));
                }
                let repo_seq = commit.repo_seq.ok_or_else(|| integrity("unsequenced commit"))?;
                held.origin_seq = repo_seq;
                held.remote = Some(Cursor {
                    repository: repository.clone(),
                    repo_seq: repo_seq as u64,
                    checkpoint,
                });
            }
            if !has_more {
                return Ok(Ok(()));
            }
        }
    }
}

fn core(storage: SqliteStorage) -> Repo {
    Repo::new(Storage::new(std::sync::Arc::new(storage)), CoreConfig::default())
}

fn write_token(metadata: &Path, token: &str) -> Result<()> {
    let path = metadata.join(TOKEN_FILE);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path).at("create", &path)?;
    file.write_all(token.trim().as_bytes()).at("write", &path)?;
    file.sync_all().at("sync", &path)
}

/// Make `destination` an HTTPS clone of the repository at `url`: its whole
/// history, verified, and the content its checkout needs. `token` is a read
/// or write token for that repository.
pub async fn clone_remote(
    url: &str,
    token: &str,
    destination: &Path,
    author: &str,
    include: &[String],
    hold: bool,
) -> Result<Repository> {
    let (url, repo_id) = parse_url(url)?;
    if author.trim().is_empty() {
        return Err(LocalError::Invalid("an HTTPS clone needs --author".into()));
    }
    let include = crate::checkout::normalize_paths(include)?;
    let remote = Remote::new(&url, token)?;
    let capabilities = remote.capabilities()?;
    if capabilities.repository.repo_id != repo_id {
        return Err(integrity("the server answered for another repository"));
    }
    if capabilities.encrypted {
        return Err(LocalError::Invalid("encrypted repositories cannot be cloned over HTTPS yet".into()));
    }
    crate::clone::create_empty_directory(destination)?;
    let destination = destination.canonicalize().at("resolve clone destination", destination)?;
    let metadata = destination.join(".synck");
    let result = make_clone(&remote, capabilities, &url, token, &destination, &metadata, author, include, hold).await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&metadata);
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn make_clone(
    remote: &Remote,
    capabilities: Capabilities,
    url: &str,
    token: &str,
    destination: &Path,
    metadata: &Path,
    author: &str,
    include: Vec<String>,
    hold: bool,
) -> Result<Repository> {
    let repo_id = capabilities.repository.repo_id.clone();
    let storage_mode = StorageMode::default();
    std::fs::create_dir(metadata).at("create clone metadata", metadata)?;
    for name in ["objects", "tmp"] {
        std::fs::create_dir(metadata.join(name)).at("create clone metadata", metadata)?;
    }
    for name in ["lock", crate::repository::MONITOR_LOCK_FILE] {
        let path = metadata.join(name);
        std::fs::write(&path, b"").at("create", &path)?;
    }
    write_token(metadata, token)?;
    let storage = SqliteStorage::new(metadata.join("repository.sqlite3"));
    storage.init_schema().await.map_err(|error| LocalError::Integrity(error.to_string()))?;
    let history = core(storage.clone());
    history.sequence_repo_activity(repo_id.clone(), 1).await?;
    storage
        .project_imported_history(&repo_id)
        .await
        .map_err(|error| LocalError::Integrity(error.to_string()))?;
    let chunks = (storage_mode == StorageMode::Chunked).then(|| {
        ChunkStore::create(&metadata.join(OBJECT_STORE_FILE), &repo_id)
            .map(|()| ChunkStore::new(metadata.join(OBJECT_STORE_FILE), &repo_id))
    });
    let store = Store {
        root: destination,
        storage: storage.clone(),
        core: &history,
        repo_id: &repo_id,
        chunks: chunks.transpose()?,
    };
    let mut held = Held {
        origin_seq: 0,
        pushed_seq: 0,
        remote: Some(Cursor::genesis(capabilities.repository, false).map_err(integrity)?),
    };
    if store.fetch(remote, &mut held).await?.is_err() {
        return Err(integrity("the server's history does not replay"));
    }
    let head = crate::repository::head_repo_seq(&storage, &repo_id)?;
    held.pushed_seq = head;
    write_held(metadata, &held)?;
    let checkout = crate::checkout::Checkout {
        paths: include,
        hidden_files: Default::default(),
        pending: Some(crate::checkout::Transition { previous_paths: None }),
        baseline: Some(head),
    };
    let source = metadata.join(crate::checkout::CHECKOUT_SOURCE_FILE);
    std::fs::write(&source, b"[]").at("write", &source)?;
    crate::checkout::write_checkout(metadata, &checkout)?;
    // The config goes last: before it, the directory opens as nothing.
    let config = Config::remote_clone(url.to_string(), repo_id, author.to_string(), checkout, storage_mode, hold);
    let path = metadata.join("tmp").join("config.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?).at("write", &path)?;
    crate::repository::sync_file(&path)?;
    crate::repository::replace_file(&path, &metadata.join("config.json")).at("publish clone config", metadata)?;
    crate::repository::sync_directory(metadata)?;
    Repository::discover(destination).await
}

/// A held commit as one proposal: pages of at most `MAX_PAGE_RECORDS`
/// records and `MAX_PAGE_BYTES`, and the request that names them.
fn proposal_pages(
    repository: &RepositoryIdentity,
    operation_id: &str,
    records: &[ProposalRecord],
    metadata: CommitMetadata,
) -> Result<(Vec<ProposalPage>, SubmitRequest)> {
    // The server sets the real expiry; this one only satisfies the verifier.
    let expires_at = chrono::Utc::now() + chrono::TimeDelta::days(1);
    let mut verifier =
        ProposalVerifier::new(repository.clone(), operation_id.to_string(), expires_at).map_err(integrity)?;
    let mut pages = Vec::new();
    let mut page: Vec<ProposalRecord> = Vec::new();
    let mut bytes = 0;
    let mut finish = |page: Vec<ProposalRecord>, verifier: &mut ProposalVerifier| -> Result<()> {
        let page = ProposalPage {
            repository: repository.clone(),
            operation_id: operation_id.to_string(),
            offset: verifier.next_offset(),
            records: page,
        };
        verifier.push(&page, chrono::Utc::now()).map_err(integrity)?;
        pages.push(page);
        Ok(())
    };
    for record in records {
        let size = serde_json::to_vec(record)?.len() + 1;
        if !page.is_empty() && (page.len() == MAX_PAGE_RECORDS || bytes + size > PAGE_RECORD_BYTES) {
            finish(std::mem::take(&mut page), &mut verifier)?;
            bytes = 0;
        }
        page.push(record.clone());
        bytes += size;
    }
    if !page.is_empty() {
        finish(page, &mut verifier)?;
    }
    let request = verifier.request(metadata).map_err(integrity)?;
    Ok((pages, request))
}

impl Repository {
    fn remote_client(&self) -> Result<Remote> {
        let url = self.remote().ok_or_else(|| LocalError::Invalid("this working tree is not an HTTPS clone".into()))?;
        let path = self.metadata_path().join(TOKEN_FILE);
        let mut token = String::new();
        std::fs::File::open(&path)
            .and_then(|mut file| file.read_to_string(&mut token))
            .at("read", &path)?;
        Remote::new(url, &token)
    }

    fn store(&self) -> Store<'_> {
        Store {
            root: self.root_path(),
            storage: self.origin_storage(),
            core: self.core(),
            repo_id: self.repository_id(),
            chunks: (self.storage_mode == StorageMode::Chunked)
                .then(|| ChunkStore::new(self.store_path().join(OBJECT_STORE_FILE), self.repository_id())),
        }
    }

    /// Take in the server's newer commits. The working files take them in
    /// afterwards, as in any pull.
    pub(crate) async fn take_in_remote(&self) -> Result<()> {
        let remote = self.remote_client()?;
        let mut held = read_held(self.metadata_path())?;
        let before = held.clone();
        let head = self.head_repo_seq()?;
        let fetched = self.store().fetch(&remote, &mut held).await?;
        if self.head_repo_seq()? != head {
            if before.pushed_seq == head {
                // Nothing was held, so everything here is on the server.
                held.pushed_seq = self.head_repo_seq()?;
            }
            self.leave_head();
            self.recover_current().await?;
        }
        if held != before {
            write_held(self.metadata_path(), &held)?;
        }
        match fetched {
            Ok(()) => Ok(()),
            Err(items) => Err(LocalError::HeldConflict { paths: self.item_paths(&items).await? }),
        }
    }

    /// Send the held commits to the server, oldest first, after taking in
    /// its newer ones. Each keeps its commit ID.
    pub(crate) async fn push_remote(&self) -> Result<PushOutcome> {
        self.pull().await?;
        let pushed = self.held_commits().await?.unwrap_or_default();
        if pushed.is_empty() {
            return Ok(PushOutcome::default());
        }
        let remote = self.remote_client()?;
        let repository = remote.capabilities()?.repository;
        for commit_id in &pushed {
            let commit_id = CommitId::parse(commit_id).map_err(|error| LocalError::Integrity(error.to_string()))?;
            self.send_commit(&remote, &repository, commit_id).await?;
        }
        let mut held = read_held(self.metadata_path())?;
        held.pushed_seq = self.head_repo_seq()?;
        write_held(self.metadata_path(), &held)?;
        Ok(PushOutcome { pushed })
    }

    async fn send_commit(&self, remote: &Remote, repository: &RepositoryIdentity, commit_id: CommitId) -> Result<()> {
        let commit = self
            .core()
            .get_commit(self.repository_id().clone(), commit_id.clone())
            .await?
            .ok_or_else(|| LocalError::Integrity(format!("held commit {commit_id} is missing")))?;
        let operation_id = commit_id.to_string();
        if let Some(receipt) = remote.receipt(&operation_id)? {
            // Accepted before its receipt reached this clone.
            if receipt.commit_id != commit_id {
                return Err(integrity(format!("operation {operation_id} was accepted as another commit")));
            }
            return Ok(());
        }
        let (proposal, ref_events) = stored_proposal(self.core(), self.repository_id(), &commit).await?;
        let records: Vec<ProposalRecord> = proposal
            .events
            .into_iter()
            .map(ProposalRecord::Item)
            .chain(ref_events.into_iter().map(ProposalRecord::Ref))
            .collect();
        let metadata = CommitMetadata {
            author_id: commit.author_id.clone(),
            committer_id: commit.committer_id.clone(),
            message: commit.message.clone(),
            occurred_at: commit.occurred_at,
        };
        let (pages, request) = proposal_pages(repository, &operation_id, &records, metadata)?;
        if synck_sync::validate_submit(&request).map_err(integrity)?.commit_id != commit_id {
            return Err(LocalError::Integrity(format!("held commit {commit_id} does not rebuild its ID")));
        }
        let store = self.store();
        // Pages expire a day after the first; one expiry starts them over.
        'upload: for _ in 0..2 {
            for page in &pages {
                if remote.put_page(page)?.is_none() {
                    continue 'upload;
                }
            }
            let mut uploaded = false;
            loop {
                match remote.submit(&request, &self.agent_lock_tokens)? {
                    Submitted::Result(SubmitResult::Accepted { receipt }) => {
                        receipt.validate_for(&request).map_err(integrity)?;
                        return Ok(());
                    }
                    Submitted::Result(SubmitResult::Conflict { items }) => {
                        let items: Vec<ItemId> = items.into_iter().map(|head| head.item_id).collect();
                        return Err(LocalError::HeldConflict { paths: self.item_paths(&items).await? });
                    }
                    Submitted::Result(SubmitResult::PolicyLimit { .. }) => return Err(refused("POLICY_LIMIT")),
                    Submitted::Missing(objects) if !uploaded => {
                        for id in &objects {
                            store.upload(remote, id)?;
                        }
                        uploaded = true;
                    }
                    Submitted::Missing(_) => return Err(integrity("objects stay missing after upload")),
                    Submitted::Expired => continue 'upload,
                }
            }
        }
        Err(refused("UPLOAD_EXPIRED"))
    }

    pub(crate) fn remote_locks(&self) -> Result<Vec<AgentLock>> {
        Ok(self.remote_client()?.leases()?.locks.into_iter().map(agent_lock).collect())
    }

    pub(crate) fn remote_reserve(&self, paths: Vec<String>, owner: &str, ttl: u64) -> Result<AgentLease> {
        let lease = self.remote_client()?.reserve(&LeaseRequest {
            paths,
            owner: owner.to_string(),
            ttl,
        })?;
        Ok(AgentLease {
            token: lease.token,
            locks: lease.locks.into_iter().map(agent_lock).collect(),
        })
    }

    pub(crate) fn remote_renew(&self, token: &str, ttl: u64) -> Result<()> {
        self.remote_client()?.renew(&LeaseRenewal { token: token.to_string(), ttl })
    }

    pub(crate) fn remote_release(&self, token: &str) -> Result<usize> {
        Ok(self.remote_client()?.release(&LeaseToken { token: token.to_string() })?.released as usize)
    }

    pub(crate) fn remote_break(&self, paths: Vec<String>) -> Result<usize> {
        Ok(self.remote_client()?.break_paths(&LeasePaths { paths })?.released as usize)
    }
}

fn agent_lock(leased: synck_sync::LeasedPath) -> AgentLock {
    AgentLock {
        path: leased.path,
        owner: leased.owner,
        expires_at: leased.expires_at,
    }
}
