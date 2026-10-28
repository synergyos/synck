use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn run(root: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn json(root: &Path, args: &[&str]) -> Value {
    serde_json::from_str(&run(root, args)).unwrap()
}

#[test]
fn init_history_maintenance_and_copy_work_in_both_modes() {
    for mode in ["WHOLE_FILE", "CHUNKED"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("existing.txt"), b"existing").unwrap();
        for name in ["~$report.docx", "edit.swp", "edit.tmp", ".DS_Store"] {
            fs::write(root.join(name), b"ignored").unwrap();
        }
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/cache"), b"ignored").unwrap();
        fs::write(root.join(".hidden"), b"private").unwrap();
        run(
            &root,
            &["init", "--author", "tester", "--storage-mode", mode],
        );
        let config: Value =
            serde_json::from_slice(&fs::read(root.join(".synck/config.json")).unwrap()).unwrap();
        assert_eq!(config["format_version"], 7);
        assert_eq!(config["authority"], "INDEPENDENT_LOCAL");
        assert_eq!(config["storage_mode"], mode);
        assert!(root.join(".synckignore").is_file());
        assert_eq!(json(&root, &["repository", "verify"])["commits"], 1);
        let changes = json(&root, &["status", "--json"]);
        assert_eq!(changes["changes"].as_array().unwrap().len(), 1);
        run(&root, &["commit", "-m", "existing"]);
        fs::write(root.join("existing.txt"), b"changed").unwrap();
        run(&root, &["commit", "-m", "edit"]);
        run(&root, &["revert", "HEAD", "-m", "undo"]);
        assert_eq!(fs::read(root.join("existing.txt")).unwrap(), b"existing");
        fs::remove_file(root.join("existing.txt")).unwrap();
        run(&root, &["commit", "-m", "delete"]);
        assert!(run(&root, &["deleted"]).contains("existing.txt"));
        run(&root, &["recover", "existing.txt", "-m", "recover"]);
        assert_eq!(fs::read(root.join("existing.txt")).unwrap(), b"existing");
        fs::write(root.join("existing.txt"), b"discard").unwrap();
        run(&root, &["restore-tree"]);
        assert_eq!(fs::read(root.join("existing.txt")).unwrap(), b"existing");
        let report = json(&root, &["repository", "verify"]);
        assert_eq!(json(&root, &["fsck"]), report);
        run(&root, &["storage"]);
        run(&root, &["gc"]);
        run(&root, &["storage", "optimize"]);
        run(&root, &["gc"]);
        let history = json(&root, &["repository", "log"]);
        let archive = temp.path().join("archive");
        run(
            &root,
            &["archive", archive.to_str().unwrap(), "--storage-mode", mode],
        );
        let imported = temp.path().join("imported");
        run(
            &root,
            &[
                "repository",
                "import",
                archive.to_str().unwrap(),
                imported.to_str().unwrap(),
            ],
        );
        assert_eq!(json(&imported, &["repository", "log"]), history);
        assert_eq!(json(&imported, &["repository", "verify"]), report);
        run(&root, &["monitor", "stop"]);
    }
}

#[test]
fn init_alias_and_empty_directory_share_defaults() {
    let temp = tempfile::tempdir().unwrap();
    for command in [
        vec!["init", "new", "--author", "tester"],
        vec!["repository", "init", "alias", "--author", "tester"],
    ] {
        run(temp.path(), &command);
    }
    for name in ["new", "alias"] {
        let root = temp.path().join(name);
        assert_eq!(json(&root, &["repository", "verify"])["commits"], 1);
        assert!(run(&root, &["status"]).contains("working tree clean"));
        assert_eq!(
            run(&root, &["config", "hidden-files"]),
            "hidden-files ignore\n"
        );
        run(&root, &["monitor", "stop"]);
    }
}

#[test]
fn help_exposes_only_supported_local_workflows() {
    let temp = tempfile::tempdir().unwrap();
    let help = run(temp.path(), &["--help"]);
    for command in [
        "clone",
        "push",
        "pull",
        "workspace",
        "import",
        "restore-archive",
    ] {
        assert!(
            !help
                .lines()
                .any(|line| line.trim_start().starts_with(&format!("{command} "))),
            "{help}"
        );
        let output = Command::new(env!("CARGO_BIN_EXE_synck"))
            .args([command, "--help"])
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    let init = run(temp.path(), &["init", "--help"]);
    assert!(init.contains("complete repository"));
    assert!(!run(temp.path(), &["revert", "--help"]).contains("import-job"));
}

#[tokio::test]
async fn emails_chats_and_attachments_round_trip_with_identical_ids() {
    use synck_local::{StorageMode, portable::PortableRepository};
    for mode in [StorageMode::WholeFile, StorageMode::Chunked] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        synck_local::portable::init_repository_with_mode(&root, "tester", mode)
            .await
            .unwrap();
        fs::write(root.join("mail.eml"), b"From: a@example.test\r\nSubject: test\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nhello\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=a.txt\r\nContent-Transfer-Encoding: base64\r\n\r\nYXR0YWNobWVudA==\r\n--x--\r\n").unwrap();
        let working = synck_local::Repository::discover(&root).await.unwrap();
        working.commit("email with attachment").await.unwrap();
        drop(working);
        let mut repo = PortableRepository::open(&root).unwrap();
        let (chat, _) = repo.create_chat("Discussion").await.unwrap();
        let source = temp.path().join("attach.bin");
        fs::write(&source, b"exact attachment").unwrap();
        repo.append_chat_message_with_attachments(&chat, "message", &[source])
            .await
            .unwrap();
        let shown = serde_json::to_value(repo.show_item(&chat, None).await.unwrap()).unwrap();
        assert!(shown.to_string().contains("ROOM"));
        assert!(shown.to_string().contains("CHAT::MESSAGE.ATTACHMENT"));
        let original = serde_json::to_value(repo.log(None, 512).await.unwrap()).unwrap();
        let items = repo.list_items(None, None, 512).await.unwrap();
        let mut snapshots = Vec::new();
        for item in items.items {
            snapshots.push((
                item.item_id.clone(),
                serde_json::to_value(repo.show_item(&item.item_id, None).await.unwrap()).unwrap(),
            ));
        }
        repo.verify().await.unwrap();
        let archive = temp.path().join("archive");
        repo.archive(&archive).await.unwrap();
        drop(repo);
        let imported = temp.path().join("imported");
        synck_local::portable::import_repository(&archive, &imported)
            .await
            .unwrap();
        let copy = PortableRepository::open(&imported).unwrap();
        copy.verify().await.unwrap();
        assert_eq!(
            serde_json::to_value(copy.log(None, 512).await.unwrap()).unwrap(),
            original
        );
        for (id, expected) in snapshots {
            assert_eq!(
                serde_json::to_value(copy.show_item(&id, None).await.unwrap()).unwrap(),
                expected
            );
        }
        // Mixed histories must not break filesystem revert/recover.
        let working = synck_local::Repository::discover(&root).await.unwrap();
        fs::write(root.join("file.txt"), b"one").unwrap();
        working.commit("file").await.unwrap();
        fs::write(root.join("file.txt"), b"two").unwrap();
        working.commit("edit").await.unwrap();
        working.revert("HEAD", "undo file").await.unwrap();
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"one");
        working.verify_complete().await.unwrap();
    }
}

#[tokio::test]
async fn garbage_collection_preserves_history_and_removes_unreachable_storage() {
    use sha2::{Digest, Sha256};
    use synck_local::{StorageMode, portable::PortableRepository};
    for mode in [StorageMode::WholeFile, StorageMode::Chunked] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        synck_local::portable::init_repository_with_mode(&root, "tester", mode)
            .await
            .unwrap();
        let bytes = b"unreachable bytes";
        let digest = Sha256::digest(bytes).to_vec();
        if mode == StorageMode::WholeFile {
            fs::write(
                root.join(".synck/objects").join(hex::encode(&digest)),
                bytes,
            )
            .unwrap();
        } else {
            let db = rusqlite::Connection::open(root.join(".synck/object-store.sqlite3")).unwrap();
            db.execute_batch("PRAGMA foreign_keys=ON; BEGIN IMMEDIATE")
                .unwrap();
            db.execute(
                "INSERT INTO chunks(digest,raw_size,codec,payload) VALUES(?1,?2,'RAW',?3)",
                rusqlite::params![&digest, bytes.len() as i64, bytes.as_slice()],
            )
            .unwrap();
            db.execute(
                "INSERT INTO blobs(digest,size_bytes,complete) VALUES(?1,?2,1)",
                rusqlite::params![&digest, bytes.len() as i64],
            )
            .unwrap();
            db.execute(
                "INSERT INTO blob_chunks(blob_digest,ordinal,chunk_digest) VALUES(?1,0,?1)",
                [&digest],
            )
            .unwrap();
            db.execute_batch("COMMIT").unwrap();
        }
        let mut repo = PortableRepository::open(&root).unwrap();
        let before = repo.verify().await.unwrap();
        let report = repo.storage_report().await.unwrap();
        assert_eq!(report.unreachable_objects, 1);
        assert_eq!(report.unreachable_bytes, bytes.len() as u64);
        assert_eq!(report.referenced_objects, 1);
        let removed = repo.gc().await.unwrap();
        assert_eq!(removed.removed_objects, 1);
        assert_eq!(removed.removed_bytes, bytes.len() as u64);
        assert_eq!(repo.storage_report().await.unwrap().unreachable_objects, 0);
        assert_eq!(
            serde_json::to_value(repo.verify().await.unwrap()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        drop(repo);
        run(&root, &["storage", "optimize"]);
        run(&root, &["gc"]);
        assert_eq!(json(&root, &["fsck"])["commits"], 1);
        run(&root, &["monitor", "stop"]);
    }
}

#[tokio::test]
async fn history_commands_refuse_partial_changes_across_checkout_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    synck_local::portable::init_repository(&root, "tester")
        .await
        .unwrap();
    fs::write(
        root.join("mail.eml"),
        b"From: a@example.test\r\nSubject: test\r\n\r\nbody",
    )
    .unwrap();
    fs::write(root.join("a.txt"), b"a").unwrap();
    fs::write(root.join("b.txt"), b"b").unwrap();
    let mut repo = synck_local::Repository::discover(&root).await.unwrap();
    repo.commit("mixed import").await.unwrap();
    let history_len = repo.log(None).await.unwrap().len();
    assert!(
        repo.revert("HEAD", "reject partial inverse")
            .await
            .unwrap_err()
            .to_string()
            .contains("non-filesystem history")
    );
    assert_eq!(repo.log(None).await.unwrap().len(), history_len);
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"a");
    fs::remove_file(root.join("a.txt")).unwrap();
    repo.commit("delete a").await.unwrap();
    repo.set_checkout_paths(&["b.txt".into()]).await.unwrap();
    let history_len = repo.log(None).await.unwrap().len();
    assert!(
        repo.revert("HEAD", "reject omitted inverse")
            .await
            .unwrap_err()
            .to_string()
            .contains("checkout selection")
    );
    assert!(
        repo.recover_item("a.txt", "reject omitted recovery")
            .await
            .unwrap_err()
            .to_string()
            .contains("checkout selection")
    );
    assert_eq!(repo.log(None).await.unwrap().len(), history_len);
    assert!(!root.join("a.txt").exists());
    repo.verify_complete().await.unwrap();
}

#[test]
fn verification_aliases_reject_corrupt_checkout_state_without_repairing_it() {
    let temp = tempfile::tempdir().unwrap();
    run(temp.path(), &["init", "--author", "tester"]);
    let db = rusqlite::Connection::open(temp.path().join(".synck/repository.sqlite3")).unwrap();
    assert_eq!(
        db.execute("UPDATE local_file_heads SET blob_id='invalid'", [])
            .unwrap(),
        1
    );
    for command in [vec!["fsck"], vec!["repository", "verify"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_synck"))
            .current_dir(temp.path())
            .args(command)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("invalid blob ID"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        db.query_row("SELECT blob_id FROM local_file_heads", [], |row| row
            .get::<_, String>(0))
            .unwrap(),
        "invalid"
    );
}

#[test]
fn init_rejects_nested_repositories_before_publication() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    run(
        temp.path(),
        &["init", root.to_str().unwrap(), "--author", "tester"],
    );
    for destination in [root.join("new-child"), root.join("existing-child")] {
        if destination.ends_with("existing-child") {
            fs::create_dir(&destination).unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_synck"))
            .args(["init", destination.to_str().unwrap(), "--author", "tester"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!destination.join(".synck").exists());
        assert!(!destination.join(".synckignore").exists());
    }
    // An outer initialization must not absorb an existing descendant either.
    let outer = temp.path().join("outer");
    fs::create_dir_all(outer.join("child/.synck")).unwrap();
    fs::write(outer.join(".synckignore"), b"*.private\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_synck"))
        .args(["init", outer.to_str().unwrap(), "--author", "tester"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("nested .synck"));
    assert!(!outer.join(".synck").exists());
    assert_eq!(
        fs::read(outer.join(".synckignore")).unwrap(),
        b"*.private\n"
    );
    assert_eq!(fs::read_dir(&outer).unwrap().count(), 2);
}

#[tokio::test]
async fn checkout_file_writers_preserve_clean_state_identity_and_dirty_files() {
    use synck_local::{CommitOutcome, Repository, StorageMode, portable::PortableRepository};
    for mode in [StorageMode::WholeFile, StorageMode::Chunked] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        synck_local::portable::init_repository_with_mode(&root, "tester", mode)
            .await
            .unwrap();
        let source = temp.path().join("source.txt");
        fs::write(&source, b"created").unwrap();
        let mut repo = PortableRepository::open(&root).unwrap();
        let (id, _) = repo.create_file(None, "file.txt", &source).await.unwrap();
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"created");
        fs::write(&source, b"saved").unwrap();
        repo.save_file(&id, &source).await.unwrap();
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"saved");
        repo.verify().await.unwrap();
        let before = repo.log(None, 512).await.unwrap();
        repo.save_file(&id, &source).await.unwrap();
        assert_eq!(
            serde_json::to_value(repo.log(None, 512).await.unwrap()).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        fs::write(root.join("file.txt"), b"dirty").unwrap();
        assert!(repo.save_file(&id, &source).await.is_err());
        assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"dirty");
        assert_eq!(
            serde_json::to_value(repo.log(None, 512).await.unwrap()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        fs::write(root.join("file.txt"), b"saved").unwrap();
        for ignored in [".hidden", "cache.tmp"] {
            assert!(repo.create_file(None, ignored, &source).await.is_err());
            assert!(!root.join(ignored).exists());
        }
        fs::write(root.join("collision.txt"), b"untracked").unwrap();
        assert!(
            repo.create_file(None, "collision.txt", &source)
                .await
                .is_err()
        );
        assert_eq!(fs::read(root.join("collision.txt")).unwrap(), b"untracked");
        fs::remove_file(root.join("collision.txt")).unwrap();
        drop(repo);
        let mut working = Repository::discover(&root).await.unwrap();
        assert!(working.status().await.unwrap().is_empty());
        assert!(matches!(
            working.commit("no stale deletion or update").await.unwrap(),
            CommitOutcome::Clean
        ));
        working
            .set_checkout_paths(&[".synckignore".into()])
            .await
            .unwrap();
        drop(working);
        let mut repo = PortableRepository::open(&root).unwrap();
        assert!(
            repo.save_file(&id, &source)
                .await
                .unwrap_err()
                .to_string()
                .contains("checkout selection")
        );
        assert!(
            repo.create_file(None, "outside.txt", &source)
                .await
                .unwrap_err()
                .to_string()
                .contains("checkout selection")
        );
        assert!(!root.join("file.txt").exists());
        repo.verify().await.unwrap();
    }
}

#[test]
fn verification_rejects_valid_but_incorrect_checkout_projection_values() {
    for assignment in [
        "blob_id='sha256:0000000000000000000000000000000000000000000000000000000000000000'",
        "size_bytes=size_bytes+1",
        "item_seq=item_seq+1",
    ] {
        let temp = tempfile::tempdir().unwrap();
        run(temp.path(), &["init", "--author", "tester"]);
        let db = rusqlite::Connection::open(temp.path().join(".synck/repository.sqlite3")).unwrap();
        db.execute(&format!("UPDATE local_file_heads SET {assignment}"), [])
            .unwrap();
        for command in [vec!["fsck"], vec!["repository", "verify"]] {
            let output = Command::new(env!("CARGO_BIN_EXE_synck"))
                .current_dir(temp.path())
                .args(command)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2));
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("local projection mismatch"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[tokio::test]
async fn imported_checkout_replays_moved_and_deleted_file_content_heads() {
    use synck_local::{Repository, portable::PortableRepository};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    synck_local::portable::init_repository(&root, "tester")
        .await
        .unwrap();
    fs::write(root.join("live.txt"), b"live").unwrap();
    fs::write(root.join("deleted.txt"), b"deleted").unwrap();
    let repo = Repository::discover(&root).await.unwrap();
    repo.commit("files").await.unwrap();
    repo.move_path(
        &root,
        Path::new("live.txt"),
        Path::new("renamed.txt"),
        "rename",
    )
    .await
    .unwrap();
    fs::remove_file(root.join("deleted.txt")).unwrap();
    repo.commit("delete").await.unwrap();
    let archive = temp.path().join("archive");
    repo.archive_complete(&archive, None).await.unwrap();
    drop(repo);
    let mut copy = PortableRepository::open(&archive).unwrap();
    copy.enable_checkout("tester", &[]).await.unwrap();
    copy.verify().await.unwrap();
    assert_eq!(fs::read(archive.join("renamed.txt")).unwrap(), b"live");
    assert!(!archive.join("deleted.txt").exists());
    drop(copy);
    let working = Repository::discover(&archive).await.unwrap();
    working
        .recover_item("deleted.txt", "recover after import")
        .await
        .unwrap();
    assert_eq!(fs::read(archive.join("deleted.txt")).unwrap(), b"deleted");
    working.verify_complete().await.unwrap();
}

#[test]
fn an_edited_email_source_reports_how_to_keep_both_messages() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    run(root, &["init", "--author", "tester"]);
    let message = "From: a@example.test\r\nSubject: first\r\n\r\nhello\r\n";
    fs::write(root.join("mail.eml"), message).unwrap();
    run(root, &["commit", "-m", "record email"]);

    fs::write(root.join("mail.eml"), message.replace("first", "second")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(root)
        .args(["commit", "-m", "edited email"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("an email named \"mail\" is already recorded"),
        "{stderr}"
    );

    // Saving the changed message under a new name records a second email.
    fs::rename(root.join("mail.eml"), root.join("mail-v2.eml")).unwrap();
    run(root, &["commit", "-m", "second email"]);
    assert_eq!(
        run(root, &["status"]),
        "nothing to commit, working tree clean\n"
    );
    let items = json(root, &["item", "list"]);
    let emails = items["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["item_type"] == "EMAIL")
        .count();
    assert_eq!(emails, 2);
}

#[tokio::test]
async fn direct_file_writes_carry_attribution_base_checks_and_replay() {
    use sha2::{Digest, Sha256};
    use synck_local::LocalError;
    use synck_local::portable::{
        PortableRepository, WriteOptions, import_repository, init_repository,
    };
    use synck_vcs::types::ObjectId;

    let temp = tempfile::tempdir().unwrap();
    let checkout = temp.path().join("checkout");
    init_repository(&checkout, "integration").await.unwrap();
    let transfer = temp.path().join("transfer");
    PortableRepository::open(&checkout)
        .unwrap()
        .archive(&transfer)
        .await
        .unwrap();
    let bare = temp.path().join("bare");
    import_repository(&transfer, &bare).await.unwrap();
    PortableRepository::open(&bare)
        .unwrap()
        .activate("integration", true)
        .await
        .unwrap();

    for root in [checkout, bare] {
        let source = temp.path().join("source.txt");
        let blob = |bytes: &[u8]| {
            ObjectId::parse(format!("sha256:{}", hex::encode(Sha256::digest(bytes)))).unwrap()
        };
        let options = |operation: u128| WriteOptions {
            message: Some("Saved from the app".into()),
            author: Some("user:alex".into()),
            occurred_at: Some("2026-10-27T09:30:00Z".parse().unwrap()),
            operation_id: Some(uuid::Uuid::from_u128(operation)),
        };
        let mut repo = PortableRepository::open(&root).unwrap();

        fs::write(&source, b"first").unwrap();
        let created = repo
            .create_file_with(None, "doc.txt", &source, &options(1))
            .await
            .unwrap();
        assert_eq!(
            repo.create_file_with(None, "doc.txt", &source, &options(1))
                .await
                .unwrap(),
            created
        );
        let id = created.0;
        let first_id = ObjectId::parse(repo.show_item(&id, None).await.unwrap().item.projection.content_blob_id.unwrap()).unwrap();

        fs::write(&source, b"second").unwrap();
        let error = repo
            .save_file_with(&id, &source, Some(&blob(b"stale")), &options(2))
            .await
            .unwrap_err();
        let LocalError::Conflict {
            head_seq,
            blob_id,
            size,
            ..
        } = &error
        else {
            panic!("expected conflict, got {error}");
        };
        assert_eq!((blob_id.as_str(), *size), (first_id.as_str(), 5));
        assert!(*head_seq >= 2);

        let saved = repo
            .save_file_with(&id, &source, Some(&first_id), &options(2))
            .await
            .unwrap();
        // A retry after acceptance returns the original commit even though its
        // base is no longer current.
        assert_eq!(
            repo.save_file_with(&id, &source, Some(&first_id), &options(2))
                .await
                .unwrap(),
            saved
        );
        fs::write(&source, b"different").unwrap();
        assert!(
            repo.save_file_with(&id, &source, None, &options(2))
                .await
                .unwrap_err()
                .to_string()
                .contains("different write")
        );

        if root.ends_with("checkout") {
            assert_eq!(fs::read(root.join("doc.txt")).unwrap(), b"second");
        }
        let log = repo.log(None, 512).await.unwrap();
        let head = &log.commits[0];
        assert_eq!(head.commit_id, saved);
        assert_eq!(head.author, "user:alex");
        assert_eq!(head.committer, "integration");
        assert_eq!(head.message.as_deref(), Some("Saved from the app"));
        assert_eq!(log.commits[1].commit_id, created.1);
        assert_eq!(log.commits.len(), 3);
        drop(repo);
        let occurred: String = rusqlite::Connection::open(root.join(".synck/repository.sqlite3"))
            .unwrap()
            .query_row(
                "SELECT occurred_at FROM vcs_commits ORDER BY repo_seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(occurred.starts_with("2026-10-27T09:30:00"));
        PortableRepository::open(&root)
            .unwrap()
            .verify()
            .await
            .unwrap();
    }
}
