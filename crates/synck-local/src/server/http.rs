//! The served repositories and the protocol-1 routes under
//! `/v1/repositories/{repo_id}/sync` (`synck/docs/spec/protocol.md`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::TryStreamExt as _;
use rand::RngCore as _;
use serde::de::DeserializeOwned;
use sha2::{Digest as _, Sha256};
use synck_sync::{
    Capabilities, HistoryPage, HistoryRequest, Lease, LeasePaths, LeaseRenewal, LeaseRequest,
    LeaseToken, LeasedPath, Leases, MAX_PACK_OBJECTS, MAX_PAGE_BYTES, ObjectRequest, PACK_BYTES,
    PACK_HEADER_BYTES, PageAck, ProposalPage, ProposalRecord, ProtocolError, Receipt, Released,
    SubmitRequest, SubmitResult, validate_submit,
};
use synck_vcs::types::{ObjectId, RepoId};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::runtime::Handle;

use crate::portable::{Acceptance, Proposal, ServedRepository};

use super::jobs::Jobs;
use super::lock::LifetimeLock;
use super::tokens::{self, Access};
use super::uploads::Uploads;
use super::{Error, Result};

/// Blocking jobs at once: repository reads and writes, token checks and
/// object downloads, which hold a slot while the client reads.
const JOBS: usize = 16;
/// A submission is a few small fields and a commit message; a history
/// request is smaller.
const MAX_SUBMIT_BYTES: usize = 64 * 1024;
/// A lease request lists paths, each at most a few kilobytes.
const MAX_LEASE_BYTES: usize = 1024 * 1024;
/// Reservation tokens a writer holds, one per header line.
const LOCK_TOKEN: &str = "synck-lock-token";
/// Object uploads are written here, checked, then stored in the repository.
const UPLOADS_DIRECTORY: &str = "tmp";

/// Create a private state root with its lock and state database.
pub fn init(state_root: &Path) -> Result<()> {
    let _lock = LifetimeLock::initialize(state_root)?;
    super::state::create(state_root)
}

pub struct Server {
    state_root: PathBuf,
    repositories: HashMap<RepoId, Arc<ServedRepository>>,
    jobs: Jobs,
    uploads: Uploads,
    /// One submission at a time: its digest is recorded, checked and
    /// accepted together.
    submissions: Mutex<()>,
    _lock: LifetimeLock,
}

impl Server {
    pub fn open(state_root: &Path, repositories: &[PathBuf]) -> Result<Self> {
        let lock = LifetimeLock::acquire(state_root)?;
        tokens::list(state_root)?;
        let uploads = state_root.join(UPLOADS_DIRECTORY);
        match std::fs::remove_dir_all(&uploads) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(Error::filesystem("clear object uploads", &uploads, error));
            }
            _ => {}
        }
        std::fs::create_dir(&uploads)
            .map_err(|error| Error::filesystem("create object uploads", &uploads, error))?;
        let mut served = HashMap::new();
        for root in repositories {
            let root = root
                .canonicalize()
                .map_err(|error| Error::filesystem("resolve repository", root, error))?;
            let repository = ServedRepository::open(&root)?;
            let repo_id = repository.repository_id().clone();
            if let Some(other) = served.insert(repo_id.clone(), Arc::new(repository)) {
                return Err(Error::Invalid(format!(
                    "{} and {} are the same repository {repo_id}",
                    other.root().display(),
                    root.display()
                )));
            }
        }
        Ok(Self {
            state_root: state_root.to_path_buf(),
            repositories: served,
            jobs: Jobs::new(JOBS),
            uploads: Uploads::new(state_root.to_path_buf()),
            submissions: Mutex::new(()),
            _lock: lock,
        })
    }

    pub fn repositories(&self) -> impl Iterator<Item = &ServedRepository> {
        self.repositories.values().map(Arc::as_ref)
    }

    /// Authenticate first: a request with a bad token learns nothing about
    /// which repositories exist.
    async fn authorize(
        &self,
        headers: &HeaderMap,
        repo_id: &str,
        need: Access,
    ) -> Result<Arc<ServedRepository>> {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(Error::Unauthenticated)?
            .to_string();
        let repo_id = RepoId::parse(repo_id).ok();
        let state_root = self.state_root.clone();
        let checked = repo_id.clone();
        self.jobs
            .run(move || tokens::authorize(&state_root, &presented, checked.as_ref(), need))
            .await?;
        repo_id
            .and_then(|repo_id| self.repositories.get(&repo_id).cloned())
            .ok_or(Error::NotFound)
    }

    /// Accept a verified upload, or return the receipt of its earlier
    /// acceptance. Blocking.
    fn submit(
        &self,
        repository: &ServedRepository,
        request: SubmitRequest,
        lock_tokens: Vec<String>,
    ) -> Result<SubmitResult> {
        let _one = self.submissions.lock().expect("submissions");
        if request.repository != repository.identity()? {
            return Err(synck_sync::ProtocolError::Generation.into());
        }
        let repo_id = repository.repository_id();
        let validated = validate_submit(&request)?;
        let receipt = |accepted: crate::portable::Accepted| Receipt {
            operation_id: request.operation_id.clone(),
            request_digest: validated.request_digest.clone(),
            commit_id: accepted.commit_id,
            accepted_at: accepted.accepted_at,
            published: accepted.published,
        };
        if let Some(digest) = self.uploads.submitted(repo_id, &request.operation_id)?
            && let Some(accepted) = Handle::current().block_on(repository.accepted(&request.operation_id))?
        {
            if digest != validated.request_digest {
                return Err(Error::OperationReused);
            }
            return Ok(SubmitResult::Accepted { receipt: receipt(accepted) });
        }
        let (validated, records) = self.uploads.staged(&request)?;
        let mut proposal = Proposal {
            repository_head: request.repository_head,
            operation_id: request.operation_id.clone(),
            commit_id: validated.commit_id,
            metadata: request.metadata.clone(),
            events: Vec::new(),
            ref_events: Vec::new(),
            guards: Vec::new(),
            subtree_guards: Vec::new(),
            lock_tokens,
        };
        for record in records {
            match record {
                ProposalRecord::Item(event) => proposal.events.push(event),
                ProposalRecord::Ref(event) => proposal.ref_events.push(event),
                ProposalRecord::Guard(guard) => proposal.guards.push(guard),
                ProposalRecord::SubtreeGuard(guard) => proposal.subtree_guards.push(guard),
            }
        }
        self.uploads
            .submit(repo_id, &request.operation_id, &validated.request_digest)?;
        match Handle::current().block_on(repository.accept(proposal))? {
            Acceptance::Accepted(accepted) => {
                self.uploads.remove(repo_id, &request.operation_id)?;
                Ok(SubmitResult::Accepted { receipt: receipt(accepted) })
            }
            Acceptance::Conflict(items) => Ok(SubmitResult::Conflict { items }),
            Acceptance::MissingObjects(objects) => Err(Error::ObjectsMissing(objects)),
        }
    }
}

pub fn router(server: Arc<Server>) -> Router {
    let sync = "/v1/repositories/{repo_id}/sync";
    Router::new()
        .route(&format!("{sync}/capabilities"), get(capabilities))
        .route(
            &format!("{sync}/uploads/{{operation_id}}/pages/{{offset}}"),
            put(upload_page).layer(DefaultBodyLimit::max(MAX_PAGE_BYTES)),
        )
        .route(
            &format!("{sync}/proposals"),
            post(submit).layer(DefaultBodyLimit::max(MAX_SUBMIT_BYTES)),
        )
        .route(&format!("{sync}/receipts/{{operation_id}}"), get(receipt))
        .route(
            &format!("{sync}/history"),
            post(history).layer(DefaultBodyLimit::max(MAX_SUBMIT_BYTES)),
        )
        .route(
            &format!("{sync}/leases"),
            get(leases).post(reserve).layer(DefaultBodyLimit::max(MAX_LEASE_BYTES)),
        )
        .route(&format!("{sync}/leases/renew"), post(renew))
        .route(&format!("{sync}/leases/release"), post(release))
        .route(
            &format!("{sync}/leases/break"),
            post(break_leases).layer(DefaultBodyLimit::max(MAX_LEASE_BYTES)),
        )
        .route(
            &format!("{sync}/objects"),
            post(download_objects).layer(DefaultBodyLimit::max(MAX_SUBMIT_BYTES)).put(upload_objects),
        )
        .route(
            &format!("{sync}/objects/{{object_id}}"),
            get(download_object).put(upload_object),
        )
        .fallback(|| async { Error::NotFound })
        .with_state(server)
}

fn json<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|error| Error::Invalid(error.to_string()))
}

async fn capabilities(
    State(server): State<Arc<Server>>,
    UrlPath(repo_id): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Json<Capabilities>> {
    let repository = server.authorize(&headers, &repo_id, Access::Read).await?;
    let capabilities = server
        .jobs
        .run(move || Ok(repository.capabilities()?))
        .await?;
    Ok(Json(capabilities))
}

async fn upload_page(
    State(server): State<Arc<Server>>,
    UrlPath((repo_id, operation_id, offset)): UrlPath<(String, String, u64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<PageAck>> {
    let repository = server.authorize(&headers, &repo_id, Access::Write).await?;
    let page: ProposalPage = json(&body)?;
    if page.operation_id != operation_id || page.offset != offset {
        return Err(Error::Invalid("the page names another operation or offset".into()));
    }
    let job = server.clone();
    let ack = server
        .jobs
        .run(move || job.uploads.put_page(&repository.identity()?, &page))
        .await?;
    Ok(Json(ack))
}

async fn submit(
    State(server): State<Arc<Server>>,
    UrlPath(repo_id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<SubmitResult>> {
    let repository = server.authorize(&headers, &repo_id, Access::Write).await?;
    let request: SubmitRequest = json(&body)?;
    let lock_tokens = headers
        .get_all(LOCK_TOKEN)
        .iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_string)
                .map_err(|_| Error::Invalid("a lock token is not text".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let job = server.clone();
    let result = server
        .jobs
        .run(move || job.submit(&repository, request, lock_tokens))
        .await?;
    Ok(Json(result))
}

async fn receipt(
    State(server): State<Arc<Server>>,
    UrlPath((repo_id, operation_id)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Receipt>> {
    let repository = server.authorize(&headers, &repo_id, Access::Read).await?;
    let job = server.clone();
    let receipt = server
        .jobs
        .run(move || {
            let digest = job
                .uploads
                .submitted(repository.repository_id(), &operation_id)?
                .ok_or(Error::NotFound)?;
            let accepted = Handle::current()
                .block_on(repository.accepted(&operation_id))?
                .ok_or(Error::NotFound)?;
            Ok(Receipt {
                operation_id,
                request_digest: digest,
                commit_id: accepted.commit_id,
                accepted_at: accepted.accepted_at,
                published: accepted.published,
            })
        })
        .await?;
    Ok(Json(receipt))
}

async fn history(
    State(server): State<Arc<Server>>,
    UrlPath(repo_id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<HistoryPage>> {
    let repository = server.authorize(&headers, &repo_id, Access::Read).await?;
    let request: HistoryRequest = json(&body)?;
    let page = server
        .jobs
        .run(move || {
            if request.after.completed.repository != repository.identity()? {
                return Err(ProtocolError::Generation.into());
            }
            Handle::current()
                .block_on(repository.history(&request))?
                .ok_or(Error::Protocol(ProtocolError::Checkpoint))
        })
        .await?;
    Ok(Json(page))
}

fn leased(locks: Vec<crate::AgentLock>) -> Vec<LeasedPath> {
    locks
        .into_iter()
        .map(|lock| LeasedPath {
            path: lock.path,
            owner: lock.owner,
            expires_at: lock.expires_at,
        })
        .collect()
}

async fn leases(
    State(server): State<Arc<Server>>,
    UrlPath(repo_id): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Json<Leases>> {
    let repository = server.authorize(&headers, &repo_id, Access::Read).await?;
    let locks = server.jobs.run(move || Ok(repository.reservations()?)).await?;
    Ok(Json(Leases { locks: leased(locks) }))
}

async fn reserve(
    State(server): State<Arc<Server>>,
    UrlPath(repo_id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Lease>> {
    let repository = server.authorize(&headers, &repo_id, Access::Write).await?;
    let request: LeaseRequest = json(&body)?;
    let lease = server
        .jobs
        .run(move || Ok(repository.reserve(request.paths, &request.owner, request.ttl)?))
        .await?;
    Ok(Json(Lease {
        token: lease.token,
        locks: leased(lease.locks),
    }))
}

async fn renew(
    State(server): State<Arc<Server>>,
    UrlPath(repo_id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode> {
    let repository = server.authorize(&headers, &repo_id, Access::Write).await?;
    let request: LeaseRenewal = json(&body)?;
    server
        .jobs
        .run(move || Ok(repository.renew_reservation(&request.token, request.ttl)?))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn release(
    State(server): State<Arc<Server>>,
    UrlPath(repo_id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Released>> {
    let repository = server.authorize(&headers, &repo_id, Access::Write).await?;
    let request: LeaseToken = json(&body)?;
    let released = server
        .jobs
        .run(move || Ok(repository.release_reservation(&request.token)?))
        .await?;
    Ok(Json(Released { released: released as u64 }))
}

async fn break_leases(
    State(server): State<Arc<Server>>,
    UrlPath(repo_id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Released>> {
    let repository = server.authorize(&headers, &repo_id, Access::Write).await?;
    let request: LeasePaths = json(&body)?;
    let released = server
        .jobs
        .run(move || Ok(repository.break_reservations(&request.paths)?))
        .await?;
    Ok(Json(Released { released: released as u64 }))
}

fn object_id(text: &str) -> Result<ObjectId> {
    ObjectId::parse(text).map_err(|error| Error::Invalid(error.to_string()))
}

async fn download_object(
    State(server): State<Arc<Server>>,
    UrlPath((repo_id, object_id_text)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let repository = server.authorize(&headers, &repo_id, Access::Read).await?;
    let id = object_id(&object_id_text)?;
    let (checked, checked_id) = (repository.clone(), id.clone());
    if let Some(bytes) = server.jobs.run(move || Ok(checked.metadata_object(&checked_id)?)).await? {
        return Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response());
    }
    let (checked, checked_id) = (repository.clone(), id.clone());
    if !server.jobs.run(move || Ok(checked.has_object(&checked_id)?)).await? {
        return Err(Error::ObjectMissing);
    }
    let body = server
        .jobs
        .stream(move |out| {
            repository.copy_object(&id, out)?;
            Ok(())
        })
        .await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], body).into_response())
}

/// Removes an object upload however its request ends.
struct Staged(PathBuf);

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The request body as one reader.
fn body_reader(body: Body) -> impl AsyncRead + Unpin {
    tokio_util::io::StreamReader::new(body.into_data_stream().map_err(std::io::Error::other))
}

/// Write an object upload to the state root's `tmp`, refusing bytes that
/// do not hash to `id`.
async fn stage(server: &Server, mut bytes: impl AsyncRead + Unpin, id: &ObjectId) -> Result<Staged> {
    let mut name = [0_u8; 16];
    rand::rng().fill_bytes(&mut name);
    let staged = Staged(
        server
            .state_root
            .join(UPLOADS_DIRECTORY)
            .join(format!("object-{}", hex(&name))),
    );
    let mut file = tokio::fs::File::create_new(&staged.0)
        .await
        .map_err(|error| Error::filesystem("create object upload", &staged.0, error))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let count = bytes
            .read(&mut buffer)
            .await
            .map_err(|error| Error::Invalid(format!("object upload: {error}")))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        file.write_all(&buffer[..count])
            .await
            .map_err(|error| Error::filesystem("write object upload", &staged.0, error))?;
    }
    // Read once and removed: durability comes from the repository store.
    file.flush()
        .await
        .map_err(|error| Error::filesystem("write object upload", &staged.0, error))?;
    if format!("sha256:{}", hex(&digest.finalize())) != id.as_str() {
        return Err(Error::Invalid("object bytes do not match its ID".into()));
    }
    Ok(staged)
}

async fn upload_object(
    State(server): State<Arc<Server>>,
    UrlPath((repo_id, object_id_text)): UrlPath<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode> {
    let repository = server.authorize(&headers, &repo_id, Access::Write).await?;
    let id = object_id(&object_id_text)?;
    let staged = stage(&server, body_reader(body), &id).await?;
    server
        .jobs
        .run(move || {
            repository.install_objects([(&id, staged.0.as_path())])?;
            drop(staged);
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// An object pack: entries of `PACK_HEADER_BYTES` and the object's bytes.
/// All are checked before any is stored, then stored under one lock.
async fn upload_objects(
    State(server): State<Arc<Server>>,
    UrlPath(repo_id): UrlPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode> {
    let repository = server.authorize(&headers, &repo_id, Access::Write).await?;
    let mut body = body_reader(body);
    let mut staged = Vec::new();
    let mut header = [0_u8; PACK_HEADER_BYTES];
    loop {
        let read = body
            .read(&mut header)
            .await
            .map_err(|error| Error::Invalid(format!("object pack: {error}")))?;
        if read == 0 {
            break;
        }
        body.read_exact(&mut header[read..])
            .await
            .map_err(|error| Error::Invalid(format!("object pack: {error}")))?;
        if staged.len() == MAX_PACK_OBJECTS {
            return Err(Error::Invalid(format!("an object pack holds at most {MAX_PACK_OBJECTS} objects")));
        }
        let (id, size) = header.split_at(PACK_HEADER_BYTES - 8);
        let id = object_id(std::str::from_utf8(id).map_err(|error| Error::Invalid(error.to_string()))?)?;
        let size = u64::from_be_bytes(size.try_into().expect("eight bytes"));
        let file = stage(&server, (&mut body).take(size), &id).await?;
        staged.push((id, file));
    }
    server
        .jobs
        .run(move || {
            repository.install_objects(staged.iter().map(|(id, file)| (id, file.0.as_path())))?;
            drop(staged);
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The requested objects as one pack, in order, ending after the object that
/// reaches `PACK_BYTES`. One missing object refuses the request.
async fn download_objects(
    State(server): State<Arc<Server>>,
    UrlPath(repo_id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let repository = server.authorize(&headers, &repo_id, Access::Read).await?;
    let request: ObjectRequest = json(&body)?;
    if request.objects.is_empty() || request.objects.len() > MAX_PACK_OBJECTS {
        return Err(Error::Invalid(format!("ask for 1 to {MAX_PACK_OBJECTS} objects")));
    }
    let checked = repository.clone();
    let ids = request.objects;
    let (ids, entries) = server
        .jobs
        .run(move || match checked.pack_entries(&ids)? {
            Some(entries) => Ok((ids, entries)),
            None => Err(Error::ObjectMissing),
        })
        .await?;
    let body = server
        .jobs
        .stream(move |out| Ok(repository.write_pack(&ids, entries, PACK_BYTES, out)?))
        .await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], body).into_response())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Plain HTTP on loopback only: protocol 1 requires TLS on any network, so a
/// proxy on this host terminates it.
pub async fn serve(server: Server, listen: SocketAddr) -> Result<()> {
    if !listen.ip().is_loopback() {
        return Err(Error::Invalid(format!(
            "{listen} is not a loopback address; terminate TLS in a proxy on this host"
        )));
    }
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| Error::filesystem("listen on", listen.to_string(), error))?;
    eprintln!("listening on http://{listen}");
    let server = Arc::new(server);
    let jobs = server.jobs.clone();
    axum::serve(listener, router(server))
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|error| Error::filesystem("serve on", listen.to_string(), error))?;
    jobs.close();
    jobs.wait().await;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install the SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
