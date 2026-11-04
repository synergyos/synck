//! `state.sqlite3` in the state root: tokens, upload staging and the request
//! digests of submitted proposals. Nothing here is repository history.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::{Error, Result};

pub const STATE_FILE: &str = "state.sqlite3";
const SCHEMA_VERSION: i32 = 1;
const SCHEMA: &str = "
CREATE TABLE tokens (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  access TEXT NOT NULL CHECK (access IN ('READ', 'WRITE')),
  secret_sha256 BLOB NOT NULL CHECK (length(secret_sha256) = 32),
  created_at INTEGER NOT NULL
) STRICT;
CREATE TABLE token_repositories (
  token_id TEXT NOT NULL REFERENCES tokens(id) ON DELETE CASCADE,
  repo_id TEXT NOT NULL,
  PRIMARY KEY (token_id, repo_id)
) STRICT, WITHOUT ROWID;
CREATE TABLE uploads (
  repo_id TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  generation TEXT NOT NULL,
  expires_at INTEGER NOT NULL,
  PRIMARY KEY (repo_id, operation_id)
) STRICT;
CREATE INDEX uploads_by_expiry ON uploads (expires_at);
CREATE TABLE upload_pages (
  repo_id TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  first INTEGER NOT NULL,
  next INTEGER NOT NULL,
  records BLOB NOT NULL,
  PRIMARY KEY (repo_id, operation_id, first),
  FOREIGN KEY (repo_id, operation_id) REFERENCES uploads (repo_id, operation_id) ON DELETE CASCADE
) STRICT;
CREATE TABLE submissions (
  repo_id TEXT NOT NULL,
  operation_id TEXT NOT NULL,
  request_digest TEXT NOT NULL,
  PRIMARY KEY (repo_id, operation_id)
) STRICT, WITHOUT ROWID;
";

/// SQLite refuses a link anywhere in the path, so resolve the root's own
/// (macOS `/var`, for one); the state file itself must not be a link.
fn open(root: &Path, flags: OpenFlags) -> Result<Connection> {
    let root = root
        .canonicalize()
        .map_err(|error| Error::filesystem("resolve state root", root, error))?;
    let conn = Connection::open_with_flags(
        root.join(STATE_FILE),
        flags | OpenFlags::SQLITE_OPEN_NOFOLLOW | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(conn)
}

/// Create the state database in a new state root.
pub(crate) fn create(root: &Path) -> Result<()> {
    let conn = open(
        root,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
    if version != 0 || tables != 0 {
        return Err(Error::State(format!("{STATE_FILE} already exists")));
    }
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(&format!(
        "BEGIN IMMEDIATE; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
    ))?;
    Ok(())
}

pub(crate) fn open_existing(root: &Path) -> Result<Connection> {
    let conn = open(root, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(Error::State(format!(
            "{STATE_FILE} has schema {version}; this server reads schema {SCHEMA_VERSION}"
        )));
    }
    Ok(conn)
}
