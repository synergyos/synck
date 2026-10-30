use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use serde_json::json;
use sha2::{Digest, Sha256};
use synck_vcs::bundle::BundleReader;
use synck_vcs::metadata::MetadataRef;
use synck_vcs::object::{Object, plain_id};
use synck_local::portable::{
    Authority, BundleSnapshot, PortableRepository, WriteOptions, import_repository, materialize_repository,
};
use synck_vcs::config::CoreConfig;
use synck_vcs::repo::Repo;
use synck_vcs::storage::{Storage, StorageBackend};
use synck_vcs::types::{
    CommitProposal, CreateItem, EventContent, EventProposal, ItemId, ItemRevision, RepoId,
};
use synck_vcs_sqlite::SqliteStorage;

struct Fixture {
    _temporary: tempfile::TempDir,
    source: PathBuf,
    transfer: PathBuf,
    root: PathBuf,
    repo_id: RepoId,
    folder: ItemId,
    file: ItemId,
    chat: ItemId,
    body: Object,
    primary_blob: String,
    body_blob: String,
}

fn proposal(events: Vec<EventProposal>) -> CommitProposal {
    CommitProposal {
        author_id: "original-author".into(),
        committer_id: "original-recorder".into(),
        message: None,
        occurred_at: None,
        operation_id: Some(format!("test:{}", ItemId::new())),
        events,
    }
}

fn creation(id: &ItemId, kind: &str, parent: Option<ItemId>, name: &str) -> EventProposal {
    EventProposal::from_fields(json!({"item_id":id,"event_type":format!("{kind}::CREATE"),"create_item":CreateItem::tree(kind, parent, name)})).unwrap()
}

async fn fixture() -> Fixture {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    let source = root.join("source");
    let metadata = source.join(".synck");
    fs::create_dir_all(metadata.join("objects")).unwrap();
    let repo_id = RepoId::new();
    let folder = ItemId::new();
    let file = ItemId::new();
    let chat = ItemId::new();
    fs::write(metadata.join("config.json"),serde_json::to_vec(&json!({"format_version":7,"repo_id":repo_id,"author":"original-author","authority":"LOCAL"})).unwrap()).unwrap();
    fs::write(metadata.join("lock"), []).unwrap();
    let storage = SqliteStorage::new(metadata.join("repository.sqlite3"));
    storage.init_schema().await.unwrap();
    let core = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    let empty = plain_id(b"");
    fs::write(metadata.join("objects").join(hex::encode(empty.as_bytes())), b"").unwrap();
    let mut file_creation = creation(&file, "FILE", Some(folder.clone()), "original.txt");
    file_creation.set_content = Some(EventContent::bytes(empty));
    file_creation.set_metadata = Some(MetadataRef::new(json!({"content":{"size":0}})).unwrap());
    let chat_creation: EventProposal = EventProposal::from_fields(json!({
        "item_id":chat,"event_type":"CHAT::CREATE",
        "create_item":CreateItem::record("CHAT", None, None),
        "set_metadata":{"title":"discussion"}
    }))
    .unwrap();
    core.append(
        repo_id.clone(),
        proposal(vec![
            creation(&folder, "FOLDER", None, "documents"),
            file_creation,
            chat_creation,
        ]),
    )
    .await
    .unwrap();
    core.sequence_repo_activity(repo_id.clone(), 100)
        .await
        .unwrap();
    let primary = b"first version\n";
    let body = Object::new("original body\0🦀".as_bytes()).unwrap();
    // Body bytes also exercise exact-object transfer.
    let body_bytes = body.payload();
    let primary_blob = plain_id(primary).to_string();
    let body_blob = format!("sha256:{}", hex::encode(Sha256::digest(body_bytes)));
    fs::write(metadata.join("objects").join(&primary_blob[7..]), primary).unwrap();
    fs::write(metadata.join("objects").join(&body_blob[7..]), body_bytes).unwrap();
    fs::write(
        metadata
            .join("objects")
            .join(hex::encode(body.id().as_bytes())),
        body.payload(),
    )
    .unwrap();
    let file_event:EventProposal=EventProposal::from_fields(json!({"item_id":file,"event_type":"FILE::SET_CONTENT","base_seq":1,"set_metadata":{"content":{"size":primary.len()}},"set_content":EventContent::bytes(synck_vcs::types::ObjectId::parse(primary_blob.clone()).unwrap())})).unwrap();
    let message:EventProposal=EventProposal::from_fields(json!({"item_id":chat,"event_type":"CHAT::MESSAGE::CREATE","base_seq":1,"set_content":EventContent::body(body.id())})).unwrap();
    core.append(repo_id.clone(), proposal(vec![file_event, message]))
        .await
        .unwrap();
    core.sequence_repo_activity(repo_id.clone(), 100)
        .await
        .unwrap();
    storage.open_writable().unwrap().execute("INSERT INTO vcs_federation_domains VALUES (?1,'preserved-domain',9,'2026-01-01T00:00:00.000000000Z','2026-01-01T00:00:00.000000000Z')",[repo_id.as_str()]).unwrap();
    storage.project_imported_history(&repo_id).await.unwrap();
    let transfer = root.join("transfer");
    PortableRepository::open(&source)
        .unwrap()
        .archive(&transfer)
        .await
        .unwrap();
    Fixture {
        _temporary: temporary,
        source,
        transfer,
        root,
        repo_id,
        folder,
        file,
        chat,
        body,
        primary_blob,
        body_blob,
    }
}

#[tokio::test]
async fn complete_mixed_repository_reads_offline_and_keeps_exact_history_and_body_bytes() {
    let fixture = fixture().await;
    let destination = fixture.root.join("imported");
    import_repository(&fixture.transfer, &destination)
        .await
        .unwrap();
    let repository = PortableRepository::open(&destination).unwrap();
    assert_eq!(repository.repo_id(), &fixture.repo_id);
    assert_eq!(repository.authority(), Authority::Follower);
    let report = repository.verify().await.unwrap();
    assert_eq!(
        (report.commits, report.items, report.objects, report.metadata),
        (2, 3, 3, 3)
    );
    assert_eq!(
        repository.read_body(fixture.body.id()).unwrap().payload(),
        fixture.body.payload()
    );
    let chat =
        serde_json::to_value(repository.show_item(&fixture.chat, None).await.unwrap()).unwrap();
    assert_eq!(chat["item"]["subitems"][0]["body"], "original body\0🦀");
    let historical =
        serde_json::to_value(repository.show_item(&fixture.chat, Some(1)).await.unwrap()).unwrap();
    assert!(
        historical["item"]["subitems"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let file = repository.show_item(&fixture.file, None).await.unwrap();
    assert!(file.refs.is_empty());
    let mut bytes = Vec::new();
    repository
        .copy_blob(
            &synck_vcs::types::ObjectId::parse(&fixture.primary_blob).unwrap(),
            &mut bytes,
        )
        .unwrap();
    assert_eq!(bytes, b"first version\n");
    let folder = repository
        .list_items(Some(&fixture.folder), None, 1)
        .await
        .unwrap();
    assert_eq!(folder.items[0].item_id, fixture.file);
    let canonical = destination.join(".synck/repository.sqlite3");
    let cache = canonical.with_extension("state.sqlite3");
    let canonical_bytes = fs::read(&canonical).unwrap();
    repository.list_historical_items(1, None, None, 1).await.unwrap();
    fs::remove_file(&cache).unwrap();
    repository.verify().await.unwrap();
    assert!(!cache.exists(), "verification must not require a state cache");
    let historical = repository
        .list_historical_items(1, Some(&fixture.folder), None, 1)
        .await
        .unwrap();
    assert_eq!(historical.items[0].head_seq, 1);
    assert!(cache.exists());
    assert_eq!(fs::read(&canonical).unwrap(), canonical_bytes);
    let connection = rusqlite::Connection::open(&canonical).unwrap();
    let count: i64 = connection.query_row("SELECT count(*) FROM sqlite_schema WHERE name LIKE 'vcs_state_%'", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 0);

    let mut after = None;
    let mut names = Vec::new();
    loop {
        let page = repository
            .list_items(None, after.as_ref(), 1)
            .await
            .unwrap();
        names.extend(page.items.into_iter().filter_map(|item| item.name));
        after = page.next_cursor;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(names, vec!["documents"]);
    let exported = fixture.root.join("exported-primary.txt");
    let result = Command::new(env!("CARGO_BIN_EXE_synck"))
        .args(["item", "export"])
        .arg(fixture.file.as_str())
        .arg(&exported)
        .arg("--path")
        .arg(&destination)
        .args(["--through", "2"])
        .output()
        .unwrap();
    // The open library handle owns the same lock as the CLI. Release it first.
    assert!(!result.status.success());
    repository
        .export_file(&fixture.file, &exported, Some(2))
        .await
        .unwrap();
    assert_eq!(fs::read(&exported).unwrap(), b"first version\n");
    assert!(
        repository
            .export_file(&fixture.file, &exported, None)
            .await
            .is_err()
    );
    let archived = fixture.root.join("second-transfer");
    repository.archive(&archived).await.unwrap();
    assert!(!archived.join("history.ndjson").exists());
    assert!(!archived.join("transfer.json").exists());
    assert!(!archived.join(".synck/bodies").exists());
    assert_eq!(
        fs::read_dir(archived.join(".synck/objects"))
            .unwrap()
            .count(),
        3
    );
    for name in [&fixture.primary_blob[7..], &fixture.body_blob[7..]] {
        assert_eq!(
            fs::read(fixture.transfer.join(".synck/objects").join(name)).unwrap(),
            fs::read(archived.join(".synck/objects").join(name)).unwrap(),
        );
    }
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(archived.join(".synck/config.json")).unwrap()).unwrap();
    assert_eq!(config["format_version"], 7);
    assert!(config.get("author").is_none());
    assert_eq!(canonical_rows(&fixture.source), canonical_rows(&archived));
    let reimported = fixture.root.join("reimported");
    import_repository(&archived, &reimported).await.unwrap();
    assert_eq!(
        PortableRepository::open(&reimported).unwrap().repo_id(),
        &fixture.repo_id
    );
}

#[tokio::test]
async fn follower_activation_continuation_and_explicit_finalize_preserve_old_bytes() {
    let fixture = fixture().await;
    let destination = fixture.root.join("continued");
    import_repository(&fixture.transfer, &destination)
        .await
        .unwrap();
    let mut repository = PortableRepository::open(&destination).unwrap();
    let source = fixture.root.join("new-file");
    let prefix_bytes = b"SYNCK_UTF8_BODY_V1\0not a wrapped body\xff";
    fs::write(&source, prefix_bytes).unwrap();
    assert!(
        repository
            .create_file(None, "new.txt", &source)
            .await
            .is_err()
    );
    assert!(
        repository
            .append_chat_message(&fixture.chat, "denied")
            .await
            .is_err()
    );
    assert!(repository.activate("local-author", false).await.is_err());
    repository.activate("local-author", true).await.unwrap();
    let (created, _) = repository
        .create_file(Some(fixture.folder.clone()), "new.txt", &source)
        .await
        .unwrap();
    let extracted = fixture.root.join("prefix-file");
    repository
        .export_file(&created, &extracted, None)
        .await
        .unwrap();
    assert_eq!(fs::read(extracted).unwrap(), prefix_bytes);
    fs::write(&source, b"saved file bytes").unwrap();
    repository.save_file(&fixture.file, &source).await.unwrap();
    repository
        .append_chat_message(&fixture.chat, "offline continuation")
        .await
        .unwrap();
    let chat =
        serde_json::to_value(repository.show_item(&fixture.chat, None).await.unwrap()).unwrap();
    assert_eq!(chat["item"]["subitems"].as_array().unwrap().len(), 2);
    let file = repository.show_item(&fixture.file, None).await.unwrap();
    assert!(file.refs.is_empty());
    assert_eq!(file.item.projection.content_item_seq, Some(3));
    assert_eq!(
        repository
            .show_item(&created, None)
            .await
            .unwrap()
            .item
            .projection
            .item_type,
        "FILE"
    );
    repository.verify().await.unwrap();
    drop(repository);
    // Simulate a crash after an accepted local commit but before derived sequencing.
    let storage = SqliteStorage::new(destination.join(".synck/repository.sqlite3"));
    let core = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    core.append(fixture.repo_id.clone(),proposal(vec![EventProposal::from_fields(json!({"item_id":fixture.chat,"event_type":"CHAT::MESSAGE::CREATE","base_seq":3,"set_content":EventContent::body(fixture.body.id())})).unwrap()])).await.unwrap();
    let mut repository = PortableRepository::open(&destination).unwrap();
    assert!(repository.verify().await.is_err());
    repository.finalize().await.unwrap();
    repository.verify().await.unwrap();
    let mut original = Vec::new();
    repository
        .copy_blob(
            &synck_vcs::types::ObjectId::parse(&fixture.primary_blob).unwrap(),
            &mut original,
        )
        .unwrap();
    assert_eq!(original, b"first version\n");
    let archive = fixture.root.join("continued-transfer");
    repository.archive(&archive).await.unwrap();
    let second = fixture.root.join("continued-import");
    import_repository(&archive, &second).await.unwrap();
    assert_eq!(
        PortableRepository::open(&second).unwrap().authority(),
        Authority::Follower
    );
}

#[tokio::test]
async fn invalid_materialization_missing_body_and_extra_objects_never_publish() {
    let fixture = fixture().await;
    let body_path = fixture
        .transfer
        .join(".synck/objects")
        .join(hex::encode(fixture.body.id().as_bytes()));
    let body = fs::read(&body_path).unwrap();
    fs::remove_file(&body_path).unwrap();
    let destination = fixture.root.join("missing-body");
    assert!(
        import_repository(&fixture.transfer, &destination)
            .await
            .is_err()
    );
    assert!(!destination.exists());
    fs::write(body_path, body).unwrap();

    let source = fixture.root.join("empty-bundle");
    fs::create_dir_all(source.join("objects")).unwrap();
    let repo_id = RepoId::new();
    let generation = uuid::Uuid::now_v7();
    let mut bytes = serde_json::to_vec(&json!({
        "type": "synck_vcs_bundle_header", "bundle_format_version": 5,
        "repo_id": repo_id, "history_generation": generation.to_string(), "encrypted": false,
        "exported_at": "2026-01-02T00:00:00Z", "commit_format_version": 6,
        "schema": {"subsystem":"synck_vcs_sqlite", "schema_id":"core-v7"}
    })).unwrap();
    bytes.push(b'\n');
    let mut header_bytes = bytes.as_slice();
    let header = BundleReader::new(&mut header_bytes).read_header().unwrap();
    let end = serde_json::to_vec(&json!({
        "type": "synck_vcs_bundle_end", "record_count": 1,
        "checkpoint": format!("sha256:{}", hex::encode(header.genesis().unwrap())),
        "digest": format!("sha256:{}", hex::encode(Sha256::digest(&bytes))),
    })).unwrap();
    bytes.extend_from_slice(&end);
    bytes.push(b'\n');
    fs::write(source.join("history.ndjson"), &bytes).unwrap();
    let snapshot = BundleSnapshot {
        repo_id, terminal_repo_seq: 0,
        history_sha256: format!("sha256:{}", hex::encode(Sha256::digest(&bytes))),
        history_bytes: bytes.len() as u64,
    };
    materialize_repository(&source.join("history.ndjson"), &source.join("objects"), &snapshot, &fixture.root.join("valid-empty")).await.unwrap();
    let history = source.join("history.ndjson");
    let original = fs::read(&history).unwrap();
    fs::write(&history, &original[..original.len() - 1]).unwrap();
    let destination = fixture.root.join("truncated");
    assert!(
        materialize_repository(&history, &source.join("objects"), &snapshot, &destination)
            .await
            .is_err()
    );
    assert!(!destination.exists());
    fs::write(&history, original).unwrap();
    fs::write(
        source.join("objects").join("0".repeat(64)),
        b"unrelated private bytes",
    )
    .unwrap();
    let destination = fixture.root.join("extra");
    assert!(
        materialize_repository(&history, &source.join("objects"), &snapshot, &destination)
            .await
            .is_err()
    );
    assert!(!destination.exists());
    assert!(!fs::read_dir(&fixture.root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".synck-portable-")
    }));
}

#[tokio::test]
async fn bare_open_leaves_database_and_untracked_files_untouched_and_cli_requires_ack() {
    let fixture = fixture().await;
    let database = fixture.source.join(".synck/repository.sqlite3");
    let before = fs::read(&database).unwrap();
    fs::create_dir(fixture.source.join(".synck/tmp")).unwrap();
    fs::write(
        fixture.source.join(".synck/tmp/checkout-intent"),
        b"must remain",
    )
    .unwrap();
    fs::write(
        fixture.source.join("untracked"),
        b"working tree is not consulted",
    )
    .unwrap();
    let repository = PortableRepository::open(&fixture.source).unwrap();
    repository.list_items(None, None, 1).await.unwrap();
    drop(repository);
    assert_eq!(fs::read(database).unwrap(), before);
    assert_eq!(
        fs::read(fixture.source.join(".synck/tmp/checkout-intent")).unwrap(),
        b"must remain"
    );
    assert!(
        synck_local::Repository::discover(&fixture.source)
            .await
            .is_err()
    );
    let local = fixture.root.join("local");
    synck_local::init(&local, "author").await.unwrap();
    PortableRepository::open(&local)
        .unwrap()
        .verify()
        .await
        .unwrap();
    let denied = Command::new(env!("CARGO_BIN_EXE_synck"))
        .args(["repository", "activate"])
        .arg(&fixture.source)
        .args(["--author", "operator"])
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--source-retired"));
    let transfer = fixture.root.join("cli-transfer");
    let archived = Command::new(env!("CARGO_BIN_EXE_synck"))
        .args(["repository", "archive"])
        .arg(&transfer)
        .arg("--path")
        .arg(&fixture.source)
        .output()
        .unwrap();
    assert!(
        archived.status.success(),
        "{}",
        String::from_utf8_lossy(&archived.stderr)
    );
    let extracted = fixture.root.join("cli-extracted.txt");
    let result = Command::new(env!("CARGO_BIN_EXE_synck"))
        .args(["item", "export"])
        .arg(fixture.file.as_str())
        .arg(&extracted)
        .arg("--path")
        .arg(&fixture.source)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(extracted).unwrap(), b"first version\n");
    fs::write(
        fixture
            .source
            .join(".synck/objects")
            .join(&fixture.primary_blob[7..]),
        b"corrupt",
    )
    .unwrap();
    for json in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_synck"));
        if json {
            command.arg("--json");
        }
        let result = command
            .args(["repository", "verify"])
            .arg(&fixture.source)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn linked_repository_content_is_rejected_without_touching_its_target() {
    let fixture = fixture().await;
    let blob = fixture
        .transfer
        .join(".synck/objects")
        .join(&fixture.primary_blob[7..]);
    fs::remove_file(&blob).unwrap();
    let outside = fixture.root.join("outside");
    fs::write(&outside, b"untouched").unwrap();
    std::os::unix::fs::symlink(&outside, &blob).unwrap();
    let destination = fixture.root.join("linked");
    assert!(
        import_repository(&fixture.transfer, &destination)
            .await
            .is_err()
    );
    assert!(!destination.exists());
    assert_eq!(fs::read(outside).unwrap(), b"untouched");
}

// Frozen sidecar writes are deliberately unavailable through today's append
// API. Build their historical canonical records to exercise the import reader.
fn canonical_rows(root: &std::path::Path) -> Vec<Vec<String>> {
    let db = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    [
        "SELECT commit_id,repo_seq,author_id,committer_id,hex(events_digest) FROM vcs_commits ORDER BY repo_seq",
        "SELECT c.commit_id,e.item_id,e.item_seq,hex(e.payload),hex(e.event_payload_digest) FROM vcs_events e JOIN vcs_commits c USING(repo_id,commit_pk) ORDER BY c.repo_seq,e.commit_event_idx",
        "SELECT c.commit_id,e.owner_item_id,e.ref_seq,hex(e.payload),hex(e.event_payload_digest) FROM vcs_ref_events e JOIN vcs_commits c USING(repo_id,commit_pk) ORDER BY c.repo_seq,e.commit_event_idx",
        "SELECT item_id,head_seq,ref_head_seq FROM vcs_items ORDER BY item_id",
        "SELECT domain_id,last_domain_seq FROM vcs_federation_domains ORDER BY domain_id",
    ].into_iter().map(|sql| {
        let mut query = db.prepare(sql).unwrap();
        let columns = query.column_count();
        query.query_map([], |row| Ok((0..columns).map(|i| format!("{:?}", row.get_ref(i).unwrap())).collect::<Vec<_>>().join("|"))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }).collect()
}

#[tokio::test]
async fn unsupported_format_is_rejected_without_mutation() {
    let fixture = fixture().await;
    let source = &fixture.source;
    let path = source.join(".synck/config.json");
    let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for version in [3, 4, 5, 6, 8] {
        config["format_version"] = json!(version);
        let bytes = serde_json::to_vec(&config).unwrap();
        fs::write(&path, &bytes).unwrap();
        let error = PortableRepository::open(source).err().expect("unsupported format").to_string();
        assert!(error.contains("format-7"));
        if version == 6 { assert!(error.contains("Synck 0.1.x")); }
        let destination = fixture.root.join("rejected");
        assert!(import_repository(source, &destination).await.is_err());
        assert!(!destination.exists());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    // Identify the reader before interpreting another release's config shape.
    fs::write(&path, br#"{"format_version":6,"old_release_field":true}"#).unwrap();
    let error = PortableRepository::open(source).err().unwrap().to_string();
    assert!(error.contains("Synck 0.1.x"), "{error}");
}

#[tokio::test]
async fn cli_import_needs_no_author_and_local_writer_requires_one() {
    let fixture = fixture().await;
    let destination = fixture.root.join("cli-import");
    let result = Command::new(env!("CARGO_BIN_EXE_synck"))
        .args(["repository", "import"])
        .arg(&fixture.transfer)
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        PortableRepository::open(&destination).unwrap().authority(),
        Authority::Follower
    );
    let path = destination.join(".synck/config.json");
    let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(config.get("author").is_none());
    config["authority"] = json!("LOCAL");
    fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    assert!(
        PortableRepository::open(&destination)
            .err()
            .unwrap()
            .to_string()
            .contains("local writer has no author")
    );
}

#[tokio::test]
async fn repository_copy_includes_committed_wal_without_checkpointing_source() {
    let fixture = fixture().await;
    let database = fixture.source.join(".synck/repository.sqlite3");
    let storage = SqliteStorage::new(database.clone());
    // Keep a connection open so closing each writer does not checkpoint WAL.
    let keeper = storage.open_writable().unwrap();
    keeper.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    let before = fs::read(&database).unwrap();
    let core = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    core.append(
        fixture.repo_id.clone(),
        proposal(vec![
            EventProposal::from_fields(json!({
                "item_id":fixture.chat,
                "event_type":"CHAT::MESSAGE::CREATE",
                "base_seq":2,
                "set_content":EventContent::body(fixture.body.id())
            }))
            .unwrap(),
        ]),
    )
    .await
    .unwrap();
    core.sequence_repo_activity(fixture.repo_id.clone(), 100)
        .await
        .unwrap();
    storage
        .project_imported_history(&fixture.repo_id)
        .await
        .unwrap();
    assert!(
        fs::metadata(database.with_extension("sqlite3-wal"))
            .unwrap()
            .len()
            > 0
    );
    assert_eq!(fs::read(&database).unwrap(), before);
    let expected = canonical_rows(&fixture.source);
    let destination = fixture.root.join("wal-copy");
    PortableRepository::open(&fixture.source)
        .unwrap()
        .archive(&destination)
        .await
        .unwrap();
    assert_eq!(fs::read(&database).unwrap(), before);
    assert_eq!(canonical_rows(&destination), expected);
    let copy = PortableRepository::open(&destination).unwrap();
    assert_eq!(copy.verify().await.unwrap().commits, 3);
    assert_eq!(
        copy.show_item(&fixture.chat, None)
            .await
            .unwrap()
            .item
            .subitems
            .len(),
        2
    );
    drop(keeper);
}

#[tokio::test]
async fn complete_checkout_edits_files_without_rewriting_mixed_history() {
    use synck_local::{CommitOutcome, Repository};
    let fixture = fixture().await;
    let mut portable = PortableRepository::open(&fixture.transfer).unwrap();
    let before = portable.show_item(&fixture.chat, None).await.unwrap();
    portable
        .enable_checkout("independent-author", &[])
        .await
        .unwrap();
    assert_eq!(portable.authority(), Authority::IndependentLocal);
    drop(portable);
    let repository = Repository::discover(&fixture.transfer).await.unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    fs::write(
        fixture.transfer.join("documents/original.txt"),
        b"edited in Finder\n",
    )
    .unwrap();
    assert!(matches!(
        repository.commit("edit exported file").await.unwrap(),
        CommitOutcome::Committed(_)
    ));
    drop(repository);
    let portable = PortableRepository::open(&fixture.transfer).unwrap();
    assert_eq!(
        serde_json::to_value(portable.show_item(&fixture.chat, None).await.unwrap()).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert_eq!(portable.verify().await.unwrap().commits, 3);
    let copy = fixture.root.join("after-checkout");
    portable.archive(&copy).await.unwrap();
    drop(portable);
    let copied = PortableRepository::open(&copy).unwrap();
    assert!(copied.checkout_paths().is_none());
    assert!(!copy.join(".synck/repository.local.sqlite3").exists());
    assert_eq!(copied.authority(), Authority::Follower);
    assert_eq!(copied.verify().await.unwrap().commits, 3);
    assert_eq!(
        SqliteStorage::new(copy.join(".synck/repository.sqlite3"))
            .open_readonly()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name GLOB 'local_*'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn complete_checkout_history_shows_only_the_filesystem_view() {
    use synck_local::{CommitOutcome, Repository};
    let fixture = fixture().await;
    let mut portable = PortableRepository::open(&fixture.transfer).unwrap();
    portable
        .enable_checkout("independent-author", &[])
        .await
        .unwrap();
    drop(portable);
    let repository = Repository::discover(&fixture.transfer).await.unwrap();
    fs::write(fixture.transfer.join("documents/original.txt"), b"edited\n").unwrap();
    assert!(matches!(
        repository.commit("edit exported file").await.unwrap(),
        CommitOutcome::Committed(_)
    ));
    // The CHAT shares the repository root and history with the files, but
    // history commands report only files and folders, without failing.
    let paths = |changes: &[synck_local::Change]| {
        changes.iter().map(|c| c.path.clone()).collect::<Vec<_>>()
    };
    assert_eq!(repository.log(None).await.unwrap().len(), 3);
    assert_eq!(repository.detailed_log(Some(1)).await.unwrap().len(), 1);
    // The fixture's commit 1 creates a FILE before commit 2 gives it content,
    // so history comparisons start at commit 2. Commit 2 also appends a chat
    // message, and commit 1 creates the CHAT beside the folder.
    assert_eq!(
        paths(&repository.show("HEAD").await.unwrap().changes),
        ["documents/original.txt"]
    );
    assert_eq!(
        paths(&repository.compare("2", "HEAD").await.unwrap().changes),
        ["documents/original.txt"]
    );
    assert_eq!(
        repository.diff_revisions("2", "HEAD").await.unwrap().len(),
        1
    );
    let exported = fixture.root.join("exported-tree");
    repository
        .export_tree(&fixture.transfer, "HEAD", None, &exported)
        .await
        .unwrap();
    assert_eq!(
        fs::read(exported.join("documents/original.txt")).unwrap(),
        b"edited\n"
    );
    assert_eq!(fs::read_dir(&exported).unwrap().count(), 1);
    let view = repository.open_historical_view("HEAD").await.unwrap();
    let page = repository
        .historical_children(&view, None, None)
        .await
        .unwrap();
    assert_eq!(
        page.entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        ["documents"]
    );
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn complete_repository_log_pages_newest_first_with_events() {
    let fixture = fixture().await;
    let repository = PortableRepository::open(&fixture.transfer).unwrap();
    let first = repository.log(None, 1).await.unwrap();
    assert_eq!(first.commits.len(), 1);
    assert_eq!(first.commits[0].repo_seq, 2);
    assert_eq!(first.next_before, Some(2));
    let events = &first.commits[0].events;
    assert!(events.iter().any(|e| e.ledger == "ITEM"
        && e.item_id == fixture.chat
        && e.event_type == "CHAT::MESSAGE::CREATE"));
    assert!(events.iter().any(|e| e.ledger == "ITEM"
        && e.item_id == fixture.file
        && e.event_type == "FILE::SET_CONTENT"));
    let second = repository.log(first.next_before, 1).await.unwrap();
    assert_eq!(second.commits[0].repo_seq, 1);
    assert_eq!(second.next_before, None);
    assert!(repository.log(None, 0).await.is_err());
}

#[tokio::test]
async fn mismatched_repository_commands_name_the_repository_kind() {
    let fixture = fixture().await;
    let synck = |args: &[&str], cwd: &std::path::Path| {
        let output = Command::new(env!("CARGO_BIN_EXE_synck"))
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(!output.status.success());
        String::from_utf8(output.stderr).unwrap()
    };
    assert!(
        synck(&["status"], &fixture.transfer).contains("complete repository without a checkout")
    );
    assert!(
        synck(
            &["item", "list", "--after", "", "--path", "."],
            &fixture.transfer
        )
        .contains("--after must be the exact next_cursor JSON")
    );
}

#[tokio::test]
async fn complete_checkout_sparse_selection_never_commits_omitted_deletions() {
    use synck_local::Repository;
    let fixture = fixture().await;
    let mut portable = PortableRepository::open(&fixture.transfer).unwrap();
    portable
        .enable_checkout("independent-author", &["documents/original.txt".into()])
        .await
        .unwrap();
    drop(portable);
    let mut repository = Repository::discover(&fixture.transfer).await.unwrap();
    fs::write(fixture.transfer.join("documents/new.txt"), b"new\n").unwrap();
    // A sibling outside the literal selection is ignored by file status/commit.
    assert!(repository.status().await.unwrap().is_empty());
    let error = repository.set_checkout_paths(&[]).await.unwrap_err();
    assert!(error.to_string().contains("untracked"), "{error}");
    fs::remove_file(fixture.transfer.join("documents/new.txt")).unwrap();
    repository.set_checkout_paths(&[]).await.unwrap();
    fs::write(fixture.transfer.join("documents/new.txt"), b"new\n").unwrap();
    repository.commit("add sibling").await.unwrap();
    repository
        .set_checkout_paths(&["documents/original.txt".into()])
        .await
        .unwrap();
    assert!(!fixture.transfer.join("documents/new.txt").exists());
    assert!(repository.status().await.unwrap().is_empty());
    fs::remove_dir_all(fixture.transfer.join("documents")).unwrap();
    assert!(repository.commit("unsafe sparse deletion").await.is_err());
    drop(repository);
    assert_eq!(
        PortableRepository::open(&fixture.transfer)
            .unwrap()
            .verify()
            .await
            .unwrap()
            .commits,
        3
    );
}

#[tokio::test]
async fn complete_checkout_preserves_file_ids_through_filesystem_moves() {
    use synck_local::Repository;
    let fixture = fixture().await;
    let before = canonical_rows(&fixture.transfer);
    PortableRepository::open(&fixture.transfer)
        .unwrap()
        .enable_checkout("editor", &[])
        .await
        .unwrap();
    let repository = Repository::discover(&fixture.transfer).await.unwrap();
    fs::rename(
        fixture.transfer.join("documents/original.txt"),
        fixture.transfer.join("renamed.txt"),
    )
    .unwrap();
    fs::create_dir(fixture.transfer.join("new-folder")).unwrap();
    fs::write(fixture.transfer.join("new-folder/new.txt"), b"new").unwrap();
    repository.commit("move and create").await.unwrap();
    drop(repository);
    let portable = PortableRepository::open(&fixture.transfer).unwrap();
    let file = portable.show_item(&fixture.file, None).await.unwrap();
    assert_eq!(file.item.projection.name, "renamed.txt");
    assert!(file.item.projection.parent_folder_id.is_none());
    portable.verify().await.unwrap();
    let after = canonical_rows(&fixture.transfer);
    for (table, rows) in before.iter().enumerate() {
        if table == 3 {
            continue;
        }
        for row in rows {
            assert!(
                after[table].contains(row),
                "historical table {table} row changed"
            );
        }
    }
}

#[tokio::test]
async fn checkout_folder_deletion_leaves_linked_records_in_place() {
    use synck_local::Repository;
    let fixture = fixture().await;
    let storage = SqliteStorage::new(fixture.source.join(".synck/repository.sqlite3"));
    let core = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    let hidden = ItemId::new();
    core.append(
        fixture.repo_id.clone(),
        proposal(vec![
            EventProposal::from_fields(json!({
                "item_id":hidden,"event_type":"CHAT::CREATE",
                "create_item":CreateItem::record("CHAT", None, Some(fixture.folder.clone())),
            }))
            .unwrap(),
        ]),
    )
    .await
    .unwrap();
    core.sequence_repo_activity(fixture.repo_id.clone(), 100)
        .await
        .unwrap();
    storage
        .project_imported_history(&fixture.repo_id)
        .await
        .unwrap();
    let destination = fixture.root.join("with-hidden");
    PortableRepository::open(&fixture.source)
        .unwrap()
        .archive(&destination)
        .await
        .unwrap();
    PortableRepository::open(&destination)
        .unwrap()
        .enable_checkout("editor", &[])
        .await
        .unwrap();
    let repository = Repository::discover(&destination).await.unwrap();
    fs::remove_dir_all(destination.join("documents")).unwrap();
    repository.commit("delete folder").await.unwrap();
    drop(repository);
    let portable = PortableRepository::open(&destination).unwrap();
    assert_eq!(portable.verify().await.unwrap().commits, 4);
    assert_eq!(
        portable
            .show_item(&hidden, None)
            .await
            .unwrap()
            .item
            .projection
            .lifecycle_state,
        synck_vcs::types::LifecycleState::Live
    );
}

#[tokio::test]
async fn complete_checkout_fences_other_openers_and_refuses_initial_collisions() {
    use synck_local::{LocalError, Repository};
    let fixture = fixture().await;
    let mut portable = PortableRepository::open(&fixture.transfer).unwrap();
    fs::write(fixture.transfer.join("keep.txt"), b"do not overwrite").unwrap();
    assert!(portable.enable_checkout("editor", &[]).await.is_err());
    assert!(portable.checkout_paths().is_none());
    assert_eq!(
        fs::read(fixture.transfer.join("keep.txt")).unwrap(),
        b"do not overwrite"
    );
    fs::remove_file(fixture.transfer.join("keep.txt")).unwrap();
    portable.enable_checkout("editor", &[]).await.unwrap();
    assert!(matches!(
        Repository::discover(&fixture.transfer).await,
        Err(LocalError::Busy)
    ));
    drop(portable);
    let repository = Repository::discover(&fixture.transfer).await.unwrap();
    assert!(matches!(
        PortableRepository::open(&fixture.transfer),
        Err(LocalError::Busy)
    ));
    assert!(
        repository
            .working_tree()
            .await
            .unwrap()
            .iter()
            .any(|e| e.path == "documents/original.txt")
    );
    repository.gc().await.unwrap();
    assert!(
        repository
            .archive(&fixture.root.join("complete-archive"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn complete_checkout_sparse_move_requires_widening_before_commit() {
    let fixture = fixture().await;
    PortableRepository::open(&fixture.transfer)
        .unwrap()
        .enable_checkout("editor", &["documents/original.txt".into()])
        .await
        .unwrap();
    let repository = synck_local::Repository::discover(&fixture.transfer)
        .await
        .unwrap();
    fs::rename(
        fixture.transfer.join("documents/original.txt"),
        fixture.transfer.join("documents/renamed.txt"),
    )
    .unwrap();
    assert!(
        repository
            .status()
            .await
            .unwrap_err()
            .to_string()
            .contains("outside checkout selection")
    );
    assert!(repository.commit("unsafe boundary move").await.is_err());
    assert_eq!(
        fs::read(fixture.transfer.join("documents/renamed.txt")).unwrap(),
        b"first version\n"
    );
}

#[tokio::test]
async fn complete_checkout_recovery_refuses_changed_source_and_bare_open_stays_readonly() {
    let fixture = fixture().await;
    PortableRepository::open(&fixture.transfer)
        .unwrap()
        .enable_checkout("editor", &["documents/original.txt".into()])
        .await
        .unwrap();
    let metadata = fixture.transfer.join(".synck");
    let path = metadata.join("config.json");
    let config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    fs::write(
        metadata.join("checkout.json"),
        serde_json::to_vec(&json!({"paths":[],"pending":{"previous_paths":["documents/original.txt"]}})).unwrap(),
    )
    .unwrap();
    fs::write(metadata.join("checkout-source.json"),serde_json::to_vec(&json!([
        {"path":"documents","item_id":null,"item_type":"FOLDER","plain_digest":null,"size":null},
        {"path":"documents/original.txt","item_id":null,"item_type":"FILE","plain_digest":hex::encode(Sha256::digest(b"first version\n")),"size":14}
    ])).unwrap()).unwrap();
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    fs::write(
        fixture.transfer.join("documents/original.txt"),
        b"new uncommitted bytes",
    )
    .unwrap();
    let portable = PortableRepository::open(&fixture.transfer).unwrap();
    assert!(
        portable
            .archive(&fixture.root.join("unsafe-copy"))
            .await
            .is_err()
    );
    drop(portable);
    let error = synck_local::Repository::discover(&fixture.transfer)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("changed during checkout"), "{error}");
    assert_eq!(
        fs::read(fixture.transfer.join("documents/original.txt")).unwrap(),
        b"new uncommitted bytes"
    );
    fs::write(
        fixture.transfer.join("documents/original.txt"),
        b"first version\n",
    )
    .unwrap();
    let repository = synck_local::Repository::discover(&fixture.transfer)
        .await
        .unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    drop(repository);
    assert!(
        PortableRepository::open(&fixture.transfer)
            .unwrap()
            .checkout_paths()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn local_email_import_preserves_source_and_materializes_one_atomic_email() {
    let fixture = fixture().await;
    let source = fixture.root.join("source.EML");
    let bytes =
        include_bytes!("../../synck-items/tests/fixtures/imported_email/representative.eml");
    fs::write(&source, bytes).unwrap();
    let mut repository = PortableRepository::open(&fixture.source).unwrap();
    let (id, commit) = repository
        .import_email(Some(fixture.folder.clone()), &source, None)
        .await
        .unwrap();
    let item = repository.show_item(&id, None).await.unwrap();
    assert_eq!(item.item.projection.item_type, "EMAIL");
    assert_eq!(item.item.projection.name, "Imported fixture ✓");
    let metadata = &item.item.projection.metadata_json["mailcal"];
    assert_eq!(metadata["source"]["filename"], "source.EML");
    let rendered = repository.render_email(&id, None).await.unwrap();
    assert!(!rendered.body_markdown.is_empty());
    assert!(metadata["source"].get("blob_id").is_none());
    assert!(metadata.get("raw_rfc822_blob_id").is_none());
    assert!(metadata.get("body").is_none());
    assert_eq!(fs::read(&source).unwrap(), bytes);
    let source_blob = &item.item.projection.content_roles["source"];
    let stored = fs::read(fixture.source.join(".synck/objects").join(&source_blob.as_str()[7..])).unwrap();
    assert_eq!(stored, bytes);
    assert!(
        item.item.subitems.len() >= 2,
        "all MIME attachments must remain durable"
    );
    let exported = fixture.root.join("download-original.eml");
    repository.export_file(&id, &exported, None).await.unwrap();
    assert_eq!(fs::read(exported).unwrap(), bytes);
    repository.verify().await.unwrap();
    let storage = SqliteStorage::new(fixture.source.join(".synck/repository.sqlite3"));
    let connection = storage.open_readonly().unwrap();
    let author: String = connection
        .query_row(
            "SELECT author_id FROM vcs_commits WHERE repo_id=?1 AND commit_id=?2",
            rusqlite::params![
                fixture.repo_id.as_str(),
                hex::decode(&commit.to_string()[7..]).unwrap()
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(author, "original-author");
}

#[tokio::test]
async fn local_email_import_rejects_bad_sources_without_writing() {
    let fixture = fixture().await;
    let source = fixture.root.join("bad.msg");
    let bytes = b"this is not an Outlook message";
    fs::write(&source, bytes).unwrap();
    let not_mail = fixture.root.join("message.txt");
    fs::write(&not_mail, b"From: sender@example.test\r\n\r\nbody").unwrap();
    let mut repository = PortableRepository::open(&fixture.source).unwrap();
    let before = repository.verify().await.unwrap();
    for (path, title, expected) in [
        (&source, None, "EMAIL_IMPORT_"),
        (&not_mail, None, "EMAIL_IMPORT_INVALID_FORMAT"),
        (&source, Some(" "), "title must not be empty"),
    ] {
        let error = repository.import_email(None, path, title).await.unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
    let after = repository.verify().await.unwrap();
    assert_eq!(
        (before.commits, before.items, before.objects),
        (after.commits, after.items, after.objects)
    );
    assert_eq!(fs::read(source).unwrap(), bytes);
}

#[tokio::test]
async fn email_files_stay_files_through_commit_edit_and_rename() {
    let fixture = fixture().await;
    let mut portable = PortableRepository::open(&fixture.source).unwrap();
    // A new source may hold an email; it is still the FILE's next bytes.
    let saved = fixture.root.join("new.eml");
    fs::write(&saved, b"From: sender@example.test\r\n\r\nsaved").unwrap();
    portable.save_file(&fixture.file, &saved).await.unwrap();
    portable.enable_checkout("local-editor", &[]).await.unwrap();
    drop(portable);
    let path = fixture.source.join("documents/new.EML");
    let original =
        include_bytes!("../../synck-items/tests/fixtures/imported_email/representative.eml");
    fs::write(&path, original).unwrap();
    fs::write(fixture.source.join("bad.msg"), b"not a message").unwrap();
    let repository = synck_local::Repository::discover(&fixture.source)
        .await
        .unwrap();
    repository.commit("Add message files").await.unwrap();
    assert!(repository.status().await.unwrap().is_empty());

    fs::write(&path, b"edited message").unwrap();
    repository.commit("Edit the message").await.unwrap();
    fs::rename(&path, fixture.source.join("documents/renamed.txt")).unwrap();
    repository.commit("Rename it").await.unwrap();
    fs::rename(
        fixture.source.join("documents/renamed.txt"),
        fixture.source.join("documents/renamed.msg"),
    )
    .unwrap();
    repository.commit("Rename it back").await.unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    drop(repository);

    let portable = PortableRepository::open(&fixture.source).unwrap();
    portable.verify().await.unwrap();
    assert!(portable.list_records(None, None, 100).await.unwrap().iter()
        .all(|item| item.item_type != "EMAIL"));
    let page = portable
        .list_items(Some(&fixture.folder), None, 100)
        .await
        .unwrap();
    assert!(page.items.iter().any(|item| {
        item.item_type == "FILE" && item.name.as_deref() == Some("renamed.msg")
    }));
}

#[tokio::test]
async fn import_email_records_an_email_and_leaves_the_working_file_alone() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cli");
    let binary = env!("CARGO_BIN_EXE_synck");
    let run = |arguments: &[&str]| {
        let output = Command::new(binary)
            .current_dir(&root)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    synck_local::portable::init_repository(&root, "cli-author")
        .await
        .unwrap();
    let message = b"From: sender@example.test\r\nSubject: Message\r\n\r\nbody";
    fs::write(root.join("message.eml"), message).unwrap();
    let imported: serde_json::Value = serde_json::from_str(&run(&[
        "repository", "import-email", "message.eml", "--title", "Saved", "-m", "Import email",
    ]))
    .unwrap();
    assert!(imported["commit_id"].is_string());
    assert!(run(&["show", "HEAD", "--events"]).contains("EMAIL::CREATE"));
    assert!(run(&["log", "--events"]).contains("EMAIL::CREATE"));
    assert_eq!(fs::read(root.join("message.eml")).unwrap(), message);

    // The file on disk is not the record. Committing it adds a FILE.
    assert!(run(&["status"]).contains("message.eml"));
    run(&["commit", "-m", "Keep the file too"]);
    assert!(run(&["status"]).contains("working tree clean"));
    let portable = PortableRepository::open(&root).unwrap();
    portable.verify().await.unwrap();
    let records = portable.list_records(None, None, 10).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].item_type, "EMAIL");
    assert_eq!(
        portable.show_item(&records[0].item_id, None).await.unwrap().item.projection.name,
        "Saved"
    );
    let items = portable.list_items(None, None, 10).await.unwrap().items;
    assert!(items.iter().any(|item| {
        item.item_type == "FILE" && item.name.as_deref() == Some("message.eml")
    }));
}

#[tokio::test]
async fn duplicates_record_their_origin_and_merges_move_the_merge_base() {
    let fixture = fixture().await;
    let mut repository = PortableRepository::open(&fixture.source).unwrap();
    let head = repository.show_item(&fixture.file, None).await.unwrap();
    let origin = head.item.projection.latest_item_seq;
    let (copy, _) = repository
        .duplicate_file_with(&fixture.file, None, "copy.txt", &Default::default())
        .await
        .unwrap();
    let shown = repository.show_item(&copy, None).await.unwrap().item.projection;
    assert_eq!(shown.derived_from.as_ref().unwrap().item_id, fixture.file);
    assert_eq!(shown.derived_from.as_ref().unwrap().item_seq, origin);
    assert_eq!(shown.content_blob_id, head.item.projection.content_blob_id);

    let edited = fixture.root.join("edited.txt");
    fs::write(&edited, b"edited in the copy").unwrap();
    repository.save_file(&copy, &edited).await.unwrap();
    let copy_seq = repository.show_item(&copy, None).await.unwrap().item.projection.latest_item_seq;
    let merge = |item_seq| WriteOptions {
        merged_from: Some(ItemRevision { item_id: copy.clone(), item_seq }),
        ..Default::default()
    };
    for (item_seq, expected) in [(copy_seq + 1, "does not exist"), (0, "does not exist")] {
        let error = repository
            .save_file_with(&fixture.file, &edited, None, &merge(item_seq))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
    repository
        .save_file_with(&fixture.file, &edited, None, &merge(copy_seq))
        .await
        .unwrap();
    let merged = repository.show_item(&fixture.file, None).await.unwrap().item.projection;
    assert_eq!(merged.merged_from, vec![ItemRevision { item_id: copy.clone(), item_seq: copy_seq }]);
    repository.verify().await.unwrap();

    // A checkout's write path records the same fact.
    repository.enable_checkout("local-editor", &[]).await.unwrap();
    let (second, _) = repository
        .duplicate_file_with(&fixture.file, None, "second.txt", &Default::default())
        .await
        .unwrap();
    assert_eq!(fs::read(fixture.source.join("second.txt")).unwrap(), b"edited in the copy");
    let shown = repository.show_item(&second, None).await.unwrap().item.projection;
    assert_eq!(shown.derived_from.unwrap().item_id, fixture.file);
    repository.verify().await.unwrap();
}

#[tokio::test]
async fn email_import_regression_msg_rename_roundtrip_preserves_original_and_generated_mail() {
    use base64::Engine as _;
    let fixture = fixture().await;
    let source = fixture.root.join("original.MSG");
    let encoded = include_str!("../../synck-items/tests/fixtures/imported_email/ascii.msg.b64");
    let encoded = encoded
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    fs::write(&source, &bytes).unwrap();
    let mut repository = PortableRepository::open(&fixture.source).unwrap();
    let (id, _) = repository
        .import_email(None, &source, Some("Renamed message"))
        .await
        .unwrap();
    let item = repository.show_item(&id, None).await.unwrap();
    assert_eq!(item.item.projection.name, "Renamed message");
    let mail = &item.item.projection.metadata_json["mailcal"];
    assert_eq!(mail["source"]["filename"], "original.MSG");
    assert_eq!(mail["source"]["format"], "msg");
    assert!(mail.get("raw_rfc822_provenance").is_none());
    assert!(mail.get("raw_rfc822_blob_id").is_none());
    assert!(mail["source"].get("blob_id").is_none());
    assert!(item.item.projection.content_roles.contains_key("source"));
    let rendered = repository.render_email(&id, None).await.unwrap();
    assert!(rendered.raw_rfc822_generated);
    assert!(!rendered.raw_rfc822.is_empty());
    let copy = fixture.root.join("email-copy");
    repository.archive(&copy).await.unwrap();
    let imported = PortableRepository::open(&copy).unwrap();
    let destination = fixture.root.join("retained-original.msg");
    imported.export_file(&id, &destination, None).await.unwrap();
    assert_eq!(fs::read(destination).unwrap(), bytes);
    imported.verify().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn email_import_failed_object_write_can_retry_without_corrupt_publication() {
    use std::os::unix::process::CommandExt;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    synck_local::portable::init_repository_with_mode(
        &root,
        "local-author",
        synck_local::StorageMode::WholeFile,
    )
    .await
    .unwrap();
    let source = temporary.path().join("message.eml");
    let mut bytes = b"From: sender@example.test\r\nSubject: Message\r\n\r\n".to_vec();
    bytes.resize(8 * 1024 * 1024, b'a');
    fs::write(&source, &bytes).unwrap();
    let digest = hex::encode(Sha256::digest(&bytes));
    let mut command = Command::new(env!("CARGO_BIN_EXE_synck"));
    command
        .args(["repository", "import-email"])
        .arg(&source)
        .args(["--title", "Message", "--path"])
        .arg(&root);
    // Fault only the child: let SQLite open normally, then fail the larger
    // immutable source write partway through with a recoverable write error.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let limit = libc::rlimit {
                rlim_cur: 4 * 1024 * 1024,
                rlim_max: 4 * 1024 * 1024,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let failed = command.output().unwrap();
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("write portable file"),
        "{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    let objects = root.join(".synck/objects");
    assert!(!objects.join(&digest).exists());
    assert_eq!(fs::read_dir(&objects).unwrap().count(), 1);
    let mut repository = PortableRepository::open(&root).unwrap();
    let (id, _) = repository
        .import_email(None, &source, Some("Message"))
        .await
        .unwrap();
    let source_id = repository.show_item(&id, None).await.unwrap().item.projection.content_roles["source"].clone();
    let object_path = objects.join(hex::encode(source_id.as_bytes()));
    assert_eq!(fs::read(&object_path).unwrap(), bytes);
    let exported = temporary.path().join("original.eml");
    repository.export_file(&id, &exported, None).await.unwrap();
    assert_eq!(fs::read(exported).unwrap(), bytes);
    assert_eq!(repository.verify().await.unwrap().commits, 2);

    fs::write(&object_path, b"corrupt existing object").unwrap();
    assert!(repository.verify().await.is_err());
    assert_eq!(fs::read(object_path).unwrap(), b"corrupt existing object");
}

#[tokio::test]
async fn storage_modes_preserve_canonical_history_ids_and_exact_closure() {
    use synck_local::StorageMode;
    let fixture = fixture().await;
    let source = PortableRepository::open(&fixture.transfer).unwrap();
    assert_eq!(source.storage_mode(), StorageMode::WholeFile);
    let history = serde_json::to_value(source.log(None, 512).await.unwrap()).unwrap();
    let original = source.verify().await.unwrap();
    let chunked = fixture.root.join("chunked");
    source
        .archive_with_mode(&chunked, StorageMode::Chunked)
        .await
        .unwrap();
    let mut copy = PortableRepository::open(&chunked).unwrap();
    assert_eq!(copy.storage_mode(), StorageMode::Chunked);
    assert!(!copy.optimize_storage().await.unwrap());
    assert_eq!(
        serde_json::to_value(copy.log(None, 512).await.unwrap()).unwrap(),
        history
    );
    assert_eq!(
        copy.read_body(fixture.body.id()).unwrap().payload(),
        fixture.body.payload()
    );
    let report = copy.verify().await.unwrap();
    assert_eq!(
        (report.objects, report.metadata),
        (original.objects, original.metadata)
    );
    let preserved = fixture.root.join("preserved");
    copy.archive(&preserved).await.unwrap();
    let preserved = PortableRepository::open(&preserved).unwrap();
    assert_eq!(preserved.storage_mode(), StorageMode::Chunked);
    assert_eq!(
        serde_json::to_value(preserved.log(None, 512).await.unwrap()).unwrap(),
        history
    );
    let preserved_report = preserved.verify().await.unwrap();
    assert_eq!(preserved_report.objects, original.objects);
    assert!(preserved_report.missing_objects.is_empty());
    for id in [&fixture.primary_blob, &fixture.body_blob] {
        let id = synck_vcs::types::ObjectId::parse(id).unwrap();
        let mut expected = Vec::new();
        let mut actual = Vec::new();
        source.copy_blob(&id, &mut expected).unwrap();
        copy.copy_blob(&id, &mut actual).unwrap();
        assert_eq!(actual, expected);
        actual.clear();
        preserved.copy_blob(&id, &mut actual).unwrap();
        assert_eq!(actual, expected);
    }
    let whole = fixture.root.join("whole-again");
    copy.archive_with_mode(&whole, StorageMode::WholeFile)
        .await
        .unwrap();
    let mut whole = PortableRepository::open(&whole).unwrap();
    assert_eq!(whole.storage_mode(), StorageMode::WholeFile);
    assert_eq!(
        serde_json::to_value(whole.log(None, 512).await.unwrap()).unwrap(),
        history
    );
    assert!(whole.optimize_storage().await.unwrap());
    whole.verify().await.unwrap();
    // Missing logical mappings are closure failures, even if all other chunks are valid.
    let connection =
        rusqlite::Connection::open(chunked.join(".synck/object-store.sqlite3")).unwrap();
    connection
        .execute(
            "DELETE FROM blob_chunks WHERE blob_digest=?1",
            [hex::decode(&fixture.primary_blob[7..]).unwrap()],
        )
        .unwrap();
    assert!(copy.verify().await.is_err());
}

#[tokio::test]
async fn chunked_append_stores_only_the_changed_tail_and_keeps_old_version() {
    use std::io::Write;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    synck_local::portable::init_repository(&root, "author")
        .await
        .unwrap();
    let source = temporary.path().join("large.bin");
    // Deterministic incompressible bytes: compression cannot hide a full rewrite.
    let mut state = 0x1234_5678_9abc_def0u64;
    let original: Vec<u8> = (0..10 * 1024 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    fs::write(&source, &original).unwrap();
    let mut repository = PortableRepository::open(&root).unwrap();
    let (id, _) = repository
        .create_file(None, "large.bin", &source)
        .await
        .unwrap();
    let database = root.join(".synck/object-store.sqlite3");
    let inventory = || {
        rusqlite::Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT count(*),sum(raw_size),sum(length(payload)) FROM chunks",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .unwrap()
    };
    let before = inventory();
    let before_disk = fs::metadata(&database).unwrap().len();
    fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(&[42; 100])
        .unwrap();
    repository.save_file(&id, &source).await.unwrap();
    let after = inventory();
    let disk_growth = fs::metadata(&database).unwrap().len() - before_disk;
    assert!(
        disk_growth <= 512 * 1024,
        "object database grew by {disk_growth} bytes"
    );
    assert!((1..=2).contains(&(after.0 - before.0)));
    assert!(after.1 - before.1 <= 256 * 1024 + 100);
    assert!(after.2 - before.2 <= 256 * 1024 + 100);
    eprintln!(
        "10 MiB + 100 bytes: added {} chunks, {} raw bytes, {} stored payload bytes; database grew {disk_growth} bytes",
        after.0 - before.0,
        after.1 - before.1,
        after.2 - before.2
    );
    let old = temporary.path().join("old.bin");
    repository.export_file(&id, &old, Some(2)).await.unwrap();
    assert_eq!(fs::read(old).unwrap(), original);
    let current = temporary.path().join("current.bin");
    repository.export_file(&id, &current, None).await.unwrap();
    assert_eq!(fs::read(current).unwrap(), fs::read(source).unwrap());
    repository.verify().await.unwrap();
}

#[tokio::test]
async fn chunked_bare_writes_and_cli_conversion_preserve_readable_content() {
    let fixture = fixture().await;
    let destination = fixture.root.join("cli-chunked");
    let binary = env!("CARGO_BIN_EXE_synck");
    let imported = Command::new(binary)
        .args(["repository", "import"])
        .arg(&fixture.transfer)
        .arg(&destination)
        .args(["--storage-mode", "CHUNKED"])
        .output()
        .unwrap();
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let mut repository = PortableRepository::open(&destination).unwrap();
    repository.activate("local", true).await.unwrap();
    repository
        .append_chat_message(&fixture.chat, "new chunked body")
        .await
        .unwrap();
    let mail = fixture.root.join("message.eml");
    let bytes = b"From: sender@example.test\r\nSubject: Message\r\n\r\nchunked email";
    fs::write(&mail, bytes).unwrap();
    let (email, _) = repository.import_email(None, &mail, None).await.unwrap();
    let exported = fixture.root.join("exported.eml");
    repository
        .export_file(&email, &exported, None)
        .await
        .unwrap();
    assert_eq!(fs::read(exported).unwrap(), bytes);
    repository.verify().await.unwrap();
    drop(repository);
    let optimized = Command::new(binary)
        .current_dir(&destination)
        .args(["storage", "optimize"])
        .output()
        .unwrap();
    assert!(
        optimized.status.success(),
        "{}",
        String::from_utf8_lossy(&optimized.stderr)
    );
    assert!(String::from_utf8_lossy(&optimized.stdout).contains("already optimized"));
    PortableRepository::open(&destination)
        .unwrap()
        .verify()
        .await
        .unwrap();
}

#[tokio::test]
async fn storage_optimization_retries_without_switching_authority_early() {
    use synck_local::StorageMode;
    let fixture = fixture().await;
    let root = fixture.root.join("retry");
    import_repository(&fixture.transfer, &root).await.unwrap();
    let objects = root.join(".synck/objects");
    let old = fs::read(objects.join(&fixture.primary_blob[7..])).unwrap();
    // Valid objects sort first: capture them, then fail before publishing the mode.
    fs::write(objects.join("z-unexpected-entry"), b"keep me").unwrap();
    let mut repository = PortableRepository::open(&root).unwrap();
    assert!(repository.optimize_storage().await.is_err());
    assert_eq!(repository.storage_mode(), StorageMode::WholeFile);
    assert_eq!(
        fs::read(objects.join(&fixture.primary_blob[7..])).unwrap(),
        old
    );
    drop(repository);
    let mut repository = PortableRepository::open(&root).unwrap();
    assert_eq!(repository.storage_mode(), StorageMode::WholeFile);
    repository.verify().await.unwrap();
    fs::remove_file(objects.join("z-unexpected-entry")).unwrap();
    assert!(repository.optimize_storage().await.unwrap());
    // Model a crash after publication but before deleting the last whole object.
    fs::write(objects.join(&fixture.primary_blob[7..]), &old).unwrap();
    drop(repository);
    let mut repository = PortableRepository::open(&root).unwrap();
    assert_eq!(repository.storage_mode(), StorageMode::Chunked);
    assert!(!repository.optimize_storage().await.unwrap());
    assert_eq!(fs::read_dir(&objects).unwrap().count(), 0);
    fs::write(objects.join(&fixture.primary_blob[7..]), &old).unwrap();
    let connection = rusqlite::Connection::open(root.join(".synck/object-store.sqlite3")).unwrap();
    connection
        .execute(
            "DELETE FROM blob_chunks WHERE blob_digest=?1",
            [hex::decode(&fixture.primary_blob[7..]).unwrap()],
        )
        .unwrap();
    // A leftover whole file must never mask damage to the authoritative store.
    assert!(repository.verify().await.is_err());
    assert!(repository.optimize_storage().await.is_err());
    assert_eq!(
        fs::read(objects.join(&fixture.primary_blob[7..])).unwrap(),
        old
    );
}

#[tokio::test]
async fn whole_export_cli_import_verify_and_optimize() {
    let fixture = fixture().await;
    let root = fixture.root.join("whole-import");
    let binary = env!("CARGO_BIN_EXE_synck");
    let imported = Command::new(binary)
        .args(["repository", "import"])
        .arg(&fixture.transfer)
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    assert_eq!(
        PortableRepository::open(&root).unwrap().storage_mode(),
        synck_local::StorageMode::WholeFile
    );
    for arguments in [
        vec!["repository", "verify"],
        vec!["storage", "optimize"],
        vec!["repository", "verify"],
    ] {
        let result = Command::new(binary)
            .current_dir(&root)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_eq!(
        PortableRepository::open(&root).unwrap().storage_mode(),
        synck_local::StorageMode::Chunked
    );
    assert_eq!(
        PortableRepository::open(&fixture.transfer)
            .unwrap()
            .storage_mode(),
        synck_local::StorageMode::WholeFile
    );
}

#[tokio::test]
async fn history_verifies_with_absent_content_and_metadata_but_complete_export_refuses() {
    let fixture = fixture().await;
    let repository = PortableRepository::open(&fixture.source).unwrap();
    let before = repository.verify().await.unwrap();
    fs::remove_file(fixture.source.join(".synck/objects").join(&fixture.primary_blob[7..])).unwrap();
    let storage = SqliteStorage::new(fixture.source.join(".synck/repository.sqlite3"));
    storage.open_writable().unwrap().execute("DELETE FROM vcs_metadata_objects", []).unwrap();
    let after = repository.verify().await.unwrap();
    assert_eq!(before.commits, after.commits);
    assert_eq!(after.missing_objects, vec![synck_vcs::types::ObjectId::parse(&fixture.primary_blob).unwrap()]);
    assert_eq!(after.missing_metadata.len(), before.metadata);
    assert!(!after.missing_metadata.is_empty());
    let export = fixture.root.join("incomplete-export");
    assert!(repository.archive(&export).await.unwrap_err().to_string().contains("complete export requires"));
}
