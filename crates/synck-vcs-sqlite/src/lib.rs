//! SQLite storage backend for Synck VCS.

pub mod bundle;
mod connection;
pub mod schema;
mod state;
mod storage;
mod metadata;

pub use connection::open_connection;
pub use state::SqliteStateStore;
pub use storage::{SqliteStorage, format_timestamp, parse_timestamp, validate_database_family};

#[test]
fn bundled_sqlite_includes_wal_reset_fix() {
    assert!(
        rusqlite::version_number() >= 3_051_003,
        "bundled SQLite {} predates the WAL-reset corruption fix",
        rusqlite::version()
    );
}

#[test]
fn published_schema_matches_standalone_specification() {
    assert_eq!(include_str!("schema.sql"), include_str!("../../../docs/spec/history.sql"));
}
