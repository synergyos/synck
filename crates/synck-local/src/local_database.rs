//! Checkout state is local to this filesystem, never part of portable history.
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use synck_vcs_sqlite::{SqliteStorage, validate_database_family};

use crate::error::{LocalError, Result};

pub(crate) fn path(storage: &SqliteStorage) -> PathBuf {
    storage.path().with_extension("local.sqlite3")
}

pub(crate) fn open_readonly(storage: &SqliteStorage) -> Result<Connection> {
    open(storage, false, false)
}

pub(crate) fn open_writable(storage: &SqliteStorage) -> Result<Connection> {
    open(storage, true, false)
}

pub(crate) fn create(storage: &SqliteStorage) -> Result<Connection> {
    open(storage, true, true)
}

fn open(storage: &SqliteStorage, writable: bool, create: bool) -> Result<Connection> {
    // Validate history independently. Local transactions can read it through the
    // attachment, but cannot write it or couple acceptance to a cache update.
    drop(
        storage
            .open_readonly()
            .map_err(|e| LocalError::Integrity(e.to_string()))?,
    );
    let path = path(storage);
    validate_database_family(&path, create).map_err(|e| LocalError::Integrity(e.to_string()))?;
    let parent = path.parent().unwrap().canonicalize().map_err(|e| {
        LocalError::Invalid(format!("cannot resolve local database directory: {e}"))
    })?;
    let path = parent.join(path.file_name().unwrap());
    let mut flags = OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_URI;
    flags |= if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    if create {
        flags |= OpenFlags::SQLITE_OPEN_CREATE;
    }
    let connection = Connection::open_with_flags(&path, flags)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    if writable {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
    }
    let history = storage
        .path()
        .canonicalize()
        .map_err(|e| LocalError::Invalid(format!("cannot resolve history path: {e}")))?;
    let history = history
        .to_str()
        .ok_or_else(|| LocalError::Invalid("SQLite path is not UTF-8".into()))?;
    // URI syntax must not interpret characters from an ordinary filesystem path.
    #[cfg(windows)]
    let history = history.replace('\\', "/");
    let history = history
        .replace('%', "%25")
        .replace('?', "%3F")
        .replace('#', "%23");
    connection.execute(
        "ATTACH DATABASE ?1 AS history",
        [format!("file:{history}?mode=ro")],
    )?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use synck_vcs::storage::StorageBackend;
    use synck_vcs::types::RepoId;

    #[tokio::test]
    async fn local_transactions_cannot_write_attached_history() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo % # ü");
        std::fs::create_dir(&root).unwrap();
        let storage = SqliteStorage::new(root.join("repository.sqlite3"));
        storage.init_schema().await.unwrap();
        crate::repository::initialize_checkout_schema(&storage, &RepoId::new()).unwrap();
        let local = open_writable(&storage).unwrap();
        local
            .execute("DELETE FROM local_worktree_entries", [])
            .unwrap();
        let error = local
            .execute("DELETE FROM history.vcs_commits", [])
            .unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ReadOnly)
        );
        let canonical = storage.open_readonly().unwrap();
        let local_tables: i64 = canonical
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name GLOB 'local_*'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(local_tables, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn linked_local_database_is_rejected_without_writing_the_target() {
        let directory = tempfile::tempdir().unwrap();
        let storage = SqliteStorage::new(directory.path().join("repository.sqlite3"));
        storage.init_schema().await.unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"untouched").unwrap();
        std::os::unix::fs::symlink(&target, path(&storage)).unwrap();
        assert!(create(&storage).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
    }
}
