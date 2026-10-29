use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

#[cfg(test)]
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::types::{Type, Value};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params, params_from_iter};
use synck_vcs::errors::StorageError;
use synck_vcs::storage::{SealedCommitInsert, StorageBackend, StorageTxn};
use synck_vcs::types::{
    CommitEventRow, CommitId, CommitPk, CommitRow, EventInsertRow, EventRow, ItemCursor, ItemId,
    ItemRow, LifecycleState, RefEventInsertRow, RefEventRow, RepoEventCursor, RepoId,
    SequencingMode, Sha256Digest, Timestamp, commit_id_from_digest_bytes,
    commit_id_to_digest_bytes, sha256_digest_from_digest_bytes, sha256_digest_to_digest_bytes,
    sha256_prefixed_hex_from_digest_bytes,
};

const APPLICATION_ID: i32 = 0x5359_4E4B;
const SCHEMA_VERSION: i32 = 7;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const GET_ITEMS_CHUNK: usize = 500;
const USER_SCHEMA_OBJECT_COUNT_SQL: &str =
    "SELECT count(*) FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT GLOB 'sqlite_*'";

use crate::schema::SchemaShape;

#[cfg(test)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AppendMetrics {
    item_batches: Vec<(usize, usize)>,
    single_item_reads: Vec<bool>,
    conflict_key_batches: Vec<usize>,
    descendant_batches: Vec<(usize, usize)>,
}

static EXPECTED_SCHEMA: OnceLock<SchemaShape> = OnceLock::new();
const SCHEMA: &str = include_str!("schema.sql");
pub(crate) const STATE_SCHEMA: &str = include_str!("state-schema.sql");

#[derive(Clone, Debug)]
pub struct SqliteStorage {
    path: PathBuf,
    #[cfg(test)]
    append_metrics: Arc<Mutex<AppendMetrics>>,
}

impl SqliteStorage {
    pub(crate) fn begin_sqlite_txn(&self) -> Result<SqliteTxn, StorageError> {
        let conn = self.open_writable()?;
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(map_sqlite_error)?;
        Ok(SqliteTxn {
            conn: Some(conn),
            acceptance_timestamp: None,
            active: true,
            #[cfg(test)]
            append_metrics: self.append_metrics.clone(),
        })
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            #[cfg(test)]
            append_metrics: Arc::new(Mutex::new(AppendMetrics::default())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Install one already validated, sealed history commit atomically.
    ///
    /// This is deliberately narrower than append: it preserves the supplied
    /// repository sequence and cannot construct or modify a proposal.
    pub async fn install_sealed_history_commit(
        &self,
        repo_id: &RepoId,
        commit: &SealedCommitInsert,
        events: &[EventInsertRow],
        ref_events: &[RefEventInsertRow],
    ) -> Result<bool, StorageError> {
        if commit.repo_seq.is_none()
            || usize::try_from(commit.event_count).ok()
                != events.len().checked_add(ref_events.len())
            || events
                .iter()
                .any(|event| event.repo_id != *repo_id || event.commit_pk != commit.commit_pk)
            || ref_events
                .iter()
                .any(|event| event.repo_id != *repo_id || event.commit_pk != commit.commit_pk)
        {
            return Err(StorageError::Other {
                detail: "sealed history commit is incomplete or has inconsistent identity"
                    .to_string(),
            });
        }

        if let Some(existing) = self.get_commit_readonly(repo_id, &commit.commit_id).await? {
            let conn = self.open_readonly()?;
            let (existing_commit_pk, existing_operation_id): (String, Option<String>) = conn
                .query_row(
                    "SELECT commit_pk, operation_id FROM vcs_commits \
                     WHERE repo_id = ?1 AND commit_id = ?2",
                    params![
                        repo_id.as_str(),
                        commit_id_to_digest_bytes(&commit.commit_id).to_vec()
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(map_sqlite_error)?;
            let same_header = existing_commit_pk == commit.commit_pk.as_str()
                && existing.repo_seq == commit.repo_seq
                && existing_operation_id == commit.operation_id
                && existing.format_version == commit.format_version
                && existing.import_job_id == commit.import_job_id
                && existing.author_id == commit.author_id
                && existing.committer_id == commit.committer_id
                && existing.message == commit.message
                && existing.occurred_at == commit.occurred_at
                && existing.created_at == commit.created_at
                && existing.event_count == commit.event_count
                && existing.events_digest == commit.events_digest;
            let existing_events = self
                .list_commit_events_readonly(
                    repo_id,
                    &commit.commit_id,
                    events.len() + ref_events.len() + 1,
                )
                .await?;
            let same_events = existing_events.len() == events.len() + ref_events.len()
                && existing_events.iter().all(|existing| match existing {
                    CommitEventRow::Item(left) => events.iter().any(|right| {
                        left.repo_id == right.repo_id
                            && left.commit_event_idx == right.commit_event_idx
                            && left.item_id == right.item_id
                            && left.item_seq == right.item_seq
                            && left.event_type == right.event_type
                            && left.payload == right.payload
                            && left.event_payload_digest.as_str()
                                == sha256_prefixed_hex_from_digest_bytes(right.event_payload_digest)
                            && left.created_at == right.created_at
                            && left.parent_folder_id == right.parent_folder_id
                            && left.parent_folder_id_before == right.parent_folder_id_before
                    }),
                    CommitEventRow::Ref(left) => ref_events.iter().any(|right| {
                        left.repo_id == right.repo_id
                            && left.commit_event_idx == right.commit_event_idx
                            && left.owner_item_id == right.owner_item_id
                            && left.ref_seq == right.ref_seq
                            && left.event_type == right.event_type
                            && left.payload == right.payload
                            && left.event_payload_digest.as_str()
                                == sha256_prefixed_hex_from_digest_bytes(right.event_payload_digest)
                            && left.created_at == right.created_at
                    }),
                });
            if same_header && same_events {
                return Ok(false);
            }
            return Err(StorageError::Other {
                detail: format!("sealed history collision for commit {}", commit.commit_id),
            });
        }

        let mut txn = self.begin_txn().await?;
        async {
            txn.insert_commits_sealed(repo_id, std::slice::from_ref(commit))
                .await?;
            txn.append_events(events).await?;
            txn.append_ref_events(ref_events).await?;
            txn.commit().await
        }
        .await
        .map(|_| true)
    }

    #[cfg(test)]
    fn reset_append_metrics(&self) {
        *self.append_metrics.lock().expect("append metrics lock") = AppendMetrics::default();
    }

    #[cfg(test)]
    fn append_metrics(&self) -> AppendMetrics {
        self.append_metrics
            .lock()
            .expect("append metrics lock")
            .clone()
    }

    pub fn open_writable(&self) -> Result<Connection, StorageError> {
        let conn = self.open_writable_unchecked(false)?;
        validate_schema_identity(&conn)?;
        Ok(conn)
    }

    pub(crate) fn open_writable_unchecked(&self, create: bool) -> Result<Connection, StorageError> {
        let path = self.resolved_open_path(create)?;
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        if create {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let conn = Connection::open_with_flags(path, flags).map_err(map_sqlite_error)?;
        configure_writable(&conn)?;
        Ok(conn)
    }

    pub fn open_readonly(&self) -> Result<Connection, StorageError> {
        let conn = self.open_readonly_unchecked()?;
        validate_schema_identity(&conn)?;
        Ok(conn)
    }

    pub fn open_readonly_for_initialization(&self) -> Result<Connection, StorageError> {
        let conn = self.open_readonly_unchecked()?;
        validate_initialization_schema(&conn)?;
        Ok(conn)
    }

    fn open_readonly_unchecked(&self) -> Result<Connection, StorageError> {
        let path = self.resolved_open_path(false)?;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(map_sqlite_error)?;
        conn.busy_timeout(BUSY_TIMEOUT).map_err(map_sqlite_error)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_sqlite_error)?;
        Ok(conn)
    }

    fn resolved_open_path(&self, allow_missing: bool) -> Result<PathBuf, StorageError> {
        validate_database_family(&self.path, allow_missing)?;
        let name = self.path.file_name().ok_or_else(|| StorageError::Io {
            detail: format!("database path has no file name: {}", self.path.display()),
        })?;
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = parent.canonicalize().map_err(|error| StorageError::Io {
            detail: format!(
                "cannot resolve database parent {}: {error}",
                parent.display()
            ),
        })?;
        Ok(parent.join(name))
    }
}

/// Reject links, non-regular database files and orphan SQLite sidecars.
pub fn validate_database_family(path: &Path, allow_missing: bool) -> Result<(), StorageError> {
    let present = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !is_link_or_reparse(path, &metadata)? => {
            true
        }
        Ok(_) => {
            return Err(StorageError::Io {
                detail: format!("database is not a regular file: {}", path.display()),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && allow_missing => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(StorageError::Io {
                detail: format!("database is missing: {}", path.display()),
            });
        }
        Err(error) => {
            return Err(StorageError::Io {
                detail: format!("cannot inspect database {}: {error}", path.display()),
            });
        }
    };

    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let sidecar = PathBuf::from(name);
        match fs::symlink_metadata(&sidecar) {
            Ok(metadata)
                if present
                    && metadata.file_type().is_file()
                    && !is_link_or_reparse(&sidecar, &metadata)? => {}
            Ok(_) => {
                return Err(StorageError::Io {
                    detail: format!(
                        "database sidecar is not a regular file: {}",
                        sidecar.display()
                    ),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(StorageError::Io {
                    detail: format!(
                        "cannot inspect database sidecar {}: {error}",
                        sidecar.display()
                    ),
                });
            }
        }
    }
    Ok(())
}

fn is_link_or_reparse(path: &Path, metadata: &fs::Metadata) -> Result<bool, StorageError> {
    if metadata.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let _ = path;
        return Ok(metadata.nlink() != 1);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
        };

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|error| StorageError::Io {
                detail: format!(
                    "cannot open database file {} for inspection: {error}",
                    path.display()
                ),
            })?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            return Err(StorageError::Io {
                detail: format!(
                    "cannot inspect database file {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ),
            });
        }
        return Ok(
            info.nNumberOfLinks != 1 || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        );
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(false)
    }
}

pub fn format_timestamp(value: Timestamp) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

pub fn parse_timestamp(value: &str) -> Result<Timestamp, StorageError> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|error| StorageError::Other {
            detail: format!("invalid SQLite timestamp {value:?}: {error}"),
        })?
        .with_timezone(&Utc);
    if format_timestamp(parsed) != value {
        return Err(StorageError::Other {
            detail: format!("noncanonical SQLite timestamp: {value:?}"),
        });
    }
    Ok(parsed)
}

fn configure_writable(conn: &Connection) -> Result<(), StorageError> {
    conn.busy_timeout(BUSY_TIMEOUT).map_err(map_sqlite_error)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(map_sqlite_error)?;
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(map_sqlite_error)?;
    Ok(())
}

fn prepare_schema_initialization(conn: &Connection) -> Result<(), StorageError> {
    let application_id: i32 = conn
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(map_sqlite_error)?;
    let user_version: i32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if application_id != 0 || user_version != 0 {
        if application_id != APPLICATION_ID || user_version != SCHEMA_VERSION {
            return Err(StorageError::Other {
                detail: format!(
                    "unsupported SQLite schema: application_id={application_id} user_version={user_version}"
                ),
            });
        }
        validate_schema_version(conn, user_version)?;
        return Ok(());
    }
    let user_schema_objects: i64 = conn
        .query_row(USER_SCHEMA_OBJECT_COUNT_SQL, [], |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if user_schema_objects != 0 {
        return Err(StorageError::Other {
            detail: "unversioned SQLite database is not empty".to_string(),
        });
    }
    let journal_mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(map_sqlite_error)?;
    }
    Ok(())
}

fn validate_schema_identity(conn: &Connection) -> Result<(), StorageError> {
    let application_id: i32 = conn
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(map_sqlite_error)?;
    let user_version: i32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if application_id != APPLICATION_ID || user_version != SCHEMA_VERSION {
        return Err(StorageError::Other {
            detail: format!(
                "SQLite schema identity mismatch: application_id={application_id} user_version={user_version}"
            ),
        });
    }
    let journal_mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(StorageError::Other {
            detail: format!("SQLite journal mode is {journal_mode:?}, expected WAL"),
        });
    }
    validate_schema_version(conn, SCHEMA_VERSION)
}

fn validate_initialization_schema(conn: &Connection) -> Result<(), StorageError> {
    let application_id: i32 = conn
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(map_sqlite_error)?;
    let user_version: i32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if application_id == 0 && user_version == 0 {
        let user_schema_objects: i64 = conn
            .query_row(USER_SCHEMA_OBJECT_COUNT_SQL, [], |row| row.get(0))
            .map_err(map_sqlite_error)?;
        if user_schema_objects == 0 {
            return Ok(());
        }
    }
    if application_id != APPLICATION_ID || user_version != SCHEMA_VERSION {
        return Err(StorageError::Other {
            detail: format!(
                "unsupported SQLite schema: application_id={application_id} user_version={user_version}"
            ),
        });
    }
    let journal_mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(map_sqlite_error)?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(StorageError::Other {
            detail: format!("SQLite journal mode is {journal_mode:?}, expected WAL"),
        });
    }
    validate_schema_version(conn, user_version)
}

fn validate_schema_version(conn: &Connection, version: i32) -> Result<(), StorageError> {
    let actual = schema_objects(conn)?;
    if version != SCHEMA_VERSION {
        return Err(StorageError::Other {
            detail: format!("unsupported SQLite schema version {version}"),
        });
    }
    let expected = EXPECTED_SCHEMA.get_or_init(expected_schema);
    if &actual != expected {
        return Err(StorageError::Other {
            detail: format!("SQLite schema definition does not match version {version}"),
        });
    }
    Ok(())
}

fn expected_schema() -> SchemaShape {
    let expected = Connection::open_in_memory().expect("open expected SQLite schema");
    expected
        .execute_batch(SCHEMA)
        .expect("install expected SQLite schema");
    schema_objects(&expected).expect("read expected SQLite schema")
}

fn schema_objects(conn: &Connection) -> Result<SchemaShape, StorageError> {
    crate::schema::inspect(conn, "").map_err(map_sqlite_error)
}

fn map_sqlite_error(error: rusqlite::Error) -> StorageError {
    use rusqlite::ErrorCode;

    if let rusqlite::Error::SqliteFailure(native, message) = &error {
        let detail = message.clone().unwrap_or_else(|| error.to_string());
        return match native.code {
            ErrorCode::ConstraintViolation
                if matches!(
                    native.extended_code,
                    rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                        | rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                ) =>
            {
                StorageError::UniqueViolation {
                    detail,
                    constraint: None,
                }
            }
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                StorageError::Overloaded { detail }
            }
            ErrorCode::SystemIoFailure
            | ErrorCode::CannotOpen
            | ErrorCode::DiskFull
            | ErrorCode::ReadOnly
            | ErrorCode::PermissionDenied => StorageError::Io { detail },
            _ => StorageError::Other { detail },
        };
    }
    StorageError::Other {
        detail: error.to_string(),
    }
}

fn decode_error(column: usize, detail: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            detail.into(),
        )),
    )
}

fn parse_repo(value: String, column: usize) -> rusqlite::Result<RepoId> {
    RepoId::parse(value).map_err(|error| decode_error(column, error.to_string()))
}

fn parse_item(value: String, column: usize) -> rusqlite::Result<ItemId> {
    ItemId::parse(value).map_err(|error| decode_error(column, error.to_string()))
}

fn parse_commit(value: Vec<u8>, column: usize) -> rusqlite::Result<CommitId> {
    commit_id_from_digest_bytes(&value).map_err(|error| decode_error(column, error.to_string()))
}

fn parse_digest(value: Vec<u8>, column: usize) -> rusqlite::Result<Sha256Digest> {
    sha256_digest_from_digest_bytes(&value).map_err(|error| decode_error(column, error.to_string()))
}

fn parse_time_row(value: String, column: usize) -> rusqlite::Result<Timestamp> {
    parse_timestamp(&value).map_err(|error| decode_error(column, error.to_string()))
}

fn item_from_row(row: &Row<'_>) -> rusqlite::Result<ItemRow> {
    let name: String = row.get(4)?;
    synck_vcs::limits::check_bytes("name", name.len(), synck_vcs::limits::MAX_NAME_BYTES)
        .map_err(|e| decode_error(4, e.to_string()))?;
    let lifecycle = match row.get::<_, String>(8)?.as_str() {
        "LIVE" => LifecycleState::Live,
        "TRASH" => LifecycleState::Trash,
        "TOMBSTONED" => LifecycleState::Tombstoned,
        other => return Err(decode_error(8, format!("invalid lifecycle_state: {other}"))),
    };
    let sequencing = match row.get::<_, String>(11)?.as_str() {
        "LOCAL" => SequencingMode::Local,
        "FEDERATED" => SequencingMode::Federated,
        other => {
            return Err(decode_error(
                11,
                format!("invalid sequencing_mode: {other}"),
            ));
        }
    };
    let parent = row
        .get::<_, Option<String>>(3)?
        .map(|value| parse_item(value, 3))
        .transpose()?;
    Ok(ItemRow {
        repo_id: parse_repo(row.get(0)?, 0)?,
        item_id: parse_item(row.get(1)?, 1)?,
        item_type: row.get(2)?,
        parent_folder_id: parent,
        name: row.get(4)?,
        name_key: row.get(5)?,
        head_seq: row.get(6)?,
        ref_head_seq: row.get(7)?,
        lifecycle_state: lifecycle,
        created_at: parse_time_row(row.get(9)?, 9)?,
        created_by: row.get(10)?,
        sequencing_mode: sequencing,
        federation_stream_id: row.get(12)?,
    })
}

fn commit_from_row(row: &Row<'_>) -> rusqlite::Result<CommitRow> {
    let author: String = row.get(5)?;
    let committer: String = row.get(6)?;
    let message: Option<String> = row.get(7)?;
    let operation: Option<String> = row.get(12)?;
    synck_vcs::limits::validate_commit_metadata(
        &author, &committer, message.as_deref(), operation.as_deref(),
    ).map_err(|e| decode_error(5, e.to_string()))?;
    let occurred_at = row
        .get::<_, Option<String>>(8)?
        .map(|value| parse_time_row(value, 8))
        .transpose()?;
    Ok(CommitRow {
        repo_id: parse_repo(row.get(0)?, 0)?,
        commit_id: parse_commit(row.get(1)?, 1)?,
        repo_seq: row.get(2)?,
        format_version: row.get(3)?,
        import_job_id: row.get(4)?,
        author_id: author,
        committer_id: committer,
        message,
        occurred_at,
        created_at: parse_time_row(row.get(9)?, 9)?,
        event_count: row.get(10)?,
        events_digest: parse_digest(row.get(11)?, 11)?,
    })
}

fn checked_payload(row: &Row<'_>, column: usize) -> rusqlite::Result<Vec<u8>> {
    let bytes: Vec<u8> = row.get(column)?;
    synck_vcs::limits::check_bytes("event payload", bytes.len(), synck_vcs::limits::MAX_EVENT_PAYLOAD_BYTES)
        .map_err(|e| decode_error(column, e.to_string()))?;
    Ok(bytes)
}

fn metadata_from_row(row: &Row<'_>, event_column: usize, metadata_column: usize) -> rusqlite::Result<Option<synck_vcs::metadata::Metadata>> {
    let Some(bytes) = row.get::<_, Option<Vec<u8>>>(metadata_column)? else { return Ok(None); };
    let payload: Vec<u8> = row.get(event_column)?;
    let event: serde_json::Value = serde_json::from_slice(&payload).map_err(|e| decode_error(event_column, e.to_string()))?;
    let id = event.get("metadata_id").and_then(serde_json::Value::as_str).ok_or_else(|| decode_error(event_column, "missing metadata ID"))?;
    let id = synck_vcs::types::ObjectId::parse(id).map_err(|e| decode_error(event_column, e.to_string()))?;
    synck_vcs::metadata::Metadata::verify(&id, bytes).map(Some).map_err(|e| decode_error(metadata_column, e.to_string()))
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<EventRow> {
    let parent = row
        .get::<_, Option<String>>(10)?
        .map(|value| parse_item(value, 10))
        .transpose()?;
    let parent_before = row
        .get::<_, Option<String>>(11)?
        .map(|value| parse_item(value, 11))
        .transpose()?;
    Ok(EventRow {
        repo_id: parse_repo(row.get(0)?, 0)?,
        item_id: parse_item(row.get(1)?, 1)?,
        item_seq: row.get(2)?,
        event_type: row.get(3)?,
        commit_id: parse_commit(row.get(4)?, 4)?,
        commit_event_idx: row.get(5)?,
        repo_seq: row.get(6)?,
        payload: checked_payload(row, 7)?,
        metadata: metadata_from_row(row, 7, 12)?,
        event_payload_digest: parse_digest(row.get(8)?, 8)?,
        created_at: parse_time_row(row.get(9)?, 9)?,
        parent_folder_id: parent,
        parent_folder_id_before: parent_before,
    })
}

fn ref_event_from_row(row: &Row<'_>) -> rusqlite::Result<RefEventRow> {
    Ok(RefEventRow {
        repo_id: parse_repo(row.get(0)?, 0)?,
        owner_item_id: parse_item(row.get(1)?, 1)?,
        ref_seq: row.get(2)?,
        event_type: row.get(3)?,
        commit_id: parse_commit(row.get(4)?, 4)?,
        commit_event_idx: row.get(5)?,
        repo_seq: row.get(6)?,
        payload: checked_payload(row, 7)?,
        metadata: metadata_from_row(row, 7, 10)?,
        event_payload_digest: parse_digest(row.get(8)?, 8)?,
        created_at: parse_time_row(row.get(9)?, 9)?,
    })
}

fn commit_event_from_row(row: &Row<'_>) -> rusqlite::Result<CommitEventRow> {
    let event_family: i32 = row.get(0)?;
    match event_family {
        0 => {
            let parent = row
                .get::<_, Option<String>>(11)?
                .map(|value| parse_item(value, 11))
                .transpose()?;
            let parent_before = row
                .get::<_, Option<String>>(12)?
                .map(|value| parse_item(value, 12))
                .transpose()?;
            Ok(CommitEventRow::Item(EventRow {
                repo_id: parse_repo(row.get(1)?, 1)?,
                item_id: parse_item(row.get(2)?, 2)?,
                item_seq: row.get(3)?,
                event_type: row.get(4)?,
                commit_id: parse_commit(row.get(5)?, 5)?,
                commit_event_idx: row.get(6)?,
                repo_seq: row.get(7)?,
                payload: checked_payload(row, 8)?,
                metadata: metadata_from_row(row, 8, 13)?,
                event_payload_digest: parse_digest(row.get(9)?, 9)?,
                created_at: parse_time_row(row.get(10)?, 10)?,
                parent_folder_id: parent,
                parent_folder_id_before: parent_before,
            }))
        }
        1 => Ok(CommitEventRow::Ref(RefEventRow {
            repo_id: parse_repo(row.get(1)?, 1)?,
            owner_item_id: parse_item(row.get(2)?, 2)?,
            ref_seq: row.get(3)?,
            event_type: row.get(4)?,
            commit_id: parse_commit(row.get(5)?, 5)?,
            commit_event_idx: row.get(6)?,
            repo_seq: row.get(7)?,
            payload: checked_payload(row, 8)?,
                metadata: metadata_from_row(row, 8, 13)?,
            event_payload_digest: parse_digest(row.get(9)?, 9)?,
            created_at: parse_time_row(row.get(10)?, 10)?,
        })),
        other => Err(decode_error(
            0,
            format!("invalid commit event family: {other}"),
        )),
    }
}

const ITEM_COLUMNS: &str = "repo_id, item_id, item_type, parent_folder_id, name, name_key, head_seq, ref_head_seq, lifecycle_state, created_at, created_by, sequencing_mode, federation_stream_id";
const COMMIT_COLUMNS: &str = "repo_id, commit_id, repo_seq, format_version, import_job_id, author_id, committer_id, message, occurred_at, created_at, event_count, events_digest, operation_id";
const EVENT_COLUMNS: &str = "e.repo_id, e.item_id, e.item_seq, e.event_type, c.commit_id, e.commit_event_idx, c.repo_seq, e.payload, e.event_payload_digest, e.created_at, e.parent_folder_id, e.parent_folder_id_before, (SELECT m.payload FROM vcs_metadata_objects m WHERE m.repo_id=e.repo_id AND m.object_id=unhex(substr(json_extract(CAST(e.payload AS TEXT), '$.metadata_id'),8)))";
const REF_EVENT_COLUMNS: &str = "r.repo_id, r.owner_item_id, r.ref_seq, r.event_type, c.commit_id, r.commit_event_idx, c.repo_seq, r.payload, r.event_payload_digest, r.created_at, (SELECT m.payload FROM vcs_metadata_objects m WHERE m.repo_id=r.repo_id AND m.object_id=unhex(substr(json_extract(CAST(r.payload AS TEXT), '$.metadata_id'),8)))";

fn checked_limit(limit: usize) -> Result<i64, StorageError> {
    i64::try_from(limit).map_err(|_| StorageError::Other {
        detail: format!("query limit does not fit SQLite INTEGER: {limit}"),
    })
}

fn text_value(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn initialize_schema(conn: &Connection) -> Result<(), StorageError> {
    prepare_schema_initialization(conn)?;
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(map_sqlite_error)?;
    let result = (|| {
        let application_id: i32 = conn
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(map_sqlite_error)?;
        let user_version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(map_sqlite_error)?;
        if application_id == 0 && user_version == 0 {
            let user_schema_objects: i64 = conn
                .query_row(USER_SCHEMA_OBJECT_COUNT_SQL, [], |row| row.get(0))
                .map_err(map_sqlite_error)?;
            if user_schema_objects != 0 {
                return Err(StorageError::Other {
                    detail: "unversioned SQLite database is not empty".to_string(),
                });
            }
            conn.execute_batch(SCHEMA).map_err(map_sqlite_error)?;
            validate_schema_version(conn, SCHEMA_VERSION)?;
            conn.pragma_update(None, "application_id", APPLICATION_ID)
                .map_err(map_sqlite_error)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(map_sqlite_error)?;
        } else if application_id != APPLICATION_ID || user_version != SCHEMA_VERSION {
            return Err(StorageError::Other {
                detail: format!(
                    "unsupported SQLite schema: application_id={application_id} user_version={user_version}"
                ),
            });
        }
        validate_schema_identity(conn)
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT").map_err(map_sqlite_error),
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

#[async_trait]
impl StorageBackend for SqliteStorage {
    async fn init_schema(&self) -> Result<(), StorageError> {
        let conn = self.open_writable_unchecked(true)?;
        initialize_schema(&conn)
    }

    async fn begin_txn(&self) -> Result<Box<dyn StorageTxn>, StorageError> {
        Ok(Box::new(self.begin_sqlite_txn()?))
    }

    async fn get_item_readonly(
        &self,
        repo_id: &RepoId,
        item_id: &ItemId,
    ) -> Result<Option<ItemRow>, StorageError> {
        let conn = self.open_readonly()?;
        conn.query_row(
            &format!("SELECT {ITEM_COLUMNS} FROM vcs_items WHERE repo_id = ?1 AND item_id = ?2"),
            params![repo_id.as_str(), item_id.as_str()],
            item_from_row,
        )
        .optional()
        .map_err(map_sqlite_error)
    }

    async fn get_items_readonly(
        &self,
        repo_id: &RepoId,
        item_ids: &[ItemId],
    ) -> Result<Vec<ItemRow>, StorageError> {
        let conn = self.open_readonly()?;
        get_items(&conn, repo_id, item_ids)
    }

    async fn get_commit_readonly(
        &self,
        repo_id: &RepoId,
        commit_id: &CommitId,
    ) -> Result<Option<CommitRow>, StorageError> {
        let conn = self.open_readonly()?;
        conn.query_row(
            &format!(
                "SELECT {COMMIT_COLUMNS} FROM vcs_commits WHERE repo_id = ?1 AND commit_id = ?2"
            ),
            params![
                repo_id.as_str(),
                commit_id_to_digest_bytes(commit_id).to_vec()
            ],
            commit_from_row,
        )
        .optional()
        .map_err(map_sqlite_error)
    }

    async fn get_commit_by_operation_id_readonly(
        &self,
        repo_id: &RepoId,
        operation_id: &str,
    ) -> Result<Option<CommitRow>, StorageError> {
        let conn = self.open_readonly()?;
        conn.query_row(
            &format!(
                "SELECT {COMMIT_COLUMNS} FROM vcs_commits WHERE repo_id = ?1 AND operation_id = ?2 AND commit_id IS NOT NULL"
            ),
            params![repo_id.as_str(), operation_id],
            commit_from_row,
        )
        .optional()
        .map_err(map_sqlite_error)
    }

    async fn list_events_for_commit_readonly(
        &self,
        repo_id: &RepoId,
        commit_id: &CommitId,
        limit: usize,
    ) -> Result<Vec<EventRow>, StorageError> {
        let conn = self.open_readonly()?;
        let limit = checked_limit(limit)?;
        collect_rows(
            &conn,
            &format!(
                "SELECT {EVENT_COLUMNS} FROM vcs_commits c JOIN vcs_events e ON e.repo_id = c.repo_id AND e.commit_pk = c.commit_pk WHERE c.repo_id = ? AND c.commit_id = ? ORDER BY e.commit_event_idx LIMIT ?"
            ),
            vec![
                text_value(repo_id.as_str()),
                Value::Blob(commit_id_to_digest_bytes(commit_id).to_vec()),
                limit.into(),
            ],
            event_from_row,
        )
    }

    async fn list_events_for_item_readonly(
        &self,
        repo_id: &RepoId,
        item_id: &ItemId,
        from_seq: i64,
        limit: usize,
    ) -> Result<Vec<EventRow>, StorageError> {
        let conn = self.open_readonly()?;
        list_events_for_item(&conn, repo_id, item_id, from_seq, limit)
    }

    async fn list_ref_events_for_owner_readonly(
        &self,
        repo_id: &RepoId,
        owner_item_id: &ItemId,
        from_seq: i64,
        limit: usize,
    ) -> Result<Vec<RefEventRow>, StorageError> {
        let conn = self.open_readonly()?;
        collect_rows(
            &conn,
            &format!(
                "SELECT {REF_EVENT_COLUMNS} FROM vcs_ref_events r \
                 JOIN vcs_commits c ON c.repo_id = r.repo_id AND c.commit_pk = r.commit_pk \
                 WHERE r.repo_id = ? AND r.owner_item_id = ? AND r.ref_seq >= ? \
                 ORDER BY r.ref_seq LIMIT ?"
            ),
            vec![
                text_value(repo_id.as_str()),
                text_value(owner_item_id.as_str()),
                from_seq.into(),
                checked_limit(limit)?.into(),
            ],
            ref_event_from_row,
        )
    }

    async fn list_commit_events_readonly(
        &self,
        repo_id: &RepoId,
        commit_id: &CommitId,
        limit: usize,
    ) -> Result<Vec<CommitEventRow>, StorageError> {
        let conn = self.open_readonly()?;
        collect_rows(
            &conn,
            "SELECT event_family, repo_id, owner_item_id, event_seq, event_type, commit_id, \
                    commit_event_idx, repo_seq, payload, event_payload_digest, created_at, \
                    parent_folder_id, parent_folder_id_before, \
                    (SELECT m.payload FROM vcs_metadata_objects m WHERE m.repo_id=events.repo_id AND m.object_id=unhex(substr(json_extract(CAST(events.payload AS TEXT), '$.metadata_id'),8))) \
             FROM ( \
                 SELECT 0 AS event_family, e.repo_id, e.item_id AS owner_item_id, \
                        e.item_seq AS event_seq, e.event_type, c.commit_id, e.commit_event_idx, \
                        c.repo_seq, e.payload, e.event_payload_digest, e.created_at, \
                        e.parent_folder_id, e.parent_folder_id_before \
                 FROM vcs_events e JOIN vcs_commits c \
                   ON c.repo_id = e.repo_id AND c.commit_pk = e.commit_pk \
                 WHERE c.repo_id = ?1 AND c.commit_id = ?2 \
                 UNION ALL \
                 SELECT 1, r.repo_id, r.owner_item_id, r.ref_seq, r.event_type, c.commit_id, \
                        r.commit_event_idx, c.repo_seq, r.payload, r.event_payload_digest, \
                        r.created_at, NULL, NULL \
                 FROM vcs_ref_events r JOIN vcs_commits c \
                   ON c.repo_id = r.repo_id AND c.commit_pk = r.commit_pk \
                 WHERE c.repo_id = ?1 AND c.commit_id = ?2 \
             ) AS events ORDER BY commit_event_idx LIMIT ?3",
            vec![
                text_value(repo_id.as_str()),
                Value::Blob(commit_id_to_digest_bytes(commit_id).to_vec()),
                checked_limit(limit)?.into(),
            ],
            commit_event_from_row,
        )
    }

    async fn list_repo_commit_events_readonly(
        &self,
        repo_id: &RepoId,
        after: Option<&RepoEventCursor>,
        limit: usize,
    ) -> Result<Vec<CommitEventRow>, StorageError> {
        let conn = self.open_readonly()?;
        let (after_repo_seq, after_event_idx) = after
            .map(|cursor| (cursor.repo_seq, cursor.commit_event_idx))
            .unwrap_or((0, -1));
        collect_rows(
            &conn,
            "SELECT event_family, repo_id, owner_item_id, event_seq, event_type, commit_id, \
                    commit_event_idx, repo_seq, payload, event_payload_digest, created_at, \
                    parent_folder_id, parent_folder_id_before, \
                    (SELECT m.payload FROM vcs_metadata_objects m WHERE m.repo_id=events.repo_id AND m.object_id=unhex(substr(json_extract(CAST(events.payload AS TEXT), '$.metadata_id'),8))) \
             FROM ( \
                 SELECT 0 AS event_family, e.repo_id, e.item_id AS owner_item_id, \
                        e.item_seq AS event_seq, e.event_type, c.commit_id, e.commit_event_idx, \
                        c.repo_seq, e.payload, e.event_payload_digest, e.created_at, \
                        e.parent_folder_id, e.parent_folder_id_before \
                 FROM vcs_events e JOIN vcs_commits c \
                   ON c.repo_id = e.repo_id AND c.commit_pk = e.commit_pk \
                 WHERE c.repo_id = ?1 AND c.commit_id IS NOT NULL AND c.repo_seq IS NOT NULL \
                 UNION ALL \
                 SELECT 1, r.repo_id, r.owner_item_id, r.ref_seq, r.event_type, c.commit_id, \
                        r.commit_event_idx, c.repo_seq, r.payload, r.event_payload_digest, \
                        r.created_at, NULL, NULL \
                 FROM vcs_ref_events r JOIN vcs_commits c \
                   ON c.repo_id = r.repo_id AND c.commit_pk = r.commit_pk \
                 WHERE c.repo_id = ?1 AND c.commit_id IS NOT NULL AND c.repo_seq IS NOT NULL \
             ) AS events \
             WHERE repo_seq > ?2 OR (repo_seq = ?2 AND commit_event_idx > ?3) \
             ORDER BY repo_seq, commit_event_idx LIMIT ?4",
            vec![
                text_value(repo_id.as_str()),
                after_repo_seq.into(),
                i64::from(after_event_idx).into(),
                checked_limit(limit)?.into(),
            ],
            commit_event_from_row,
        )
    }

    async fn list_repo_events_readonly(
        &self,
        repo_id: &RepoId,
        after: Option<&RepoEventCursor>,
        limit: usize,
    ) -> Result<Vec<EventRow>, StorageError> {
        let conn = self.open_readonly()?;
        let limit = checked_limit(limit)?;
        let (sql, values): (String, Vec<Value>) = match after {
            Some(cursor) => (
                format!(
                    "SELECT {EVENT_COLUMNS} FROM vcs_events e JOIN vcs_commits c ON c.repo_id = e.repo_id AND c.commit_pk = e.commit_pk WHERE e.repo_id = ? AND c.repo_seq IS NOT NULL AND (c.repo_seq > ? OR (c.repo_seq = ? AND e.commit_event_idx > ?)) ORDER BY c.repo_seq, e.commit_event_idx LIMIT ?"
                ),
                vec![
                    text_value(repo_id.as_str()),
                    cursor.repo_seq.into(),
                    cursor.repo_seq.into(),
                    i64::from(cursor.commit_event_idx).into(),
                    limit.into(),
                ],
            ),
            None => (
                format!(
                    "SELECT {EVENT_COLUMNS} FROM vcs_events e JOIN vcs_commits c ON c.repo_id = e.repo_id AND c.commit_pk = e.commit_pk WHERE e.repo_id = ? AND c.repo_seq IS NOT NULL ORDER BY c.repo_seq, e.commit_event_idx LIMIT ?"
                ),
                vec![text_value(repo_id.as_str()), limit.into()],
            ),
        };
        collect_rows(&conn, &sql, values, event_from_row)
    }

    async fn list_commits_by_repo_seq_readonly(
        &self,
        repo_id: &RepoId,
        from_repo_seq: i64,
        to_repo_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<CommitRow>, StorageError> {
        let conn = self.open_readonly()?;
        let limit = checked_limit(limit)?;
        let (sql, values): (String, Vec<Value>) = match to_repo_seq {
            Some(to) => (
                format!(
                    "SELECT {COMMIT_COLUMNS} FROM vcs_commits WHERE repo_id = ? AND repo_seq >= ? AND repo_seq <= ? ORDER BY repo_seq LIMIT ?"
                ),
                vec![
                    text_value(repo_id.as_str()),
                    from_repo_seq.into(),
                    to.into(),
                    limit.into(),
                ],
            ),
            None => (
                format!(
                    "SELECT {COMMIT_COLUMNS} FROM vcs_commits WHERE repo_id = ? AND repo_seq >= ? ORDER BY repo_seq LIMIT ?"
                ),
                vec![
                    text_value(repo_id.as_str()),
                    from_repo_seq.into(),
                    limit.into(),
                ],
            ),
        };
        let commits = collect_rows(&conn, &sql, values, commit_from_row)?;
        let mut previous = from_repo_seq.max(1) - 1;
        for commit in &commits {
            let sequence = commit.repo_seq.expect("sequenced query");
            if previous.checked_add(1) != Some(sequence) {
                return Err(StorageError::Other {
                    detail: format!("repository sequence must be consecutive after {previous}, got {sequence}"),
                });
            }
            previous = sequence;
        }
        Ok(commits)
    }

    async fn list_items_readonly(
        &self,
        repo_id: &RepoId,
        after: Option<&ItemCursor>,
        lifecycle_state: Option<LifecycleState>,
        limit: usize,
    ) -> Result<Vec<ItemRow>, StorageError> {
        let conn = self.open_readonly()?;
        let limit = checked_limit(limit)?;
        let lifecycle = lifecycle_state.map(lifecycle_text);
        let (sql, values): (String, Vec<Value>) = match (after, lifecycle) {
            (Some(cursor), Some(state)) => (
                format!(
                    "SELECT {ITEM_COLUMNS} FROM vcs_items WHERE repo_id = ? AND lifecycle_state = ? AND (created_at > ? OR (created_at = ? AND item_id > ?)) ORDER BY created_at, item_id LIMIT ?"
                ),
                vec![
                    text_value(repo_id.as_str()),
                    text_value(state),
                    format_timestamp(cursor.created_at).into(),
                    format_timestamp(cursor.created_at).into(),
                    text_value(cursor.item_id.as_str()),
                    limit.into(),
                ],
            ),
            (Some(cursor), None) => (
                format!(
                    "SELECT {ITEM_COLUMNS} FROM vcs_items WHERE repo_id = ? AND (created_at > ? OR (created_at = ? AND item_id > ?)) ORDER BY created_at, item_id LIMIT ?"
                ),
                vec![
                    text_value(repo_id.as_str()),
                    format_timestamp(cursor.created_at).into(),
                    format_timestamp(cursor.created_at).into(),
                    text_value(cursor.item_id.as_str()),
                    limit.into(),
                ],
            ),
            (None, Some(state)) => (
                format!(
                    "SELECT {ITEM_COLUMNS} FROM vcs_items WHERE repo_id = ? AND lifecycle_state = ? ORDER BY created_at, item_id LIMIT ?"
                ),
                vec![
                    text_value(repo_id.as_str()),
                    text_value(state),
                    limit.into(),
                ],
            ),
            (None, None) => (
                format!(
                    "SELECT {ITEM_COLUMNS} FROM vcs_items WHERE repo_id = ? ORDER BY created_at, item_id LIMIT ?"
                ),
                vec![text_value(repo_id.as_str()), limit.into()],
            ),
        };
        collect_rows(&conn, &sql, values, item_from_row)
    }
}

fn collect_rows<T>(
    conn: &Connection,
    sql: &str,
    values: Vec<Value>,
    decode: fn(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, StorageError> {
    let mut statement = conn.prepare(sql).map_err(map_sqlite_error)?;
    let rows = statement
        .query_map(params_from_iter(values), decode)
        .map_err(map_sqlite_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_sqlite_error)
}

fn get_items(
    conn: &Connection,
    repo_id: &RepoId,
    item_ids: &[ItemId],
) -> Result<Vec<ItemRow>, StorageError> {
    let mut items = Vec::with_capacity(item_ids.len());
    for chunk in item_ids.chunks(GET_ITEMS_CHUNK) {
        if chunk.is_empty() {
            continue;
        }
        let placeholders = (0..chunk.len()).map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT {ITEM_COLUMNS} FROM vcs_items WHERE repo_id = ? AND item_id IN ({placeholders})"
        );
        let mut values = Vec::with_capacity(chunk.len() + 1);
        values.push(Value::Text(repo_id.as_str().to_string()));
        values.extend(
            chunk
                .iter()
                .map(|item_id| Value::Text(item_id.as_str().to_string())),
        );
        items.extend(collect_rows(conn, &sql, values, item_from_row)?);
    }
    Ok(items)
}

fn list_events_for_item(
    conn: &Connection,
    repo_id: &RepoId,
    item_id: &ItemId,
    from_seq: i64,
    limit: usize,
) -> Result<Vec<EventRow>, StorageError> {
    let limit = checked_limit(limit)?;
    collect_rows(
        conn,
        &format!(
            "SELECT {EVENT_COLUMNS} FROM vcs_events e JOIN vcs_commits c ON c.repo_id = e.repo_id AND c.commit_pk = e.commit_pk WHERE e.repo_id = ? AND e.item_id = ? AND e.item_seq >= ? ORDER BY e.item_seq LIMIT ?"
        ),
        vec![
            text_value(repo_id.as_str()),
            text_value(item_id.as_str()),
            from_seq.into(),
            limit.into(),
        ],
        event_from_row,
    )
}

fn lifecycle_text(value: LifecycleState) -> &'static str {
    match value {
        LifecycleState::Live => "LIVE",
        LifecycleState::Trash => "TRASH",
        LifecycleState::Tombstoned => "TOMBSTONED",
    }
}

fn sequencing_text(value: SequencingMode) -> &'static str {
    match value {
        SequencingMode::Local => "LOCAL",
        SequencingMode::Federated => "FEDERATED",
    }
}

pub(crate) struct SqliteTxn {
    conn: Option<Connection>,
    acceptance_timestamp: Option<Timestamp>,
    active: bool,
    #[cfg(test)]
    append_metrics: Arc<Mutex<AppendMetrics>>,
}

impl SqliteTxn {
    pub(crate) fn conn(&self) -> Result<&Connection, StorageError> {
        if !self.active {
            return Err(StorageError::Other {
                detail: "SQLite transaction is not active".to_string(),
            });
        }
        self.conn.as_ref().ok_or_else(|| StorageError::Other {
            detail: "SQLite transaction connection is missing".to_string(),
        })
    }
}

impl Drop for SqliteTxn {
    fn drop(&mut self) {
        if self.active
            && let Some(conn) = self.conn.as_ref()
        {
            let _ = conn.execute_batch("ROLLBACK");
        }
    }
}

#[async_trait]
impl StorageTxn for SqliteTxn {
    async fn acceptance_timestamp(&mut self) -> Result<Timestamp, StorageError> {
        if let Some(value) = self.acceptance_timestamp {
            return Ok(value);
        }
        let value = Utc::now();
        self.acceptance_timestamp = Some(value);
        Ok(value)
    }

    async fn lock_conflict_keys(&mut self, keys: &[i64]) -> Result<(), StorageError> {
        let _ = self.conn()?;
        #[cfg(not(test))]
        let _ = keys;
        #[cfg(test)]
        self.append_metrics
            .lock()
            .expect("append metrics lock")
            .conflict_key_batches
            .push(keys.len());
        Ok(())
    }

    async fn get_items(
        &mut self,
        repo_id: &RepoId,
        item_ids: &[ItemId],
    ) -> Result<Vec<ItemRow>, StorageError> {
        let rows = get_items(self.conn()?, repo_id, item_ids)?;
        #[cfg(test)]
        self.append_metrics
            .lock()
            .expect("append metrics lock")
            .item_batches
            .push((item_ids.len(), rows.len()));
        Ok(rows)
    }

    async fn has_live_descendants(
        &mut self,
        repo_id: &RepoId,
        root_item_id: &ItemId,
    ) -> Result<bool, StorageError> {
        let found = self
            .conn()?
            .query_row(
                "SELECT 1 FROM vcs_items WHERE repo_id = ?1 AND parent_folder_id = ?2 \
                 AND lifecycle_state = 'LIVE' LIMIT 1",
                params![repo_id.as_str(), root_item_id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        Ok(found.is_some())
    }

    async fn list_non_tombstoned_descendants_bounded(
        &mut self,
        repo_id: &RepoId,
        root_item_id: &ItemId,
        remaining: usize,
    ) -> Result<Vec<(ItemId, i64)>, StorageError> {
        let row_limit = remaining
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or_else(|| StorageError::Other {
                detail: "descendant row limit exceeds SQLite INTEGER".to_string(),
            })?;
        let mut statement = self
            .conn()?
            .prepare(
                "WITH RECURSIVE descendants(item_id, head_seq) AS ( \
                     SELECT item_id, head_seq \
                     FROM vcs_items \
                     WHERE repo_id = ?1 \
                       AND parent_folder_id = ?2 \
                       AND lifecycle_state <> 'TOMBSTONED' \
                     UNION ALL \
                     SELECT child.item_id, child.head_seq \
                     FROM vcs_items child \
                     JOIN descendants parent \
                       ON child.repo_id = ?1 \
                      AND child.parent_folder_id = parent.item_id \
                     WHERE child.lifecycle_state <> 'TOMBSTONED' \
                 ) \
                 SELECT item_id, head_seq FROM descendants LIMIT ?3",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![repo_id.as_str(), root_item_id.as_str(), row_limit],
                |row| {
                    let item_id: String = row.get(0)?;
                    Ok((parse_item(item_id, 0)?, row.get(1)?))
                },
            )
            .map_err(map_sqlite_error)?;
        let rows = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        #[cfg(test)]
        self.append_metrics
            .lock()
            .expect("append metrics lock")
            .descendant_batches
            .push((remaining, rows.len()));
        Ok(rows)
    }

    async fn get_item(
        &mut self,
        repo_id: &RepoId,
        item_id: &ItemId,
    ) -> Result<Option<ItemRow>, StorageError> {
        let row = self
            .conn()?
            .query_row(
                &format!(
                    "SELECT {ITEM_COLUMNS} FROM vcs_items WHERE repo_id = ?1 AND item_id = ?2"
                ),
                params![repo_id.as_str(), item_id.as_str()],
                item_from_row,
            )
            .optional()
            .map_err(map_sqlite_error)?;
        #[cfg(test)]
        self.append_metrics
            .lock()
            .expect("append metrics lock")
            .single_item_reads
            .push(row.is_some());
        Ok(row)
    }

    async fn get_item_by_federation_stream_id(
        &mut self,
        repo_id: &RepoId,
        federation_stream_id: &str,
    ) -> Result<Option<ItemRow>, StorageError> {
        self.conn()?
            .query_row(
                &format!("SELECT {ITEM_COLUMNS} FROM vcs_items WHERE repo_id = ?1 AND federation_stream_id = ?2"),
                params![repo_id.as_str(), federation_stream_id],
                item_from_row,
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    async fn put_item(&mut self, item: &ItemRow) -> Result<(), StorageError> {
        self.conn()?
            .execute(
                "INSERT INTO vcs_items (repo_id, item_id, item_type, parent_folder_id, name, name_key, head_seq, ref_head_seq, lifecycle_state, created_at, created_by, sequencing_mode, federation_stream_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) ON CONFLICT(repo_id, item_id) DO UPDATE SET item_type = excluded.item_type, parent_folder_id = excluded.parent_folder_id, name = excluded.name, name_key = excluded.name_key, head_seq = excluded.head_seq, ref_head_seq = excluded.ref_head_seq, lifecycle_state = excluded.lifecycle_state, created_at = excluded.created_at, created_by = excluded.created_by, sequencing_mode = excluded.sequencing_mode, federation_stream_id = excluded.federation_stream_id",
                params![
                    item.repo_id.as_str(),
                    item.item_id.as_str(),
                    item.item_type,
                    item.parent_folder_id.as_ref().map(ItemId::as_str),
                    item.name,
                    item.name_key,
                    item.head_seq,
                    item.ref_head_seq,
                    lifecycle_text(item.lifecycle_state),
                    format_timestamp(item.created_at),
                    item.created_by,
                    sequencing_text(item.sequencing_mode),
                    item.federation_stream_id,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    async fn update_item_heads(
        &mut self,
        repo_id: &RepoId,
        updates: &[(ItemId, i64)],
    ) -> Result<(), StorageError> {
        for (item_id, head_seq) in updates {
            let changed = self
                .conn()?
                .execute(
                    "UPDATE vcs_items SET head_seq = ?3 WHERE repo_id = ?1 AND item_id = ?2",
                    params![repo_id.as_str(), item_id.as_str(), head_seq],
                )
                .map_err(map_sqlite_error)?;
            if changed != 1 {
                return Err(StorageError::Io {
                    detail: format!("missing item during head update: {item_id}"),
                });
            }
        }
        Ok(())
    }

    async fn update_item_ref_heads(
        &mut self,
        repo_id: &RepoId,
        updates: &[(ItemId, i64, i64)],
    ) -> Result<(), StorageError> {
        for (item_id, expected, next) in updates {
            let changed = self
                .conn()?
                .execute(
                    "UPDATE vcs_items SET ref_head_seq = ?4 \
                     WHERE repo_id = ?1 AND item_id = ?2 AND ref_head_seq = ?3",
                    params![repo_id.as_str(), item_id.as_str(), expected, next],
                )
                .map_err(map_sqlite_error)?;
            if changed != 1 {
                return Err(StorageError::Io {
                    detail: format!(
                        "ref head compare-and-update mismatch: repo_id={} item_id={} expected={}",
                        repo_id, item_id, expected
                    ),
                });
            }
        }
        Ok(())
    }

    async fn get_commit(
        &mut self,
        repo_id: &RepoId,
        commit_id: &CommitId,
    ) -> Result<Option<CommitRow>, StorageError> {
        self.conn()?
            .query_row(
                &format!(
                    "SELECT {COMMIT_COLUMNS} FROM vcs_commits WHERE repo_id = ?1 AND commit_id = ?2"
                ),
                params![
                    repo_id.as_str(),
                    commit_id_to_digest_bytes(commit_id).to_vec()
                ],
                commit_from_row,
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    async fn get_commit_by_operation_id(
        &mut self,
        repo_id: &RepoId,
        operation_id: &str,
    ) -> Result<Option<CommitRow>, StorageError> {
        self.conn()?
            .query_row(
                &format!("SELECT {COMMIT_COLUMNS} FROM vcs_commits WHERE repo_id = ?1 AND operation_id = ?2 AND commit_id IS NOT NULL"),
                params![repo_id.as_str(), operation_id],
                commit_from_row,
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    async fn list_events_for_item(
        &mut self,
        repo_id: &RepoId,
        item_id: &ItemId,
        from_seq: i64,
        limit: usize,
    ) -> Result<Vec<EventRow>, StorageError> {
        list_events_for_item(self.conn()?, repo_id, item_id, from_seq, limit)
    }

    async fn sequence_repo_activity(
        &mut self,
        repo_id: &RepoId,
        limit: usize,
    ) -> Result<usize, StorageError> {
        if limit == 0 {
            return Ok(0);
        }
        ensure_history_generation(self.conn()?, repo_id)?;
        let now = format_timestamp(Utc::now());
        self.conn()?
            .execute(
                "INSERT INTO vcs_repo_activity_heads (repo_id, next_repo_seq, updated_at) VALUES (?1, 1, ?2) ON CONFLICT(repo_id) DO NOTHING",
                params![repo_id.as_str(), now],
            )
            .map_err(map_sqlite_error)?;
        let limit = checked_limit(limit)?;
        let mut statement = self
            .conn()?
            .prepare("SELECT commit_pk FROM vcs_commits WHERE repo_id = ?1 AND commit_id IS NOT NULL AND repo_seq IS NULL ORDER BY created_at, commit_pk LIMIT ?2")
            .map_err(map_sqlite_error)?;
        let commit_pks = statement
            .query_map(params![repo_id.as_str(), limit], |row| {
                row.get::<_, String>(0)
            })
            .map_err(map_sqlite_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_sqlite_error)?;
        drop(statement);
        if commit_pks.is_empty() {
            return Ok(0);
        }
        let next = checked_publication_counter(self.conn()?, repo_id)?;
        for (offset, commit_pk) in commit_pks.iter().enumerate() {
            let offset = i64::try_from(offset).map_err(|_| StorageError::Other {
                detail: "sequencer offset overflow".to_string(),
            })?;
            let repo_seq = next
                .checked_add(offset)
                .ok_or_else(|| StorageError::Other {
                    detail: "repository sequence overflow".to_string(),
                })?;
            self.conn()?
                .execute(
                    "UPDATE vcs_commits SET repo_seq = ?3 WHERE repo_id = ?1 AND commit_pk = ?2 AND repo_seq IS NULL",
                    params![repo_id.as_str(), commit_pk, repo_seq],
                )
                .map_err(map_sqlite_error)?;
            publish_checkpoint(self.conn()?, repo_id, commit_pk)?;
        }
        if !commit_pks.is_empty() {
            let count = i64::try_from(commit_pks.len()).map_err(|_| StorageError::Other {
                detail: "sequencer count overflow".to_string(),
            })?;
            let new_next = next.checked_add(count).ok_or_else(|| StorageError::Other {
                detail: "repository sequence overflow".to_string(),
            })?;
            self.conn()?
                .execute(
                    "UPDATE vcs_repo_activity_heads SET next_repo_seq = ?2, updated_at = ?3 WHERE repo_id = ?1",
                    params![repo_id.as_str(), new_next, now],
                )
                .map_err(map_sqlite_error)?;
        }
        Ok(commit_pks.len())
    }

    async fn start_commit(
        &mut self,
        repo_id: &RepoId,
        commit_pk: &CommitPk,
        repo_seq: Option<i64>,
        operation_id: Option<&str>,
        format_version: i32,
        import_job_id: Option<&str>,
        author_id: &str,
        committer_id: &str,
        message: Option<&str>,
        occurred_at: Option<Timestamp>,
        created_at: Timestamp,
    ) -> Result<(), StorageError> {
        self.conn()?
            .execute(
                "INSERT INTO vcs_commits (repo_id, commit_pk, commit_id, repo_seq, operation_id, format_version, import_job_id, author_id, committer_id, message, occurred_at, created_at, event_count, events_digest) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, NULL)",
                params![
                    repo_id.as_str(),
                    commit_pk.as_str(),
                    repo_seq,
                    operation_id,
                    format_version,
                    import_job_id,
                    author_id,
                    committer_id,
                    message,
                    occurred_at.map(format_timestamp),
                    format_timestamp(created_at),
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    async fn append_event(&mut self, event: &EventInsertRow) -> Result<(), StorageError> {
        if let Some(metadata) = &event.metadata {
            crate::metadata::insert(self.conn()?, &event.repo_id, metadata)?;
        }
        self.conn()?
            .execute(
                "INSERT INTO vcs_events (repo_id, commit_pk, commit_event_idx, item_id, item_seq, event_type, payload, event_payload_digest, created_at, parent_folder_id, parent_folder_id_before) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    event.repo_id.as_str(),
                    event.commit_pk.as_str(),
                    event.commit_event_idx,
                    event.item_id.as_str(),
                    event.item_seq,
                    event.event_type,
                    event.payload,
                    event.event_payload_digest.to_vec(),
                    format_timestamp(event.created_at),
                    event.parent_folder_id.as_ref().map(ItemId::as_str),
                    event.parent_folder_id_before.as_ref().map(ItemId::as_str),
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    async fn append_ref_event(&mut self, event: &RefEventInsertRow) -> Result<(), StorageError> {
        if let Some(metadata) = &event.metadata {
            crate::metadata::insert(self.conn()?, &event.repo_id, metadata)?;
        }
        self.conn()?
            .execute(
                "INSERT INTO vcs_ref_events (repo_id, commit_pk, commit_event_idx, owner_item_id, ref_seq, event_type, payload, event_payload_digest, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    event.repo_id.as_str(),
                    event.commit_pk.as_str(),
                    event.commit_event_idx,
                    event.owner_item_id.as_str(),
                    event.ref_seq,
                    event.event_type,
                    event.payload,
                    event.event_payload_digest.to_vec(),
                    format_timestamp(event.created_at),
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    async fn seal_commit(
        &mut self,
        repo_id: &RepoId,
        commit_pk: &CommitPk,
        commit_id: &CommitId,
        events_digest: &Sha256Digest,
        event_count: i32,
    ) -> Result<(), StorageError> {
        let changed = self
            .conn()?
            .execute(
                "UPDATE vcs_commits SET commit_id = ?3, events_digest = ?4, event_count = ?5 WHERE repo_id = ?1 AND commit_pk = ?2",
                params![
                    repo_id.as_str(),
                    commit_pk.as_str(),
                    commit_id_to_digest_bytes(commit_id).to_vec(),
                    sha256_digest_to_digest_bytes(events_digest).to_vec(),
                    event_count,
                ],
            )
            .map_err(map_sqlite_error)?;
        if changed != 1 {
            return Err(StorageError::Io {
                detail: format!("missing commit during seal: {commit_pk}"),
            });
        }
        publish_checkpoint(self.conn()?, repo_id, commit_pk.as_str())?;
        Ok(())
    }

    async fn federation_domain_last_seq_for_update(
        &mut self,
        repo_id: &RepoId,
        domain_id: &str,
    ) -> Result<i64, StorageError> {
        let now = format_timestamp(Utc::now());
        self.conn()?
            .execute(
                "INSERT INTO vcs_federation_domains (repo_id, domain_id, last_domain_seq, created_at, updated_at) VALUES (?1, ?2, 0, ?3, ?3) ON CONFLICT(repo_id, domain_id) DO NOTHING",
                params![repo_id.as_str(), domain_id, now],
            )
            .map_err(map_sqlite_error)?;
        self.conn()?
            .query_row(
                "SELECT last_domain_seq FROM vcs_federation_domains WHERE repo_id = ?1 AND domain_id = ?2",
                params![repo_id.as_str(), domain_id],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)
    }

    async fn federation_domain_set_last_seq(
        &mut self,
        repo_id: &RepoId,
        domain_id: &str,
        last_domain_seq: i64,
        updated_at: Timestamp,
    ) -> Result<(), StorageError> {
        let changed = self
            .conn()?
            .execute(
                "UPDATE vcs_federation_domains SET last_domain_seq = ?3, updated_at = ?4 WHERE repo_id = ?1 AND domain_id = ?2",
                params![repo_id.as_str(), domain_id, last_domain_seq, format_timestamp(updated_at)],
            )
            .map_err(map_sqlite_error)?;
        if changed != 1 {
            return Err(StorageError::Io {
                detail: format!("missing federation domain: repo={repo_id} domain={domain_id}"),
            });
        }
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<(), StorageError> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Other {
            detail: "SQLite transaction connection is missing".to_string(),
        })?;
        conn.execute_batch("COMMIT").map_err(map_sqlite_error)?;
        self.active = false;
        Ok(())
    }

    async fn rollback(mut self: Box<Self>) -> Result<(), StorageError> {
        let conn = self.conn.take().ok_or_else(|| StorageError::Other {
            detail: "SQLite transaction connection is missing".to_string(),
        })?;
        conn.execute_batch("ROLLBACK").map_err(map_sqlite_error)?;
        self.active = false;
        Ok(())
    }
}

#[cfg(test)]
mod checked_append_work_tests {
    use super::*;
    use synck_vcs::config::CoreConfig;
    use synck_vcs::repo::Repo;
    use synck_vcs::storage::Storage;
    use synck_vcs::types::{CommitProposal, CreateItem, EventContent, EventProposal};

    #[derive(Clone, Copy)]
    enum Case {
        Content,
        Rename,
        Move,
    }

    #[tokio::test]
    async fn checked_file_folder_work_ignores_unrelated_repository_items() {
        for case in [Case::Content, Case::Rename, Case::Move] {
            let small = measured_checked_append(case, 100).await;
            let large = measured_checked_append(case, 10_000).await;
            assert_eq!(small, large);

            let expected = match case {
                Case::Content => AppendMetrics {
                    item_batches: vec![(1, 1)],
                    single_item_reads: Vec::new(),
                    conflict_key_batches: vec![1],
                    descendant_batches: Vec::new(),
                },
                Case::Rename => AppendMetrics {
                    item_batches: vec![(1, 1), (1, 1)],
                    single_item_reads: Vec::new(),
                    conflict_key_batches: vec![2],
                    descendant_batches: Vec::new(),
                },
                Case::Move => AppendMetrics {
                    // Source lookup, locked touched/destination rows, then the
                    // fixed parent-existence and acyclicity ancestor reads.
                    item_batches: vec![(1, 1), (2, 2), (1, 1), (1, 1)],
                    single_item_reads: Vec::new(),
                    conflict_key_batches: vec![3],
                    descendant_batches: Vec::new(),
                },
            };
            assert_eq!(small, expected);
        }
    }

    async fn measured_checked_append(case: Case, unrelated_items: usize) -> AppendMetrics {
        let directory = tempfile::tempdir().expect("tempdir");
        let storage = SqliteStorage::new(directory.path().join("repository.sqlite3"));
        storage.init_schema().await.expect("initialize schema");
        let repo_id = RepoId::new();
        let repo = Repo::new(
            Storage::new(Arc::new(storage.clone())),
            CoreConfig::default(),
        );

        let file_id = ItemId::new();
        let first_parent_id = if matches!(case, Case::Rename | Case::Move) {
            Some(ItemId::new())
        } else {
            None
        };
        let second_parent_id = if matches!(case, Case::Move) {
            Some(ItemId::new())
        } else {
            None
        };
        let mut setup_events = Vec::new();
        if let Some(parent_id) = &first_parent_id {
            setup_events.push(create_event(parent_id.clone(), "FOLDER", None, "first"));
        }
        if let Some(parent_id) = &second_parent_id {
            setup_events.push(create_event(parent_id.clone(), "FOLDER", None, "second"));
        }
        setup_events.push(create_event(
            file_id.clone(),
            "FILE",
            first_parent_id,
            "file.txt",
        ));
        repo.append(
            repo_id.clone(),
            CommitProposal {
                author_id: "test".to_string(),
                committer_id: "test".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("setup".to_string()),
                events: setup_events,
            },
        )
        .await
        .expect("setup append");
        insert_unrelated_items(&storage, &repo_id, unrelated_items);

        let (event_type, set_content, set_parent, set_name) = match case {
            Case::Content => (
                "FILE::SET_CONTENT",
                Some(EventContent::bytes(synck_vcs::types::ObjectId::from_digest([42;32]))),
                None,
                None,
            ),
            Case::Rename => (
                "FILE::SET_NAME",
                None,
                None,
                Some("renamed.txt".to_string()),
            ),
            Case::Move => ("FILE::SET_PARENT", None, Some(second_parent_id), None),
        };
        storage.reset_append_metrics();
        repo.append_file_folder_checked(
            repo_id,
            CommitProposal {
                author_id: "test".to_string(),
                committer_id: "test".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("measured".to_string()),
                events: vec![EventProposal {
                    item_id: Some(file_id),
                    event_type: event_type.to_string(),
                    set_metadata: None,
                    set_content,
                    create_item: None,
                    set_parent,
                    set_name,
                    set_tombstoned: None,
                    set_in_trash: None,
                    set_is_federated: None,
                    set_federation_stream_id: None,
                    base_seq: Some(1),
                }],
            },
            &[],
            &[],
        )
        .await
        .expect("checked append");
        storage.append_metrics()
    }

    fn create_event(
        item_id: ItemId,
        item_type: &str,
        parent_folder_id: Option<ItemId>,
        name: &str,
    ) -> EventProposal {
        EventProposal {
            item_id: Some(item_id),
            event_type: format!("{item_type}::CREATE"),
            set_metadata: None,
            set_content: None,
            create_item: Some(CreateItem {
                item_type: item_type.to_string(),
                parent_folder_id,
                name: name.to_string(),
            }),
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: None,
        }
    }

    fn insert_unrelated_items(storage: &SqliteStorage, repo_id: &RepoId, count: usize) {
        let mut connection = storage.open_writable().expect("open database");
        let transaction = connection.transaction().expect("begin transaction");
        let created_at = format_timestamp(Utc::now());
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO vcs_items (
                        repo_id, item_id, item_type, parent_folder_id, name, name_key,
                        head_seq, lifecycle_state, created_at, created_by,
                        sequencing_mode, federation_stream_id
                     ) VALUES (?1, ?2, 'FILE', NULL, ?3, ?3, 1, 'LIVE', ?4,
                               'test', 'LOCAL', NULL)",
                )
                .expect("prepare unrelated-item insert");
            for index in 0..count {
                let item_id = ItemId::new();
                let name = format!("unrelated-{index:05}");
                statement
                    .execute(params![
                        repo_id.as_str(),
                        item_id.as_str(),
                        name,
                        created_at
                    ])
                    .expect("insert unrelated item");
            }
        }
        transaction.commit().expect("commit unrelated items");
    }
}

fn ensure_history_generation(conn: &Connection, repo_id: &RepoId) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO vcs_history_generations (repo_id,generation) VALUES (?1,?2) ON CONFLICT(repo_id) DO NOTHING",
        params![repo_id.as_str(), CommitPk::new().as_str()],
    ).map_err(map_sqlite_error)?;
    Ok(())
}

/// Called only inside the transaction which assigns or imports the position.
fn publish_checkpoint(
    conn: &Connection,
    repo_id: &RepoId,
    commit_pk: &str,
) -> Result<(), StorageError> {
    let (position, commit, created): (Option<i64>, Vec<u8>, String) = conn.query_row(
        "SELECT repo_seq,commit_id,created_at FROM vcs_commits WHERE repo_id=?1 AND commit_pk=?2",
        params![repo_id.as_str(), commit_pk],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).map_err(map_sqlite_error)?;
    let Some(position) = position else {
        return Ok(());
    };
    ensure_history_generation(conn, repo_id)?;
    let previous: Option<(i64, Vec<u8>)> = conn.query_row(
        "SELECT repo_seq,checkpoint FROM vcs_checkpoints WHERE repo_id=?1 ORDER BY repo_seq DESC LIMIT 1",
        [repo_id.as_str()], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(map_sqlite_error)?;
    let previous = match previous {
        None if position == 1 => synck_vcs::checkpoint::GENESIS,
        Some((seq, bytes)) if seq.checked_add(1) == Some(position) => {
            bytes.try_into().map_err(|_| StorageError::Other {
                detail: "invalid checkpoint digest length".into(),
            })?
        }
        _ => {
            return Err(StorageError::Other {
                detail: "checkpoint publication must be consecutive".into(),
            });
        }
    };
    let commit_id = commit_id_from_digest_bytes(&commit).map_err(|error| StorageError::Other {
        detail: error.to_string(),
    })?;
    let timestamp = parse_timestamp(&created)?;
    let digest = synck_vcs::checkpoint::digest(&previous, position as u64, &commit_id, timestamp)
        .map_err(|error| StorageError::Other {
        detail: error.to_string(),
    })?;
    conn.execute(
        "INSERT INTO vcs_checkpoints (repo_id,repo_seq,commit_id,checkpoint) VALUES (?1,?2,?3,?4)",
        params![repo_id.as_str(), position, commit, digest.as_slice()],
    )
    .map_err(map_sqlite_error)?;
    Ok(())
}

impl SqliteStorage {
    /// Verify publication independently from historical-state caches.
    /// Unsequenced accepted commits intentionally have no checkpoint.
    pub fn verify_checkpoints(&self, repo_id: &RepoId) -> Result<(), StorageError> {
        let conn = self.open_readonly()?;
        conn.execute_batch("BEGIN").map_err(map_sqlite_error)?;
        verify_checkpoints_in(&conn, repo_id)
    }

    /// Repair only the derived counter, after verifying published history.
    /// Repair and sequencing use the same SQLite write exclusion.
    pub fn repair_publication_counter(&self, repo_id: &RepoId) -> Result<(), StorageError> {
        let mut conn = self.open_writable()?;
        let txn = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        let next = next_published_sequence(&txn, repo_id)?;
        if next == 1 {
            ensure_history_generation(&txn, repo_id)?;
        }
        verify_checkpoints_in(&txn, repo_id)?;
        txn.execute(
            "INSERT INTO vcs_repo_activity_heads (repo_id,next_repo_seq,updated_at) VALUES (?1,?2,?3)
             ON CONFLICT(repo_id) DO UPDATE SET next_repo_seq=excluded.next_repo_seq,updated_at=excluded.updated_at",
            params![repo_id.as_str(), next, format_timestamp(Utc::now())],
        ).map_err(map_sqlite_error)?;
        txn.commit().map_err(map_sqlite_error)
    }
}

fn verify_checkpoints_in(conn: &Connection, repo_id: &RepoId) -> Result<(), StorageError> {
    let generation: String = conn
        .query_row(
            "SELECT generation FROM vcs_history_generations WHERE repo_id=?1",
            [repo_id.as_str()],
            |row| row.get(0),
        )
        .map_err(map_sqlite_error)?;
    CommitPk::parse(generation).map_err(|error| StorageError::Other {
        detail: format!("invalid history generation: {error}"),
    })?;
    let mut query = conn.prepare(
        "SELECT c.repo_seq,c.commit_id,c.created_at,p.commit_id,p.checkpoint FROM vcs_commits c LEFT JOIN vcs_checkpoints p ON p.repo_id=c.repo_id AND p.repo_seq=c.repo_seq WHERE c.repo_id=?1 AND c.repo_seq IS NOT NULL ORDER BY c.repo_seq",
    ).map_err(map_sqlite_error)?;
    let mut rows = query.query([repo_id.as_str()]).map_err(map_sqlite_error)?;
    let mut previous = synck_vcs::checkpoint::GENESIS;
    let mut last = 0i64;
    let mut count = 0i64;
    while let Some(row) = rows.next().map_err(map_sqlite_error)? {
        let seq: i64 = row.get(0).map_err(map_sqlite_error)?;
        let commit: Vec<u8> = row.get(1).map_err(map_sqlite_error)?;
        let accepted: String = row.get(2).map_err(map_sqlite_error)?;
        let published_commit: Option<Vec<u8>> = row.get(3).map_err(map_sqlite_error)?;
        let published_digest: Option<Vec<u8>> = row.get(4).map_err(map_sqlite_error)?;
        if last.checked_add(1) != Some(seq)
            || published_commit.as_deref() != Some(commit.as_slice())
        {
            return Err(StorageError::Other {
                detail: format!("checkpoint placement mismatch at sequence {seq}"),
            });
        }
        let id = commit_id_from_digest_bytes(&commit).map_err(|error| StorageError::Other {
            detail: error.to_string(),
        })?;
        let digest = synck_vcs::checkpoint::digest(
            &previous,
            seq as u64,
            &id,
            parse_timestamp(&accepted)?,
        )
        .map_err(|error| StorageError::Other {
            detail: error.to_string(),
        })?;
        if published_digest.as_deref() != Some(digest.as_slice()) {
            return Err(StorageError::Other {
                detail: format!("checkpoint digest mismatch at sequence {seq}"),
            });
        }
        previous = digest;
        last = seq;
        count += 1;
    }
    let stored: i64 = conn
        .query_row(
            "SELECT count(*) FROM vcs_checkpoints WHERE repo_id=?1",
            [repo_id.as_str()],
            |row| row.get(0),
        )
        .map_err(map_sqlite_error)?;
    if stored != count {
        return Err(StorageError::Other {
            detail: "checkpoint without a published commit".into(),
        });
    }
    Ok(())
}

// MAX uses the publication index; pending accepted commits do not consume positions.
fn next_published_sequence(conn: &Connection, repo_id: &RepoId) -> Result<i64, StorageError> {
    let last: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(repo_seq),0) FROM vcs_commits WHERE repo_id=?1",
            [repo_id.as_str()],
            |row| row.get(0),
        )
        .map_err(map_sqlite_error)?;
    last.checked_add(1).ok_or_else(|| StorageError::Other {
        detail: "repository sequence exhausted".into(),
    })
}

fn checked_publication_counter(conn: &Connection, repo_id: &RepoId) -> Result<i64, StorageError> {
    let expected = next_published_sequence(conn, repo_id)?;
    let stored: Option<i64> = conn
        .query_row(
            "SELECT next_repo_seq FROM vcs_repo_activity_heads WHERE repo_id=?1",
            [repo_id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_sqlite_error)?;
    // A new, never-published repository has no counter yet.
    if stored != Some(expected) && !(stored.is_none() && expected == 1) {
        let stored = stored.map_or_else(|| "none".to_owned(), |seq| seq.to_string());
        return Err(StorageError::Other {
            detail: format!(
                "integrity error: repository publication counter disagrees with history: expected {expected}, stored {stored}; run synck repository finalize"
            ),
        });
    }
    Ok(expected)
}
