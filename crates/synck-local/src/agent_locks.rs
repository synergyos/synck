//! Local path reservations. Every entry point runs under RepositoryLock; leases
//! never hold that OS lock while an agent is editing. These are not history/cache.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use uuid::Uuid;

use crate::error::{IoContext, LocalError, Result};
use crate::repository::{Repository, is_link_or_reparse, rename_noreplace, sync_directory};

const FILE: &str = "agent-locks.sqlite3";
const APPLICATION_ID: i64 = 0x53414c4b;
const SCHEMA: &str = "
CREATE TABLE identity (singleton INTEGER PRIMARY KEY CHECK(singleton=1), repo_id TEXT NOT NULL) STRICT;
CREATE TABLE reservations (
 path_key TEXT PRIMARY KEY, path TEXT NOT NULL, owner TEXT NOT NULL,
 token TEXT NOT NULL, expires_at INTEGER NOT NULL CHECK(expires_at>0)
) STRICT;";

#[derive(Clone, Debug, Serialize)]
pub struct AgentLock {
    pub path: String,
    pub owner: String,
    pub expires_at: i64,
}

#[derive(Debug, Serialize)]
pub struct AgentLease {
    pub token: String,
    pub locks: Vec<AgentLock>,
}

struct Reservation {
    info: AgentLock,
    key: String,
    token: String,
}

fn invalid(message: impl Into<String>) -> LocalError {
    LocalError::Invalid(message.into())
}
fn corrupt(message: impl Into<String>) -> LocalError {
    LocalError::Integrity(message.into())
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn expiry(ttl: u64) -> Result<i64> {
    if !(1..=86_400).contains(&ttl) {
        return Err(invalid("lock TTL must be between 1 and 86400 seconds"));
    }
    now()
        .checked_add(ttl as i64)
        .ok_or_else(|| invalid("lock expiry overflow"))
}

pub(crate) fn validate_tokens(tokens: &[String]) -> Result<()> {
    for token in tokens {
        if Uuid::parse_str(token).is_err() {
            return Err(invalid("lock token must be a UUID"));
        }
    }
    Ok(())
}

// The same portable name equivalence used by VCS namespace admission. This also
// prevents two agents reserving Foo and foo on a case-sensitive filesystem.
fn key(path: &str) -> Result<String> {
    if path.is_empty() {
        return Err(invalid("reservation path must not be empty"));
    }
    let keys = path
        .split('/')
        .map(|part| synck_vcs::naming::compute_name_key(part).map_err(|e| invalid(e.to_string())))
        .collect::<Result<Vec<_>>>()?;
    if keys[0] == ".synck" {
        return Err(invalid("cannot reserve repository control files"));
    }
    Ok(keys.join("/"))
}
fn covers(reservation: &str, path: &str) -> bool {
    path == reservation
        || path
            .strip_prefix(reservation)
            .is_some_and(|s| s.starts_with('/'))
}
fn overlaps(a: &str, b: &str) -> bool {
    covers(a, b) || covers(b, a)
}

fn open(root: &Path, repo_id: &str, create: bool) -> Result<Option<Connection>> {
    let metadata = root.join(".synck");
    let path = metadata.join(FILE);
    synck_vcs_sqlite::validate_database_family(&path, true).map_err(|e| corrupt(e.to_string()))?;
    if !path.try_exists().at("inspect agent locks", &path)? {
        if !create {
            return Ok(None);
        }
        let temporary = metadata.join(format!("agent-locks-{}.sqlite3", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options
                .open(&temporary)
                .at("create agent locks", &temporary)?;
            let connection = Connection::open_with_flags(
                &temporary,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            connection.execute_batch("PRAGMA synchronous=FULL; BEGIN IMMEDIATE;")?;
            connection.execute_batch(SCHEMA)?;
            connection.pragma_update(None, "application_id", APPLICATION_ID)?;
            connection.pragma_update(None, "user_version", 1)?;
            connection.execute("INSERT INTO identity VALUES(1,?1)", [repo_id])?;
            connection.execute_batch("COMMIT")?;
            drop(connection);
            file.sync_all().at("sync agent locks", &temporary)?;
            drop(file);
            rename_noreplace(&temporary, &path).at("publish agent locks", &path)?;
            sync_directory(&metadata)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
    }
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let application: i64 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    static EXPECTED: OnceLock<synck_vcs_sqlite::schema::SchemaShape> = OnceLock::new();
    let expected = EXPECTED.get_or_init(|| {
        let db = Connection::open_in_memory().expect("open reservation schema");
        db.execute_batch(SCHEMA).expect("create reservation schema");
        synck_vcs_sqlite::schema::inspect(&db, "").expect("inspect reservation schema")
    });
    if application != APPLICATION_ID
        || version != 1
        || &synck_vcs_sqlite::schema::inspect(&connection, "")? != expected
    {
        return Err(corrupt("invalid agent lock database schema"));
    }
    let identity: String =
        connection.query_row("SELECT repo_id FROM identity WHERE singleton=1", [], |r| {
            r.get(0)
        })?;
    if identity != repo_id {
        return Err(corrupt("agent lock repository identity mismatch"));
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(Some(connection))
}

fn reservations(connection: &Connection) -> Result<Vec<Reservation>> {
    let mut statement = connection.prepare(
        "SELECT path_key,path,owner,token,expires_at FROM reservations ORDER BY path_key",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(Reservation {
            key: row.get(0)?,
            info: AgentLock {
                path: row.get(1)?,
                owner: row.get(2)?,
                expires_at: row.get(4)?,
            },
            token: row.get(3)?,
        })
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let row = row?;
        if key(&row.info.path)? != row.key
            || row.info.owner.trim().is_empty()
            || row.info.owner.len() > 128
            || row.info.expires_at <= 0
            || Uuid::parse_str(&row.token).is_err()
        {
            return Err(corrupt("invalid agent reservation"));
        }
        entries.push(row);
    }
    Ok(entries)
}

// Export/archives can also create a reserved path inside their own checkout.
// Destinations outside this checkout remain the caller's responsibility.
pub(crate) fn guard_output(
    root: &Path,
    repo_id: &str,
    tokens: &[String],
    destination: &Path,
) -> Result<()> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let resolved = parent.canonicalize().at("resolve output parent", parent)?;
    let name = destination
        .file_name()
        .ok_or_else(|| invalid("output must name a destination"))?;
    let root = root.canonicalize().at("resolve repository root", root)?;
    if let Ok(relative) = resolved.join(name).strip_prefix(&root) {
        let path = relative
            .components()
            .map(|c| {
                c.as_os_str()
                    .to_str()
                    .ok_or_else(|| invalid("output path must be Unicode"))
            })
            .collect::<Result<Vec<_>>>()?
            .join("/");
        guard(&root, repo_id, tokens, Some(&[&path]))?;
    }
    Ok(())
}

pub(crate) fn guard(
    root: &Path,
    repo_id: &str,
    tokens: &[String],
    paths: Option<&[&str]>,
) -> Result<()> {
    let records = match open(root, repo_id, false)? {
        Some(db) => reservations(&db)?,
        None if tokens.is_empty() => return Ok(()),
        None => return Err(LocalError::LockLost),
    };
    let time = now();
    for token in tokens {
        if !records
            .iter()
            .any(|r| r.token == *token && r.info.expires_at > time)
        {
            return Err(LocalError::LockLost);
        }
    }
    let paths = paths
        .map(|paths| paths.iter().map(|p| key(p)).collect::<Result<Vec<_>>>())
        .transpose()?;
    for row in &records {
        if paths
            .as_ref()
            .is_none_or(|paths| paths.iter().any(|p| overlaps(p, &row.key)))
            && (row.info.expires_at <= time || !tokens.contains(&row.token))
        {
            return Err(LocalError::Locked {
                path: row.info.path.clone(),
                owner: row.info.owner.clone(),
            });
        }
    }
    // Supplying credentials opts into reserved writes. Never silently fall back
    // to an unreserved write after a path was released or a token was lost.
    if !tokens.is_empty() {
        if let Some(paths) = paths {
            for path in paths {
                if !records
                    .iter()
                    .any(|r| tokens.contains(&r.token) && covers(&r.key, &path))
                {
                    return Err(LocalError::LockLost);
                }
            }
        }
    }
    Ok(())
}

impl Repository {
    pub fn set_agent_lock_tokens(&mut self, tokens: &[String]) -> Result<()> {
        validate_tokens(tokens)?;
        self.agent_lock_tokens = tokens.to_vec();
        Ok(())
    }
    pub(crate) fn guard_agent_paths(&self, paths: &[&str]) -> Result<()> {
        guard(
            self.root_path(),
            self.repository_id().as_str(),
            &self.agent_lock_tokens,
            Some(paths),
        )
    }
    pub(crate) fn guard_agent_all(&self) -> Result<()> {
        guard(
            self.root_path(),
            self.repository_id().as_str(),
            &self.agent_lock_tokens,
            None,
        )
    }
    fn agent_path(&self, caller: &Path, path: &Path) -> Result<String> {
        let path = crate::commands::normalize_input_path(self.root_path(), caller, path)?;
        key(&path)?;
        let mut prefix = self.root_path().to_path_buf();
        for part in path.split('/') {
            prefix.push(part);
            match std::fs::symlink_metadata(&prefix) {
                Ok(m) if is_link_or_reparse(&m) => {
                    return Err(invalid("reservation paths must not traverse links"));
                }
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(e).at("inspect reservation path", &prefix),
            }
        }
        Ok(path)
    }
    pub fn agent_locks(&self) -> Result<Vec<AgentLock>> {
        Ok(
            match open(self.root_path(), self.repository_id().as_str(), false)? {
                Some(db) => reservations(&db)?.into_iter().map(|r| r.info).collect(),
                None => Vec::new(),
            },
        )
    }
    pub async fn acquire_agent_locks(
        &self,
        caller: &Path,
        paths: &[PathBuf],
        owner: &str,
        ttl: u64,
    ) -> Result<AgentLease> {
        if paths.is_empty() || owner.trim().is_empty() || owner.len() > 128 {
            return Err(invalid(
                "provide reservation paths and an owner of 1 to 128 bytes",
            ));
        }
        expiry(ttl)?;
        let paths = paths
            .iter()
            .map(|p| self.agent_path(caller, p))
            .collect::<Result<Vec<_>>>()?;
        let keys = paths.iter().map(|p| key(p)).collect::<Result<Vec<_>>>()?;
        for (i, a) in keys.iter().enumerate() {
            if keys[..i].iter().any(|b| overlaps(a, b)) {
                return Err(invalid("reservation paths overlap"));
            }
        }
        // Refuse to take over uncommitted work, including ignored/untracked files.
        let (committed, working, changes, _) = self.changes().await?;
        let mut changed_keys = Vec::new();
        for change in &changes {
            let new_folder = change.kind == crate::worktree::ChangeKind::Add
                && working
                    .get(&change.path)
                    .is_some_and(|node| node.kind == crate::worktree::NodeKind::Folder);
            changed_keys.push((key(&change.path)?, new_folder));
            if let Some(source) = &change.source_path {
                changed_keys.push((key(source)?, false));
            }
        }
        for (path, path_key) in paths.iter().zip(&keys) {
            let exists = self
                .root_path()
                .join(path)
                .try_exists()
                .at("inspect reservation target", path)?;
            let untracked = exists
                && crate::worktree::scan_entry_without_ignore(self.root(), path)?
                    .keys()
                    .any(|p| !committed.contains_key(p));
            if untracked
                || changed_keys.iter().any(|(k, new_folder)| {
                    // Creating a parent for one agent's file must not prevent
                    // another agent reserving an absent sibling. Other ancestor
                    // changes (including moves/deletions) still block acquisition.
                    overlaps(k, path_key)
                        && !(!exists && *new_folder && covers(k, path_key) && k != path_key)
                })
                || (!committed
                    .keys()
                    .any(|p| key(p).is_ok_and(|k| k == *path_key))
                    && self
                        .root_path()
                        .join(path)
                        .try_exists()
                        .at("inspect reservation target", path)?)
            {
                return Err(invalid(format!(
                    "cannot reserve dirty or untracked existing path: {path}; preserve and resolve its changes first"
                )));
            }
        }
        let mut db =
            open(self.root_path(), self.repository_id().as_str(), true)?.expect("created database");
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for row in reservations(&tx)? {
            if keys.iter().any(|p| overlaps(p, &row.key)) {
                // A child takeover must not drop the fence on the rest of an
                // expired folder. Acquire the whole old scope or release it.
                let covers_old_scope = keys.iter().any(|p| covers(p, &row.key));
                if row.info.expires_at > now() || !covers_old_scope {
                    return Err(LocalError::Locked {
                        path: row.info.path,
                        owner: row.info.owner,
                    });
                }
                tx.execute("DELETE FROM reservations WHERE path_key=?1", [row.key])?;
            }
        }
        let expires_at = expiry(ttl)?;
        let token = Uuid::new_v4().to_string();
        let mut locks = Vec::new();
        for (path, path_key) in paths.into_iter().zip(keys) {
            tx.execute(
                "INSERT INTO reservations VALUES(?1,?2,?3,?4,?5)",
                params![path_key, path, owner, token, expires_at],
            )?;
            locks.push(AgentLock {
                path,
                owner: owner.into(),
                expires_at,
            });
        }
        tx.commit()?;
        Ok(AgentLease { token, locks })
    }
    pub fn renew_agent_locks(&self, token: &str, ttl: u64) -> Result<()> {
        validate_tokens(&[token.into()])?;
        let expires_at = expiry(ttl)?;
        let mut db = open(self.root_path(), self.repository_id().as_str(), false)?
            .ok_or(LocalError::LockLost)?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let rows = reservations(&tx)?;
        if !rows.iter().any(|r| r.token == token)
            || rows
                .iter()
                .any(|r| r.token == token && r.info.expires_at <= now())
        {
            return Err(LocalError::LockLost);
        }
        tx.execute(
            "UPDATE reservations SET expires_at=?1 WHERE token=?2",
            params![expires_at, token],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Exact-token release is idempotent and never changes working files.
    pub fn release_agent_locks(&self, token: &str) -> Result<usize> {
        validate_tokens(&[token.into()])?;
        let Some(db) = open(self.root_path(), self.repository_id().as_str(), false)? else {
            return Ok(0);
        };
        db.execute("DELETE FROM reservations WHERE token=?1", [token])
            .map_err(Into::into)
    }
    /// Explicit recovery after inspecting abandoned work. Never removes files.
    pub fn break_agent_locks(&self, caller: &Path, paths: &[PathBuf]) -> Result<usize> {
        if paths.is_empty() {
            return Err(invalid("name paths to release"));
        }
        let keys = paths
            .iter()
            .map(|p| self.agent_path(caller, p).and_then(|p| key(&p)))
            .collect::<Result<Vec<_>>>()?;
        let Some(mut db) = open(self.root_path(), self.repository_id().as_str(), false)? else {
            return Ok(0);
        };
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut count = 0;
        for row in reservations(&tx)? {
            if keys.iter().any(|p| overlaps(p, &row.key)) {
                count += tx.execute("DELETE FROM reservations WHERE path_key=?1", [row.key])?;
            }
        }
        tx.commit()?;
        Ok(count)
    }
}
