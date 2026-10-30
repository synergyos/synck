use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(not(windows))]
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use synck_vcs::types::RepoId;
use uuid::Uuid;

use crate::error::{IoContext, LocalError, Result};
use crate::repository::{MONITOR_LOCK_FILE, Repository, is_link_or_reparse};

const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_CANDIDATES: usize = 65_536;
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);
/// Watchers do not always report the removal of their own root.
#[cfg(unix)]
const REPOSITORY_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const SOCKET_FILE: &str = "monitor.sock";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MonitorStatus {
    Running { instance_id: Uuid },
    Stopped,
    Unavailable { detail: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum QueryResult {
    Reset {
        instance_id: Uuid,
        sequence: i64,
    },
    Changes {
        instance_id: Uuid,
        sequence: i64,
        paths: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Operation {
    Query,
    Status,
    Stop,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    operation: Operation,
    repo_id: String,
    instance_id: Option<Uuid>,
    sequence: Option<i64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ResponseKind {
    Reset,
    Changes,
    Running,
    Stopping,
    Error,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u32,
    kind: ResponseKind,
    instance_id: Option<Uuid>,
    sequence: Option<i64>,
    paths: Option<Vec<String>>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MonitorConfig {
    #[serde(default, rename = "storage_mode")]
    _storage_mode: Option<crate::StorageMode>,
    format_version: u32,
    repo_id: RepoId,
    author: String,
    #[serde(default)]
    authority: Option<crate::portable::Authority>,
}

#[derive(Debug)]
struct EventHistory {
    instance_id: Uuid,
    sequence: i64,
    reset_floor: i64,
    retained_paths: usize,
    events: VecDeque<(i64, Vec<String>)>,
}

impl EventHistory {
    fn new() -> Self {
        Self {
            instance_id: Uuid::new_v4(),
            sequence: 0,
            reset_floor: 0,
            retained_paths: 0,
            events: VecDeque::new(),
        }
    }

    fn record(&mut self, paths: Vec<String>) {
        if self.sequence == i64::MAX {
            *self = Self::new();
        }
        self.sequence += 1;
        self.retained_paths += paths.len();
        self.events.push_back((self.sequence, paths));
        while self.retained_paths > MAX_CANDIDATES {
            if let Some((sequence, paths)) = self.events.pop_front() {
                self.retained_paths -= paths.len();
                self.reset_floor = sequence;
            }
        }
    }

    fn invalidate(&mut self) {
        if self.sequence == i64::MAX {
            *self = Self::new();
            return;
        }
        self.sequence += 1;
        self.events.clear();
        self.retained_paths = 0;
        self.reset_floor = self.sequence;
    }

    fn query(&self, instance_id: Option<Uuid>, sequence: Option<i64>) -> QueryResult {
        let valid = instance_id == Some(self.instance_id)
            && sequence.is_some_and(|value| {
                value >= self.reset_floor && value <= self.sequence && value >= 0
            });
        if !valid {
            return QueryResult::Reset {
                instance_id: self.instance_id,
                sequence: self.sequence,
            };
        }
        let sequence = sequence.expect("validated monitor sequence");
        let mut paths = BTreeSet::new();
        for (_, event_paths) in self.events.iter().filter(|(event, _)| *event > sequence) {
            paths.extend(event_paths.iter().cloned());
            if paths.len() > MAX_CANDIDATES {
                return QueryResult::Reset {
                    instance_id: self.instance_id,
                    sequence: self.sequence,
                };
            }
        }
        QueryResult::Changes {
            instance_id: self.instance_id,
            sequence: self.sequence,
            paths: paths.into_iter().collect(),
        }
    }
}

impl Repository {
    pub fn monitor_status(&self) -> MonitorStatus {
        match status_request(self.metadata_path(), self.repository_id()) {
            Ok(status) => status,
            Err(LocalError::MonitorUnavailable(detail)) => MonitorStatus::Unavailable { detail },
            Err(error) => MonitorStatus::Unavailable {
                detail: error.to_string(),
            },
        }
    }

    pub fn start_monitor(&self, executable: &Path) -> Result<MonitorStatus> {
        // A checkout can be deeper than the Unix socket address limit. Refuse
        // before spawning: the scanner can fall back immediately instead of
        // waiting for a server that cannot bind its endpoint.
        #[cfg(unix)]
        rustix::net::SocketAddrUnix::new(self.metadata_path().join(SOCKET_FILE))
            .map_err(std::io::Error::from)
            .at("address monitor connection", self.metadata_path().join(SOCKET_FILE))?;
        if let MonitorStatus::Running { instance_id } = self.monitor_status() {
            return Ok(MonitorStatus::Running { instance_id });
        }
        spawn_server(executable, self.repository_root(), self.repository_id())?;
        let deadline = Instant::now() + REQUEST_DEADLINE;
        loop {
            if let Ok(MonitorStatus::Running { instance_id }) =
                status_request(self.metadata_path(), self.repository_id())
            {
                return Ok(MonitorStatus::Running { instance_id });
            }
            if Instant::now() >= deadline {
                return Err(LocalError::MonitorUnavailable(
                    "monitor did not become ready within two seconds".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn stop_monitor(&self) -> Result<MonitorStatus> {
        let instance_id = match status_request(self.metadata_path(), self.repository_id())? {
            MonitorStatus::Running { instance_id } => instance_id,
            MonitorStatus::Stopped => return Ok(MonitorStatus::Stopped),
            MonitorStatus::Unavailable { detail } => {
                return Err(LocalError::MonitorUnavailable(detail));
            }
        };
        let request = Request {
            version: PROTOCOL_VERSION,
            operation: Operation::Stop,
            repo_id: self.repository_id().as_str().to_string(),
            instance_id: Some(instance_id),
            sequence: None,
        };
        let response = exchange(self.metadata_path(), &request)?;
        validate_common_response(&response)?;
        if response.kind != ResponseKind::Stopping || response.instance_id != Some(instance_id) {
            return Err(LocalError::MonitorUnavailable(
                "monitor returned an invalid stop response".to_string(),
            ));
        }
        Ok(MonitorStatus::Stopped)
    }

    pub(crate) fn query_monitor(
        &self,
        token: Option<&crate::worktree::MonitorToken>,
    ) -> Result<QueryResult> {
        let request = Request {
            version: PROTOCOL_VERSION,
            operation: Operation::Query,
            repo_id: self.repository_id().as_str().to_string(),
            instance_id: token.map(|token| token.instance_id),
            sequence: token.map(|token| token.sequence),
        };
        let response = exchange(self.metadata_path(), &request)?;
        validate_common_response(&response)?;
        let instance_id = response.instance_id.ok_or_else(|| {
            LocalError::MonitorUnavailable("monitor omitted its instance ID".to_string())
        })?;
        let sequence = response
            .sequence
            .filter(|value| *value >= 0)
            .ok_or_else(|| {
                LocalError::MonitorUnavailable("monitor returned an invalid sequence".to_string())
            })?;
        match response.kind {
            ResponseKind::Reset if response.paths.is_none() => Ok(QueryResult::Reset {
                instance_id,
                sequence,
            }),
            ResponseKind::Changes => {
                let paths = response.paths.ok_or_else(|| {
                    LocalError::MonitorUnavailable("monitor omitted candidate paths".to_string())
                })?;
                if paths.len() > MAX_CANDIDATES
                    || !paths.iter().all(|path| valid_relative_path(path))
                    || !paths.windows(2).all(|pair| pair[0] < pair[1])
                {
                    return Err(LocalError::MonitorUnavailable(
                        "monitor returned invalid candidate paths".to_string(),
                    ));
                }
                Ok(QueryResult::Changes {
                    instance_id,
                    sequence,
                    paths,
                })
            }
            _ => Err(LocalError::MonitorUnavailable(
                "monitor returned an invalid query response".to_string(),
            )),
        }
    }
}

fn status_request(metadata_path: &Path, repo_id: &RepoId) -> Result<MonitorStatus> {
    let request = Request {
        version: PROTOCOL_VERSION,
        operation: Operation::Status,
        repo_id: repo_id.as_str().to_string(),
        instance_id: None,
        sequence: None,
    };
    match exchange(metadata_path, &request) {
        Ok(response) => {
            validate_common_response(&response)?;
            if response.kind == ResponseKind::Running
                && response.sequence.is_some_and(|sequence| sequence >= 0)
                && response.paths.is_none()
                && response.error.is_none()
            {
                Ok(MonitorStatus::Running {
                    instance_id: response.instance_id.ok_or_else(|| {
                        LocalError::MonitorUnavailable(
                            "monitor omitted its instance ID".to_string(),
                        )
                    })?,
                })
            } else {
                Err(LocalError::MonitorUnavailable(
                    "monitor returned an invalid status response".to_string(),
                ))
            }
        }
        Err(LocalError::Io { source, .. })
            if matches!(
                source.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(MonitorStatus::Stopped)
        }
        Err(error) => Err(error),
    }
}

fn validate_common_response(response: &Response) -> Result<()> {
    if response.version != PROTOCOL_VERSION {
        return Err(LocalError::MonitorUnavailable(
            "unsupported monitor protocol version".to_string(),
        ));
    }
    if response.kind == ResponseKind::Error {
        return Err(LocalError::MonitorUnavailable(
            response
                .error
                .clone()
                .unwrap_or_else(|| "monitor reported an unspecified error".to_string()),
        ));
    }
    if response.error.is_some() {
        return Err(LocalError::MonitorUnavailable(
            "monitor returned an unexpected error field".to_string(),
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn spawn_server(executable: &Path, root: &Path, repo_id: &RepoId) -> Result<()> {
    let mut command = Command::new(executable);
    command
        .arg("__monitor-server")
        .arg("--root")
        .arg(root)
        .arg("--repo-id")
        .arg(repo_id.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("DYLD_FALLBACK_LIBRARY_PATH");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and the closure touches nothing else.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn().map(|_| ()).at("start monitor", executable)
}

#[cfg(windows)]
fn spawn_server(executable: &Path, root: &Path, repo_id: &RepoId) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    };

    let application = executable
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut command_line = wide_string(&format!(
        "synck __monitor-server --root . --repo-id {}",
        repo_id.as_str()
    ));
    let current_directory = root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb =
        u32::try_from(std::mem::size_of::<STARTUPINFOW>()).expect("STARTUPINFOW size fits u32");
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS,
            std::ptr::null(),
            current_directory.as_ptr(),
            &startup,
            &mut process,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).at("start monitor", executable);
    }
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

pub fn run_monitor_server(root: &Path, expected_repo_id: &str) -> Result<()> {
    let root = root
        .canonicalize()
        .at("resolve monitor repository root", root)?;
    let metadata_path = root.join(".synck");
    let metadata = fs::symlink_metadata(&metadata_path).at("inspect", &metadata_path)?;
    if !metadata.file_type().is_dir() || is_link_or_reparse(&metadata) {
        return Err(LocalError::MonitorUnavailable(
            "repository metadata is not a real directory".to_string(),
        ));
    }
    let config_path = metadata_path.join("config.json");
    let config_file = crate::repository::open_regular_file_nofollow(&config_path)?;
    let config: MonitorConfig = serde_json::from_reader(config_file)?;
    let complete_checkout = config.format_version == crate::portable::FORMAT_VERSION
        && crate::checkout::read_checkout(&metadata_path)?.is_some()
        && config
            .authority
            .is_some_and(|authority| authority != crate::portable::Authority::Follower);
    if !complete_checkout || config.author.is_empty() || config.repo_id.as_str() != expected_repo_id
    {
        return Err(LocalError::MonitorUnavailable(
            "repository config does not match monitor invocation".to_string(),
        ));
    }
    run_platform_server(&root, &metadata_path, &config.repo_id)
}

fn process_request(request: Request, repo_id: &RepoId, history: &EventHistory) -> (Response, bool) {
    let error = |message: &str| {
        (
            Response {
                version: PROTOCOL_VERSION,
                kind: ResponseKind::Error,
                instance_id: Some(history.instance_id),
                sequence: Some(history.sequence),
                paths: None,
                error: Some(message.to_string()),
            },
            false,
        )
    };
    if request.version != PROTOCOL_VERSION || request.repo_id != repo_id.as_str() {
        return error("protocol version or repository ID mismatch");
    }
    match request.operation {
        Operation::Status if request.instance_id.is_none() && request.sequence.is_none() => (
            Response {
                version: PROTOCOL_VERSION,
                kind: ResponseKind::Running,
                instance_id: Some(history.instance_id),
                sequence: Some(history.sequence),
                paths: None,
                error: None,
            },
            false,
        ),
        Operation::Query
            if (request.instance_id.is_none() && request.sequence.is_none())
                || (request.instance_id.is_some()
                    && request.sequence.is_some_and(|sequence| sequence >= 0)) =>
        {
            let query = history.query(request.instance_id, request.sequence);
            let response = match query {
                QueryResult::Reset {
                    instance_id,
                    sequence,
                } => Response {
                    version: PROTOCOL_VERSION,
                    kind: ResponseKind::Reset,
                    instance_id: Some(instance_id),
                    sequence: Some(sequence),
                    paths: None,
                    error: None,
                },
                QueryResult::Changes {
                    instance_id,
                    sequence,
                    paths,
                } => Response {
                    version: PROTOCOL_VERSION,
                    kind: ResponseKind::Changes,
                    instance_id: Some(instance_id),
                    sequence: Some(sequence),
                    paths: Some(paths),
                    error: None,
                },
            };
            (response, false)
        }
        Operation::Stop
            if request.instance_id == Some(history.instance_id) && request.sequence.is_none() =>
        {
            (
                Response {
                    version: PROTOCOL_VERSION,
                    kind: ResponseKind::Stopping,
                    instance_id: Some(history.instance_id),
                    sequence: Some(history.sequence),
                    paths: None,
                    error: None,
                },
                true,
            )
        }
        _ => error("invalid fields for monitor operation"),
    }
}

fn bounded_response(response: Response, history: &EventHistory) -> Response {
    if response.kind == ResponseKind::Changes
        && serde_json::to_vec(&response).is_ok_and(|payload| payload.len() > MAX_FRAME_BYTES)
    {
        Response {
            version: PROTOCOL_VERSION,
            kind: ResponseKind::Reset,
            instance_id: Some(history.instance_id),
            sequence: Some(history.sequence),
            paths: None,
            error: None,
        }
    } else {
        response
    }
}

fn process_event(event: notify::Result<Event>, root: &Path, metadata: &Path) -> EventDisposition {
    let event = match event {
        Ok(event) if !event.need_rescan() => event,
        _ => return EventDisposition::Invalidate,
    };
    let terminal_kind = matches!(
        event.kind,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    );
    if terminal_kind
        && event
            .paths
            .iter()
            .any(|path| path == root || path == metadata)
    {
        return EventDisposition::Stop;
    }
    if matches!(
        event.kind,
        EventKind::Modify(ModifyKind::Name(RenameMode::Both))
    ) && event.paths.len() != 2
    {
        return EventDisposition::Invalidate;
    }
    if matches!(event.kind, EventKind::Modify(ModifyKind::Name(_))) && event.paths.len() < 2 {
        return EventDisposition::Invalidate;
    }
    if matches!(event.kind, EventKind::Other | EventKind::Any) || event.paths.is_empty() {
        return EventDisposition::Invalidate;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return EventDisposition::Ignore;
    }
    let mut paths = BTreeSet::new();
    for path in event.paths {
        let relative = match path.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_) => return EventDisposition::Invalidate,
        };
        let display = match display_relative_path(relative) {
            Some(path) => path,
            None if relative.as_os_str().is_empty() => return EventDisposition::Invalidate,
            None => return EventDisposition::Invalidate,
        };
        if display == ".synck" || display.starts_with(".synck/") {
            continue;
        }
        paths.insert(display);
    }
    if paths.is_empty() {
        EventDisposition::Ignore
    } else {
        EventDisposition::Paths(paths.into_iter().collect())
    }
}

enum EventDisposition {
    Paths(Vec<String>),
    Invalidate,
    Ignore,
    Stop,
}

fn display_relative_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        parts.push(name.to_str()?.to_string());
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && !path.starts_with('/')
        && !path.ends_with('/')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && path != ".synck"
        && !path.starts_with(".synck/")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PathIdentity {
    device: u64,
    file: u64,
}

#[cfg(unix)]
fn path_identity(path: &Path) -> Result<PathIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path).at("inspect monitor root", path)?;
    if !metadata.file_type().is_dir() || is_link_or_reparse(&metadata) {
        return Err(LocalError::MonitorUnavailable(format!(
            "monitored directory was replaced: {}",
            path.display()
        )));
    }
    Ok(PathIdentity {
        device: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
fn path_identity(path: &Path) -> Result<PathIdentity> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        GetFileInformationByHandle,
    };

    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .at("open monitor root", path)?;
    let metadata = directory.metadata().at("inspect monitor root", path)?;
    if !metadata.file_type().is_dir() || is_link_or_reparse(&metadata) {
        return Err(LocalError::MonitorUnavailable(format!(
            "monitored directory was replaced: {}",
            path.display()
        )));
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(directory.as_raw_handle() as _, &mut info) } == 0 {
        return Err(LocalError::MonitorUnavailable(format!(
            "cannot read monitored directory identity: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(PathIdentity {
        device: u64::from(info.dwVolumeSerialNumber),
        file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

#[cfg(unix)]
fn run_platform_server(root: &Path, metadata_path: &Path, repo_id: &RepoId) -> Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::net::UnixListener;

    let root_identity = path_identity(root)?;
    let metadata_identity = path_identity(metadata_path)?;
    let lock_path = metadata_path.join(MONITOR_LOCK_FILE);
    let lock = crate::repository::open_regular_file_nofollow(&lock_path)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(LocalError::MonitorUnavailable(
                "repository monitor is already running".to_string(),
            ));
        }
        Err(std::fs::TryLockError::Error(source)) => {
            return Err(LocalError::Io {
                action: "lock monitor",
                path: lock_path,
                source,
            });
        }
    }

    let socket_path = metadata_path.join(SOCKET_FILE);
    match fs::symlink_metadata(&socket_path) {
        Ok(entry) if entry.file_type().is_socket() => {
            fs::remove_file(&socket_path).at("remove stale monitor socket", &socket_path)?;
        }
        Ok(_) => {
            return Err(LocalError::MonitorUnavailable(format!(
                "monitor endpoint is not a socket: {}",
                socket_path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect monitor socket",
                path: socket_path,
                source,
            });
        }
    }
    let listener = UnixListener::bind(&socket_path).at("bind monitor socket", &socket_path)?;
    let bound = fs::symlink_metadata(&socket_path).at("inspect monitor socket", &socket_path)?;
    let bound_identity = (bound.dev(), bound.ino());
    let cleanup = SocketCleanup {
        path: socket_path.clone(),
        identity: bound_identity,
    };
    listener
        .set_nonblocking(true)
        .at("configure monitor socket", &socket_path)?;

    let (sender, receiver) = mpsc::channel();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .map_err(|error| LocalError::MonitorUnavailable(format!("cannot create watcher: {error}")))?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|error| {
            LocalError::MonitorUnavailable(format!("cannot watch repository: {error}"))
        })?;

    let repository_replaced = || {
        path_identity(root).ok() != Some(root_identity)
            || path_identity(metadata_path).ok() != Some(metadata_identity)
    };
    let mut history = EventHistory::new();
    let mut checked_at = Instant::now();
    let mut stop = false;
    while !stop {
        while let Ok(event) = receiver.try_recv() {
            match process_event(event, root, metadata_path) {
                EventDisposition::Paths(paths) => history.record(paths),
                EventDisposition::Invalidate => history.invalidate(),
                EventDisposition::Ignore => {}
                EventDisposition::Stop => {
                    stop = true;
                    break;
                }
            }
        }
        if stop {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                if repository_replaced() {
                    stop = true;
                    continue;
                }
                if !same_user_peer(&stream) {
                    continue;
                }
                if stream.set_nonblocking(true).is_err() {
                    continue;
                }
                let deadline = Instant::now() + REQUEST_DEADLINE;
                let request: Request = match read_frame(&mut stream, deadline) {
                    Ok(request) => request,
                    Err(_) => continue,
                };
                if matches!(request.operation, Operation::Query) {
                    match synchronize_watcher(
                        &receiver,
                        root,
                        metadata_path,
                        deadline,
                        &mut history,
                    ) {
                        Ok(false) => {}
                        Ok(true) => {
                            stop = true;
                            continue;
                        }
                        Err(_) => continue,
                    }
                }
                let (response, requested_stop) = process_request(request, repo_id, &history);
                let response = bounded_response(response, &history);
                if write_frame(&mut stream, &response, deadline).is_ok() && requested_stop {
                    stop = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                match receiver.recv_timeout(Duration::from_millis(25)) {
                    Ok(event) => match process_event(event, root, metadata_path) {
                        EventDisposition::Paths(paths) => history.record(paths),
                        EventDisposition::Invalidate => history.invalidate(),
                        EventDisposition::Ignore => {}
                        EventDisposition::Stop => stop = true,
                    },
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => stop = true,
                }
                if checked_at.elapsed() >= REPOSITORY_CHECK_INTERVAL {
                    checked_at = Instant::now();
                    stop |= repository_replaced();
                }
            }
            Err(source) => {
                return Err(LocalError::Io {
                    action: "accept monitor connection",
                    path: socket_path,
                    source,
                });
            }
        }
    }
    drop(watcher);
    drop(listener);
    drop(cleanup);
    drop(lock);
    Ok(())
}

#[cfg(any(unix, windows))]
fn synchronize_watcher(
    receiver: &mpsc::Receiver<notify::Result<Event>>,
    root: &Path,
    metadata_path: &Path,
    deadline: Instant,
    history: &mut EventHistory,
) -> Result<bool> {
    let cookie_path = metadata_path.join(format!("monitor-cookie-{}", Uuid::new_v4()));
    let cookie = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&cookie_path)
        .at("create monitor cookie", &cookie_path)?;
    drop(cookie);
    let _cleanup = CookieCleanup(cookie_path.clone());
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(LocalError::MonitorUnavailable(
                "filesystem watcher did not reach the query boundary".to_string(),
            ));
        }
        let event = receiver
            .recv_timeout(deadline - now)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => LocalError::MonitorUnavailable(
                    "filesystem watcher did not reach the query boundary".to_string(),
                ),
                mpsc::RecvTimeoutError::Disconnected => {
                    LocalError::MonitorUnavailable("filesystem watcher stopped".to_string())
                }
            })?;
        let cookie_seen = event
            .as_ref()
            .is_ok_and(|event| event.paths.iter().any(|path| path == &cookie_path));
        match process_event(event, root, metadata_path) {
            EventDisposition::Paths(paths) => history.record(paths),
            EventDisposition::Invalidate => history.invalidate(),
            EventDisposition::Ignore => {}
            EventDisposition::Stop => return Ok(true),
        }
        if cookie_seen {
            return Ok(false);
        }
    }
}

#[cfg(any(unix, windows))]
struct CookieCleanup(PathBuf);

#[cfg(any(unix, windows))]
impl Drop for CookieCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
struct SocketCleanup {
    path: PathBuf,
    identity: (u64, u64),
}

#[cfg(unix)]
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && (metadata.dev(), metadata.ino()) == self.identity
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(target_os = "linux")]
fn same_user_peer(stream: &std::os::unix::net::UnixStream) -> bool {
    rustix::net::sockopt::socket_peercred(stream)
        .is_ok_and(|credentials| credentials.uid == rustix::process::geteuid())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn same_user_peer(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: both outputs are live integers and `stream` keeps the descriptor open.
    unsafe {
        libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == libc::geteuid()
    }
}

#[cfg(unix)]
fn exchange(metadata_path: &Path, request: &Request) -> Result<Response> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;

    let socket_path = metadata_path.join(SOCKET_FILE);
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let socket = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
        .map_err(std::io::Error::from)
        .at("create monitor connection", &socket_path)?;
    // SAFETY: `into_raw_fd` hands over sole ownership of this open socket.
    let mut stream = unsafe { UnixStream::from_raw_fd(socket.into_raw_fd()) };
    stream
        .set_nonblocking(true)
        .at("configure monitor connection", &socket_path)?;
    // SAFETY: F_GETFD only reads flags from a descriptor `stream` keeps open.
    let descriptor_flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFD) };
    if descriptor_flags < 0
        // SAFETY: F_SETFD only sets flags on a descriptor `stream` keeps open.
        || unsafe {
            libc::fcntl(
                stream.as_raw_fd(),
                libc::F_SETFD,
                descriptor_flags | libc::FD_CLOEXEC,
            )
        } < 0
    {
        return Err(std::io::Error::last_os_error())
            .at("configure monitor connection", &socket_path);
    }
    let address = SocketAddrUnix::new(&socket_path)
        .map_err(std::io::Error::from)
        .at("address monitor connection", &socket_path)?;
    match rustix::net::connect(&stream, &address) {
        Ok(()) => {}
        Err(rustix::io::Errno::INPROGRESS | rustix::io::Errno::WOULDBLOCK) => {
            wait_for_unix_connection(&stream, deadline, &socket_path)?;
        }
        Err(error) => {
            return Err(std::io::Error::from(error)).at("connect monitor", &socket_path);
        }
    }
    if !same_user_peer(&stream) {
        return Err(LocalError::MonitorUnavailable(
            "monitor peer belongs to another user".to_string(),
        ));
    }
    write_frame(&mut stream, request, deadline)?;
    read_frame(&mut stream, deadline)
}

#[cfg(unix)]
fn wait_for_unix_connection(
    socket: &std::os::unix::net::UnixStream,
    deadline: Instant,
    socket_path: &Path,
) -> Result<()> {
    use std::os::fd::AsRawFd;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(LocalError::MonitorUnavailable(
                "monitor request timed out".to_string(),
            ));
        }
        let remaining = deadline - now;
        let timeout_ms = i32::try_from(remaining.as_millis().saturating_add(1)).unwrap_or(i32::MAX);
        let mut descriptor = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: `descriptor` is one live pollfd for a socket that stays open here.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result == 0 {
            return Err(LocalError::MonitorUnavailable(
                "monitor request timed out".to_string(),
            ));
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(LocalError::Io {
                action: "connect monitor",
                path: socket_path.to_path_buf(),
                source: error,
            });
        }
        match rustix::net::sockopt::socket_error(socket).map_err(std::io::Error::from) {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => {
                return Err(LocalError::Io {
                    action: "connect monitor",
                    path: socket_path.to_path_buf(),
                    source: std::io::Error::from(error),
                });
            }
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect monitor connection",
                    path: socket_path.to_path_buf(),
                    source,
                });
            }
        }
    }
}

#[cfg(windows)]
fn run_platform_server(root: &Path, metadata_path: &Path, repo_id: &RepoId) -> Result<()> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING};
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, DisconnectNamedPipe};

    let root_identity = path_identity(root)?;
    let metadata_identity = path_identity(metadata_path)?;
    let lock_path = metadata_path.join(MONITOR_LOCK_FILE);
    let lock = crate::repository::open_regular_file_nofollow(&lock_path)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(LocalError::MonitorUnavailable(
                "repository monitor is already running".to_string(),
            ));
        }
        Err(std::fs::TryLockError::Error(source)) => {
            return Err(LocalError::Io {
                action: "lock monitor",
                path: lock_path,
                source,
            });
        }
    }

    let pipe_name = windows_pipe_name(metadata_path, repo_id);
    let (sender, receiver) = mpsc::channel();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .map_err(|error| LocalError::MonitorUnavailable(format!("cannot create watcher: {error}")))?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|error| {
            LocalError::MonitorUnavailable(format!("cannot watch repository: {error}"))
        })?;

    let mut history = EventHistory::new();
    let mut stop = false;
    while !stop {
        let pipe = create_windows_pipe(&pipe_name)?;
        loop {
            while let Ok(event) = receiver.try_recv() {
                match process_event(event, root, metadata_path) {
                    EventDisposition::Paths(paths) => history.record(paths),
                    EventDisposition::Invalidate => history.invalidate(),
                    EventDisposition::Ignore => {}
                    EventDisposition::Stop => {
                        stop = true;
                        break;
                    }
                }
            }
            if stop {
                break;
            }
            let connected = unsafe { ConnectNamedPipe(pipe.0, std::ptr::null_mut()) } != 0;
            if connected {
                break;
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error().map(|code| code as u32) {
                Some(ERROR_PIPE_CONNECTED) => break,
                Some(ERROR_PIPE_LISTENING) => {
                    match receiver.recv_timeout(Duration::from_millis(25)) {
                        Ok(event) => match process_event(event, root, metadata_path) {
                            EventDisposition::Paths(paths) => history.record(paths),
                            EventDisposition::Invalidate => history.invalidate(),
                            EventDisposition::Ignore => {}
                            EventDisposition::Stop => stop = true,
                        },
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => stop = true,
                    }
                }
                _ => {
                    return Err(LocalError::Io {
                        action: "accept monitor connection",
                        path: PathBuf::from(&pipe_name),
                        source: error,
                    });
                }
            }
        }
        if stop {
            break;
        }

        if path_identity(root).ok() != Some(root_identity)
            || path_identity(metadata_path).ok() != Some(metadata_identity)
        {
            break;
        }
        if !same_windows_user_client(pipe.0) {
            unsafe { DisconnectNamedPipe(pipe.0) };
            continue;
        }

        let mut stream = unsafe { std::fs::File::from_raw_handle(pipe.into_raw() as _) };
        let deadline = Instant::now() + REQUEST_DEADLINE;
        let request: Request = match read_frame(&mut stream, deadline) {
            Ok(request) => request,
            Err(_) => {
                unsafe { DisconnectNamedPipe(stream.as_raw_handle() as _) };
                continue;
            }
        };
        if matches!(request.operation, Operation::Query) {
            match synchronize_watcher(&receiver, root, metadata_path, deadline, &mut history) {
                Ok(false) => {}
                Ok(true) => {
                    stop = true;
                    unsafe { DisconnectNamedPipe(stream.as_raw_handle() as _) };
                    continue;
                }
                Err(_) => {
                    unsafe { DisconnectNamedPipe(stream.as_raw_handle() as _) };
                    continue;
                }
            }
        }
        let (response, requested_stop) = process_request(request, repo_id, &history);
        let response = bounded_response(response, &history);
        let wrote_response = write_frame(&mut stream, &response, deadline).is_ok();
        if wrote_response && requested_stop {
            stop = true;
        }
        if !wrote_response {
            unsafe { DisconnectNamedPipe(stream.as_raw_handle() as _) };
        }
    }
    drop(watcher);
    drop(lock);
    Ok(())
}

#[cfg(windows)]
fn exchange(metadata_path: &Path, request: &Request) -> Result<Response> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
    use windows_sys::Win32::System::Pipes::{
        PIPE_READMODE_BYTE, PIPE_WAIT, SetNamedPipeHandleState,
    };

    let repo_id = RepoId::parse(request.repo_id.clone()).map_err(|error| {
        LocalError::MonitorUnavailable(format!("invalid repository ID for monitor: {error}"))
    })?;
    let pipe_name = windows_pipe_name(metadata_path, &repo_id);
    let wide = wide_string(&pipe_name);
    let deadline = Instant::now() + REQUEST_DEADLINE;
    let handle = loop {
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            break handle;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error().map(|code| code as u32) == Some(ERROR_FILE_NOT_FOUND) {
            return Err(LocalError::Io {
                action: "connect monitor",
                path: PathBuf::from(&pipe_name),
                source: error,
            });
        }
        if Instant::now() >= deadline
            || error.raw_os_error().map(|code| code as u32) != Some(ERROR_PIPE_BUSY)
        {
            return Err(LocalError::Io {
                action: "connect monitor",
                path: PathBuf::from(&pipe_name),
                source: error,
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let handle = WindowsHandle(handle);
    validate_windows_server_user(handle.0)?;
    let mode = PIPE_READMODE_BYTE | PIPE_WAIT;
    if unsafe { SetNamedPipeHandleState(handle.0, &mode, std::ptr::null(), std::ptr::null()) } == 0
    {
        return Err(std::io::Error::last_os_error()).at("configure monitor pipe", &pipe_name);
    }
    let mut stream = unsafe { std::fs::File::from_raw_handle(handle.into_raw() as _) };
    write_frame(&mut stream, request, deadline)?;
    read_frame(&mut stream, deadline)
}

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(windows)]
struct WindowsHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl WindowsHandle {
    fn into_raw(self) -> windows_sys::Win32::Foundation::HANDLE {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }
}

#[cfg(windows)]
impl Drop for WindowsHandle {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
fn windows_pipe_name(metadata_path: &Path, repo_id: &RepoId) -> String {
    use sha2::Digest;
    use std::os::windows::ffi::OsStrExt;

    let mut hasher = sha2::Sha256::new();
    for unit in metadata_path.as_os_str().encode_wide() {
        hasher.update(unit.to_le_bytes());
    }
    hasher.update([0, 0]);
    hasher.update(repo_id.as_str().as_bytes());
    format!(r"\\.\pipe\synck-{}", hex::encode(hasher.finalize()))
}

#[cfg(windows)]
fn wide_string(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn create_windows_pipe(name: &str) -> Result<WindowsHandle> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_NOWAIT, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE,
    };

    let security = WindowsPipeSecurity::current_user()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())
            .expect("SECURITY_ATTRIBUTES fits u32"),
        lpSecurityDescriptor: security.descriptor,
        bInheritHandle: 0,
    };
    let wide = wide_string(name);
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            u32::try_from(MAX_FRAME_BYTES + 4).expect("frame limit fits u32"),
            u32::try_from(MAX_FRAME_BYTES + 4).expect("frame limit fits u32"),
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error()).at("create monitor pipe", name);
    }
    Ok(WindowsHandle(handle))
}

#[cfg(windows)]
struct WindowsPipeSecurity {
    descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
}

#[cfg(windows)]
impl WindowsPipeSecurity {
    fn current_user() -> Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        };

        let sid = current_process_sid()?;
        let mut sid_string = std::ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid.as_ptr(), &mut sid_string) } == 0 {
            return Err(std::io::Error::last_os_error()).at("format monitor user SID", "monitor");
        }
        let sid_text = unsafe {
            let mut length = 0;
            while *sid_string.add(length) != 0 {
                length += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(sid_string, length))
        };
        unsafe { windows_sys::Win32::Foundation::LocalFree(sid_string.cast()) };
        let sddl = wide_string(&format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid_text})"));
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .at("create monitor security descriptor", "monitor");
        }
        Ok(Self { descriptor })
    }
}

#[cfg(windows)]
impl Drop for WindowsPipeSecurity {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.descriptor.cast()) };
    }
}

#[cfg(windows)]
struct OwnedSid {
    storage: Vec<usize>,
}

#[cfg(windows)]
impl OwnedSid {
    fn copy_from(sid: windows_sys::Win32::Security::PSID) -> Result<Self> {
        use windows_sys::Win32::Security::GetLengthSid;

        let length = unsafe { GetLengthSid(sid) };
        if length == 0 {
            return Err(std::io::Error::last_os_error()).at("read process SID length", "monitor");
        }
        let length = usize::try_from(length).expect("SID length fits usize");
        let words = length.div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0_usize; words];
        unsafe {
            std::ptr::copy_nonoverlapping(
                sid.cast::<u8>(),
                storage.as_mut_ptr().cast::<u8>(),
                length,
            );
        }
        Ok(Self { storage })
    }

    fn as_ptr(&self) -> windows_sys::Win32::Security::PSID {
        self.storage.as_ptr().cast_mut().cast()
    }
}

#[cfg(windows)]
fn current_process_sid() -> Result<OwnedSid> {
    let process = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() };
    process_sid(process)
}

#[cfg(windows)]
fn process_sid(process: windows_sys::Win32::Foundation::HANDLE) -> Result<OwnedSid> {
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::OpenProcessToken;

    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).at("open process token", "monitor");
    }
    let token = WindowsHandle(token);
    let mut required = 0_u32;
    unsafe {
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(std::io::Error::last_os_error()).at("size process SID", "monitor");
    }
    let required_size = usize::try_from(required).expect("token information size fits usize");
    let words = required_size.div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).at("read process SID", "monitor");
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    OwnedSid::copy_from(user.User.Sid)
}

#[cfg(windows)]
fn validate_windows_server_user(pipe: windows_sys::Win32::Foundation::HANDLE) -> Result<()> {
    use windows_sys::Win32::Security::EqualSid;
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let mut pid = 0_u32;
    if unsafe { GetNamedPipeServerProcessId(pipe, &mut pid) } == 0 || pid == 0 {
        return Err(std::io::Error::last_os_error()).at("identify monitor server", "monitor");
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error()).at("open monitor server", "monitor");
    }
    let process = WindowsHandle(process);
    let server_sid = process_sid(process.0)?;
    let client_sid = current_process_sid()?;
    if unsafe { EqualSid(server_sid.as_ptr(), client_sid.as_ptr()) } == 0 {
        return Err(LocalError::MonitorUnavailable(
            "monitor server belongs to another user".to_string(),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn same_windows_user_client(pipe: windows_sys::Win32::Foundation::HANDLE) -> bool {
    use windows_sys::Win32::Security::EqualSid;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let mut pid = 0_u32;
    if unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) } == 0 || pid == 0 {
        return false;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let process = WindowsHandle(process);
    let Ok(client_sid) = process_sid(process.0) else {
        return false;
    };
    let Ok(server_sid) = current_process_sid() else {
        return false;
    };
    unsafe { EqualSid(client_sid.as_ptr(), server_sid.as_ptr()) != 0 }
}

trait MonitorIo: Read + Write {
    fn set_read_limit(&self, timeout: Duration) -> std::io::Result<()>;
    fn set_write_limit(&self, timeout: Duration) -> std::io::Result<()>;
}

#[cfg(unix)]
impl MonitorIo for std::os::unix::net::UnixStream {
    fn set_read_limit(&self, _timeout: Duration) -> std::io::Result<()> {
        Ok(())
    }

    fn set_write_limit(&self, _timeout: Duration) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
impl MonitorIo for std::fs::File {
    fn set_read_limit(&self, timeout: Duration) -> std::io::Result<()> {
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;

        let deadline = Instant::now() + timeout;
        loop {
            let mut available = 0_u32;
            if unsafe {
                PeekNamedPipe(
                    self.as_raw_handle() as _,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                let error = std::io::Error::last_os_error();
                if !io_would_block(&error) {
                    return Err(error);
                }
            }
            if available > 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "monitor request timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn set_write_limit(&self, _timeout: Duration) -> std::io::Result<()> {
        Ok(())
    }
}

fn read_frame<T: for<'de> Deserialize<'de>>(
    stream: &mut impl MonitorIo,
    deadline: Instant,
) -> Result<T> {
    let mut length = [0_u8; 4];
    read_exact_deadline(stream, &mut length, deadline)?;
    let length = usize::try_from(u32::from_be_bytes(length)).expect("u32 fits usize");
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(LocalError::MonitorUnavailable(
            "monitor frame length is invalid".to_string(),
        ));
    }
    let mut payload = vec![0_u8; length];
    read_exact_deadline(stream, &mut payload, deadline)?;
    serde_json::from_slice(&payload)
        .map_err(|error| LocalError::MonitorUnavailable(format!("invalid monitor JSON: {error}")))
}

fn write_frame<T: Serialize>(
    stream: &mut impl MonitorIo,
    value: &T,
    deadline: Instant,
) -> Result<()> {
    let payload = serde_json::to_vec(value).map_err(|error| {
        LocalError::MonitorUnavailable(format!("cannot encode monitor JSON: {error}"))
    })?;
    if payload.is_empty() || payload.len() > MAX_FRAME_BYTES {
        return Err(LocalError::MonitorUnavailable(
            "monitor response exceeds the frame limit".to_string(),
        ));
    }
    let length = u32::try_from(payload.len())
        .expect("monitor frame limit fits u32")
        .to_be_bytes();
    write_all_deadline(stream, &length, deadline)?;
    write_all_deadline(stream, &payload, deadline)
}

fn read_exact_deadline(
    stream: &mut impl MonitorIo,
    mut output: &mut [u8],
    deadline: Instant,
) -> Result<()> {
    while !output.is_empty() {
        let now = Instant::now();
        if now >= deadline {
            return Err(LocalError::MonitorUnavailable(
                "monitor request timed out".to_string(),
            ));
        }
        match stream.set_read_limit(deadline - now) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                return Err(LocalError::MonitorUnavailable(
                    "monitor request timed out".to_string(),
                ));
            }
            Err(source) => {
                return Err(LocalError::Io {
                    action: "set monitor read deadline",
                    path: PathBuf::from(SOCKET_FILE),
                    source,
                });
            }
        }
        match stream.read(output) {
            Ok(0) if cfg!(windows) => {
                // A nonblocking Windows named pipe reports "no data yet" as a
                // successful zero-byte read. A closed peer is reported as a
                // broken-pipe error, so keep polling until the total deadline.
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(0) => {
                return Err(LocalError::MonitorUnavailable(
                    "monitor frame was truncated".to_string(),
                ));
            }
            Ok(count) => output = &mut output[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if io_would_block(&error) => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(source) => {
                return Err(LocalError::Io {
                    action: "read monitor frame",
                    path: PathBuf::from(SOCKET_FILE),
                    source,
                });
            }
        }
    }
    Ok(())
}

fn write_all_deadline(
    stream: &mut impl MonitorIo,
    mut input: &[u8],
    deadline: Instant,
) -> Result<()> {
    while !input.is_empty() {
        let now = Instant::now();
        if now >= deadline {
            return Err(LocalError::MonitorUnavailable(
                "monitor request timed out".to_string(),
            ));
        }
        stream
            .set_write_limit(deadline - now)
            .at("set monitor write deadline", SOCKET_FILE)?;
        match stream.write(input) {
            Ok(0) => {
                return Err(LocalError::MonitorUnavailable(
                    "monitor frame write made no progress".to_string(),
                ));
            }
            Ok(count) => input = &input[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if io_would_block(&error) => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(source) => {
                return Err(LocalError::Io {
                    action: "write monitor frame",
                    path: PathBuf::from(SOCKET_FILE),
                    source,
                });
            }
        }
    }
    Ok(())
}

fn io_would_block(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED};

        return matches!(
            error.raw_os_error().map(|code| code as u32),
            Some(ERROR_NO_DATA) | Some(ERROR_PIPE_NOT_CONNECTED)
        );
    }
    #[cfg(not(windows))]
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn overlong_socket_path_is_rejected_before_spawning() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("a".repeat(120));
        fs::create_dir(&root).unwrap();
        crate::portable::init_repository(&root, "author").await.unwrap();
        let repository = Repository::discover(&root).await.unwrap();
        // A nonexistent executable makes the ordering observable without a
        // timing assertion: endpoint validation must precede process creation.
        let error = repository.start_monitor(&root.join("does-not-exist")).unwrap_err();
        assert!(error.to_string().contains("address monitor connection"), "{error}");
        fs::write(root.join("file.txt"), b"one").unwrap();
        assert_eq!(repository.status().await.unwrap().len(), 1);
        repository.commit("record file").await.unwrap();
        assert!(repository.status().await.unwrap().is_empty());
        fs::write(root.join("file.txt"), b"two").unwrap();
        assert_eq!(repository.status().await.unwrap().len(), 1);
    }

    #[test]
    fn event_history_returns_deduplicated_changes_after_token() {
        let mut history = EventHistory::new();
        let instance = history.instance_id;
        history.record(vec!["b".to_string(), "a".to_string()]);
        let baseline = history.sequence;
        history.record(vec!["b".to_string(), "c".to_string()]);
        assert_eq!(
            history.query(Some(instance), Some(baseline)),
            QueryResult::Changes {
                instance_id: instance,
                sequence: history.sequence,
                paths: vec!["b".to_string(), "c".to_string()],
            }
        );
    }

    #[test]
    fn event_history_reset_invalidates_old_tokens() {
        let mut history = EventHistory::new();
        let instance = history.instance_id;
        let baseline = history.sequence;
        history.invalidate();
        assert!(matches!(
            history.query(Some(instance), Some(baseline)),
            QueryResult::Reset { .. }
        ));
    }

    #[test]
    fn event_history_overflow_invalidates_the_evicted_boundary() {
        let mut history = EventHistory::new();
        let instance = history.instance_id;
        let baseline = history.sequence;
        history.record(
            (0..=MAX_CANDIDATES)
                .map(|index| format!("file-{index}"))
                .collect(),
        );
        assert!(matches!(
            history.query(Some(instance), Some(baseline)),
            QueryResult::Reset { .. }
        ));
    }

    #[test]
    fn protocol_rejects_unknown_fields() {
        let payload = br#"{"version":1,"operation":"STATUS","repo_id":"x","instance_id":null,"sequence":null,"extra":true}"#;
        assert!(serde_json::from_slice::<Request>(payload).is_err());
    }

    #[test]
    fn incomplete_rename_and_root_events_invalidate_coverage() {
        let root = Path::new("/repository");
        let metadata = root.join(".synck");
        let rename = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
            .add_path(root.join("old"));
        assert!(matches!(
            process_event(Ok(rename), root, &metadata),
            EventDisposition::Invalidate
        ));

        let root_event = Event::new(EventKind::Modify(ModifyKind::Metadata(
            notify::event::MetadataKind::Any,
        )))
        .add_path(root.to_path_buf());
        assert!(matches!(
            process_event(Ok(root_event), root, &metadata),
            EventDisposition::Invalidate
        ));
    }

    #[test]
    fn candidate_paths_reject_nul_and_metadata_paths() {
        assert!(valid_relative_path("Folder/file.txt"));
        assert!(!valid_relative_path("Folder/\0file.txt"));
        assert!(!valid_relative_path(".synck/objects"));
    }

    #[cfg(windows)]
    #[test]
    fn current_process_sid_has_aligned_owned_storage() {
        use windows_sys::Win32::Security::EqualSid;

        let sid = current_process_sid().unwrap();
        assert_eq!(sid.as_ptr() as usize % std::mem::align_of::<usize>(), 0);
        assert_ne!(unsafe { EqualSid(sid.as_ptr(), sid.as_ptr()) }, 0);
    }
}
