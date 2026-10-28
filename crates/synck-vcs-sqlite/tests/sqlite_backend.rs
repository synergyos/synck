use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Timelike, Utc};
use rusqlite::{Connection, params};
use synck_vcs::config::CoreConfig;
use synck_vcs::errors::{CoreError, StorageError};
use synck_vcs::repo::Repo;
use synck_vcs::state::{
    ItemValue, MAX_INDEX_NODE_BYTES, NamespaceKey, RootHeader, StateError, StateIndexStore,
    StateNode, StateNodeStore, StateProjectorStore, StateRootId, StateRootRecord, StateTree,
    catch_up_state_index, catch_up_state_index_bounded, encode_node, hash_node_bytes,
};
use synck_vcs::storage::{Storage, StorageBackend};
use synck_vcs::types::{
    COMMIT_FORMAT_VERSION, CommitEventRow, CommitProposal, CreateItem, EventProposal,
    FederatedProposal, ItemCursor, ItemId, ItemRow,
    LifecycleState, RefEventProposal, RepoEventCursor, RepoId, SequencingMode, SubtreeClosureGuard,
};
use synck_vcs_sqlite::{SqliteStorage, format_timestamp, parse_timestamp};

fn backend() -> (tempfile::TempDir, SqliteStorage) {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = SqliteStorage::new(dir.path().join("repository.sqlite3"));
    (dir, storage)
}

fn create_event(name: &str) -> EventProposal {
    EventProposal {
        item_id: None,
        event_type: "FILE::CREATE".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: Some(CreateItem {
            item_type: "FILE".to_string(),
            parent_folder_id: None,
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

fn commit(events: Vec<EventProposal>) -> CommitProposal {
    CommitProposal {
        author_id: "principal".to_string(),
        committer_id: "principal".to_string(),
        message: None,
        occurred_at: None,
        operation_id: Some(format!("test:{}", ItemId::new())),
        events,
    }
}

#[tokio::test]
async fn state_verification_bounds_bytes_without_rejecting_large_item_values() {
    let (_directory, backend) = backend();
    let repo_id = RepoId::new();
    let mut state = backend.verification_state_store(repo_id.clone()).unwrap();
    let mut roots = Vec::new();
    for size in [9, 9, 17] {
        let bytes = encode_node(&StateNode::ItemValue(ItemValue {
            item_id: ItemId::new(),
            item_type: "FILE".to_string(),
            parent_folder_id: None,
            name: "large".to_string(),
            name_key: "large".to_string(),
            head_seq: 1,
            ref_head_seq: 0,
            lifecycle_state: LifecycleState::Live,
            created_at: Utc::now(),
            created_by: "a".repeat(size * 1024 * 1024),
            sequencing_mode: SequencingMode::Local,
            federation_stream_id: None,
            content: None,
        }))
        .unwrap();
        let id = hash_node_bytes(&bytes);
        state
            .authority_connection()
            .execute(
                "INSERT INTO vcs_state_nodes(repo_id, node_hash, payload) VALUES (?1, ?2, ?3)",
                params![repo_id.as_str(), id.as_bytes().as_slice(), bytes],
            )
            .unwrap();
        roots.push(StateRootId::from_node_id(id));
    }
    state.begin_state_verification().await.unwrap();
    for index in 0..3 {
        let batch = state.verify_root_closures(&roots).await.unwrap();
        assert!(batch.findings.is_empty(), "{:?}", batch.findings);
        assert_eq!(batch.nodes_verified, 1);
        assert_eq!(batch.complete, index == 2);
    }
    state.finish_state_verification().await.unwrap();

    roots.clear();
    for kind in [0x02, 0x03, 0x04, 0x05, 0x06] {
        let size = if kind == 0x06 {
            124
        } else {
            MAX_INDEX_NODE_BYTES + 1
        };
        let mut bytes = vec![0; size];
        bytes[..2].copy_from_slice(&[2, kind]);
        let id = hash_node_bytes(&bytes);
        state
            .authority_connection()
            .execute(
                "INSERT INTO vcs_state_nodes(repo_id, node_hash, payload) VALUES (?1, ?2, ?3)",
                params![repo_id.as_str(), id.as_bytes().as_slice(), bytes],
            )
            .unwrap();
        roots.push(StateRootId::from_node_id(id));
    }
    state.begin_state_verification().await.unwrap();
    let batch = state.verify_root_closures(&roots).await.unwrap();
    assert_eq!(batch.nodes_verified, 0);
    assert!(batch.complete);
    assert_eq!(batch.findings.len(), 5);
    assert!(batch.findings.iter().all(|error| matches!(error,
        StateError::InvalidEncoding(message) if message.contains("format limit"))));
    let repeat = state.verify_root_closures(&roots).await.unwrap();
    assert!(repeat.complete);
    assert!(repeat.findings.is_empty());
    state.finish_state_verification().await.unwrap();

    // A short incremental header read is corrupt data, not a storage failure.
    roots.clear();
    for bytes in [vec![], vec![2], vec![2, 0xff]] {
        let id = hash_node_bytes(&bytes);
        state
            .authority_connection()
            .execute(
                "INSERT INTO vcs_state_nodes(repo_id, node_hash, payload) VALUES (?1, ?2, ?3)",
                params![repo_id.as_str(), id.as_bytes().as_slice(), bytes],
            )
            .unwrap();
        roots.push(StateRootId::from_node_id(id));
    }
    state.begin_state_verification().await.unwrap();
    let batch = state.verify_root_closures(&roots).await.unwrap();
    assert!(batch.complete);
    assert_eq!(batch.nodes_verified, 0);
    assert_eq!(batch.findings.len(), 3);
    assert!(
        batch
            .findings
            .iter()
            .all(|error| matches!(error, StateError::InvalidEncoding(_)))
    );
    state.finish_state_verification().await.unwrap();
}

#[tokio::test]
async fn verification_state_store_replays_without_local_canonical_rows() {
    let (_source_directory, source) = backend();
    source.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(
        Storage::new(Arc::new(source.clone())),
        CoreConfig::default(),
    );
    repo.append(repo_id.clone(), commit(vec![create_event("verified.txt")]))
        .await
        .unwrap();
    repo.sequence_repo_activity(repo_id.clone(), 1)
        .await
        .unwrap();

    let scratch = tempfile::tempdir().unwrap();
    let verifier = SqliteStorage::new(scratch.path().join("expected-state.sqlite"));
    let mut state = verifier.verification_state_store(repo_id.clone()).unwrap();
    let report = catch_up_state_index(&repo, &repo_id, &mut state, None)
        .await
        .unwrap();
    assert_eq!((report.applied_repo_seq, report.commits_processed), (1, 1));

    let head = state.projection_head().await.unwrap().unwrap();
    state.begin_state_verification().await.unwrap();
    let mut batch = state
        .verify_root_closures(&[head.state_root, head.state_root])
        .await
        .unwrap();
    let mut nodes_verified = batch.nodes_verified;
    assert!(batch.findings.is_empty());
    while !batch.complete {
        batch = state
            .verify_root_closures(&[head.state_root])
            .await
            .unwrap();
        assert!(batch.findings.is_empty());
        nodes_verified += batch.nodes_verified;
    }
    assert!(nodes_verified > 0);
    let second = state
        .verify_root_closures(&[head.state_root])
        .await
        .unwrap();
    assert_eq!(second.nodes_verified, 0);
    assert!(second.findings.is_empty());
    assert!(second.complete);
    state.finish_state_verification().await.unwrap();

    let root = StateTree::new(&mut state)
        .read_root(head.state_root)
        .await
        .unwrap();
    state
        .authority_connection()
        .execute(
            "DELETE FROM vcs_state_nodes WHERE repo_id = ?1 AND node_hash = ?2",
            params![repo_id.as_str(), root.item_map_root.as_bytes().as_slice()],
        )
        .unwrap();
    state.begin_state_verification().await.unwrap();
    let mut missing_reported = false;
    loop {
        let batch = state
            .verify_root_closures(&[head.state_root])
            .await
            .unwrap();
        missing_reported |= batch
            .findings
            .iter()
            .any(|error| matches!(error, StateError::MissingNode(_)));
        if batch.complete {
            break;
        }
    }
    assert!(missing_reported);
    let complete = state
        .verify_root_closures(&[head.state_root])
        .await
        .unwrap();
    assert_eq!(complete.nodes_verified, 0);
    assert!(complete.findings.is_empty());
    assert!(complete.complete);
    state.finish_state_verification().await.unwrap();
}

fn ref_event(owner_item_id: ItemId, base_ref_seq: i64, event_type: &str) -> RefEventProposal {
    RefEventProposal {
        owner_item_id,
        event_type: event_type.to_string(),
        set_metadata: Some(synck_vcs::metadata::MetadataRef::new(serde_json::json!({"version_id": ItemId::new().as_str()})).unwrap()),
        set_content: None,
        base_ref_seq,
    }
}

#[tokio::test]
async fn ref_ledger_has_independent_cas_and_order() {
    let (_directory, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    let item_id = ItemId::new();
    let mut create = create_event("versioned.txt");
    create.item_id = Some(item_id.clone());
    repo.append(repo_id.clone(), commit(vec![create]))
        .await
        .unwrap();

    let first = repo
        .append_with_refs(
            repo_id.clone(),
            commit(Vec::new()),
            vec![ref_event(item_id.clone(), 0, "FILE_VERSION::CREATE")],
        )
        .await
        .unwrap();
    assert_eq!(first.ref_event_seqs, vec![1]);
    let item = repo
        .get_item(repo_id.clone(), item_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((item.head_seq, item.ref_head_seq), (1, 1));
    let stored = repo
        .get_commit(repo_id.clone(), first.commit_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.format_version, COMMIT_FORMAT_VERSION);

    let stale = repo
        .append_with_refs(
            repo_id.clone(),
            commit(Vec::new()),
            vec![ref_event(
                item_id.clone(),
                0,
                "FILE_VERSION::SET_DESCRIPTION",
            )],
        )
        .await
        .unwrap_err();
    assert!(matches!(stale, CoreError::RefBaseSeqMismatch { .. }));
    assert_eq!(
        repo.list_ref_events(repo_id.clone(), item_id.clone(), 1, 10)
            .await
            .unwrap()
            .len(),
        1
    );

    let multiple = repo
        .append_with_refs(
            repo_id.clone(),
            commit(Vec::new()),
            vec![
                ref_event(item_id.clone(), 1, "FILE_VERSION::SET_DESCRIPTION"),
                ref_event(item_id.clone(), 1, "FILE_VERSION::SET_ORDER"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(multiple.ref_event_seqs, vec![2, 3]);
    assert_eq!(
        repo.get_item(repo_id.clone(), item_id.clone())
            .await
            .unwrap()
            .unwrap()
            .head_seq,
        1
    );

    repo.sequence_repo_activity(repo_id.clone(), 64)
        .await
        .unwrap();
    let combined = repo
        .list_repo_commit_events(repo_id.clone(), None, 64)
        .await
        .unwrap();
    assert!(
        combined
            .iter()
            .any(|event| matches!(event, CommitEventRow::Ref(_)))
    );
    let mut state = storage.state_index_store(repo_id.clone()).unwrap();
    catch_up_state_index(&repo, &repo_id, &mut state, None)
        .await
        .unwrap();
    let head = state.projection_head().await.unwrap().unwrap();
    let root = StateTree::new(&mut state)
        .read_root(head.state_root)
        .await
        .unwrap();
    let items = StateTree::new(&mut state)
        .all_items(root.item_map_root)
        .await
        .unwrap();
    assert_eq!(items[&item_id].ref_head_seq, 3);
}

#[tokio::test]
async fn owner_create_and_first_ref_are_one_atomic_commit() {
    let (_directory, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());
    let item_id = ItemId::new();
    let mut create = create_event("atomic.txt");
    create.item_id = Some(item_id.clone());
    let accepted = repo
        .append_with_refs(
            repo_id.clone(),
            commit(vec![create]),
            vec![ref_event(item_id.clone(), 0, "FILE_VERSION::CREATE")],
        )
        .await
        .unwrap();
    assert_eq!(accepted.event_item_seqs, vec![1]);
    assert_eq!(accepted.ref_event_seqs, vec![1]);
    let events = repo
        .list_commit_events(repo_id, accepted.commit_id, 3)
        .await
        .unwrap();
    assert!(matches!(
        events.as_slice(),
        [CommitEventRow::Item(_), CommitEventRow::Ref(_)]
    ));
}

#[tokio::test]
async fn replay_reads_use_exact_operation_and_commit_bounds() {
    let (_directory, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let other_repo_id = RepoId::new();
    let repo = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    let operation_id = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let accepted = repo
        .append(
            repo_id.clone(),
            CommitProposal {
                author_id: "principal".to_string(),
                committer_id: "assistant:test".to_string(),
                message: Some("two files".to_string()),
                occurred_at: None,
                operation_id: Some(operation_id.to_string()),
                events: vec![create_event("one"), create_event("two")],
            },
        )
        .await
        .unwrap();
    repo.sequence_repo_activity(repo_id.clone(), 1)
        .await
        .unwrap();

    let replay = storage
        .get_commit_by_operation_id_readonly(&repo_id, operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replay.commit_id, accepted.commit_id);
    assert_eq!(replay.author_id, "principal");
    assert_eq!(replay.committer_id, "assistant:test");
    assert!(
        storage
            .get_commit_by_operation_id_readonly(&other_repo_id, operation_id)
            .await
            .unwrap()
            .is_none()
    );

    let bounded = storage
        .list_events_for_commit_readonly(&repo_id, &accepted.commit_id, 1)
        .await
        .unwrap();
    assert_eq!(bounded.len(), 1);
    assert_eq!(bounded[0].commit_event_idx, 0);

    let complete = storage
        .list_events_for_commit_readonly(&repo_id, &accepted.commit_id, 3)
        .await
        .unwrap();
    assert_eq!(complete.len(), 2);
    assert_eq!(complete[0].commit_event_idx, 0);
    assert_eq!(complete[1].commit_event_idx, 1);
}

#[tokio::test]
async fn committer_changes_canonical_commit_identity() {
    let (_directory, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    let item_id = ItemId::new();
    let mut event = create_event("same-file");
    event.item_id = Some(item_id);

    let first = repo
        .append(
            RepoId::new(),
            CommitProposal {
                author_id: "principal".to_string(),
                committer_id: "principal".to_string(),
                message: Some("same content".to_string()),
                occurred_at: None,
                operation_id: Some("operation:first".to_string()),
                events: vec![event.clone()],
            },
        )
        .await
        .unwrap();
    let second = repo
        .append(
            RepoId::new(),
            CommitProposal {
                author_id: "principal".to_string(),
                committer_id: "assistant:test".to_string(),
                message: Some("same content".to_string()),
                occurred_at: None,
                operation_id: Some("operation:second".to_string()),
                events: vec![event],
            },
        )
        .await
        .unwrap();

    assert_ne!(first.commit_id, second.commit_id);
}

fn create_item_event(
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

fn set_parent_event(
    item_id: ItemId,
    item_type: &str,
    base_seq: i64,
    parent_folder_id: Option<ItemId>,
) -> EventProposal {
    EventProposal {
        item_id: Some(item_id),
        event_type: format!("{item_type}::SET_PARENT"),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: Some(parent_folder_id),
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(base_seq),
    }
}

fn lifecycle_event(
    item_id: ItemId,
    item_type: &str,
    base_seq: i64,
    in_trash: bool,
) -> EventProposal {
    EventProposal {
        item_id: Some(item_id),
        event_type: format!(
            "{item_type}::{}",
            if in_trash { "TRASH" } else { "RESTORE" }
        ),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: Some(in_trash),
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(base_seq),
    }
}

fn trash_event(item_id: ItemId, base_seq: i64) -> EventProposal {
    EventProposal {
        item_id: Some(item_id),
        event_type: "FILE::TRASH".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: Some(true),
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(base_seq),
    }
}

#[test]
fn writable_open_never_creates_a_missing_database() {
    let (directory, storage) = backend();
    assert!(storage.open_writable().is_err());
    assert!(!storage.path().exists());
    drop(directory);
}

#[tokio::test]
async fn writable_open_rejects_a_foreign_database_without_mutation() {
    let (directory, storage) = backend();
    let connection = rusqlite::Connection::open(storage.path()).unwrap();
    connection
        .execute_batch("CREATE VIEW sqliteXforeign AS SELECT 'value' AS value;")
        .unwrap();
    drop(connection);
    let before = std::fs::read(storage.path()).unwrap();

    assert!(storage.open_writable().is_err());
    assert!(storage.init_schema().await.is_err());
    assert_eq!(std::fs::read(storage.path()).unwrap(), before);
    assert!(!directory.path().join("repository.sqlite3-wal").exists());
    assert!(!directory.path().join("repository.sqlite3-shm").exists());
}

#[tokio::test]
async fn database_family_rejects_a_regular_orphan_sidecar() {
    let (directory, storage) = backend();
    let sidecar = directory.path().join("repository.sqlite3-shm");
    std::fs::write(&sidecar, b"sentinel").unwrap();

    assert!(storage.open_readonly().is_err());
    assert!(storage.open_writable().is_err());
    assert!(storage.init_schema().await.is_err());
    assert!(!storage.path().exists());
    assert_eq!(std::fs::read(sidecar).unwrap(), b"sentinel");
}

#[cfg(unix)]
#[tokio::test]
async fn database_family_opens_reject_symlinks() {
    use std::os::unix::fs::symlink;

    let (directory, storage) = backend();
    storage.init_schema().await.unwrap();
    let connection = storage.open_writable().unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(connection);
    for suffix in ["-wal", "-shm"] {
        let path = directory.path().join(format!("repository.sqlite3{suffix}"));
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    let external = directory.path().join("external");
    std::fs::write(&external, b"sentinel").unwrap();

    let link = directory.path().join("linked.sqlite3");
    symlink(storage.path(), &link).unwrap();
    let linked = SqliteStorage::new(link);
    assert!(linked.open_readonly().is_err());
    assert!(linked.open_writable().is_err());

    let wal = directory.path().join("repository.sqlite3-wal");
    symlink(&external, &wal).unwrap();
    assert!(storage.open_readonly().is_err());
    assert!(storage.open_writable().is_err());
    assert_eq!(std::fs::read(&external).unwrap(), b"sentinel");
    std::fs::remove_file(&wal).unwrap();
    assert!(storage.open_readonly().is_ok());
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn database_family_opens_reject_hard_links() {
    let (directory, storage) = backend();
    storage.init_schema().await.unwrap();
    let connection = storage.open_writable().unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(connection);
    for suffix in ["-wal", "-shm"] {
        let path = directory.path().join(format!("repository.sqlite3{suffix}"));
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }

    let alias = directory.path().join("repository-alias.sqlite3");
    std::fs::hard_link(storage.path(), &alias).unwrap();
    assert!(storage.open_readonly().is_err());
    assert!(storage.open_writable().is_err());
    std::fs::remove_file(alias).unwrap();
    assert!(storage.open_readonly().is_ok());
}

#[cfg(windows)]
#[tokio::test]
async fn database_family_opens_reject_a_reparse_sidecar() {
    use std::os::windows::fs::MetadataExt;

    let (directory, storage) = backend();
    storage.init_schema().await.unwrap();
    let connection = storage.open_writable().unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(connection);
    for suffix in ["-wal", "-shm"] {
        let path = directory.path().join(format!("repository.sqlite3{suffix}"));
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    let external = directory.path().join("external");
    std::fs::create_dir(&external).unwrap();
    let wal = directory.path().join("repository.sqlite3-wal");
    let output = std::process::Command::new("cmd")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&wal)
        .arg(&external)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mklink failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        std::fs::symlink_metadata(&wal).unwrap().file_attributes() & 0x400,
        0
    );

    assert!(storage.open_readonly().is_err());
    assert!(storage.open_writable().is_err());
}

#[test]
fn timestamp_encoding_is_fixed_width_and_strict() {
    let value = Utc
        .with_ymd_and_hms(2026, 8, 16, 12, 34, 56)
        .single()
        .unwrap()
        .with_nanosecond(42)
        .unwrap();
    let encoded = format_timestamp(value);
    assert_eq!(encoded, "2026-08-16T12:34:56.000000042Z");
    assert_eq!(parse_timestamp(&encoded).unwrap(), value);
    assert!(parse_timestamp("2026-08-16T12:34:56Z").is_err());
}

#[tokio::test]
async fn schema_init_is_idempotent_and_rejects_unknown_version() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    storage.init_schema().await.unwrap();

    let conn = storage.open_writable().unwrap();
    let strict: i64 = conn
        .query_row(
            "SELECT strict FROM pragma_table_list WHERE name = 'vcs_events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(strict, 1);
    conn.pragma_update(None, "user_version", 7).unwrap();
    drop(conn);

    let error = storage.init_schema().await.unwrap_err();
    assert!(matches!(error, StorageError::Other { .. }));
}

#[tokio::test]
async fn old_schema_versions_are_rejected_without_mutation() {
    for version in 1..6 {
        let (_directory, storage) = backend();
        storage.init_schema().await.unwrap();
        let connection = storage.open_writable().unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        let before: String = connection
            .query_row("SELECT group_concat(sql) FROM sqlite_schema", [], |row| {
                row.get(0)
            })
            .unwrap();
        drop(connection);
        assert!(storage.open_readonly().is_err());
        assert!(storage.init_schema().await.is_err());
        let connection = Connection::open(storage.path()).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i32>(0))
                .unwrap(),
            version
        );
        let after: String = connection
            .query_row("SELECT group_concat(sql) FROM sqlite_schema", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(before, after);
    }
}

#[tokio::test]
async fn state_store_publishes_complete_roots_and_enforces_foreign_keys() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    let accepted = repo
        .append(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("state-root-test".to_string()),
                events: vec![create_event("file")],
            },
        )
        .await
        .unwrap();
    repo.sequence_repo_activity(repo_id.clone(), 1)
        .await
        .unwrap();
    let commit_id = accepted.commit_id.clone();

    let mut store = storage.state_index_store(repo_id.clone()).unwrap();
    store.begin_batch().await.unwrap();
    let mut tree = StateTree::new(&mut store);
    let item_root = tree.empty_item_root().await.unwrap();
    let namespace_root = tree.empty_namespace_root().await.unwrap();
    let empty_root = tree
        .store_root(RootHeader {
            repo_id: repo_id.clone(),
            repo_seq: 0,
            commit_id: None,
            item_map_root: item_root,
            namespace_root,
        })
        .await
        .unwrap();
    store.finish_batch(&[empty_root.node_id()]).await.unwrap();
    store.install_empty_head(empty_root).await.unwrap();

    // Publishing a staged root must stop when it reaches an existing durable
    // child. Decoding that child here would make every small mutation walk the
    // complete historical tree.
    let empty_item_bytes = encode_node(&StateNode::ItemLeaf(Vec::new())).unwrap();
    let connection = rusqlite::Connection::open(storage.state_index_path()).unwrap();
    connection.pragma_update(None, "foreign_keys", "ON").unwrap();
    connection
        .execute(
            "UPDATE vcs_state_nodes SET payload = x'00'
             WHERE repo_id = ?1 AND node_hash = ?2",
            params![repo_id.as_str(), item_root.as_bytes().as_slice()],
        )
        .unwrap();
    drop(connection);
    store.begin_batch().await.unwrap();
    let staged_root = StateTree::new(&mut store)
        .store_root(RootHeader {
            repo_id: repo_id.clone(),
            repo_seq: 1,
            commit_id: Some(commit_id.clone()),
            item_map_root: item_root,
            namespace_root,
        })
        .await
        .unwrap();
    store.finish_batch(&[staged_root.node_id()]).await.unwrap();
    let connection = rusqlite::Connection::open(storage.state_index_path()).unwrap();
    connection.pragma_update(None, "foreign_keys", "ON").unwrap();
    connection
        .execute(
            "DELETE FROM vcs_state_nodes WHERE repo_id = ?1 AND node_hash = ?2",
            params![repo_id.as_str(), staged_root.as_bytes().as_slice()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE vcs_state_nodes SET payload = ?1
             WHERE repo_id = ?2 AND node_hash = ?3",
            params![
                empty_item_bytes,
                repo_id.as_str(),
                item_root.as_bytes().as_slice()
            ],
        )
        .unwrap();
    drop(connection);

    let mut tree = StateTree::new(&mut store);
    let commit_root = tree
        .store_root(RootHeader {
            repo_id: repo_id.clone(),
            repo_seq: 1,
            commit_id: Some(commit_id.clone()),
            item_map_root: item_root,
            namespace_root,
        })
        .await
        .unwrap();
    store
        .publish_root(
            0,
            &StateRootRecord {
                repo_seq: 1,
                commit_id: commit_id.clone(),
                state_root: commit_root,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .projection_head()
            .await
            .unwrap()
            .unwrap()
            .applied_repo_seq,
        1
    );
    assert_eq!(store.state_row_counts().await.unwrap(), (4, 1, 1));
    drop(store);

    let connection = rusqlite::Connection::open(storage.state_index_path()).unwrap();
    connection.pragma_update(None, "foreign_keys", "ON").unwrap();
    assert!(
        connection
            .execute(
                "DELETE FROM vcs_state_nodes WHERE repo_id = ?1 AND node_hash = ?2",
                params![repo_id.as_str(), commit_root.as_bytes().as_slice()],
            )
            .is_err()
    );
    let connection = storage.open_writable().unwrap();
    assert!(
        connection
            .execute(
                "DELETE FROM vcs_commits WHERE repo_id = ?1 AND commit_id = ?2",
                params![
                    repo_id.as_str(),
                    synck_vcs::types::commit_id_to_digest_bytes(&commit_id).as_slice()
                ],
            )
            .is_err()
    );
}

#[tokio::test]
async fn projector_is_commit_bounded_handles_name_swaps_and_rebuilds_identically() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    let created = repo
        .append(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: Some("create pair".to_string()),
                occurred_at: None,
                operation_id: Some("project-create".to_string()),
                events: vec![create_event("alpha"), create_event("beta")],
            },
        )
        .await
        .unwrap();
    repo.sequence_repo_activity(repo_id.clone(), 32)
        .await
        .unwrap();
    let alpha = created.created_items[0].item_id.clone();
    let beta = created.created_items[1].item_id.clone();

    let rename = |item_id: ItemId, name: &str| EventProposal {
        item_id: Some(item_id),
        event_type: "FILE::RENAME".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: Some(name.to_string()),
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(1),
    };
    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: Some("swap names".to_string()),
            occurred_at: None,
            operation_id: Some("project-swap".to_string()),
            events: vec![rename(alpha.clone(), "beta"), rename(beta.clone(), "alpha")],
        },
    )
    .await
    .unwrap();
    repo.sequence_repo_activity(repo_id.clone(), 32)
        .await
        .unwrap();
    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: Some("trash alpha".to_string()),
            occurred_at: None,
            operation_id: Some("project-trash".to_string()),
            events: vec![trash_event(beta.clone(), 2)],
        },
    )
    .await
    .unwrap();
    repo.sequence_repo_activity(repo_id.clone(), 32)
        .await
        .unwrap();

    let mut store = storage.state_index_store(repo_id.clone()).unwrap();
    let first = catch_up_state_index_bounded(&repo, &repo_id, &mut store, None, Some(1))
        .await
        .unwrap();
    assert_eq!(
        (
            first.start_repo_seq,
            first.applied_repo_seq,
            first.commits_processed
        ),
        (0, 1, 1)
    );
    let second = catch_up_state_index_bounded(&repo, &repo_id, &mut store, None, Some(1))
        .await
        .unwrap();
    assert_eq!(
        (
            second.start_repo_seq,
            second.applied_repo_seq,
            second.commits_processed
        ),
        (1, 2, 1)
    );

    let head = store.projection_head().await.unwrap().unwrap();
    let root = StateTree::new(&mut store)
        .read_root(head.state_root)
        .await
        .unwrap();
    let items = StateTree::new(&mut store)
        .all_items(root.item_map_root)
        .await
        .unwrap();
    assert_eq!(items[&alpha].name, "beta");
    assert_eq!(items[&beta].name, "alpha");
    assert_eq!(
        StateTree::new(&mut store)
            .get_namespace(
                root.namespace_root,
                &NamespaceKey::new(None, "alpha".to_string())
            )
            .await
            .unwrap(),
        Some(beta.clone())
    );
    assert_eq!(
        StateTree::new(&mut store)
            .get_namespace(
                root.namespace_root,
                &NamespaceKey::new(None, "beta".to_string())
            )
            .await
            .unwrap(),
        Some(alpha.clone())
    );

    catch_up_state_index(&repo, &repo_id, &mut store, None)
        .await
        .unwrap();
    let original = store.projection_head().await.unwrap().unwrap();
    store.begin_projection_commit().await.unwrap();
    drop(store);
    let mut store = storage.state_index_store(repo_id.clone()).unwrap();
    assert_eq!(
        store.projection_head().await.unwrap(),
        Some(original.clone())
    );
    store.remove_head().await.unwrap();
    catch_up_state_index(&repo, &repo_id, &mut store, None)
        .await
        .unwrap();
    let rebuilt = store.projection_head().await.unwrap().unwrap();
    assert_eq!(rebuilt, original);
    assert_eq!(store.state_row_counts().await.unwrap().1, 3);
}

#[tokio::test]
async fn projector_rejects_a_head_that_does_not_name_its_published_root() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("stale-head".to_string()),
            events: vec![create_event("file")],
        },
    )
    .await
    .unwrap();
    repo.sequence_repo_activity(repo_id.clone(), 32)
        .await
        .unwrap();
    let mut store = storage.state_index_store(repo_id.clone()).unwrap();
    catch_up_state_index(&repo, &repo_id, &mut store, None)
        .await
        .unwrap();
    drop(store);

    let connection = rusqlite::Connection::open(storage.state_index_path()).unwrap();
    connection.pragma_update(None, "foreign_keys", "ON").unwrap();
    let wrong_root: Vec<u8> = connection
        .query_row(
             "SELECT node_hash FROM vcs_state_nodes
             WHERE repo_id = ?1
               AND substr(payload, 2, 1) = x'06'
               AND node_hash != (SELECT state_root FROM vcs_state_projection_heads WHERE repo_id = ?1)
             ORDER BY node_hash LIMIT 1",
            [repo_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE vcs_state_projection_heads SET state_root = ?1 WHERE repo_id = ?2",
            params![wrong_root, repo_id.as_str()],
        )
        .unwrap();
    drop(connection);

    let mut store = storage.state_index_store(repo_id.clone()).unwrap();
    let error = catch_up_state_index(&repo, &repo_id, &mut store, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("projection head"));
}

#[tokio::test]
async fn open_rejects_schema_drift_at_the_declared_version() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let connection = storage.open_writable().unwrap();
    connection
        .execute_batch("DROP INDEX idx_vcs_items_unique_name_key")
        .unwrap();
    drop(connection);

    let error = match storage.open_readonly() {
        Ok(_) => panic!("read-only open accepted a damaged schema"),
        Err(error) => error,
    };
    assert!(matches!(error, StorageError::Other { .. }));
    let error = match storage.open_writable() {
        Ok(_) => panic!("writable open accepted a damaged schema"),
        Err(error) => error,
    };
    assert!(matches!(error, StorageError::Other { .. }));
    let error = match storage.begin_txn().await {
        Ok(_) => panic!("write transaction accepted a damaged schema"),
        Err(error) => error,
    };
    assert!(matches!(error, StorageError::Other { .. }));
}

#[tokio::test]
async fn transaction_timestamp_is_cached() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let mut txn = storage.begin_txn().await.unwrap();
    let first = txn.acceptance_timestamp().await.unwrap();
    let second = txn.acceptance_timestamp().await.unwrap();
    assert_eq!(first, second);
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn rollback_discards_all_writes() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let item_id = ItemId::new();
    let mut transaction = storage.begin_txn().await.unwrap();
    transaction
        .put_item(&ItemRow {
            repo_id: repo_id.clone(),
            item_id: item_id.clone(),
            item_type: "FILE".to_string(),
            parent_folder_id: None,
            name: "file".to_string(),
            name_key: "file".to_string(),
            head_seq: 1,
            ref_head_seq: 0,
            lifecycle_state: LifecycleState::Live,
            created_at: Utc::now(),
            created_by: "author".to_string(),
            sequencing_mode: SequencingMode::Local,
            federation_stream_id: None,
        })
        .await
        .unwrap();
    transaction.rollback().await.unwrap();

    assert!(
        storage
            .get_item_readonly(&repo_id, &item_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn schema_rejects_partially_sealed_commits() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let connection = storage.open_writable().unwrap();
    let timestamp = "2026-08-16T12:34:56.000000000Z";

    let error = connection
        .execute(
            "INSERT INTO vcs_commits
             (repo_id, commit_pk, commit_id, format_version, author_id, committer_id,
              created_at, event_count, events_digest)
             VALUES ('repo', 'partial', zeroblob(32), 1, 'author', 'author', ?1,
                     NULL, zeroblob(32))",
            [timestamp],
        )
        .unwrap_err();
    assert!(matches!(error, rusqlite::Error::SqliteFailure(_, _)));
}

#[tokio::test]
async fn item_timestamp_order_and_cursor_are_chronological() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let earlier = Utc
        .with_ymd_and_hms(2026, 8, 16, 12, 0, 0)
        .single()
        .unwrap();
    let later = earlier.with_nanosecond(1).unwrap();
    let first_id = ItemId::new();
    let second_id = ItemId::new();

    let make_item = |item_id: ItemId, name: &str, created_at| ItemRow {
        repo_id: repo_id.clone(),
        item_id,
        item_type: "FILE".to_string(),
        parent_folder_id: None,
        name: name.to_string(),
        name_key: name.to_string(),
        head_seq: 1,
        ref_head_seq: 0,
        lifecycle_state: LifecycleState::Live,
        created_at,
        created_by: "author".to_string(),
        sequencing_mode: SequencingMode::Local,
        federation_stream_id: None,
    };

    let mut txn = storage.begin_txn().await.unwrap();
    txn.put_item(&make_item(second_id.clone(), "later", later))
        .await
        .unwrap();
    txn.put_item(&make_item(first_id.clone(), "earlier", earlier))
        .await
        .unwrap();
    txn.commit().await.unwrap();

    let items = storage
        .list_items_readonly(&repo_id, None, None, 10)
        .await
        .unwrap();
    assert_eq!(items[0].item_id, first_id);
    assert_eq!(items[1].item_id, second_id);

    let cursor = ItemCursor {
        created_at: items[0].created_at,
        item_id: items[0].item_id.clone(),
    };
    let page = storage
        .list_items_readonly(&repo_id, Some(&cursor), None, 10)
        .await
        .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].item_id, second_id);
}

#[tokio::test]
async fn repo_append_sequence_and_read_contract() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());

    let accepted = repo
        .append(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: Some("initial".to_string()),
                occurred_at: None,
                operation_id: Some("initial-operation".to_string()),
                events: vec![create_event("file.txt")],
            },
        )
        .await
        .unwrap();
    let item_id = accepted.created_items[0].item_id.clone();
    assert_eq!(
        repo.sequence_repo_activity(repo_id.clone(), 32)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        repo.sequence_repo_activity(repo_id.clone(), 32)
            .await
            .unwrap(),
        0
    );

    let item = repo
        .get_item(repo_id.clone(), item_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(item.name, "file.txt");
    let commits = repo
        .list_commits_by_repo_seq(repo_id.clone(), 1, None, 10)
        .await
        .unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].commit_id, accepted.commit_id);
    let events = repo
        .list_repo_events(repo_id.clone(), None, 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    let cursor = RepoEventCursor {
        repo_seq: events[0].repo_seq.unwrap(),
        commit_event_idx: events[0].commit_event_idx,
    };
    assert!(
        repo.list_repo_events(repo_id, Some(cursor), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn live_name_conflict_maps_to_unique_violation() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let now = Utc::now();
    let item = |item_id| ItemRow {
        repo_id: repo_id.clone(),
        item_id,
        item_type: "FILE".to_string(),
        parent_folder_id: None,
        name: "same".to_string(),
        name_key: "same".to_string(),
        head_seq: 1,
        ref_head_seq: 0,
        lifecycle_state: LifecycleState::Live,
        created_at: now,
        created_by: "author".to_string(),
        sequencing_mode: SequencingMode::Local,
        federation_stream_id: None,
    };
    let mut txn = storage.begin_txn().await.unwrap();
    txn.put_item(&item(ItemId::new())).await.unwrap();
    let error = txn.put_item(&item(ItemId::new())).await.unwrap_err();
    assert!(matches!(error, StorageError::UniqueViolation { .. }));
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn non_unique_constraint_is_not_reported_as_a_name_conflict() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let item = ItemRow {
        repo_id: RepoId::new(),
        item_id: ItemId::new(),
        item_type: "FILE".to_string(),
        parent_folder_id: None,
        name: "file".to_string(),
        name_key: "file".to_string(),
        head_seq: 0,
        ref_head_seq: 0,
        lifecycle_state: LifecycleState::Live,
        created_at: Utc::now(),
        created_by: "author".to_string(),
        sequencing_mode: SequencingMode::Local,
        federation_stream_id: None,
    };

    let mut txn = storage.begin_txn().await.unwrap();
    let error = txn.put_item(&item).await.unwrap_err();
    assert!(matches!(error, StorageError::Other { .. }));
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn federation_cursor_contract_is_complete() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let mut transaction = storage.begin_txn().await.unwrap();
    assert_eq!(
        transaction
            .federation_domain_last_seq_for_update(&repo_id, "domain")
            .await
            .unwrap(),
        0
    );
    transaction
        .federation_domain_set_last_seq(&repo_id, "domain", 7, Utc::now())
        .await
        .unwrap();
    transaction.commit().await.unwrap();

    let mut transaction = storage.begin_txn().await.unwrap();
    assert_eq!(
        transaction
            .federation_domain_last_seq_for_update(&repo_id, "domain")
            .await
            .unwrap(),
        7
    );
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn writer_contention_waits_for_the_fixed_timeout_then_reports_overload() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let first = storage.begin_txn().await.unwrap();
    let started = Instant::now();
    let error = match storage.begin_txn().await {
        Ok(_) => panic!("second SQLite writer unexpectedly acquired the reservation"),
        Err(error) => error,
    };
    assert!(started.elapsed() >= Duration::from_secs(4));
    assert!(matches!(error, StorageError::Overloaded { .. }));
    first.rollback().await.unwrap();
}

#[tokio::test]
async fn repo_enforces_base_sequence_and_operation_id() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());
    let accepted = repo
        .append(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("create".to_string()),
                events: vec![create_event("file")],
            },
        )
        .await
        .unwrap();
    let item_id = accepted.created_items[0].item_id.clone();
    let stale = EventProposal {
        item_id: Some(item_id),
        event_type: "FILE::TRASH".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: Some(true),
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(0),
    };
    let error = repo
        .append(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("stale".to_string()),
                events: vec![stale],
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CoreError::BaseSeqMismatch { .. }));

    let error = repo
        .append(
            repo_id,
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("create".to_string()),
                events: vec![create_event("other")],
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CoreError::OperationAlreadyApplied { .. }));
}

#[tokio::test]
async fn federated_bootstrap_rejects_a_second_rebind_event() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());
    let stream = "bootstrap-stream".to_string();
    let create = EventProposal {
        item_id: None,
        event_type: "FILE::CREATE".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: Some(CreateItem {
            item_type: "FILE".to_string(),
            parent_folder_id: None,
            name: "federated.txt".to_string(),
        }),
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: None,
    };
    let rebind = EventProposal {
        item_id: None,
        event_type: "FILE::FEDERATE".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: Some(true),
        set_federation_stream_id: Some(stream.clone()),
        base_seq: None,
    };
    repo.ingest(
        repo_id.clone(),
        FederatedProposal {
            federation_stream_id: stream,
            item_seq: 1,
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            events: vec![create.clone(), rebind.clone()],
        },
    )
    .await
    .unwrap();

    let duplicate_stream = "duplicate-bootstrap-stream".to_string();
    let mut duplicate_create = create;
    duplicate_create
        .create_item
        .as_mut()
        .expect("create event")
        .name = "duplicate.txt".to_string();
    let mut duplicate_rebind = rebind;
    duplicate_rebind.set_federation_stream_id = Some(duplicate_stream.clone());
    let error = repo
        .ingest(
            repo_id,
            FederatedProposal {
                federation_stream_id: duplicate_stream,
                item_seq: 1,
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                events: vec![duplicate_create, duplicate_rebind.clone(), duplicate_rebind],
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CoreError::IllegalFederationTransition));
}

#[tokio::test]
async fn guarded_append_rejects_a_changed_guard_head() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());
    let accepted = repo
        .append(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("create-guarded-items".to_string()),
                events: vec![create_event("guard"), create_event("target")],
            },
        )
        .await
        .unwrap();
    let guard_id = accepted.created_items[0].item_id.clone();
    let target_id = accepted.created_items[1].item_id.clone();

    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("advance-guard".to_string()),
            events: vec![trash_event(guard_id.clone(), 1)],
        },
    )
    .await
    .unwrap();

    let error = repo
        .append_guarded(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("guarded-target-change".to_string()),
                events: vec![trash_event(target_id.clone(), 1)],
            },
            &[(guard_id, 1)],
        )
        .await
        .unwrap_err();

    assert!(matches!(error, CoreError::BaseSeqMismatch { .. }));
    assert_eq!(
        repo.get_item(repo_id, target_id)
            .await
            .unwrap()
            .unwrap()
            .head_seq,
        1
    );
}

#[tokio::test]
async fn checked_file_folder_create_rejects_a_trashed_parent_but_generic_append_is_unchanged() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());
    let folder_id = ItemId::new();

    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("create-parent".to_string()),
            events: vec![create_item_event(
                folder_id.clone(),
                "FOLDER",
                None,
                "parent",
            )],
        },
    )
    .await
    .unwrap();
    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("trash-parent".to_string()),
            events: vec![lifecycle_event(folder_id.clone(), "FOLDER", 1, true)],
        },
    )
    .await
    .unwrap();

    let child_id = ItemId::new();
    let error = repo
        .append_file_folder_checked(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("checked-child".to_string()),
                events: vec![create_item_event(
                    child_id.clone(),
                    "FILE",
                    Some(folder_id.clone()),
                    "child",
                )],
            },
            &[],
            &[],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        CoreError::ParentNotLive { parent_item_id, .. } if parent_item_id == folder_id
    ));
    assert!(
        repo.get_item(repo_id.clone(), child_id.clone())
            .await
            .unwrap()
            .is_none()
    );

    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("generic-child".to_string()),
            events: vec![create_item_event(
                child_id.clone(),
                "FILE",
                Some(folder_id),
                "child",
            )],
        },
    )
    .await
    .unwrap();
    assert!(repo.get_item(repo_id, child_id).await.unwrap().is_some());
}

#[tokio::test]
async fn checked_file_folder_relocation_and_restore_reject_a_trashed_parent() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());
    let folder_id = ItemId::new();
    let file_id = ItemId::new();
    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("create-relocation-items".to_string()),
            events: vec![
                create_item_event(folder_id.clone(), "FOLDER", None, "parent"),
                create_item_event(file_id.clone(), "FILE", None, "file"),
            ],
        },
    )
    .await
    .unwrap();
    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("trash-relocation-parent".to_string()),
            events: vec![lifecycle_event(folder_id.clone(), "FOLDER", 1, true)],
        },
    )
    .await
    .unwrap();

    let relocation_error = repo
        .append_file_folder_checked(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("relocate-under-trash".to_string()),
                events: vec![set_parent_event(
                    file_id.clone(),
                    "FILE",
                    1,
                    Some(folder_id.clone()),
                )],
            },
            &[],
            &[],
        )
        .await
        .unwrap_err();
    assert!(matches!(relocation_error, CoreError::ParentNotLive { .. }));

    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("trash-relocation-file".to_string()),
            events: vec![lifecycle_event(file_id.clone(), "FILE", 1, true)],
        },
    )
    .await
    .unwrap();
    let restore_error = repo
        .append_file_folder_checked(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("restore-under-trash".to_string()),
                events: vec![
                    set_parent_event(file_id.clone(), "FILE", 2, Some(folder_id)),
                    lifecycle_event(file_id.clone(), "FILE", 2, false),
                ],
            },
            &[],
            &[],
        )
        .await
        .unwrap_err();
    assert!(matches!(restore_error, CoreError::ParentNotLive { .. }));
    let file = repo.get_item(repo_id, file_id).await.unwrap().unwrap();
    assert_eq!(file.head_seq, 2);
    assert_eq!(file.lifecycle_state, LifecycleState::Trash);
    assert_eq!(file.parent_folder_id, None);
}

#[tokio::test]
async fn checked_subtree_guard_rejects_membership_mismatch_without_appending() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());
    let folder_id = ItemId::new();
    let child_id = ItemId::new();
    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("create-subtree".to_string()),
            events: vec![
                create_item_event(folder_id.clone(), "FOLDER", None, "folder"),
                create_item_event(child_id.clone(), "FILE", Some(folder_id.clone()), "child"),
            ],
        },
    )
    .await
    .unwrap();

    let error = repo
        .append_file_folder_checked(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("underspecified-subtree".to_string()),
                events: vec![lifecycle_event(folder_id.clone(), "FOLDER", 1, true)],
            },
            &[],
            &[SubtreeClosureGuard {
                root_item_id: folder_id.clone(),
                expected_non_tombstoned_descendants: Vec::new(),
                require_empty_live_descendants: false,
            }],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        CoreError::SubtreeChanged { root_item_id, .. } if root_item_id == folder_id
    ));
    assert_eq!(
        repo.get_item(repo_id.clone(), folder_id.clone())
            .await
            .unwrap()
            .unwrap()
            .head_seq,
        1
    );

    repo.append_file_folder_checked(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("exact-subtree".to_string()),
            events: vec![
                lifecycle_event(child_id.clone(), "FILE", 1, true),
                lifecycle_event(folder_id.clone(), "FOLDER", 1, true),
            ],
        },
        &[],
        &[SubtreeClosureGuard {
            root_item_id: folder_id.clone(),
            expected_non_tombstoned_descendants: vec![(child_id.clone(), 1)],
            require_empty_live_descendants: false,
        }],
    )
    .await
    .unwrap();
    assert_eq!(
        repo.get_item(repo_id.clone(), folder_id)
            .await
            .unwrap()
            .unwrap()
            .lifecycle_state,
        LifecycleState::Trash
    );
    assert_eq!(
        repo.get_item(repo_id, child_id)
            .await
            .unwrap()
            .unwrap()
            .lifecycle_state,
        LifecycleState::Trash
    );
}

#[tokio::test]
async fn empty_live_subtree_guard_allows_trash_after_child_batch() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());
    let folder_id = ItemId::new();
    let child_id = ItemId::new();
    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".into(),
            committer_id: "author".into(),
            message: None,
            occurred_at: None,
            operation_id: Some("create-large-trash-fixture".into()),
            events: vec![
                create_item_event(folder_id.clone(), "FOLDER", None, "folder"),
                create_item_event(child_id.clone(), "FILE", Some(folder_id.clone()), "child"),
            ],
        },
    )
    .await
    .unwrap();
    let empty_live = SubtreeClosureGuard {
        root_item_id: folder_id.clone(),
        expected_non_tombstoned_descendants: Vec::new(),
        require_empty_live_descendants: true,
    };
    let rejected = repo
        .append_file_folder_checked(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".into(),
                committer_id: "author".into(),
                message: None,
                occurred_at: None,
                operation_id: Some("premature-root-trash".into()),
                events: vec![lifecycle_event(folder_id.clone(), "FOLDER", 1, true)],
            },
            &[],
            &[empty_live.clone()],
        )
        .await
        .unwrap_err();
    assert!(matches!(rejected, CoreError::SubtreeChanged { .. }));
    repo.append_file_folder_checked(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".into(),
            committer_id: "author".into(),
            message: None,
            occurred_at: None,
            operation_id: Some("child-trash".into()),
            events: vec![lifecycle_event(child_id.clone(), "FILE", 1, true)],
        },
        &[],
        &[],
    )
    .await
    .unwrap();
    repo.append_file_folder_checked(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".into(),
            committer_id: "author".into(),
            message: None,
            occurred_at: None,
            operation_id: Some("root-trash".into()),
            events: vec![lifecycle_event(folder_id.clone(), "FOLDER", 1, true)],
        },
        &[],
        &[empty_live],
    )
    .await
    .unwrap();
    assert_eq!(
        repo.get_item(repo_id, folder_id)
            .await
            .unwrap()
            .unwrap()
            .lifecycle_state,
        LifecycleState::Trash
    );
}

#[tokio::test]
async fn checked_subtree_guard_rejects_a_changed_descendant_head_without_appending() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());
    let folder_id = ItemId::new();
    let child_id = ItemId::new();
    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("create-head-guard-subtree".to_string()),
            events: vec![
                create_item_event(folder_id.clone(), "FOLDER", None, "folder"),
                create_item_event(child_id.clone(), "FILE", Some(folder_id.clone()), "child"),
            ],
        },
    )
    .await
    .unwrap();

    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("trash-child-after-preparation".to_string()),
            events: vec![lifecycle_event(child_id.clone(), "FILE", 1, true)],
        },
    )
    .await
    .unwrap();

    let error = repo
        .append_file_folder_checked(
            repo_id.clone(),
            CommitProposal {
                author_id: "author".to_string(),
                committer_id: "author".to_string(),
                message: None,
                occurred_at: None,
                operation_id: Some("stale-descendant-head".to_string()),
                events: vec![lifecycle_event(folder_id.clone(), "FOLDER", 1, true)],
            },
            &[],
            &[SubtreeClosureGuard {
                root_item_id: folder_id.clone(),
                expected_non_tombstoned_descendants: vec![(child_id.clone(), 1)],
                require_empty_live_descendants: false,
            }],
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CoreError::BaseSeqMismatch { .. }));
    assert_eq!(
        repo.get_item(repo_id.clone(), folder_id)
            .await
            .unwrap()
            .unwrap()
            .lifecycle_state,
        LifecycleState::Live
    );
    assert_eq!(
        repo.get_item(repo_id, child_id)
            .await
            .unwrap()
            .unwrap()
            .head_seq,
        2
    );
}

#[tokio::test]
async fn sqlite_descendant_read_stops_at_the_overflow_sentinel() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let root_id = ItemId::new();
    let mut connection = storage.open_writable().unwrap();
    let transaction = connection.transaction().unwrap();
    let created_at = format_timestamp(Utc::now());
    transaction
        .execute(
            "INSERT INTO vcs_items (
                repo_id, item_id, item_type, parent_folder_id, name, name_key,
                head_seq, lifecycle_state, created_at, created_by,
                sequencing_mode, federation_stream_id
             ) VALUES (?1, ?2, 'FOLDER', NULL, 'root', 'root', 1, 'LIVE', ?3,
                       'author', 'LOCAL', NULL)",
            params![repo_id.as_str(), root_id.as_str(), created_at],
        )
        .unwrap();
    {
        let mut statement = transaction
            .prepare(
                "INSERT INTO vcs_items (
                    repo_id, item_id, item_type, parent_folder_id, name, name_key,
                    head_seq, lifecycle_state, created_at, created_by,
                    sequencing_mode, federation_stream_id
                 ) VALUES (?1, ?2, 'FILE', ?3, ?4, ?4, 1, 'LIVE', ?5,
                           'author', 'LOCAL', NULL)",
            )
            .unwrap();
        for index in 0..=9_999 {
            let item_id = ItemId::new();
            let name = format!("child-{index:05}");
            statement
                .execute(params![
                    repo_id.as_str(),
                    item_id.as_str(),
                    root_id.as_str(),
                    name,
                    created_at
                ])
                .unwrap();
        }
    }
    transaction.commit().unwrap();
    drop(connection);

    let mut txn = storage.begin_txn().await.unwrap();
    let rows = txn
        .list_non_tombstoned_descendants_bounded(
            &repo_id,
            &root_id,
            9_999,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 9_999 + 1);
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn checked_subtree_guards_have_no_size_cap_and_reject_overlap() {
    let (_dir, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());
    let first_root_id = ItemId::new();
    let second_root_id = ItemId::new();
    repo.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "author".to_string(),
            committer_id: "author".to_string(),
            message: None,
            occurred_at: None,
            operation_id: Some("create-guard-roots".to_string()),
            events: vec![
                create_item_event(first_root_id.clone(), "FOLDER", None, "first"),
                create_item_event(second_root_id.clone(), "FOLDER", None, "second"),
            ],
        },
    )
    .await
    .unwrap();
    let proposal = || CommitProposal {
        author_id: "author".to_string(),
        committer_id: "author".to_string(),
        message: None,
        occurred_at: None,
        operation_id: Some(ItemId::new().to_string()),
        events: vec![
            lifecycle_event(first_root_id.clone(), "FOLDER", 1, true),
            lifecycle_event(second_root_id.clone(), "FOLDER", 1, true),
        ],
    };
    let sorted_fake_descendants = |count: usize| {
        let mut descendants: Vec<(ItemId, i64)> = (0..count).map(|_| (ItemId::new(), 1)).collect();
        descendants.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        descendants
    };

    let oversized_error = repo
        .append_file_folder_checked(
            repo_id.clone(),
            proposal(),
            &[],
            &[SubtreeClosureGuard {
                root_item_id: first_root_id.clone(),
                expected_non_tombstoned_descendants: sorted_fake_descendants(
                    9_999 + 1,
                ),
                require_empty_live_descendants: false,
            }],
        )
        .await
        .unwrap_err();
    assert!(
        matches!(oversized_error, CoreError::ItemNotFound { .. }),
        "unexpected error: {oversized_error:?}"
    );

    let first_count = 9_999 / 2 + 1;
    let aggregate_error = repo
        .append_file_folder_checked(
            repo_id.clone(),
            proposal(),
            &[],
            &[
                SubtreeClosureGuard {
                    root_item_id: first_root_id.clone(),
                    expected_non_tombstoned_descendants: sorted_fake_descendants(first_count),
                    require_empty_live_descendants: false,
                },
                SubtreeClosureGuard {
                    root_item_id: second_root_id.clone(),
                    expected_non_tombstoned_descendants: sorted_fake_descendants(
                        9_999 - first_count + 1,
                    ),
                    require_empty_live_descendants: false,
                },
            ],
        )
        .await
        .unwrap_err();
    assert!(matches!(aggregate_error, CoreError::ItemNotFound { .. }));

    let overlap_error = repo
        .append_file_folder_checked(
            repo_id.clone(),
            proposal(),
            &[],
            &[
                SubtreeClosureGuard {
                    root_item_id: first_root_id.clone(),
                    expected_non_tombstoned_descendants: vec![(second_root_id.clone(), 1)],
                    require_empty_live_descendants: false,
                },
                SubtreeClosureGuard {
                    root_item_id: second_root_id.clone(),
                    expected_non_tombstoned_descendants: Vec::new(),
                    require_empty_live_descendants: false,
                },
            ],
        )
        .await
        .unwrap_err();
    assert!(matches!(overlap_error, CoreError::InvalidProposal(_)));

    let exact_limit_error = repo
        .append_file_folder_checked(
            repo_id,
            proposal(),
            &[],
            &[SubtreeClosureGuard {
                root_item_id: first_root_id,
                expected_non_tombstoned_descendants: sorted_fake_descendants(
                    9_999,
                ),
                require_empty_live_descendants: false,
            }],
        )
        .await
        .unwrap_err();
    assert!(matches!(exact_limit_error, CoreError::ItemNotFound { .. }));
}

#[tokio::test]
async fn checkpoint_publication_is_atomic_and_does_not_gate_acceptance() {
    let (_directory, backend) = backend();
    backend.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(
        Storage::new(Arc::new(backend.clone())),
        CoreConfig::default(),
    );
    let first = repo
        .append(repo_id.clone(), commit(vec![create_event("first")]))
        .await
        .unwrap();
    let second = repo
        .append(repo_id.clone(), commit(vec![create_event("second")]))
        .await
        .unwrap();
    assert!(first.repo_seq.is_none());
    assert!(second.repo_seq.is_none());
    let connection = backend.open_writable().unwrap();
    let accepted: String = connection
        .query_row(
            "SELECT created_at FROM vcs_commits WHERE repo_id=?1 AND commit_id=?2",
            params![
                repo_id.as_str(),
                synck_vcs::types::commit_id_to_digest_bytes(&second.commit_id).as_slice()
            ],
            |row| row.get(0),
        )
        .unwrap();
    // Force failure after the first position has been assigned inside the batch.
    connection
        .execute(
            "UPDATE vcs_commits SET created_at='zzz' WHERE commit_id=?1",
            [synck_vcs::types::commit_id_to_digest_bytes(&second.commit_id).as_slice()],
        )
        .unwrap();
    assert!(
        repo.sequence_repo_activity(repo_id.clone(), 10)
            .await
            .is_err()
    );
    let published: i64 = connection
        .query_row(
            "SELECT count(*) FROM vcs_commits WHERE repo_seq IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let checkpoints: i64 = connection
        .query_row("SELECT count(*) FROM vcs_checkpoints", [], |row| row.get(0))
        .unwrap();
    assert_eq!((published, checkpoints), (0, 0));
    connection
        .execute(
            "UPDATE vcs_commits SET created_at=?1 WHERE commit_id=?2",
            params![
                accepted,
                synck_vcs::types::commit_id_to_digest_bytes(&second.commit_id).as_slice()
            ],
        )
        .unwrap();
    assert_eq!(
        repo.sequence_repo_activity(repo_id.clone(), 10)
            .await
            .unwrap(),
        2
    );
    backend.verify_checkpoints(&repo_id).unwrap();
    connection
        .execute(
            "UPDATE vcs_commits SET created_at='2020-01-01T00:00:00Z' WHERE repo_seq=1",
            [],
        )
        .unwrap();
    assert!(backend.verify_checkpoints(&repo_id).is_err());
}

#[tokio::test]
async fn checked_lifecycle_accepts_more_than_9999_descendants_atomically() {
    let (_directory, storage) = backend();
    storage.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let repo = Repo::new(Storage::new(Arc::new(storage)), CoreConfig::default());
    let root = ItemId::new();
    let children: Vec<_> = (0..10_000).map(|_| ItemId::new()).collect();
    let mut creates = vec![create_item_event(root.clone(), "FOLDER", None, "root")];
    creates.extend(children.iter().enumerate().map(|(index, id)| {
        create_item_event(id.clone(), "FILE", Some(root.clone()), &format!("child-{index}"))
    }));
    repo.append(repo_id.clone(), commit(creates)).await.unwrap();
    let mut descendants: Vec<_> = children.iter().cloned().map(|id| (id, 1)).collect();
    descendants.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut changes = vec![lifecycle_event(root.clone(), "FOLDER", 1, true)];
    changes.extend(children.iter().map(|id| lifecycle_event(id.clone(), "FILE", 1, true)));
    let accepted = repo.append_file_folder_checked(
        repo_id.clone(), commit(changes), &[], &[SubtreeClosureGuard {
            root_item_id: root.clone(),
            expected_non_tombstoned_descendants: descendants,
            require_empty_live_descendants: false,
        }],
    ).await.unwrap();
    let stored = repo.get_commit(repo_id.clone(), accepted.commit_id).await.unwrap().unwrap();
    assert_eq!(stored.event_count, 10_001);
    for id in [root, children[0].clone(), children[9_999].clone()] {
        let item = repo.get_item(repo_id.clone(), id).await.unwrap().unwrap();
        assert_eq!(item.lifecycle_state, synck_vcs::types::LifecycleState::Trash);
        assert_eq!(item.head_seq, 2);
    }
}
