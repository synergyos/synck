//! The one way Synck opens a SQLite database.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// Open a database with the settings every connection needs.
///
/// On macOS, fsync leaves data in the drive's cache, so `synchronous = FULL`
/// alone does not survive power loss. These pragmas make SQLite issue
/// `F_FULLFSYNC`. They change nothing on other platforms.
pub fn open_connection(path: impl AsRef<Path>, flags: OpenFlags) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(path, flags)?;
    connection.pragma_update(None, "fullfsync", true)?;
    connection.pragma_update(None, "checkpoint_fullfsync", true)?;
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
}
