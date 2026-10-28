use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use serde_json::json;
use sha2::{Digest, Sha256};
use synck_items::content::ContentObject;
use synck_local::portable::{
    Authority, BundleSnapshot, PortableRepository, import_repository, materialize_repository,
};
use synck_vcs::config::CoreConfig;
use synck_vcs::repo::Repo;
use synck_vcs::storage::{Storage, StorageBackend};
use synck_vcs::types::{
    CommitProposal, CreateItem, EventContent, EventProposal, ItemId, RefEventProposal, RepoId,
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
    body: ContentObject,
    primary_blob: String,
    fork_blob: String,
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
    serde_json::from_value(json!({"item_id":id,"event_type":format!("{kind}::CREATE"),"create_item":CreateItem {item_type:kind.into(),parent_folder_id:parent,name:name.into()}})).unwrap()
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
    fs::write(metadata.join("config.json"),serde_json::to_vec(&json!({"format_version":6,"repo_id":repo_id,"author":"original-author","authority":"LOCAL"})).unwrap()).unwrap();
    fs::write(metadata.join("lock"), []).unwrap();
    let storage = SqliteStorage::new(metadata.join("repository.sqlite3"));
    storage.init_schema().await.unwrap();
    let core = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    core.append(
        repo_id.clone(),
        proposal(vec![
            creation(&folder, "FOLDER", None, "documents"),
            creation(&file, "FILE", Some(folder.clone()), "original.txt"),
            creation(&chat, "CHAT", None, "discussion"),
        ]),
    )
    .await
    .unwrap();
    core.sequence_repo_activity(repo_id.clone(), 100)
        .await
        .unwrap();
    let primary = b"first version\n";
    let body = ContentObject::new("original body\0🦀").unwrap();
    // The same exact payload is a resource-fork blob and a wrapped body.
    let fork = body.payload();
    let primary_blob = format!("sha256:{}", hex::encode(Sha256::digest(primary)));
    let fork_blob = format!("sha256:{}", hex::encode(Sha256::digest(fork)));
    fs::write(metadata.join("objects").join(&primary_blob[7..]), primary).unwrap();
    fs::write(metadata.join("objects").join(&fork_blob[7..]), fork).unwrap();
    fs::write(
        metadata
            .join("objects")
            .join(hex::encode(body.id().as_bytes())),
        body.payload(),
    )
    .unwrap();
    let file_event:EventProposal=serde_json::from_value(json!({"item_id":file,"event_type":"FILE::SET_CONTENT","base_seq":1,"set_metadata":{"content":{"size":primary.len()}},"set_content":EventContent::Blob {blob_id:primary_blob.clone()}})).unwrap();
    let message:EventProposal=serde_json::from_value(json!({"item_id":chat,"event_type":"CHAT::MESSAGE::CREATE","base_seq":1,"set_content":EventContent::Content {content_id:body.id()}})).unwrap();
    core.append_with_refs(
        repo_id.clone(),
        proposal(vec![file_event, message]),
        vec![RefEventProposal {
            owner_item_id: file.clone(),
            event_type: synck_items::file_resources::SET.into(),
            set_metadata: Some(
                json!({"primary_item_seq":2,"primary_commit_event_idx":0,"size":fork.len()}),
            ),
            set_content: Some(EventContent::Blob {
                blob_id: fork_blob.clone(),
            }),
            base_ref_seq: 0,
            initial_ref_seq: None,
        }],
    )
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
        fork_blob,
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
        (report.commits, report.items, report.blobs, report.bodies),
        (2, 3, 2, 1)
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
    assert_eq!(file.refs.len(), 1);
    assert!(file.item.resource_fork.is_some());
    let mut bytes = Vec::new();
    repository
        .copy_blob(
            &synck_vcs::types::BlobId::parse(&fixture.primary_blob).unwrap(),
            &mut bytes,
        )
        .unwrap();
    assert_eq!(bytes, b"first version\n");
    let folder = repository
        .list_items(Some(&fixture.folder), None, 1)
        .await
        .unwrap();
    assert_eq!(folder.items[0].item_id, fixture.file);
    let historical = repository
        .list_historical_items(1, Some(&fixture.folder), None, 1)
        .await
        .unwrap();
    assert_eq!(historical.items[0].head_seq, 1);
    let mut after = None;
    let mut names = Vec::new();
    loop {
        let page = repository
            .list_items(None, after.as_ref(), 1)
            .await
            .unwrap();
        names.extend(page.items.into_iter().map(|item| item.name));
        after = page.next_cursor;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(names, vec!["discussion", "documents"]);
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
        2
    );
    for name in [&fixture.primary_blob[7..], &fixture.fork_blob[7..]] {
        assert_eq!(
            fs::read(fixture.transfer.join(".synck/objects").join(name)).unwrap(),
            fs::read(archived.join(".synck/objects").join(name)).unwrap(),
        );
    }
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(archived.join(".synck/config.json")).unwrap()).unwrap();
    assert_eq!(config["format_version"], 6);
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
    assert_eq!(file.refs.len(), 1);
    assert_eq!(
        file.item.resource_fork.as_ref().unwrap().primary_item_seq,
        2
    );
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
    core.append(fixture.repo_id.clone(),proposal(vec![serde_json::from_value(json!({"item_id":fixture.chat,"event_type":"CHAT::MESSAGE::CREATE","base_seq":3,"set_content":EventContent::Json {json:json!({"body":"accepted before crash"})}})).unwrap()])).await.unwrap();
    let mut repository = PortableRepository::open(&destination).unwrap();
    assert!(repository.verify().await.is_err());
    repository.finalize().await.unwrap();
    repository.verify().await.unwrap();
    let mut original = Vec::new();
    repository
        .copy_blob(
            &synck_vcs::types::BlobId::parse(&fixture.primary_blob).unwrap(),
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

    let (source, _) = frozen_sidecar_transfer(&fixture.root, false);
    let snapshot = bundle_snapshot(&source);
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
fn frozen_sidecar_transfer(root: &PathBuf, baseline: bool) -> (PathBuf, ItemId) {
    use synck_file_folder::file_version_set as versions;
    use synck_vcs::types::{
        COMMIT_EVENT_FAMILY_REF, canonical_event_proposal_bytes,
        canonical_ref_event_proposal_bytes, canonical_stored_commit_bytes_for_hash,
        compute_commit_id, sha256_digest_from_digest_bytes,
    };
    let repo = RepoId::new();
    let file = ItemId::new();
    let sidecar = versions::item_id(&repo, &file).unwrap();
    let version = ItemId::new().to_string();
    let bytes = b"legacy bytes";
    let blob = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
    let mut initial = creation(&file, "FILE", None, "legacy.txt");
    initial.set_content = Some(EventContent::Blob {
        blob_id: blob.clone(),
    });
    initial.set_metadata = Some(json!({"content":{"size":bytes.len()}}));
    let create = versions::create_event(sidecar.clone(), &file);
    let named=versions::version_event(sidecar.clone(),versions::VERSION_CREATE_EVENT,&file,&version,
        json!({"kind":"MAJOR","order_key":1,"description":"Original","content":{"size":bytes.len()}}).as_object().unwrap().clone(),Some(EventContent::Blob {blob_id:blob.clone()}));
    let mut description = versions::version_event(
        sidecar.clone(),
        versions::VERSION_SET_DESCRIPTION_EVENT,
        &file,
        &version,
        json!({"description":"Before baseline"})
            .as_object()
            .unwrap()
            .clone(),
        None,
    );
    description.base_seq = Some(2);
    let frozen_digest = format!(
        "sha256:{}",
        hex::encode(Sha256::digest(
            canonical_event_proposal_bytes(&description).unwrap()
        ))
    );
    let mut groups = vec![
        (vec![initial], None),
        (vec![create, named], None),
        (vec![description], None),
    ];
    if baseline {
        groups.push((Vec::new(),Some(RefEventProposal {owner_item_id:file.clone(),event_type:"FILE_VERSION::LEGACY_BASELINE".into(),set_metadata:Some(json!({"legacy_sidecar_item_id":sidecar,"legacy_sidecar_head_seq":3,"legacy_sidecar_head_event_digest":frozen_digest})),set_content:None,base_ref_seq:0,initial_ref_seq:Some(3)})));
    }
    let terminal = groups.len() as i64;
    let timestamp = "2026-01-01T00:00:00.000000000Z";
    let mut records = vec![
        json!({"type":"synck_vcs_bundle_header","bundle_format_version":5,"repo_id":repo,"history_generation":synck_vcs::types::CommitPk::new(),"exported_at":timestamp,"schema":{"subsystem":"synck_vcs_pg","schema_id":"historical-fixture"},"commit_format_version":6}),
    ];
    let mut heads = std::collections::BTreeMap::<ItemId, i64>::new();
    let mut checkpoint = synck_vcs::checkpoint::GENESIS;
    for (offset, (events, reference)) in groups.into_iter().enumerate() {
        let seq = offset as i64 + 1;
        let format = synck_vcs::types::COMMIT_FORMAT_VERSION;
        let mut hasher = Sha256::new();
        let mut rows = Vec::new();
        for (index, event) in events.into_iter().enumerate() {
            let payload = canonical_event_proposal_bytes(&event).unwrap();
            let digest = Sha256::digest(&payload);
            hasher.update([synck_vcs::types::COMMIT_EVENT_FAMILY_ITEM]);
            hasher.update(digest);
            let id = event.item_id.clone().unwrap();
            let head = heads.entry(id.clone()).or_default();
            *head += 1;
            rows.push(json!({"type":"event","commit_id":"pending","repo_seq":seq,"commit_event_idx":index,"item_id":id,"item_seq":head,"event_type":event.event_type,"payload_json":String::from_utf8(payload).unwrap(),"event_payload_digest":format!("sha256:{}",hex::encode(digest)),"created_at":timestamp,"parent_folder_id":null,"parent_folder_id_before":null}));
        }
        if let Some(event) = reference {
            let payload = canonical_ref_event_proposal_bytes(&event).unwrap();
            let digest = Sha256::digest(&payload);
            hasher.update([COMMIT_EVENT_FAMILY_REF]);
            hasher.update(digest);
            rows.push(json!({"type":"ref_event","commit_id":"pending","repo_seq":seq,"commit_event_idx":0,"owner_item_id":file,"ref_seq":3,"event_type":event.event_type,"payload_json":String::from_utf8(payload).unwrap(),"event_payload_digest":format!("sha256:{}",hex::encode(digest)),"created_at":timestamp}));
        }
        let digest = sha256_digest_from_digest_bytes(&hasher.finalize()).unwrap();
        let id = compute_commit_id(
            &canonical_stored_commit_bytes_for_hash(
                format,
                &"legacy-author".into(),
                &"legacy-author".into(),
                None,
                None,
                None,
                rows.len(),
                &digest,
            )
            .unwrap(),
        );
        checkpoint =
            synck_vcs::checkpoint::digest(&checkpoint, seq as u64, &id, timestamp.parse().unwrap())
                .unwrap();
        records.push(json!({"type":"commit","commit_id":id,"checkpoint":format!("sha256:{}",hex::encode(checkpoint)),"repo_seq":seq,"operation_id":null,"import_job_id":null,"format_version":format,"author_id":"legacy-author","committer_id":"legacy-author","message":null,"occurred_at":null,"created_at":timestamp,"event_count":rows.len(),"events_digest":digest}));
        for mut row in rows {
            row["commit_id"] = json!(id);
            records.push(row);
        }
    }
    for (id, kind, name, head, refhead) in [
        (
            file.clone(),
            "FILE",
            "legacy.txt".into(),
            1,
            if baseline { 3 } else { 0 },
        ),
        (
            sidecar,
            "FILE_VERSION_SET",
            versions::item_name(&file),
            3,
            0,
        ),
    ] {
        records.push(json!({"type":"item","item_id":id,"item_type":kind,"parent_folder_id":null,"name":name,"name_key":synck_vcs::naming::compute_name_key(&name).unwrap(),"head_seq":head,"ref_head_seq":refhead,"lifecycle_state":"LIVE","created_at":timestamp,"created_by":"legacy-author","sequencing_mode":"LOCAL","federation_stream_id":null}));
    }
    let source = root.join(if baseline {
        "legacy-baseline-transfer"
    } else {
        "legacy-unmigrated-transfer"
    });
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("objects")).unwrap();
    fs::write(source.join("objects").join(&blob[7..]), bytes).unwrap();
    let history = records
        .iter()
        .map(|row| serde_json::to_string(row).unwrap() + "\n")
        .collect::<String>();
    fs::write(source.join("history.ndjson"), &history).unwrap();
    fs::write(source.join("snapshot.json"),serde_json::to_vec(&json!({"repo_id":repo,"terminal_repo_seq":terminal,"history_sha256":format!("sha256:{}",hex::encode(Sha256::digest(history.as_bytes()))),"history_bytes":history.len()})).unwrap()).unwrap();
    (source, file)
}

#[tokio::test]
async fn frozen_labels_follow_sidecar_checkpoint_before_and_after_ref_baseline() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().to_path_buf();
    for baseline in [false, true] {
        let (source, file) = frozen_sidecar_transfer(&root, baseline);
        let destination = root.join(if baseline { "migrated" } else { "unmigrated" });
        materialize_repository(
            &source.join("history.ndjson"),
            &source.join("objects"),
            &bundle_snapshot(&source),
            &destination,
        )
        .await
        .unwrap();
        let repository = PortableRepository::open(&destination).unwrap();
        assert!(
            repository
                .show_item(&file, Some(1))
                .await
                .unwrap()
                .item
                .subitems
                .is_empty()
        );
        let early = repository.show_item(&file, Some(2)).await.unwrap();
        assert_eq!(early.item.subitems.len(), 1);
        assert_eq!(early.item.subitems[0].body.as_deref(), Some("Original"));
        let before = repository.show_item(&file, Some(3)).await.unwrap();
        assert_eq!(
            before.item.subitems[0].body.as_deref(),
            Some("Before baseline")
        );
        let current = repository.show_item(&file, None).await.unwrap();
        assert_eq!(current.item.subitems.len(), 1);
        assert_eq!(
            current.item.subitems[0].body.as_deref(),
            Some("Before baseline")
        );
        assert_eq!(current.refs.len(), usize::from(baseline));
        if baseline {
            assert_eq!(
                repository
                    .show_item(&file, Some(4))
                    .await
                    .unwrap()
                    .item
                    .subitems
                    .len(),
                1
            );
        }
    }
}

fn bundle_snapshot(source: &std::path::Path) -> BundleSnapshot {
    serde_json::from_slice(&fs::read(source.join("snapshot.json")).unwrap()).unwrap()
}

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
    for version in [3, 4, 5] {
        config["format_version"] = json!(version);
        let bytes = serde_json::to_vec(&config).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(PortableRepository::open(source).is_err());
        let destination = fixture.root.join("rejected");
        assert!(import_repository(source, &destination).await.is_err());
        assert!(!destination.exists());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
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
            serde_json::from_value(json!({
                "item_id":fixture.chat,
                "event_type":"CHAT::MESSAGE::CREATE",
                "base_seq":2,
                "set_content":EventContent::Content {content_id:fixture.body.id()}
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
    assert!(
        events
            .iter()
            .any(|e| e.ledger == "REF" && e.item_id == fixture.file)
    );
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
async fn complete_checkout_rejects_hidden_mixed_descendant_deletion() {
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
        proposal(vec![creation(
            &hidden,
            "CHAT",
            Some(fixture.folder.clone()),
            "hidden-chat",
        )]),
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
    assert!(
        repository
            .commit("delete folder")
            .await
            .unwrap_err()
            .to_string()
            .contains("non-filesystem")
    );
    drop(repository);
    let portable = PortableRepository::open(&destination).unwrap();
    assert_eq!(portable.verify().await.unwrap().commits, 3);
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
async fn complete_checkout_initial_reserved_path_fails_before_activation() {
    let fixture = fixture().await;
    let storage = SqliteStorage::new(fixture.source.join(".synck/repository.sqlite3"));
    let core = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    core.append(
        fixture.repo_id.clone(),
        proposal(vec![creation(&ItemId::new(), "FOLDER", None, ".synck")]),
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
    let destination = fixture.root.join("reserved");
    PortableRepository::open(&fixture.source)
        .unwrap()
        .archive(&destination)
        .await
        .unwrap();
    let mut portable = PortableRepository::open(&destination).unwrap();
    assert!(portable.enable_checkout("editor", &[]).await.is_err());
    assert!(portable.checkout_paths().is_none());
    portable
        .archive(&fixture.root.join("still-copyable"))
        .await
        .unwrap();
    portable
        .enable_checkout("editor", &["documents".into()])
        .await
        .unwrap();
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
    let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["checkout"] =
        json!({"paths":[],"pending":{"previous_paths":["documents/original.txt"]}});
    fs::write(metadata.join("checkout-source.json"),serde_json::to_vec(&json!([
        {"path":"documents","item_id":null,"item_type":"FOLDER","blob_id":null,"size":null},
        {"path":"documents/original.txt","item_id":null,"item_type":"FILE","blob_id":fixture.primary_blob,"size":14}
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
    assert!(
        synck_local::Repository::discover(&fixture.transfer)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("changed during checkout")
    );
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
        .create_file(Some(fixture.folder.clone()), "source.EML", &source)
        .await
        .unwrap();
    let item = repository.show_item(&id, None).await.unwrap();
    assert_eq!(item.item.projection.item_type, "EMAIL");
    assert_eq!(item.item.projection.name, "source");
    let metadata = &item.item.projection.metadata_json["mailcal"];
    assert_eq!(metadata["source"]["filename"], "source.EML");
    assert_eq!(
        metadata["source"]["blob_id"],
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    );
    assert_eq!(
        metadata["raw_rfc822_blob_id"],
        metadata["source"]["blob_id"]
    );
    assert_eq!(metadata["raw_rfc822_provenance"], "original");
    assert_eq!(fs::read(&source).unwrap(), bytes);
    let source_blob = metadata["source"]["blob_id"].as_str().unwrap();
    assert_eq!(
        fs::read(
            fixture
                .source
                .join(".synck/objects")
                .join(&source_blob[7..])
        )
        .unwrap(),
        bytes
    );
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
async fn local_email_import_rejects_malformed_sources_without_creating_file_fallback() {
    let fixture = fixture().await;
    let source = fixture.root.join("bad.msg");
    let bytes = b"this is not an Outlook message";
    fs::write(&source, bytes).unwrap();
    let mut repository = PortableRepository::open(&fixture.source).unwrap();
    let before = repository.verify().await.unwrap();
    let error = repository
        .create_file(None, "bad.msg", &source)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("EMAIL_IMPORT_"));
    let after = repository.verify().await.unwrap();
    assert_eq!(
        (before.commits, before.items, before.blobs),
        (after.commits, after.items, after.blobs)
    );
    assert_eq!(fs::read(source).unwrap(), bytes);
}

#[tokio::test]
async fn complete_checkout_imports_email_additions_without_file_fallback_or_reimport() {
    let fixture = fixture().await;
    let mut portable = PortableRepository::open(&fixture.source).unwrap();
    portable.enable_checkout("local-editor", &[]).await.unwrap();
    drop(portable);
    let source = fixture.source.join("documents/new.EML");
    let bytes =
        include_bytes!("../../synck-items/tests/fixtures/imported_email/representative.eml");
    fs::write(&source, bytes).unwrap();
    let repository = synck_local::Repository::discover(&fixture.source)
        .await
        .unwrap();
    let committed = repository.commit("Import message").await.unwrap();
    assert!(matches!(
        committed,
        synck_local::CommitOutcome::Committed(_)
    ));
    assert!(
        repository.status().await.unwrap().is_empty(),
        "unchanged imported source must not be imported twice"
    );
    assert_eq!(fs::read(&source).unwrap(), bytes);
    drop(repository);
    let portable = PortableRepository::open(&fixture.source).unwrap();
    portable.verify().await.unwrap();
    let page = portable
        .list_items(Some(&fixture.folder), None, 100)
        .await
        .unwrap();
    assert!(
        page.items
            .iter()
            .any(|item| item.item_type == "EMAIL" && item.name == "new")
    );
    assert!(
        !page
            .items
            .iter()
            .any(|item| item.item_type == "FILE" && item.name == "new.EML")
    );
}

#[tokio::test]
async fn complete_checkout_rejects_malformed_email_before_any_sibling_commit() {
    let fixture = fixture().await;
    let mut portable = PortableRepository::open(&fixture.source).unwrap();
    portable.enable_checkout("local-editor", &[]).await.unwrap();
    drop(portable);
    fs::write(fixture.source.join("new.txt"), b"valid file").unwrap();
    fs::write(fixture.source.join("bad.eml"), b"not a message").unwrap();
    let repository = synck_local::Repository::discover(&fixture.source)
        .await
        .unwrap();
    let before = repository.log(None).await.unwrap().len();
    let error = repository
        .commit("Reject malformed mail")
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("EMAIL_IMPORT_INVALID_FORMAT"),
        "{error}"
    );
    assert_eq!(repository.log(None).await.unwrap().len(), before);
    assert_eq!(
        fs::read(fixture.source.join("bad.eml")).unwrap(),
        b"not a message"
    );
}

#[tokio::test]
async fn email_complete_init_preserves_existing_files_and_supports_default_commit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("existing");
    fs::create_dir(&root).unwrap();
    let bytes =
        include_bytes!("../../synck-items/tests/fixtures/imported_email/representative.eml");
    fs::write(root.join("original.eml"), bytes).unwrap();
    synck_local::portable::init_repository(&root, "local-author")
        .await
        .unwrap();
    assert_eq!(fs::read(root.join("original.eml")).unwrap(), bytes);
    let repository = synck_local::Repository::discover(&root).await.unwrap();
    assert_eq!(repository.status().await.unwrap().len(), 1);
    repository.commit("Import original mail").await.unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    drop(repository);
    let portable = PortableRepository::open(&root).unwrap();
    portable.verify().await.unwrap();
    assert_eq!(
        portable
            .list_items(None, None, 10)
            .await
            .unwrap()
            .items
            .into_iter()
            .find(|item| item.item_type == "EMAIL")
            .unwrap()
            .item_type,
        "EMAIL"
    );
    assert!(
        synck_local::portable::init_repository(&root, "another-author")
            .await
            .is_err()
    );
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
        .create_file(None, "Renamed message", &source)
        .await
        .unwrap();
    let item = repository.show_item(&id, None).await.unwrap();
    assert_eq!(item.item.projection.name, "Renamed message");
    let mail = &item.item.projection.metadata_json["mailcal"];
    assert_eq!(mail["source"]["filename"], "original.MSG");
    assert_eq!(mail["source"]["format"], "msg");
    assert_eq!(mail["raw_rfc822_provenance"], "generated");
    assert_ne!(mail["source"]["blob_id"], mail["raw_rfc822_blob_id"]);
    let copy = fixture.root.join("email-copy");
    repository.archive(&copy).await.unwrap();
    let imported = PortableRepository::open(&copy).unwrap();
    let destination = fixture.root.join("retained-original.msg");
    imported.export_file(&id, &destination, None).await.unwrap();
    assert_eq!(fs::read(destination).unwrap(), bytes);
    imported.verify().await.unwrap();
}

#[tokio::test]
async fn email_import_regression_rejected_reimport_keeps_earlier_source_mapping() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("local");
    synck_local::portable::init_repository(&root, "local-author")
        .await
        .unwrap();
    let original = b"From: sender@example.test\r\nSubject: Original\r\n\r\nbody";
    fs::write(root.join("message.eml"), original).unwrap();
    let repository = synck_local::Repository::discover(&root).await.unwrap();
    repository.commit("Initial imported email").await.unwrap();
    fs::write(
        root.join("message.eml"),
        b"From: sender@example.test\r\nSubject: Changed\r\n\r\nother body",
    )
    .unwrap();
    assert!(
        repository
            .commit("Conflicting second import")
            .await
            .is_err()
    );
    assert_eq!(
        repository.status().await.unwrap().len(),
        1,
        "failed import cannot hide source"
    );
    fs::write(root.join("message.eml"), original).unwrap();
    assert!(
        repository.status().await.unwrap().is_empty(),
        "failed later attempt cannot replace accepted source mapping"
    );
    drop(repository);
    let portable = PortableRepository::open(&root).unwrap();
    let copy = temporary.path().join("copy");
    portable.archive(&copy).await.unwrap();
    let copied = SqliteStorage::new(copy.join(".synck/repository.sqlite3"));
    assert!(
        !copied
            .open_readonly()
            .unwrap()
            .prepare("SELECT name FROM sqlite_schema WHERE name GLOB 'local_*'")
            .unwrap()
            .exists([])
            .unwrap()
    );
}

#[tokio::test]
async fn email_import_keeps_email_and_file_in_one_atomic_commit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("local");
    synck_local::portable::init_repository(&root, "local-author")
        .await
        .unwrap();
    fs::write(
        root.join("message.eml"),
        b"From: sender@example.test\r\nSubject: Message\r\n\r\nbody",
    )
    .unwrap();
    fs::write(root.join("file.txt"), b"ordinary file").unwrap();
    let repository = synck_local::Repository::discover(&root).await.unwrap();
    let outcome = repository
        .commit_selected(&root, &[], Some("Mixed import"))
        .await
        .unwrap();
    assert!(matches!(outcome, synck_local::CommitOutcome::Committed(_)));
    assert!(repository.status().await.unwrap().is_empty());
    drop(repository);
    let portable = PortableRepository::open(&root).unwrap();
    portable.verify().await.unwrap();
    let items = portable.list_items(None, None, 10).await.unwrap().items;
    assert!(items.iter().any(|item| item.item_type == "EMAIL"));
    assert!(items.iter().any(|item| item.item_type == "FILE"));
}

#[tokio::test]
async fn email_cli_commit_receipt_and_raw_history_succeed_for_email_owners() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cli");
    let init = Command::new(env!("CARGO_BIN_EXE_synck"))
        .args(["repository", "init"])
        .arg(&root)
        .args(["--author", "cli-author"])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    fs::write(
        root.join("message.eml"),
        b"From: sender@example.test\r\nSubject: Message\r\n\r\nbody",
    )
    .unwrap();
    let commit = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(&root)
        .args(["commit", "-m", "Import email from CLI"])
        .output()
        .unwrap();
    assert!(
        commit.status.success(),
        "{}",
        String::from_utf8_lossy(&commit.stderr)
    );
    assert!(String::from_utf8_lossy(&commit.stdout).contains("committed sha256:"));
    let status = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(&root)
        .arg("status")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(String::from_utf8_lossy(&status.stdout).contains("working tree clean"));
    let show = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(&root)
        .args(["show", "HEAD", "--events"])
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    assert!(String::from_utf8_lossy(&show.stdout).contains("EMAIL::CREATE"));
    let log = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(&root)
        .args(["log", "--events"])
        .output()
        .unwrap();
    assert!(
        log.status.success(),
        "{}",
        String::from_utf8_lossy(&log.stderr)
    );
    assert!(String::from_utf8_lossy(&log.stdout).contains("EMAIL::CREATE"));
}

#[tokio::test]
async fn email_ingress_requested_extension_preserves_actual_original_filename() {
    let fixture = fixture().await;
    let source = fixture.root.join("original.txt");
    let bytes =
        include_bytes!("../../synck-items/tests/fixtures/imported_email/representative.eml");
    fs::write(&source, bytes).unwrap();
    let mut repository = PortableRepository::open(&fixture.source).unwrap();
    let (id, _) = repository
        .create_file(None, "Requested.EML", &source)
        .await
        .unwrap();
    let item = repository.show_item(&id, None).await.unwrap();
    assert_eq!(item.item.projection.item_type, "EMAIL");
    assert_eq!(item.item.projection.name, "Requested");
    assert_eq!(
        item.item.projection.metadata_json["mailcal"]["source"]["filename"],
        "original.txt"
    );
    assert_eq!(
        item.item.projection.metadata_json["mailcal"]["source"]["format"],
        "eml"
    );
    repository.verify().await.unwrap();
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStringExt;
        let source = fixture
            .root
            .join(std::ffi::OsString::from_vec(b"\xff.msg".to_vec()));
        fs::write(&source, b"hidden extension").unwrap();
        let error = repository
            .create_file(None, "Renamed source", &source)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("EMAIL_IMPORT_INVALID_FORMAT"));
    }
}

#[tokio::test]
async fn email_ingress_rejects_file_appends_and_historical_email_file_edits() {
    let fixture = fixture().await;
    let source = fixture.root.join("new.eml");
    fs::write(
        &source,
        b"From: sender@example.test\r\nSubject: New\r\n\r\nbody",
    )
    .unwrap();
    let mut portable = PortableRepository::open(&fixture.source).unwrap();
    let error = portable
        .save_file(&fixture.file, &source)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("EMAIL_IMPORT_REQUIRES_NEW_ITEM"));
    drop(portable);
    // Existing canonical FILE identities remain readable and valid, including
    // history created before the product import rule.
    let storage = SqliteStorage::new(fixture.source.join(".synck/repository.sqlite3"));
    let core = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    core.append(
        fixture.repo_id.clone(),
        proposal(vec![
            serde_json::from_value(json!({
                "item_id":fixture.file,"event_type":"FILE::SET_NAME","base_seq":2,"set_name":"historical.msg"
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
    drop(core);
    let mut portable = PortableRepository::open(&fixture.source).unwrap();
    portable.verify().await.unwrap();
    portable.enable_checkout("editor", &[]).await.unwrap();
    drop(portable);
    fs::write(
        fixture.source.join("documents/historical.msg"),
        b"changed historical source",
    )
    .unwrap();
    let repository = synck_local::Repository::discover(&fixture.source)
        .await
        .unwrap();
    let before = repository.log(None).await.unwrap().len();
    let error = repository
        .commit("Reject historical FILE edit")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("EMAIL_IMPORT_REQUIRES_NEW_ITEM"));
    assert_eq!(repository.log(None).await.unwrap().len(), before);
}

#[tokio::test]
async fn email_ingress_cli_rejects_file_rename_without_changing_history() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("cli");
    let binary = env!("CARGO_BIN_EXE_synck");
    let init = Command::new(binary)
        .args(["repository", "init"])
        .arg(&root)
        .args(["--author", "review"])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    fs::write(
        root.join("draft.txt"),
        b"From: sender@example.test\r\nSubject: Rename\r\n\r\nbody",
    )
    .unwrap();
    let commit = Command::new(binary)
        .current_dir(&root)
        .args(["commit", "-m", "Initial FILE"])
        .output()
        .unwrap();
    assert!(
        commit.status.success(),
        "{}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let before = Command::new(binary)
        .current_dir(&root)
        .args(["show", "HEAD", "--events"])
        .output()
        .unwrap();
    let moved = Command::new(binary)
        .current_dir(&root)
        .args([
            "mv",
            "draft.txt",
            "draft.msg",
            "-m",
            "Reject direct email rename",
        ])
        .output()
        .unwrap();
    assert!(!moved.status.success());
    assert!(String::from_utf8_lossy(&moved.stderr).contains("EMAIL_IMPORT_REQUIRES_NEW_ITEM"));
    assert!(root.join("draft.txt").exists());
    assert!(!root.join("draft.msg").exists());
    fs::rename(root.join("draft.txt"), root.join("draft.eml")).unwrap();
    let rename = Command::new(binary)
        .current_dir(&root)
        .args(["commit", "-m", "Reject email rename"])
        .output()
        .unwrap();
    assert!(!rename.status.success());
    assert!(String::from_utf8_lossy(&rename.stderr).contains("EMAIL_IMPORT_REQUIRES_NEW_ITEM"));
    let after = Command::new(binary)
        .current_dir(&root)
        .args(["show", "HEAD", "--events"])
        .output()
        .unwrap();
    assert!(after.status.success());
    assert_eq!(before.stdout, after.stdout);
    assert!(root.join("draft.eml").exists());
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
        .args(["repository", "create-file", "Message"])
        .arg(&source)
        .arg("--path")
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
        .create_file(None, "Message", &source)
        .await
        .unwrap();
    assert_eq!(fs::read(objects.join(&digest)).unwrap(), bytes);
    let exported = temporary.path().join("original.eml");
    repository.export_file(&id, &exported, None).await.unwrap();
    assert_eq!(fs::read(exported).unwrap(), bytes);
    assert_eq!(repository.verify().await.unwrap().commits, 2);

    let other = b"From: sender@example.test\r\nSubject: Other\r\n\r\nother body";
    fs::write(&source, other).unwrap();
    let corrupt_path = objects.join(hex::encode(Sha256::digest(other)));
    fs::write(&corrupt_path, b"corrupt existing object").unwrap();
    let error = repository
        .create_file(None, "Other", &source)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("existing immutable object is corrupt")
    );
    assert_eq!(fs::read(corrupt_path).unwrap(), b"corrupt existing object");
}

#[tokio::test]
async fn email_checkout_selection_preserves_sources_and_required_directories() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    synck_local::portable::init_repository(&root, "local-author")
        .await
        .unwrap();
    fs::create_dir_all(root.join("a/nested")).unwrap();
    fs::create_dir_all(root.join("a/other")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    let original = b"From: sender@example.test\r\nSubject: Message\r\n\r\noriginal";
    let source = root.join("a/nested/message.eml");
    fs::write(&source, original).unwrap();
    fs::write(root.join("a/nested/sibling.txt"), b"sibling").unwrap();
    fs::write(root.join("a/other/file.txt"), b"other").unwrap();
    fs::write(root.join("b/file.txt"), b"selected").unwrap();
    let mut repository = synck_local::Repository::discover(&root).await.unwrap();
    repository
        .commit("Import mail and ordinary files")
        .await
        .unwrap();
    repository.set_checkout_paths(&["b".into()]).await.unwrap();
    assert_eq!(fs::read(&source).unwrap(), original);
    assert!(root.join("a/nested").is_dir());
    assert!(!root.join("a/nested/sibling.txt").exists());
    assert!(!root.join("a/other").exists());
    assert!(repository.status().await.unwrap().is_empty());
    drop(repository);

    // Simulate a crash after materialization completed but before the checkout
    // transition was cleared. Its source manifest still describes the old view.
    let metadata = root.join(".synck");
    let config_path = metadata.join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["checkout"]["pending"] = json!({"previous_paths":[]});
    let original_tree: [(&str, Option<&[u8]>); 8] = [
        ("a", None),
        ("a/nested", None),
        ("a/nested/message.eml", Some(original)),
        ("a/nested/sibling.txt", Some(b"sibling")),
        ("a/other", None),
        ("a/other/file.txt", Some(b"other")),
        ("b", None),
        ("b/file.txt", Some(b"selected")),
    ];
    let manifest: Vec<_> = original_tree
        .into_iter()
        .map(|(path, bytes)| {
            json!({"path":path,"item_id":null,
            "item_type":if bytes.is_some() {"FILE"} else {"FOLDER"},
            "blob_id":bytes.map(|bytes|format!("sha256:{}",hex::encode(Sha256::digest(bytes)))),
            "size":bytes.map(|bytes|bytes.len())})
        })
        .collect();
    fs::write(
        metadata.join("checkout-source.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut repository = synck_local::Repository::discover(&root).await.unwrap();
    assert!(!metadata.join("checkout-source.json").exists());
    let changed = b"From: sender@example.test\r\nSubject: Changed\r\n\r\nchanged";
    fs::write(&source, changed).unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    assert!(repository.set_checkout_paths(&[]).await.is_err());
    assert_eq!(repository.checkout_paths().unwrap(), &["b".to_string()]);
    assert_eq!(fs::read(&source).unwrap(), changed);
    assert!(!root.join("a/nested/sibling.txt").exists());
    fs::write(&source, original).unwrap();
    repository.set_checkout_paths(&[]).await.unwrap();
    assert_eq!(fs::read(&source).unwrap(), original);
    assert_eq!(
        fs::read(root.join("a/nested/sibling.txt")).unwrap(),
        b"sibling"
    );
    assert_eq!(fs::read(root.join("a/other/file.txt")).unwrap(), b"other");
    assert!(repository.status().await.unwrap().is_empty());
    fs::write(&source, changed).unwrap();
    assert!(repository.set_checkout_paths(&["b".into()]).await.is_err());
    assert_eq!(fs::read(&source).unwrap(), changed);
    fs::write(&source, original).unwrap();
    repository.set_checkout_paths(&["b".into()]).await.unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    drop(repository);
    assert_eq!(
        PortableRepository::open(&root)
            .unwrap()
            .verify()
            .await
            .unwrap()
            .commits,
        2
    );
}

#[tokio::test]
async fn email_sources_follow_cli_folder_moves_and_accepted_move_recovery() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    synck_local::portable::init_repository(&root, "local-author")
        .await
        .unwrap();
    fs::create_dir_all(root.join("mail/nested")).unwrap();
    fs::create_dir(root.join("other")).unwrap();
    let original = b"From: sender@example.test\r\nSubject: Message\r\n\r\noriginal";
    fs::write(root.join("mail/nested/message.eml"), original).unwrap();
    let repository = synck_local::Repository::discover(&root).await.unwrap();
    repository.commit("Import mail").await.unwrap();
    drop(repository);
    let connection = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    let (folder, head): (String, i64) = connection
        .query_row(
            "SELECT item_id,head_seq FROM vcs_items WHERE item_type='FOLDER' AND name='mail'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    drop(connection);
    let moved = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(&root)
        .args(["mv", "mail", "renamed", "-m", "Rename mail folder"])
        .output()
        .unwrap();
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    let mut repository = synck_local::Repository::discover(&root).await.unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    repository
        .set_checkout_paths(&["other".into()])
        .await
        .unwrap();
    assert_eq!(
        fs::read(root.join("renamed/nested/message.eml")).unwrap(),
        original
    );
    assert!(repository.status().await.unwrap().is_empty());
    repository.set_checkout_paths(&[]).await.unwrap();
    drop(repository);

    // Replay the existing journal as if the process stopped after acceptance
    // but before clearing its move intent. No observation rewrite is required.
    fs::write(
        root.join(".synck/move.json"),
        serde_json::to_vec(&json!({
            "version":2,"operation_id":uuid::Uuid::new_v4().to_string(),
            "source":"mail","destination":"renamed","item_id":folder,
            "item_type":"FOLDER","base_seq":head,"result_head_seq":head+1,
            "new_parent_id":null,"new_name":"renamed","message":"Rename mail folder"
        }))
        .unwrap(),
    )
    .unwrap();
    let repository = synck_local::Repository::discover(&root).await.unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    assert!(!root.join(".synck/move.json").exists());
    assert_eq!(repository.log(None).await.unwrap().len(), 3);
    assert_eq!(
        fs::read(root.join("renamed/nested/message.eml")).unwrap(),
        original
    );
}

#[tokio::test]
async fn email_sources_follow_physical_folder_moves_without_hiding_copies_or_edits() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    synck_local::portable::init_repository(&root, "local-author")
        .await
        .unwrap();
    fs::create_dir_all(root.join("mail/nested")).unwrap();
    fs::create_dir(root.join("other")).unwrap();
    let original = b"From: sender@example.test\r\nSubject: Message\r\n\r\noriginal";
    fs::write(root.join("mail/nested/message.eml"), original).unwrap();
    let repository = synck_local::Repository::discover(&root).await.unwrap();
    repository.commit("Import mail").await.unwrap();
    fs::rename(root.join("mail"), root.join("renamed")).unwrap();
    let changes = repository.status().await.unwrap();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].path, "renamed");
    assert_eq!(changes[0].kind, synck_local::ChangeKind::Rename);
    repository.commit("Rename mail folder").await.unwrap();
    drop(repository);
    let repository = synck_local::Repository::discover(&root).await.unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    fs::write(root.join("other/message.eml"), original).unwrap();
    fs::write(root.join("renamed/nested/copy.eml"), original).unwrap();
    fs::create_dir_all(root.join("mail/nested")).unwrap();
    fs::write(root.join("mail/nested/message.eml"), original).unwrap();
    fs::write(root.join("renamed/nested/message.eml"), b"changed original").unwrap();
    let changes = repository.status().await.unwrap();
    for path in [
        "other/message.eml",
        "renamed/nested/copy.eml",
        "mail/nested/message.eml",
        "renamed/nested/message.eml",
    ] {
        assert!(
            changes.iter().any(|change| change.path == path),
            "missing {path}: {changes:?}"
        );
    }
    drop(repository);
    let portable = PortableRepository::open(&root).unwrap();
    assert_eq!(portable.verify().await.unwrap().commits, 3);
    let connection = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM vcs_items WHERE item_type='EMAIL'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn email_sources_use_current_ownership_after_folder_moves() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    synck_local::portable::init_repository(&root, "local-author")
        .await
        .unwrap();
    fs::create_dir(root.join("mail")).unwrap();
    fs::create_dir_all(root.join("archive/message.eml")).unwrap();
    let original = b"From: sender@example.test\r\nSubject: Message\r\n\r\noriginal";
    fs::write(root.join("mail/message.eml"), original).unwrap();
    let repository = synck_local::Repository::discover(&root).await.unwrap();
    repository.commit("Import mail").await.unwrap();

    fs::rename(root.join("archive"), root.join("old-archive")).unwrap();
    fs::rename(root.join("mail"), root.join("archive")).unwrap();
    let changes = repository.status().await.unwrap();
    assert_eq!(changes.len(), 2, "{changes:?}");
    for path in ["archive", "old-archive"] {
        assert!(changes.iter().any(|change| {
            change.path == path && change.kind == synck_local::ChangeKind::Rename
        }));
    }
    repository.commit("Move folders").await.unwrap();
    drop(repository);

    let repository = synck_local::Repository::discover(&root).await.unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    assert_eq!(
        fs::read(root.join("archive/message.eml")).unwrap(),
        original
    );
    assert!(root.join("old-archive/message.eml").is_dir());
    assert_eq!(repository.log(None).await.unwrap().len(), 3);
    let connection = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM vcs_items WHERE item_type='EMAIL'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn email_sources_use_current_ownership_for_tracked_files() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    synck_local::portable::init_repository(&root, "local-author")
        .await
        .unwrap();
    fs::create_dir(root.join("mail")).unwrap();
    let original = b"From: sender@example.test\r\nSubject: Message\r\n\r\noriginal";
    fs::write(root.join("mail/message.eml"), original).unwrap();
    fs::write(root.join("draft.txt"), original).unwrap();
    let repository = synck_local::Repository::discover(&root).await.unwrap();
    repository
        .commit("Import mail and track draft")
        .await
        .unwrap();

    #[cfg(windows)]
    let draft_creation = fs::metadata(root.join("draft.txt"))
        .unwrap()
        .created()
        .unwrap();
    fs::remove_file(root.join("mail/message.eml")).unwrap();
    fs::rename(root.join("draft.txt"), root.join("mail/message.eml")).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTimesExt;

        // This test requires an exact tracked identity. NTFS can tunnel the
        // deleted email source's creation time onto the renamed draft.
        fs::OpenOptions::new()
            .write(true)
            .open(root.join("mail/message.eml"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_created(draft_creation))
            .unwrap();
    }
    let changes = repository.status().await.unwrap();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].kind, synck_local::ChangeKind::MoveRename);
    assert_eq!(changes[0].path, "mail/message.eml");
    assert_eq!(changes[0].source_path.as_deref(), Some("draft.txt"));
    let error = repository
        .commit("Reject FILE email rename")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("EMAIL_IMPORT_REQUIRES_NEW_ITEM"));
    assert_eq!(repository.log(None).await.unwrap().len(), 2);
    assert_eq!(fs::read(root.join("mail/message.eml")).unwrap(), original);
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
        (report.blobs, report.bodies),
        (original.blobs, original.bodies)
    );
    for id in [&fixture.primary_blob, &fixture.fork_blob] {
        let id = synck_vcs::types::BlobId::parse(id).unwrap();
        let mut expected = Vec::new();
        let mut actual = Vec::new();
        source.copy_blob(&id, &mut expected).unwrap();
        copy.copy_blob(&id, &mut actual).unwrap();
        assert_eq!(actual, expected);
    }
    let preserved = fixture.root.join("preserved");
    copy.archive(&preserved).await.unwrap();
    let preserved = PortableRepository::open(&preserved).unwrap();
    assert_eq!(preserved.storage_mode(), StorageMode::Chunked);
    preserved.verify().await.unwrap();
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
    let (email, _) = repository
        .create_file(None, "Message", &mail)
        .await
        .unwrap();
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
