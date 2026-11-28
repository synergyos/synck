//! HTTPS clones: a held clone whose origin is a protocol 2 sync server
//! (`docs/spec/protocol.md`).
//!
//! The clone keeps its own history and content store. `pull` verifies the
//! server's history pages from the cursor in `held.json`, downloads the
//! content they name and replays each commit with its ID. `push` uploads each
//! held commit as one proposal whose operation ID is its commit ID, so a
//! retry after a lost receipt finds the accepted commit. The token lives in
//! `.synck/remote-token`; it never appears in a URL.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use synck_sync::{
    Capabilities, CommitMetadata, Cursor, HistoryPage, HistoryRequest, HistoryVerifier, Lease,
    LeasePaths, LeaseRenewal, LeaseRequest, LeaseToken, Leases, MAX_PACK_OBJECTS, MAX_PAGE_BYTES,
    MAX_PAGE_RECORDS, ObjectRequest, PACK_BYTES, PACK_HEADER_BYTES, PageAck, ProposalPage,
    ProposalRecord, ProposalVerifier, Receipt, Refusal, Released, RepositoryIdentity,
    SubmitRequest, SubmitResult, Wire,
};
use synck_vcs::config::CoreConfig;
use synck_vcs::metadata::Metadata;
use synck_vcs::repo::Repo;
use synck_vcs::storage::{SealedCommitInsert, Storage, StorageBackend as _};
use synck_vcs::types::{
    CommitEventRow, CommitId, CommitPk, CommitRow, EventInsertRow, ItemId, ItemRow, ObjectId,
    RefEventInsertRow, RepoId, Sha256Digest,
};
use synck_vcs::verify::{SemanticVerifier, VerifyCommit};
use synck_vcs_sqlite::SqliteStorage;
use ureq::http::{Response, StatusCode, Uri};
use ureq::tls::{RootCerts, TlsConfig};

use crate::chunk_store::{ChunkStore, OBJECT_STORE_FILE, StorageMode};
use crate::commands::{
    Held, PushOutcome, apply, empty_history, proposal_from_rows, read_held, stored_proposal,
    write_held,
};
use crate::error::{IoContext, LocalError, Result};
use crate::portable::VerifyOwned;
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
/// Every reply but an object pack is a message the protocol bounds.
const MAX_REPLY_BYTES: u64 = MAX_PAGE_BYTES as u64;

fn refused(status: &str) -> LocalError {
    LocalError::Server {
        status: status.to_string(),
    }
}

fn unreachable(error: ureq::Error) -> LocalError {
    LocalError::Unreachable(error.to_string())
}

fn integrity(error: impl std::fmt::Display) -> LocalError {
    LocalError::Integrity(format!("sync server: {error}"))
}

/// `https://HOST/v2/repositories/REPO_ID`, or `http://` to a loopback host.
/// Returns the URL without a trailing slash and the repository ID it names.
pub(crate) fn parse_url(url: &str) -> Result<(String, RepoId)> {
    let invalid = |reason: &str| {
        LocalError::Invalid(format!(
            "{url} is not a sync server repository URL: {reason}"
        ))
    };
    let url = url.trim_end_matches('/');
    let uri: Uri = url.parse().map_err(|_| invalid("it does not parse"))?;
    let host = uri.host().ok_or_else(|| invalid("it has no host"))?;
    match uri.scheme_str() {
        Some("https") => {}
        Some("http") if matches!(host, "localhost" | "127.0.0.1" | "[::1]") => {}
        Some("http") => return Err(invalid("protocol 2 needs https except to this machine")),
        _ => return Err(invalid("it is not https")),
    }
    if uri
        .authority()
        .is_some_and(|authority| authority.as_str().contains('@'))
        || uri.query().is_some()
    {
        return Err(invalid(
            "it carries credentials or a query; give the token in SYNCK_TOKEN",
        ));
    }
    let repo_id = uri
        .path()
        .strip_prefix("/v2/repositories/")
        .filter(|id| !id.contains('/'))
        .ok_or_else(|| invalid("its path is not /v2/repositories/REPO_ID"))?;
    let repo_id = RepoId::parse(repo_id).map_err(|_| invalid("its repository ID is not valid"))?;
    Ok((url.to_string(), repo_id))
}

/// One repository on a sync server.
pub(crate) struct Remote {
    agent: ureq::Agent,
    sync: String,
    authorization: String,
}

/// An object pack read from staged files, one open at a time.
struct PackReader<'a> {
    objects: std::slice::Iter<'a, (ObjectId, PathBuf, u64)>,
    current: Option<Box<dyn Read + 'a>>,
}

impl Read for PackReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if let Some(current) = &mut self.current {
                let count = current.read(buffer)?;
                if count > 0 {
                    return Ok(count);
                }
            }
            let Some((id, path, size)) = self.objects.next() else {
                return Ok(0);
            };
            let mut header = id.as_bytes().to_vec();
            header.extend(size.to_be_bytes());
            let file = std::fs::File::open(path)?.take(*size);
            self.current = Some(Box::new(std::io::Cursor::new(header).chain(file)));
        }
    }
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
        let tls = TlsConfig::builder()
            .root_certs(RootCerts::PlatformVerifier)
            .build();
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

    fn post(
        &self,
        path: &str,
        body: &impl Wire,
        lock_tokens: &[String],
    ) -> Result<Response<ureq::Body>> {
        let mut request = self
            .agent
            .post(self.url(path))
            .header("authorization", &self.authorization)
            .header("content-type", "application/octet-stream");
        for token in lock_tokens {
            request = request.header(LOCK_TOKEN, token);
        }
        request
            .send(&synck_sync::encode(body).map_err(integrity)?[..])
            .map_err(unreachable)
    }

    fn put(&self, path: &str, body: ureq::SendBody) -> Result<Response<ureq::Body>> {
        self.agent
            .put(self.url(path))
            .header("authorization", &self.authorization)
            .send(body)
            .map_err(unreachable)
    }

    /// A success's message, or the refusal as an error.
    fn message<T: Wire>(response: Response<ureq::Body>) -> Result<T> {
        let (status, body) = Self::read(response)?;
        if !status.is_success() {
            return Err(Self::refusal(status, &body));
        }
        synck_sync::decode(&body).map_err(integrity)
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

    /// A refused request's body, if it is one.
    fn refused_with(body: &[u8]) -> Option<Refusal> {
        synck_sync::decode(body).ok()
    }

    fn refusal(status: StatusCode, body: &[u8]) -> LocalError {
        match Self::refused_with(body) {
            Some(Refusal::Locked { path, owner }) => LocalError::Locked { path, owner },
            Some(Refusal::LockLost) => LocalError::LockLost,
            Some(refusal) => refused(refusal.name()),
            None => refused(&status.to_string()),
        }
    }

    pub(crate) fn capabilities(&self) -> Result<Capabilities> {
        let capabilities: Capabilities = Self::message(self.get("capabilities")?)?;
        capabilities.validate().map_err(integrity)?;
        Ok(capabilities)
    }

    fn history(&self, request: &HistoryRequest) -> Result<HistoryPage> {
        Self::message(self.post("history", request, &[])?)
    }

    fn receipt(&self, operation_id: &str) -> Result<Option<Receipt>> {
        let response = self.get(&format!("receipts/{operation_id}"))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Self::message(response).map(Some)
    }

    fn put_page(&self, page: &ProposalPage) -> Result<Option<PageAck>> {
        let body = synck_sync::encode(page).map_err(integrity)?;
        let response = self.put(
            &format!("uploads/{}/pages/{}", page.operation_id, page.offset),
            ureq::SendBody::from_reader(&mut &body[..]),
        )?;
        let (status, body) = Self::read(response)?;
        if status.is_success() {
            return synck_sync::decode(&body).map(Some).map_err(integrity);
        }
        match Self::refused_with(&body) {
            Some(Refusal::UploadExpired) => Ok(None),
            _ => Err(Self::refusal(status, &body)),
        }
    }

    fn submit(&self, request: &SubmitRequest, lock_tokens: &[String]) -> Result<Submitted> {
        let (status, body) = Self::read(self.post("proposals", request, lock_tokens)?)?;
        if status.is_success() {
            return synck_sync::decode(&body)
                .map(Submitted::Result)
                .map_err(integrity);
        }
        match Self::refused_with(&body) {
            Some(Refusal::UploadExpired) => Ok(Submitted::Expired),
            Some(Refusal::ObjectsMissing(objects)) => Ok(Submitted::Missing(objects)),
            _ => Err(Self::refusal(status, &body)),
        }
    }

    /// Upload one object pack of staged files.
    fn put_objects(&self, objects: &[(ObjectId, PathBuf, u64)]) -> Result<()> {
        let mut reader = PackReader {
            objects: objects.iter(),
            current: None,
        };
        let (status, body) =
            Self::read(self.put("objects", ureq::SendBody::from_reader(&mut reader))?)?;
        if !status.is_success() {
            return Err(Self::refusal(status, &body));
        }
        Ok(())
    }

    /// Download `ids` in packs, handing each object's bytes to `each` in
    /// order. A pack may end early, at `PACK_BYTES`; the rest are asked for
    /// again.
    fn get_objects(
        &self,
        ids: &[ObjectId],
        mut each: impl FnMut(&ObjectId, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        let cut = |error: std::io::Error| LocalError::Unreachable(format!("object pack: {error}"));
        let mut rest = ids;
        while !rest.is_empty() {
            let request = ObjectRequest {
                objects: rest[..rest.len().min(MAX_PACK_OBJECTS)].to_vec(),
            };
            let response = self.post("objects", &request, &[])?;
            if !response.status().is_success() {
                let (status, body) = Self::read(response)?;
                return Err(Self::refusal(status, &body));
            }
            let mut reader = response.into_body().into_reader();
            let mut received = 0;
            let mut header = [0_u8; PACK_HEADER_BYTES];
            loop {
                let read = reader.read(&mut header).map_err(cut)?;
                if read == 0 {
                    break;
                }
                reader.read_exact(&mut header[read..]).map_err(cut)?;
                let (id, size) = header.split_at(PACK_HEADER_BYTES - 8);
                let id = request
                    .objects
                    .get(received)
                    .filter(|expected| expected.as_bytes() == id)
                    .ok_or_else(|| integrity("an object pack holds an object not asked for"))?;
                let mut object =
                    (&mut reader).take(u64::from_be_bytes(size.try_into().expect("eight bytes")));
                each(id, &mut object)?;
                if object.limit() != 0 {
                    return Err(LocalError::Unreachable("object pack cut short".into()));
                }
                received += 1;
            }
            if received == 0 {
                return Err(integrity("an object pack is empty"));
            }
            rest = &rest[received..];
        }
        Ok(())
    }

    fn leases(&self) -> Result<Leases> {
        Self::message(self.get("leases")?)
    }

    fn reserve(&self, request: &LeaseRequest) -> Result<Lease> {
        Self::message(self.post("leases", request, &[])?)
    }

    fn renew(&self, renewal: &LeaseRenewal) -> Result<()> {
        let (status, body) = Self::read(self.post("leases/renew", renewal, &[])?)?;
        if !status.is_success() {
            return Err(Self::refusal(status, &body));
        }
        Ok(())
    }

    fn release(&self, token: &LeaseToken) -> Result<Released> {
        Self::message(self.post("leases/release", token, &[])?)
    }

    fn break_paths(&self, paths: &LeasePaths) -> Result<Released> {
        Self::message(self.post("leases/break", paths, &[])?)
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
        let path = self
            .metadata()
            .join("objects")
            .join(&id.as_str()["sha256:".len()..]);
        path.try_exists().at("inspect object", &path)
    }

    fn temporary(&self, kind: &str) -> Result<(PathBuf, std::fs::File)> {
        let path = self
            .metadata()
            .join("tmp")
            .join(format!("{kind}-{}.tmp", uuid::Uuid::new_v4()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .at("create", &path)?;
        Ok((path, file))
    }

    /// Download the content objects `ids`, storing each only if its bytes
    /// hash to its ID.
    fn download(&self, remote: &Remote, ids: &[ObjectId]) -> Result<()> {
        let store = |id: &ObjectId, bytes: &mut dyn Read| {
            let (path, mut file) = self.temporary("download")?;
            let result = (|| {
                std::io::copy(bytes, &mut file)
                    .map_err(|error| LocalError::Unreachable(error.to_string()))?;
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
        };
        remote.get_objects(ids, store)
    }

    /// Write `id`'s exact bytes to a staging file. Metadata lives in history,
    /// content in the object store.
    fn stage_upload(&self, id: &ObjectId, metadata: Option<Vec<u8>>) -> Result<(PathBuf, u64)> {
        let (path, mut file) = self.temporary("upload")?;
        let result = (|| {
            if let Some(bytes) = metadata {
                file.write_all(&bytes).at("write", &path)?;
                return Ok(bytes.len() as u64);
            }
            let metadata =
                cap_std::fs::Dir::open_ambient_dir(self.metadata(), cap_std::ambient_authority())
                    .at("open", self.metadata())?;
            let size = crate::portable::copy_stored(
                self.root,
                &metadata,
                self.chunks.as_ref(),
                id,
                &mut file,
            )?;
            file.flush().at("write", &path)?;
            Ok(size)
        })();
        match result {
            Ok(size) => Ok((path, size)),
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                Err(error)
            }
        }
    }

    /// Upload `ids` in packs of at most `MAX_PACK_OBJECTS` and about
    /// `PACK_BYTES`.
    fn upload(&self, remote: &Remote, ids: &[ObjectId]) -> Result<()> {
        let metadata = self
            .storage
            .metadata_objects(self.repo_id, ids)
            .map_err(integrity)?;
        let mut pack: Vec<(ObjectId, PathBuf, u64)> = Vec::new();
        let result = (|| {
            let mut bytes = 0;
            for (index, (id, metadata)) in ids.iter().zip(metadata).enumerate() {
                let (path, size) = self.stage_upload(id, metadata)?;
                pack.push((id.clone(), path, size));
                bytes += size;
                let last = index + 1 == ids.len();
                if pack.len() < MAX_PACK_OBJECTS && bytes < PACK_BYTES && !last {
                    continue;
                }
                remote.put_objects(&pack)?;
                for (_, path, _) in pack.drain(..) {
                    let _ = std::fs::remove_file(path);
                }
                bytes = 0;
            }
            Ok(())
        })();
        for (_, path, _) in &pack {
            let _ = std::fs::remove_file(path);
        }
        result
    }

    /// The metadata objects `ids`, downloading those this history lacks and
    /// storing them through Core's metadata storage in one transaction.
    fn metadata_objects(
        &self,
        remote: &Remote,
        ids: BTreeSet<ObjectId>,
    ) -> Result<BTreeMap<ObjectId, Metadata>> {
        let missing: BTreeSet<ObjectId> = self
            .storage
            .missing_metadata(self.repo_id, ids.iter().cloned())
            .map_err(integrity)?
            .into_iter()
            .collect();
        let mut found = BTreeMap::new();
        let mut downloaded = Vec::new();
        let mut keep = |id: &ObjectId, bytes: Vec<u8>| -> Result<()> {
            found.insert(
                id.clone(),
                Metadata::verify(id, bytes.clone()).map_err(integrity)?,
            );
            downloaded.push((id.clone(), bytes));
            Ok(())
        };
        if !missing.is_empty() {
            let missing: Vec<ObjectId> = missing.iter().cloned().collect();
            remote.get_objects(&missing, |id, bytes| {
                let mut buffer = Vec::new();
                bytes
                    .take(MAX_REPLY_BYTES)
                    .read_to_end(&mut buffer)
                    .map_err(|error| LocalError::Unreachable(error.to_string()))?;
                keep(id, buffer)
            })?;
        }
        for id in ids.into_iter().filter(|id| !missing.contains(id)) {
            let metadata = self
                .storage
                .get_metadata(self.repo_id, &id)
                .map_err(integrity)?
                .ok_or_else(|| integrity(format!("metadata object {id} disappeared")))?;
            found.insert(id, metadata);
        }
        if !downloaded.is_empty() {
            self.storage
                .put_metadata_objects(self.repo_id, &downloaded)
                .map_err(integrity)?;
        }
        Ok(found)
    }

    /// Download the content objects `events` name that this store lacks,
    /// durably, and return each event's metadata object ID. Rows on the wire
    /// carry metadata by ID only.
    fn take_content(
        &self,
        remote: &Remote,
        events: &[CommitEventRow],
    ) -> Result<Vec<Option<ObjectId>>> {
        let mut references = Vec::with_capacity(events.len());
        let mut missing = BTreeSet::new();
        for row in events {
            let found = synck_items::references::commit_event_references(row).map_err(integrity)?;
            for id in found.objects {
                if !self.has_object(&id)? {
                    missing.insert(id);
                }
            }
            references.push(found.metadata);
        }
        if !missing.is_empty() {
            self.download(remote, &missing.into_iter().collect::<Vec<_>>())?;
            match &self.chunks {
                Some(chunks) => chunks.sync_wal()?,
                None => crate::repository::sync_directory(&self.metadata().join("objects"))?,
            }
        }
        Ok(references)
    }

    /// Install one verified server commit unless it is here already.
    async fn install(
        &self,
        remote: &Remote,
        commit: &CommitRow,
        mut events: Vec<CommitEventRow>,
    ) -> Result<std::result::Result<(), Vec<ItemId>>> {
        if self
            .core
            .get_commit(self.repo_id.clone(), commit.commit_id.clone())
            .await?
            .is_some()
        {
            return Ok(Ok(()));
        }
        let references = self.take_content(remote, &events)?;
        let metadata =
            self.metadata_objects(remote, references.iter().flatten().cloned().collect())?;
        for (row, id) in events.iter_mut().zip(&references) {
            let metadata = id.as_ref().map(|id| metadata[id].clone());
            match row {
                CommitEventRow::Item(row) => row.metadata = metadata,
                CommitEventRow::Ref(row) => row.metadata = metadata,
            }
        }
        let (proposal, ref_events) = proposal_from_rows(commit, events)?;
        apply(self.core, self.repo_id, commit, proposal, ref_events).await
    }

    /// Install one verified server commit as the server published it, at its
    /// position with its acceptance time, after `replay` has checked it as
    /// repository verification does. Its objects are stored as they arrive,
    /// in either form: this needs no key.
    async fn copy_commit(
        &self,
        remote: &Remote,
        commit: CommitRow,
        events: Vec<CommitEventRow>,
        replay: &mut SemanticVerifier,
    ) -> Result<()> {
        let references = self.take_content(remote, &events)?;
        let missing = self
            .storage
            .missing_metadata(self.repo_id, references.into_iter().flatten())
            .map_err(integrity)?;
        if !missing.is_empty() {
            let mut downloaded = Vec::with_capacity(missing.len());
            remote.get_objects(&missing, |id, bytes| {
                let mut buffer = Vec::new();
                bytes
                    .take(MAX_REPLY_BYTES)
                    .read_to_end(&mut buffer)
                    .map_err(|error| LocalError::Unreachable(error.to_string()))?;
                downloaded.push((id.clone(), buffer));
                Ok(())
            })?;
            self.storage
                .put_metadata_objects(self.repo_id, &downloaded)
                .map_err(integrity)?;
        }
        let events: Vec<VerifyOwned> = events.into_iter().map(VerifyOwned::from).collect();
        let findings = replay.verify_commit_events(
            &VerifyCommit {
                commit_id: Some(commit.commit_id.clone()),
                repo_seq: commit.repo_seq,
                committer_id: commit.committer_id.clone(),
                occurred_at: commit.occurred_at,
                created_at: commit.created_at,
                author_id: commit.author_id.clone(),
                message: commit.message.clone(),
                event_count: Some(commit.event_count),
            },
            events.iter().map(VerifyOwned::borrowed),
        );
        if let Some(finding) = findings.into_iter().next() {
            return Err(integrity(finding));
        }
        let commit_pk = CommitPk::new();
        let mut parents = replay.event_parents().iter().cloned();
        let (mut item_rows, mut ref_rows) = (Vec::new(), Vec::new());
        for event in events {
            match event {
                VerifyOwned::Item(event) => item_rows.push(EventInsertRow {
                    repo_id: self.repo_id.clone(),
                    commit_pk: commit_pk.clone(),
                    commit_event_idx: event.commit_event_idx,
                    item_id: event.item_id,
                    item_seq: event.item_seq,
                    event_type: event.event_type,
                    payload: event.payload,
                    metadata: None,
                    parent_folder_id: parents
                        .next()
                        .expect("replay gives each item event a parent"),
                }),
                VerifyOwned::Ref(event) => ref_rows.push(RefEventInsertRow {
                    repo_id: self.repo_id.clone(),
                    commit_pk: commit_pk.clone(),
                    commit_event_idx: event.commit_event_idx,
                    owner_item_id: event.owner_item_id,
                    ref_seq: event.ref_seq,
                    event_type: event.event_type,
                    payload: event.payload,
                    metadata: None,
                }),
            }
        }
        self.storage
            .install_sealed_history_commit(
                self.repo_id,
                &SealedCommitInsert {
                    commit_pk,
                    repo_seq: commit.repo_seq,
                    operation_id: None,
                    author_id: commit.author_id,
                    committer_id: commit.committer_id,
                    message: commit.message,
                    occurred_at: commit.occurred_at,
                    created_at: commit.created_at,
                    commit_id: commit.commit_id,
                    event_count: commit.event_count,
                },
                &item_rows,
                &ref_rows,
            )
            .await
            .map_err(integrity)?;
        Ok(())
    }

    /// Take in the server's commits after `held.remote`, in order and with
    /// their IDs, advancing `held` past each one installed. Returns the items
    /// of the first commit this history cannot take.
    async fn fetch(
        &self,
        remote: &Remote,
        held: &mut Held,
    ) -> Result<std::result::Result<(), Vec<ItemId>>> {
        let start = held
            .remote
            .clone()
            .ok_or_else(|| integrity("held.json has no server cursor"))?;
        let repository = start.repository.clone();
        // Idle connections keep each database's WAL index from being rebuilt
        // from the whole WAL at every open. Downloads leave their frames in
        // the object store's WAL until `install` checkpoints.
        let _history = self.storage.open_readonly().map_err(integrity)?;
        let _objects = self
            .chunks
            .as_ref()
            .map(ChunkStore::open_readonly)
            .transpose()?;
        let mut history = RemoteHistory::new(start)?;
        while let Some((commit, checkpoint, events)) = history.next(remote)? {
            if let Err(items) = self.install(remote, &commit, events).await? {
                return Ok(Err(items));
            }
            let repo_seq = commit
                .repo_seq
                .ok_or_else(|| integrity("unsequenced commit"))?;
            held.origin_seq = repo_seq;
            held.remote = Some(Cursor {
                repository: repository.clone(),
                repo_seq: repo_seq as u64,
                checkpoint,
            });
        }
        Ok(Ok(()))
    }

    /// Copy the server's whole published history into this empty store, as
    /// the server published it, and the item heads its replay gives. Returns
    /// the server cursor the copy ends at.
    async fn copy(&self, remote: &Remote, genesis: Cursor, encrypted: bool) -> Result<Cursor> {
        let _history = self.storage.open_readonly().map_err(integrity)?;
        let _objects = self
            .chunks
            .as_ref()
            .map(ChunkStore::open_readonly)
            .transpose()?;
        let mut history = RemoteHistory::new(genesis)?;
        let mut replay = SemanticVerifier::new(encrypted);
        while let Some((commit, _, events)) = history.next(remote)? {
            self.copy_commit(remote, commit, events, &mut replay)
                .await?;
        }
        let mut txn = self.storage.begin_txn().await.map_err(integrity)?;
        for head in replay.replayed_heads() {
            txn.put_item(&ItemRow {
                repo_id: self.repo_id.clone(),
                item_id: head.item_id,
                item_type: head.item_type,
                parent_folder_id: head.parent_folder_id,
                name: head.name,
                name_key: head.name_key,
                record_key: head.record_key,
                link_folder_id: head.link_folder_id,
                head_seq: head.head_seq,
                ref_head_seq: head.ref_head_seq,
                lifecycle_state: head.lifecycle_state,
                created_at: head.created_at,
                created_by: head.created_by,
                federation_stream_id: head.federation_stream_id,
            })
            .await
            .map_err(integrity)?;
        }
        txn.commit().await.map_err(integrity)?;
        Ok(history.position().clone())
    }
}

/// A commit as history pages carry it: its row, its checkpoint and its events.
type PublishedCommit = (CommitRow, Sha256Digest, Vec<CommitEventRow>);

/// The server's published commits after a cursor, from pages `HistoryVerifier`
/// accepts, each handed out whole and in order.
struct RemoteHistory {
    verifier: HistoryVerifier,
    complete: VecDeque<PublishedCommit>,
    partial: Option<PublishedCommit>,
    more: bool,
}

impl RemoteHistory {
    fn new(after: Cursor) -> Result<Self> {
        Ok(Self {
            verifier: HistoryVerifier::new(after).map_err(integrity)?,
            complete: VecDeque::new(),
            partial: None,
            more: true,
        })
    }

    fn next(&mut self, remote: &Remote) -> Result<Option<PublishedCommit>> {
        while self.complete.is_empty() && self.more {
            let page = remote.history(&HistoryRequest {
                after: self.verifier.position().clone(),
                limit: MAX_PAGE_RECORDS,
            })?;
            self.verifier.push(&page).map_err(integrity)?;
            self.more = page.has_more;
            for fragment in page.fragments {
                let (commit, _, events) = self.partial.get_or_insert_with(|| {
                    (
                        fragment.header.commit.clone(),
                        fragment.header.checkpoint.clone(),
                        Vec::new(),
                    )
                });
                events.extend(fragment.events);
                if events.len() < commit.event_count as usize {
                    continue;
                }
                let commit = self.partial.take().expect("a commit in progress");
                self.complete.push_back(commit);
            }
        }
        Ok(self.complete.pop_front())
    }

    /// The last complete commit's cursor.
    fn position(&self) -> &Cursor {
        &self.verifier.position().completed
    }
}

fn core(storage: SqliteStorage) -> Repo {
    Repo::new(
        Storage::new(std::sync::Arc::new(storage)),
        CoreConfig::default(),
    )
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

/// The repository at `url` on its sync server, and what the server says of it.
pub(crate) fn connect(url: &str, token: &str) -> Result<(String, Remote, Capabilities)> {
    let (url, repo_id) = parse_url(url)?;
    let remote = Remote::new(&url, token)?;
    let capabilities = remote.capabilities()?;
    if capabilities.latest.repository.repo_id != repo_id {
        return Err(integrity("the server answered for another repository"));
    }
    Ok((url, remote, capabilities))
}

/// Copy the server's whole published history into the new, empty repository
/// at `root`, whose history already has the server's generation: every commit
/// at its position with its acceptance time, every object it names as stored,
/// and the item heads its replay gives. Nothing here needs a key.
pub(crate) async fn copy_history(
    remote: &Remote,
    capabilities: &Capabilities,
    root: &Path,
    storage: SqliteStorage,
    chunks: Option<ChunkStore>,
) -> Result<()> {
    let identity = &capabilities.latest.repository;
    let history = Repo::new(
        Storage::new(std::sync::Arc::new(storage.clone())),
        CoreConfig {
            encrypted: capabilities.encrypted,
            follows_domains: false,
        },
    );
    let store = Store {
        root,
        storage: storage.clone(),
        core: &history,
        repo_id: &identity.repo_id,
        chunks,
    };
    let genesis = Cursor::genesis(identity.clone(), capabilities.encrypted).map_err(integrity)?;
    let head = store.copy(remote, genesis, capabilities.encrypted).await?;
    // The same generation, positions, acceptance times and commits give the
    // same chain, so the copy ends at the server's own checkpoint.
    if storage.history_head(identity).map_err(integrity)? != head {
        return Err(integrity("the copy's checkpoints are not the server's"));
    }
    Ok(())
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
    parse_url(url)?;
    if author.trim().is_empty() {
        return Err(LocalError::Invalid("an HTTPS clone needs --author".into()));
    }
    let include = crate::checkout::normalize_paths(include)?;
    let (url, remote, capabilities) = connect(url, token)?;
    if capabilities.encrypted {
        return Err(LocalError::Invalid(
            "an encrypted repository has no working tree here, since this client holds no key; copy it with synck repository import".into(),
        ));
    }
    crate::clone::create_empty_directory(destination)?;
    let destination = destination
        .canonicalize()
        .at("resolve clone destination", destination)?;
    let metadata = destination.join(".synck");
    let result = make_clone(
        &remote,
        capabilities,
        &url,
        token,
        &destination,
        &metadata,
        author,
        include,
        hold,
    )
    .await;
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
    let repo_id = capabilities.latest.repository.repo_id.clone();
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
    let storage = empty_history(metadata.join("repository.sqlite3"), &repo_id).await?;
    let history = core(storage.clone());
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
        remote: Some(
            Cursor::genesis(capabilities.latest.repository.clone(), false).map_err(integrity)?,
        ),
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
        pending: Some(crate::checkout::Transition {
            previous_paths: None,
        }),
        baseline: Some(head),
        committing: None,
    };
    let source = metadata.join(crate::checkout::CHECKOUT_SOURCE_FILE);
    std::fs::write(&source, b"[]").at("write", &source)?;
    crate::checkout::write_checkout(metadata, &checkout)?;
    // The config goes last: before it, the directory opens as nothing.
    let config = Config::remote_clone(
        url.to_string(),
        repo_id,
        author.to_string(),
        checkout,
        storage_mode,
        hold,
    );
    let path = metadata.join("tmp").join("config.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?).at("write", &path)?;
    crate::repository::sync_file(&path)?;
    crate::repository::replace_file(&path, &metadata.join("config.json"))
        .at("publish clone config", metadata)?;
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
        ProposalVerifier::new(repository.clone(), operation_id.to_string(), expires_at)
            .map_err(integrity)?;
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
        verifier
            .push(&page, chrono::Utc::now())
            .map_err(integrity)?;
        pages.push(page);
        Ok(())
    };
    for record in records {
        let size = synck_sync::encode(record).map_err(integrity)?.len();
        if !page.is_empty() && (page.len() == MAX_PAGE_RECORDS || bytes + size > PAGE_RECORD_BYTES)
        {
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
        let url = self
            .remote()
            .ok_or_else(|| LocalError::Invalid("this working tree is not an HTTPS clone".into()))?;
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
            chunks: (self.storage_mode == StorageMode::Chunked).then(|| {
                ChunkStore::new(
                    self.store_path().join(OBJECT_STORE_FILE),
                    self.repository_id(),
                )
            }),
        }
    }

    /// Take in the server's newer commits. The working files take them in
    /// afterwards, as in any pull.
    pub(crate) async fn take_in_remote(&self) -> Result<()> {
        match self.take_in_remote_items().await? {
            Ok(()) => Ok(()),
            Err(items) => Err(LocalError::HeldConflict {
                paths: self.item_paths(&items).await?,
            }),
        }
    }

    /// Take in the server's newer commits, or return the items of the first
    /// one this history cannot take.
    pub(crate) async fn take_in_remote_items(
        &self,
    ) -> Result<std::result::Result<(), Vec<ItemId>>> {
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
        Ok(fetched)
    }

    /// Send the held commits to the server, oldest first, after taking in
    /// its newer ones. Each keeps its commit ID.
    pub(crate) async fn push_remote(&self) -> Result<PushOutcome> {
        let pulled = self.pull().await?;
        if !pulled.unwound.is_empty() {
            return Err(LocalError::HeldMerged {
                paths: pulled.unwound,
                withdrawn: pulled.withdrawn,
            });
        }
        let pushed = self.held_commits().await?.unwrap_or_default();
        if pushed.is_empty() {
            return Ok(PushOutcome::default());
        }
        let remote = self.remote_client()?;
        let repository = remote.capabilities()?.latest.repository;
        for commit_id in &pushed {
            let commit_id = CommitId::parse(commit_id)
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
            self.send_commit(&remote, &repository, commit_id).await?;
        }
        let mut held = read_held(self.metadata_path())?;
        held.pushed_seq = self.head_repo_seq()?;
        write_held(self.metadata_path(), &held)?;
        Ok(PushOutcome { pushed })
    }

    async fn send_commit(
        &self,
        remote: &Remote,
        repository: &RepositoryIdentity,
        commit_id: CommitId,
    ) -> Result<()> {
        let commit = self
            .core()
            .get_commit(self.repository_id().clone(), commit_id.clone())
            .await?
            .ok_or_else(|| LocalError::Integrity(format!("held commit {commit_id} is missing")))?;
        let operation_id = commit_id.to_string();
        if let Some(receipt) = remote.receipt(&operation_id)? {
            // Accepted before its receipt reached this clone.
            if receipt.commit_id != commit_id {
                return Err(integrity(format!(
                    "operation {operation_id} was accepted as another commit"
                )));
            }
            return Ok(());
        }
        let (proposal, ref_events) =
            stored_proposal(self.core(), self.repository_id(), &commit).await?;
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
        if synck_sync::validate_submit(&request)
            .map_err(integrity)?
            .commit_id
            != commit_id
        {
            return Err(LocalError::Integrity(format!(
                "held commit {commit_id} does not rebuild its ID"
            )));
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
                        let items: Vec<ItemId> =
                            items.into_iter().map(|head| head.item_id).collect();
                        return Err(LocalError::HeldConflict {
                            paths: self.item_paths(&items).await?,
                        });
                    }
                    Submitted::Result(SubmitResult::PolicyLimit { .. }) => {
                        return Err(refused("POLICY_LIMIT"));
                    }
                    Submitted::Missing(objects) if !uploaded => {
                        store.upload(remote, &objects)?;
                        uploaded = true;
                    }
                    Submitted::Missing(_) => {
                        return Err(integrity("objects stay missing after upload"));
                    }
                    Submitted::Expired => continue 'upload,
                }
            }
        }
        Err(refused("UPLOAD_EXPIRED"))
    }

    pub(crate) fn remote_locks(&self) -> Result<Vec<AgentLock>> {
        Ok(self
            .remote_client()?
            .leases()?
            .locks
            .into_iter()
            .map(agent_lock)
            .collect())
    }

    pub(crate) fn remote_reserve(
        &self,
        paths: Vec<String>,
        owner: &str,
        ttl: u64,
    ) -> Result<AgentLease> {
        let lease = self.remote_client()?.reserve(&LeaseRequest {
            paths,
            owner: owner.to_string(),
            ttl: lease_ttl(ttl)?,
        })?;
        Ok(AgentLease {
            token: lease.token,
            locks: lease.locks.into_iter().map(agent_lock).collect(),
        })
    }

    pub(crate) fn remote_renew(&self, token: &str, ttl: u64) -> Result<()> {
        self.remote_client()?.renew(&LeaseRenewal {
            token: token.to_string(),
            ttl: lease_ttl(ttl)?,
        })
    }

    pub(crate) fn remote_release(&self, token: &str) -> Result<usize> {
        Ok(self
            .remote_client()?
            .release(&LeaseToken {
                token: token.to_string(),
            })?
            .released as usize)
    }

    pub(crate) fn remote_break(&self, paths: Vec<String>) -> Result<usize> {
        Ok(self
            .remote_client()?
            .break_paths(&LeasePaths { paths })?
            .released as usize)
    }
}

fn agent_lock(leased: synck_sync::LeasedPath) -> AgentLock {
    AgentLock {
        path: leased.path,
        owner: leased.owner,
        expires_at: leased.expires_at,
    }
}

fn lease_ttl(ttl: u64) -> Result<u32> {
    u32::try_from(ttl).map_err(|_| LocalError::Invalid(format!("lease ttl {ttl} is too long")))
}
