//! The one way Synck opens a SQLite database.

use std::path::Path;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};

/// Open a database with the settings every connection needs.
///
/// On macOS, fsync leaves data in the drive's cache, so `synchronous = FULL`
/// alone does not survive power loss. These pragmas make SQLite issue
/// `F_FULLFSYNC`. They change nothing on other platforms.
///
/// A repository can be a directory someone sent, so its schema is untrusted
/// until it is compared with the expected one, and that comparison already reads
/// the file. Defensive mode refuses schema and page writes, an untrusted schema
/// cannot call functions with side effects, and corrupt pages fail early.
pub fn open_connection(path: impl AsRef<Path>, flags: OpenFlags) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(path, flags)?;
    connection.pragma_update(None, "fullfsync", true)?;
    connection.pragma_update(None, "checkpoint_fullfsync", true)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, false)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false)?;
    connection.pragma_update(None, "cell_size_check", true)?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_flush_the_drive_cache() {
        let dir = tempfile::tempdir().unwrap();
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;
        let connection = open_connection(dir.path().join("database.sqlite3"), flags).unwrap();
        for pragma in ["fullfsync", "checkpoint_fullfsync"] {
            let value: i64 = connection.pragma_query_value(None, pragma, |row| row.get(0)).unwrap();
            assert_eq!(value, 1, "{pragma}");
        }
    }

    #[test]
    fn connections_do_not_trust_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE;
        let connection = open_connection(dir.path().join("database.sqlite3"), flags).unwrap();
        connection.execute_batch("CREATE TABLE t(a)").unwrap();
        let trusted: i64 = connection
            .pragma_query_value(None, "trusted_schema", |row| row.get(0))
            .unwrap();
        assert_eq!(trusted, 0);
        connection.execute_batch("PRAGMA writable_schema = ON").unwrap();
        connection
            .execute_batch("UPDATE sqlite_schema SET sql = 'CREATE TABLE t(b)'")
            .unwrap_err();
    }
}
