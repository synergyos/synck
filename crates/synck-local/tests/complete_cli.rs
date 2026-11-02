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
/// `cp -R` without the shell tool, which Windows lacks.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
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
fn received_text_cannot_drive_the_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    run(root, &["init", "--author", "ana\u{1b}[2J"]);
    std::fs::write(root.join("note.txt"), "hello\n").unwrap();
    let message = "line\u{1b}]0;title\u{7}\u{202e}one";
    run(root, &["commit", "-m", message]);

    let log = run(root, &["log"]);
    assert!(log.contains(r"line\u{1b}]0;title\u{7}\u{202e}one"), "{log}");
    assert!(log.contains(r"ana\u{1b}[2J"), "{log}");
    for output in [log, run(root, &["log", "--oneline"]), run(root, &["show", "HEAD"])] {
        assert!(!output.contains(['\u{1b}', '\u{7}', '\u{202e}']), "{output}");
    }

    let exact = json(root, &["log", "--json"]);
    assert_eq!(exact["commits"][0]["message"], message);
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
        let mail = temp.path().join("mail.eml");
        fs::write(&mail, b"From: a@example.test\r\nSubject: test\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nhello\r\n--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=a.txt\r\nContent-Transfer-Encoding: base64\r\n\r\nYXR0YWNobWVudA==\r\n--x--\r\n").unwrap();
        let mut repo = PortableRepository::open(&root).unwrap();
        repo.import_email(None, &mail, None).await.unwrap();
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
        let mut items = repo.list_items(None, None, 512).await.unwrap().items;
        items.extend(repo.list_records(None, None, 512).await.unwrap());
        assert!(items.iter().any(|item| item.item_type == "EMAIL"));
        let mut snapshots = Vec::new();
        for item in items {
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
    use synck_local::{StorageMode, portable::PortableRepository};
    use synck_vcs::object::plain_id;
    for mode in [StorageMode::WholeFile, StorageMode::Chunked] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        synck_local::portable::init_repository_with_mode(&root, "tester", mode)
            .await
            .unwrap();
        let bytes = b"unreachable bytes";
        let digest = plain_id(bytes).as_bytes().to_vec();
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
                rusqlite::params![&digest, bytes.len() as i64, bytes],
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
    let mail = temp.path().join("mail.eml");
    fs::write(&mail, b"From: a@example.test\r\nSubject: test\r\n\r\nbody").unwrap();
    fs::write(root.join("a.txt"), b"a").unwrap();
    fs::write(root.join("b.txt"), b"b").unwrap();
    let repo = synck_local::Repository::discover(&root).await.unwrap();
    repo.commit("add files").await.unwrap();
    drop(repo);
    let mut portable = synck_local::portable::PortableRepository::open(&root).unwrap();
    portable.import_email(None, &mail, None).await.unwrap();
    drop(portable);
    let mut repo = synck_local::Repository::discover(&root).await.unwrap();
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
    let db = rusqlite::Connection::open(temp.path().join(".synck/repository.local.sqlite3")).unwrap();
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
            String::from_utf8_lossy(&output.stderr).contains("local projection mismatch"),
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
fn verification_accepts_a_projection_behind_history() {
    let temp = tempfile::tempdir().unwrap();
    run(temp.path(), &["init", "--author", "tester"]);
    let db = rusqlite::Connection::open(temp.path().join(".synck/repository.local.sqlite3")).unwrap();
    db.execute(
        "UPDATE local_projection_state SET applied_repo_seq = applied_repo_seq - 1",
        [],
    )
    .unwrap();
    run(temp.path(), &["fsck"]);
}

#[test]
fn repeating_a_write_finishes_one_killed_before_acceptance() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repository");
    std::fs::create_dir(&root).unwrap();
    run(&root, &["init", "--author", "tester"]);
    let source = temp.path().join("source.txt");
    std::fs::write(&source, "saved\n").unwrap();
    let source = source.to_str().unwrap();

    std::fs::write(root.join("new.txt"), "saved\n").unwrap();
    run(&root, &["repository", "create-file", "new.txt", source, "-m", "create"]);
    let listed = json(&root, &["item", "list"]);
    let items = listed["items"].as_array().unwrap();
    let item = items.iter().find(|i| i["name"] == "new.txt").unwrap();
    let item = item["item_id"].as_str().unwrap();

    std::fs::write(temp.path().join("source.txt"), "again\n").unwrap();
    std::fs::write(root.join("new.txt"), "again\n").unwrap();
    run(&root, &["repository", "save-file", item, source, "-m", "save"]);
    assert!(run(&root, &["status"]).contains("working tree clean"));

    std::fs::write(root.join("new.txt"), "other work\n").unwrap();
    let refused = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(&root)
        .args(["repository", "save-file", item, source, "-m", "save"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert_eq!(std::fs::read(root.join("new.txt")).unwrap(), b"other work\n");
}

#[test]
fn item_commands_take_repository_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repository");
    fs::create_dir_all(root.join("Matters/Case A")).unwrap();
    fs::write(root.join("Matters/Case A/brief.txt"), "draft\n").unwrap();
    run(&root, &["init", "--author", "tester"]);
    run(&root, &["commit", "-m", "brief"]);

    let listed = json(&root, &["item", "list", "--parent", "Matters/Case A"]);
    let brief = listed["items"][0]["item_id"].as_str().unwrap().to_owned();
    assert_eq!(
        run(&root, &["item", "show", "matters/case a/BRIEF.txt"]),
        run(&root, &["item", "show", &brief])
    );

    let source = temp.path().join("source.txt");
    fs::write(&source, "final\n").unwrap();
    fs::write(root.join("Matters/Case A/brief.txt"), "final\n").unwrap();
    let source = source.to_str().unwrap();
    run(&root, &["repository", "save-file", "Matters/Case A/brief.txt", source, "-m", "save"]);
    let copy = json(&root, &[
        "repository", "duplicate", "Matters/Case A/brief.txt", "copy.txt",
        "--parent", "Matters/Case A", "-m", "copy",
    ]);
    assert_eq!(fs::read(root.join("Matters/Case A/copy.txt")).unwrap(), b"final\n");
    assert_eq!(
        json(&root, &["item", "show", "Matters/Case A/copy.txt"])["item"]["projection"]["item_id"],
        copy["item_id"]
    );
    fs::write(root.join("Matters/Case A/brief.txt"), "taken in\n").unwrap();
    fs::write(temp.path().join("source.txt"), "taken in\n").unwrap();
    run(&root, &[
        "repository", "save-file", &brief, source, "-m", "take in",
        "--merged-from", "Matters/Case A/copy.txt@1",
    ]);
    let exported = temp.path().join("exported.txt");
    run(&root, &["item", "export", "Matters/Case A/brief.txt", exported.to_str().unwrap()]);
    assert_eq!(fs::read(&exported).unwrap(), b"taken in\n");

    for missing in ["Matters/Case B/brief.txt", "Matters/Case A/brief.txt/x", "", "Matters//brief.txt"] {
        let output = Command::new(env!("CARGO_BIN_EXE_synck"))
            .current_dir(&root)
            .args(["item", "show", missing])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{missing}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("no live item"), "{missing}");
    }
}

#[test]
fn restore_tree_visits_an_earlier_commit_and_commits_a_rollback() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repository");
    fs::create_dir_all(root.join("Docs")).unwrap();
    fs::write(root.join("plan.txt"), "first\n").unwrap();
    fs::write(root.join("Docs/notes.txt"), "notes\n").unwrap();
    run(&root, &["init", "--author", "tester"]);
    run(&root, &["commit", "-m", "one"]);
    let id = |path: &str| json(&root, &["item", "show", path])["item"]["projection"]["item_id"].clone();
    let (plan, notes) = (id("plan.txt"), id("Docs/notes.txt"));

    fs::write(root.join("plan.txt"), "second\n").unwrap();
    fs::rename(root.join("Docs/notes.txt"), root.join("notes.txt")).unwrap();
    fs::write(root.join("added.txt"), "added\n").unwrap();
    run(&root, &["commit", "-m", "two"]);
    fs::write(root.join("scratch.tmp"), "ignored\n").unwrap();

    let restored = json(&root, &["--json", "restore-tree", "HEAD~1"]);
    assert_eq!(restored["restored"], true);
    assert_eq!(fs::read(root.join("plan.txt")).unwrap(), b"first\n");
    assert_eq!(fs::read(root.join("Docs/notes.txt")).unwrap(), b"notes\n");
    assert!(!root.join("notes.txt").exists() && !root.join("added.txt").exists());
    assert_eq!(fs::read(root.join("scratch.tmp")).unwrap(), b"ignored\n");
    let changes = json(&root, &["status", "--json"])["changes"].clone();
    assert_eq!(changes.as_array().unwrap().len(), 3, "{changes}");
    assert_eq!(json(&root, &["--json", "restore-tree", "HEAD~1"])["restored"], false);

    run(&root, &["restore-tree"]);
    assert_eq!(fs::read(root.join("plan.txt")).unwrap(), b"second\n");
    assert_eq!(fs::read(root.join("notes.txt")).unwrap(), b"notes\n");
    assert!(run(&root, &["status"]).contains("working tree clean"));

    run(&root, &["restore-tree", "HEAD~1"]);
    run(&root, &["commit", "-m", "roll back"]);
    assert!(run(&root, &["status"]).contains("working tree clean"));
    assert_eq!((id("plan.txt"), id("Docs/notes.txt")), (plan, notes));
    assert_eq!(fs::read(root.join("scratch.tmp")).unwrap(), b"ignored\n");
}

fn fails(root: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(!output.status.success(), "{args:?} succeeded");
    serde_json::from_slice(&output.stderr).unwrap()
}

#[test]
fn clones_share_one_history_and_never_overwrite_each_other() {
    let temp = tempfile::tempdir().unwrap();
    let origin = temp.path().join("origin");
    fs::create_dir_all(origin.join("docs")).unwrap();
    fs::write(origin.join("docs/a.txt"), b"a\n").unwrap();
    fs::write(origin.join("b.txt"), b"b\n").unwrap();
    run(&origin, &["init", "--author", "origin"]);
    run(&origin, &["commit", "-m", "start"]);
    run(temp.path(), &["clone", "origin", "full", "--author", "full"]);
    run(temp.path(), &["clone", "origin", "docs", "--include", "docs"]);
    let (full, docs) = (temp.path().join("full"), temp.path().join("docs"));
    // History and content stay in the origin; a clone keeps its working state.
    for name in ["repository.sqlite3", "objects", "object-store.sqlite3"] {
        assert!(!full.join(".synck").join(name).exists(), "{name}");
    }
    assert_eq!(fs::read(full.join("docs/a.txt")).unwrap(), b"a\n");
    assert!(!docs.join("b.txt").exists());
    assert!(run(&docs, &["status"]).contains("working tree clean"));
    // A clone's gc collects the origin's store and leaves its checkout alone.
    run(&full, &["gc"]);
    assert!(run(&origin, &["status"]).contains("working tree clean"));

    // Commits to different files land from both, with no action from either.
    fs::write(full.join("b.txt"), b"b full\n").unwrap();
    run(&full, &["commit", "-m", "full"]);
    fs::write(docs.join("docs/a.txt"), b"a docs\n").unwrap();
    run(&docs, &["commit", "-m", "docs"]);
    assert_eq!(fs::read(docs.join("docs/a.txt")).unwrap(), b"a docs\n");
    // The origin's files are based on its own last commit; pull brings the rest.
    assert!(run(&origin, &["status"]).contains("working tree clean"));
    let pulled = json(&origin, &["pull", "--json"]);
    assert_eq!(pulled["updated"], serde_json::json!(["b.txt", "docs/a.txt"]));
    assert_eq!(fs::read(origin.join("b.txt")).unwrap(), b"b full\n");
    assert!(run(&origin, &["status"]).contains("working tree clean"));

    // A file changed on both sides is reported, never overwritten.
    fs::write(full.join("docs/a.txt"), b"a mine\n").unwrap();
    let error = fails(&full, &["commit", "-m", "mine", "--json"]);
    assert_eq!(error["error"]["kind"], "CONFLICT");
    assert_eq!(error["error"]["paths"], serde_json::json!(["docs/a.txt"]));
    assert_eq!(fs::read(full.join("docs/a.txt")).unwrap(), b"a mine\n");
}

#[test]
fn pull_merges_text_and_keeps_both_versions_of_anything_else() {
    let temp = tempfile::tempdir().unwrap();
    let origin = temp.path().join("origin");
    fs::create_dir_all(&origin).unwrap();
    fs::write(origin.join("a.txt"), b"1\n2\n3\n4\n5\n").unwrap();
    fs::write(origin.join("c.txt"), b"x\ny\n").unwrap();
    fs::write(origin.join("plan.bin"), b"plan\0base").unwrap();
    run(&origin, &["init", "--author", "origin"]);
    run(&origin, &["commit", "-m", "start"]);
    run(temp.path(), &["clone", "origin", "b", "--author", "b"]);
    let b = temp.path().join("b");
    fs::write(origin.join("a.txt"), b"1 origin\n2\n3\n4\n5\n").unwrap();
    fs::write(origin.join("c.txt"), b"x origin\ny\n").unwrap();
    fs::write(origin.join("plan.bin"), b"plan\0origin").unwrap();
    run(&origin, &["commit", "-m", "origin"]);
    fs::write(b.join("a.txt"), b"1\n2\n3\n4\n5 b\n").unwrap();
    fs::write(b.join("c.txt"), b"x b\ny\n").unwrap();
    fs::write(b.join("plan.bin"), b"plan\0b").unwrap();

    // A commit stops; pull merges.
    assert_eq!(fails(&b, &["commit", "-m", "b", "--json"])["error"]["kind"], "CONFLICT");
    let pulled = json(&b, &["pull", "--json"]);
    assert_eq!(pulled["conflicts"], serde_json::json!(["c.txt", "plan.bin"]));
    assert_eq!(fs::read(b.join("a.txt")).unwrap(), b"1 origin\n2\n3\n4\n5 b\n");
    assert_eq!(
        fs::read(b.join("c.txt")).unwrap(),
        b"<<<<<<< ours\nx b\n=======\nx origin\n>>>>>>> theirs\ny\n"
    );
    assert_eq!(fs::read(b.join("plan.bin")).unwrap(), b"plan\0b");
    assert_eq!(fs::read(b.join("plan.~theirs.bin")).unwrap(), b"plan\0origin");
    let status = json(&b, &["status", "--json"]);
    assert_eq!(status["conflicts"][1]["theirs"], "plan.~theirs.bin");

    // Nothing is committed until every conflict is resolved.
    assert_eq!(fails(&b, &["commit", "-m", "b", "--json"])["error"]["kind"], "CONFLICT");
    assert_eq!(fails(&b, &["resolve", "c.txt", "--json"])["error"]["kind"], "INVALID");
    fs::write(b.join("c.txt"), b"x both\ny\n").unwrap();
    fs::remove_file(b.join("plan.~theirs.bin")).unwrap();
    run(&b, &["resolve", "c.txt", "plan.bin"]);
    assert!(json(&b, &["status", "--json"]).get("conflicts").is_none());
    run(&b, &["commit", "-m", "b merged"]);
    run(&origin, &["pull"]);
    assert_eq!(fs::read(origin.join("a.txt")).unwrap(), b"1 origin\n2\n3\n4\n5 b\n");
    assert_eq!(fs::read(origin.join("c.txt")).unwrap(), b"x both\ny\n");
}

#[test]
fn an_alternative_folder_is_duplicated_and_adopted_with_its_sources() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join("Design/sub")).unwrap();
    fs::write(root.join("Design/plan.txt"), b"plan\n").unwrap();
    fs::write(root.join("Design/old.txt"), b"old\n").unwrap();
    fs::write(root.join("Design/sub/spec.txt"), b"spec\n").unwrap();
    run(&root, &["init", "--author", "a"]);
    run(&root, &["commit", "-m", "start"]);
    run(&root, &["duplicate", "Design", "Option B"]);
    assert_eq!(fs::read(root.join("Option B/sub/spec.txt")).unwrap(), b"spec\n");
    let created = json(&root, &["show", "HEAD", "--events", "--json"]).to_string();
    assert!(created.contains("derived_from"), "{created}");

    fs::write(root.join("Option B/plan.txt"), b"plan B\n").unwrap();
    fs::remove_file(root.join("Option B/old.txt")).unwrap();
    fs::write(root.join("Option B/sub/new.txt"), b"new\n").unwrap();
    run(&root, &["commit", "-m", "option B"]);
    let plan = json(&root, &["item", "show", "Design/plan.txt", "--json"]);
    run(&root, &["adopt", "Option B", "Design"]);
    assert!(run(&root, &["status"]).contains("working tree clean"));
    assert_eq!(fs::read(root.join("Design/plan.txt")).unwrap(), b"plan B\n");
    assert_eq!(fs::read(root.join("Design/sub/new.txt")).unwrap(), b"new\n");
    assert!(!root.join("Design/old.txt").exists());
    // The original keeps its identity; the new version names its source.
    let adopted = json(&root, &["item", "show", "Design/plan.txt", "--json"]);
    assert_eq!(adopted["result"]["item_id"], plan["result"]["item_id"]);
    let events = json(&root, &["show", "HEAD", "--events", "--json"]).to_string();
    assert!(events.contains("merged_from"), "{events}");
    assert!(run(&root, &["adopt", "Option B", "Design"]).contains("already matches"));
}

#[test]
fn a_reservation_holds_in_every_clone() {
    let temp = tempfile::tempdir().unwrap();
    let origin = temp.path().join("origin");
    fs::create_dir_all(&origin).unwrap();
    fs::write(origin.join("report.docx"), b"r\n").unwrap();
    fs::write(origin.join("notes.txt"), b"n\n").unwrap();
    run(&origin, &["init", "--author", "origin"]);
    run(&origin, &["commit", "-m", "start"]);
    run(temp.path(), &["clone", "origin", "a", "--author", "a"]);
    run(temp.path(), &["clone", "origin", "b", "--author", "b"]);
    let (a, b) = (temp.path().join("a"), temp.path().join("b"));
    let lease = json(&a, &["--json", "lock", "acquire", "report.docx", "--owner", "agent-a"]);
    let token = lease["result"]["token"].as_str().unwrap().to_string();

    // Another clone sees the holder and cannot commit the file or take it.
    let status = json(&b, &["status", "--json"]);
    assert_eq!(status["reservations"][0]["owner"], "agent-a");
    fs::write(b.join("report.docx"), b"b\n").unwrap();
    assert_eq!(fails(&b, &["commit", "-m", "b", "--json"])["error"]["kind"], "LOCKED");
    run(&b, &["restore", "report.docx"]);
    assert_eq!(
        fails(&b, &["--json", "lock", "acquire", "report.docx", "--owner", "agent-b"])["error"]["kind"],
        "LOCKED"
    );

    // Other files still land, and the holder's commit reaches every clone.
    fs::write(b.join("notes.txt"), b"n b\n").unwrap();
    run(&b, &["commit", "-m", "notes"]);
    fs::write(a.join("report.docx"), b"r a\n").unwrap();
    run(&a, &["--lock-token", &token, "commit", "-m", "report"]);
    run(&b, &["pull"]);
    assert_eq!(fs::read(b.join("report.docx")).unwrap(), b"r a\n");
    run(&a, &["lock", "release", &token]);
    assert!(json(&b, &["status", "--json"]).get("reservations").is_none());
}

#[test]
fn held_clones_keep_commits_until_push_with_their_ids() {
    let temp = tempfile::tempdir().unwrap();
    let origin = temp.path().join("origin");
    fs::create_dir_all(origin.join("docs")).unwrap();
    fs::write(origin.join("docs/a.txt"), b"a\n").unwrap();
    fs::write(origin.join("b.txt"), b"b\n").unwrap();
    run(&origin, &["init", "--author", "origin"]);
    run(&origin, &["commit", "-m", "start"]);
    run(temp.path(), &["clone", "origin", "held", "--hold", "--include", "docs"]);
    let held = temp.path().join("held");

    fs::write(held.join("docs/a.txt"), b"a held\n").unwrap();
    let first = json(&held, &["commit", "-m", "held a", "--json"])["commit_id"].clone();
    fs::write(held.join("docs/c.txt"), b"c held\n").unwrap();
    let second = json(&held, &["commit", "-m", "held c", "--json"])["commit_id"].clone();
    let status = json(&held, &["status", "--json"]);
    assert_eq!(status["changes"], serde_json::json!([]));
    assert_eq!(status["held_commits"], serde_json::json!([first, second]));
    assert!(!run(&origin, &["log", "--oneline"]).contains("held"));
    // Held content lives in the origin's store; garbage collection keeps it.
    run(&origin, &["gc"]);

    // Commits from elsewhere to other files do not stop a push.
    fs::write(origin.join("b.txt"), b"b origin\n").unwrap();
    run(&origin, &["commit", "-m", "origin b"]);
    let pushed = json(&held, &["push", "--json"]);
    assert_eq!(pushed["pushed"], serde_json::json!([first, second]));
    assert_eq!(json(&held, &["status", "--json"])["held_commits"], serde_json::json!([]));
    run(&origin, &["pull"]);
    assert_eq!(fs::read(origin.join("docs/c.txt")).unwrap(), b"c held\n");
    run(&origin, &["fsck"]);

    // An item changed on both sides stops the push, and nothing moves.
    fs::write(held.join("docs/a.txt"), b"a mine\n").unwrap();
    run(&held, &["commit", "-m", "mine"]);
    fs::write(origin.join("docs/a.txt"), b"a theirs\n").unwrap();
    run(&origin, &["commit", "-m", "theirs"]);
    let error = fails(&held, &["push", "--json"]);
    assert_eq!(error["error"]["kind"], "CONFLICT");
    assert_eq!(error["error"]["paths"], serde_json::json!(["docs/a.txt"]));
    assert_eq!(fs::read(held.join("docs/a.txt")).unwrap(), b"a mine\n");
    assert_eq!(json(&held, &["status", "--json"])["held_commits"].as_array().unwrap().len(), 1);
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

#[tokio::test]
async fn a_file_write_refuses_a_rename_onto_its_path_and_a_merge_that_records_nothing() {
    use synck_local::portable::{PortableRepository, WriteOptions, init_repository};
    use synck_vcs::types::ItemRevision;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    init_repository(&root, "tester").await.unwrap();
    let source = temp.path().join("source.txt");
    fs::write(&source, b"same").unwrap();
    let options = || WriteOptions { message: Some("m".into()), ..Default::default() };
    let mut repo = PortableRepository::open(&root).unwrap();
    let (first, _) = repo.create_file_with(None, "a.txt", &source, &options()).await.unwrap();
    let (other, _) = repo.create_file_with(None, "b.txt", &source, &options()).await.unwrap();

    let merge = WriteOptions {
        merged_from: Some(ItemRevision { item_id: other.clone(), item_seq: 1 }),
        ..options()
    };
    let error = repo.save_file_with(&first, &source, None, &merge).await.unwrap_err();
    assert!(error.to_string().contains("records nothing"), "{error}");

    drop(repo);
    fs::rename(root.join("a.txt"), root.join("renamed.txt")).unwrap();
    let mut repo = PortableRepository::open(&root).unwrap();
    let error = repo
        .create_file_with(None, "renamed.txt", &source, &options())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("commit or restore"), "{error}");
}

#[tokio::test]
async fn an_operation_id_names_one_request_with_its_sources() {
    use synck_local::portable::{PortableRepository, WriteOptions, init_repository};
    use synck_vcs::types::ItemRevision;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    init_repository(&root, "tester").await.unwrap();
    let source = temp.path().join("source.txt");
    fs::write(&source, b"first").unwrap();
    let plain = WriteOptions { message: Some("m".into()), ..Default::default() };
    let mut repo = PortableRepository::open(&root).unwrap();
    let (file, _) = repo.create_file_with(None, "a.txt", &source, &plain).await.unwrap();
    let (other, _) = repo.create_file_with(None, "b.txt", &source, &plain).await.unwrap();
    fs::write(&source, b"second").unwrap();
    let write = WriteOptions { operation_id: Some(uuid::Uuid::from_u128(9)), ..plain };
    repo.save_file_with(&file, &source, None, &write).await.unwrap();
    repo.save_file_with(&file, &source, None, &write).await.unwrap();
    let merge = WriteOptions {
        merged_from: Some(ItemRevision { item_id: other, item_seq: 1 }),
        ..write
    };
    let error = repo.save_file_with(&file, &source, None, &merge).await.unwrap_err();
    assert!(error.to_string().contains("already used"), "{error}");
}

#[test]
fn a_missing_projection_row_is_a_finding() {
    let temp = tempfile::tempdir().unwrap();
    run(temp.path(), &["init", "--author", "tester"]);
    let db = rusqlite::Connection::open(temp.path().join(".synck/repository.local.sqlite3")).unwrap();
    db.execute("DELETE FROM local_projection_state", []).unwrap();
    drop(db);
    let output = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(temp.path())
        .args(["repository", "verify"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn a_recovery_journal_never_holds_what_acceptance_would_refuse() {
    let temp = tempfile::tempdir().unwrap();
    run(temp.path(), &["init", "--author", "tester"]);
    fs::write(temp.path().join("file.txt"), b"content").unwrap();
    run(temp.path(), &["commit", "-m", "add"]);
    fs::remove_file(temp.path().join("file.txt")).unwrap();
    run(temp.path(), &["commit", "-m", "remove"]);
    let oversized = "m".repeat(17 * 1024);
    let output = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(temp.path())
        .args(["recover", "file.txt", "-m", &oversized])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!temp.path().join(".synck/recover.json").exists());
    run(temp.path(), &["status"]);
}

#[tokio::test]
async fn gc_ignores_foreign_files_and_clears_staging_a_kill_left() {
    use synck_local::portable::PortableRepository;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    run(&source, &["init", "--author", "tester"]);
    fs::write(source.join(".synck/notes.json"), b"not ours").unwrap();
    run(&source, &["gc"]);

    let follower = temp.path().join("follower");
    run(&source, &["repository", "import", ".", follower.to_str().unwrap()]);
    fs::create_dir_all(follower.join(".synck/tmp")).unwrap();
    fs::write(follower.join(".synck/tmp/object-killed"), b"partial").unwrap();
    PortableRepository::open(&follower).unwrap().gc().await.unwrap();
    assert_eq!(fs::read_dir(follower.join(".synck/tmp")).unwrap().count(), 0);
}

#[tokio::test]
async fn a_follower_ignores_checkout_state_until_its_config_names_a_writer() {
    use synck_local::portable::PortableRepository;
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    run(&source, &["init", "--author", "tester"]);
    let follower = temp.path().join("follower");
    run(&source, &["repository", "import", ".", follower.to_str().unwrap()]);
    // What a kill between publishing checkout.json and config.json leaves.
    fs::write(
        follower.join(".synck/checkout.json"),
        br#"{"paths":["elsewhere"],"pending":{"previous_paths":null}}"#,
    )
    .unwrap();
    fs::write(follower.join(".synck/checkout-source.json"), b"[]").unwrap();

    let copy_of = |name: &str| {
        let copy = temp.path().join(name);
        copy_tree(&follower, &copy);
        copy
    };
    // Nothing here is a checkout, so nothing here needs recovery.
    let copy = copy_of("collected");
    PortableRepository::open(&copy).unwrap().gc().await.unwrap();
    assert!(!copy.join(".synck/checkout.json").exists());
    assert!(!copy.join(".synck/checkout-source.json").exists());

    // Activation makes a writer, which would read these files.
    let copy = copy_of("activated");
    run(&copy, &["repository", "activate", ".", "--author", "writer", "--source-retired"]);
    assert!(!copy.join(".synck/checkout.json").exists());
    assert!(!copy.join(".synck/checkout-source.json").exists());
    run(&copy, &["repository", "verify", "."]);

    let mut repository = PortableRepository::open(&follower).unwrap();
    assert!(repository.checkout_paths().is_none());
    repository.enable_checkout("reader", &[]).await.unwrap();
    drop(repository);
    assert!(follower.join(".synckignore").is_file());
    assert!(!follower.join(".synck/checkout-source.json").exists());
    run(&follower, &["fsck"]);
}

#[test]
fn an_operation_id_makes_an_email_import_safe_to_repeat() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repository");
    fs::create_dir(&root).unwrap();
    run(&root, &["init", "--author", "tester"]);
    let email = temp.path().join("message.eml");
    fs::write(&email, "From: a@example.com\r\nTo: b@example.com\r\nSubject: Hello\r\n\r\nBody\r\n").unwrap();
    let source = email.to_str().unwrap();
    let import = ["repository", "import-email", source, "-m", "mail",
        "--operation-id", "019a2900-0000-4000-8000-000000000001"];
    let first = run(&root, &import);
    assert_eq!(run(&root, &import), first);
    let listed = json(&root, &["item", "list"]);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);

    let mut retitled = import.to_vec();
    retitled.extend(["--title", "Another"]);
    let output = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(&root)
        .args(&retitled)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("different write"));
}

#[test]
fn a_move_refuses_an_oversized_message_before_it_journals() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    run(root, &["init", "--author", "tester"]);
    fs::write(root.join("a.txt"), "a\n").unwrap();
    run(root, &["commit", "-m", "first"]);
    let message = "m".repeat(synck_file_folder::MAX_MESSAGE_BYTES + 1);
    let output = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(root)
        .args(["mv", "-m", &message, "a.txt", "b.txt"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(root.join("a.txt").exists());
    run(root, &["status"]);
    run(root, &["mv", "-m", "moved", "a.txt", "b.txt"]);
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
        let db = rusqlite::Connection::open(temp.path().join(".synck/repository.local.sqlite3")).unwrap();
        db.execute(&format!("UPDATE local_file_heads SET {assignment}"), [])
            .unwrap();
        for command in [
            vec!["fsck"],
            vec!["repository", "verify"],
            vec!["fsck", "--rebuild-state-index"],
        ] {
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
    copy.enable_checkout("tester", &[]).await.unwrap();
    assert!(copy.enable_checkout("other", &[]).await.is_err());
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
            ..Default::default()
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
        assert!(*head_seq >= 1);

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
