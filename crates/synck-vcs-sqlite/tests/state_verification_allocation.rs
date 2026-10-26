// SQLite's allocation counters are process-global. Keep this regression in its
// own test executable so other SQLite tests cannot change the measured peak.
use rusqlite::{MAIN_DB, ffi, params};
use synck_vcs::state::{FORMAT_VERSION, StateError, StateIndexStore, StateNodeId, StateRootId};
use synck_vcs::types::RepoId;
use synck_vcs_sqlite::SqliteStorage;

fn largest_sqlite_allocation(reset: bool) -> i64 {
    let mut current = 0;
    let mut peak = 0;
    // SAFETY: both output pointers are valid; this status operation is safe
    // after SQLite initialization and does not change its allocator.
    let status = unsafe {
        ffi::sqlite3_status64(
            ffi::SQLITE_STATUS_MALLOC_SIZE,
            &mut current,
            &mut peak,
            i32::from(reset),
        )
    };
    assert_eq!(status, ffi::SQLITE_OK);
    peak
}

#[tokio::test]
async fn oversized_state_nodes_are_rejected_without_allocating_their_payloads() {
    const PAYLOAD_BYTES: i64 = 32 * 1024 * 1024;
    let directory = tempfile::tempdir().unwrap();
    let backend = SqliteStorage::new(directory.path().join("state.sqlite"));
    let repo_id = RepoId::new();
    let mut state = backend.verification_state_store(repo_id.clone()).unwrap();
    let mut roots = Vec::new();
    for kind in [0x02, 0x06] {
        let id = StateNodeId::from_bytes([kind; 32]);
        let row_id: i64 = state
            .authority_connection()
            .query_row(
                "INSERT INTO vcs_state_nodes(repo_id, node_hash, payload)
                 VALUES (?1, ?2, zeroblob(?3)) RETURNING rowid",
                params![repo_id.as_str(), id.as_bytes().as_slice(), PAYLOAD_BYTES],
                |row| row.get(0),
            )
            .unwrap();
        let mut blob = state
            .authority_connection()
            .blob_open(MAIN_DB, "vcs_state_nodes", "payload", row_id, false)
            .unwrap();
        blob.write_at(&[FORMAT_VERSION, kind], 0).unwrap();
        blob.close().unwrap();
        roots.push(StateRootId::from_node_id(id));
    }

    state.begin_state_verification().await.unwrap();
    largest_sqlite_allocation(true);
    let batch = state.verify_root_closures(&roots).await.unwrap();
    let peak = largest_sqlite_allocation(false);

    assert_eq!(batch.nodes_verified, 0);
    assert!(batch.complete);
    assert_eq!(batch.findings.len(), 2);
    assert!(batch.findings.iter().all(|error| matches!(error,
        StateError::InvalidEncoding(message) if message.contains("format limit"))));
    assert!(peak > 0, "SQLite allocation tracking must be enabled");
    assert!(
        peak < 1024 * 1024,
        "header verification allocated {peak} bytes in SQLite for a {PAYLOAD_BYTES}-byte node"
    );
    let repeated = state.verify_root_closures(&roots).await.unwrap();
    assert!(repeated.complete);
    assert!(repeated.findings.is_empty());
    assert_eq!(repeated.nodes_verified, 0);
    state.finish_state_verification().await.unwrap();
}
