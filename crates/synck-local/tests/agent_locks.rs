use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use serde_json::Value;

fn run(root: &Path, args: &[&str]) -> Output {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = Command::new(env!("CARGO_BIN_EXE_synck"))
            .current_dir(root)
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        if out.status.success() || error_kind(&out) != "BUSY" {
            return out;
        }
        assert!(Instant::now() < deadline, "repository stayed busy");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn error_kind(out: &Output) -> String {
    serde_json::from_slice::<Value>(&out.stderr)
        .ok()
        .and_then(|v| v["error"]["kind"].as_str().map(str::to_owned))
        .unwrap_or_default()
}
fn ok(root: &Path, args: &[&str]) -> Value {
    let out = run(root, args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}
fn refused(root: &Path, args: &[&str], kind: &str) {
    let out = run(root, args);
    assert!(!out.status.success(), "unexpected success: {args:?}");
    assert_eq!(
        error_kind(&out),
        kind,
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn fixture() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_synck"))
        .args(["init", temp.path().to_str().unwrap(), "--author", "tests"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    temp
}
fn acquire(root: &Path, paths: &[&str], owner: &str) -> String {
    let mut args = vec!["lock", "acquire", "--owner", owner];
    args.extend_from_slice(paths);
    ok(root, &args)["result"]["token"]
        .as_str()
        .unwrap()
        .to_owned()
}
fn create(root: &Path, name: &str, source: &Path, token: Option<&str>) -> String {
    let mut args = vec![
        "repository",
        "create-file",
        name,
        source.to_str().unwrap(),
        "--path",
        ".",
    ];
    if let Some(t) = token {
        args.extend(["--lock-token", t]);
    }
    ok(root, &args)["item_id"].as_str().unwrap().into()
}
fn expire(root: &Path, token: &str) {
    rusqlite::Connection::open(root.join(".synck/agent-locks.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE reservations SET expires_at=1 WHERE token=?1",
            [token],
        )
        .unwrap();
}
fn race(root: &Path, first: Vec<String>, second: Vec<String>) -> [Output; 2] {
    let barrier = Arc::new(Barrier::new(2));
    let handles = [first, second].map(|args| {
        let root = root.to_path_buf();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            run(&root, &args.iter().map(String::as_str).collect::<Vec<_>>())
        })
    });
    handles.map(|h| h.join().unwrap())
}
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

#[test]
fn parallel_new_path_acquisition_has_one_owner_and_stale_tokens_are_fenced() {
    let temp = fixture();
    let root = temp.path();
    let outputs = race(
        root,
        args(&["lock", "acquire", "new.txt", "--owner", "agent-a"]),
        args(&["lock", "acquire", "new.txt", "--owner", "agent-b"]),
    );
    assert_eq!(outputs.iter().filter(|o| o.status.success()).count(), 1);
    let loser = outputs.iter().find(|o| !o.status.success()).unwrap();
    assert_eq!(error_kind(loser), "LOCKED");
    let lease: Value =
        serde_json::from_slice(&outputs.iter().find(|o| o.status.success()).unwrap().stdout)
            .unwrap();
    let token = lease["result"]["token"].as_str().unwrap();
    let input = tempfile::tempdir().unwrap();
    let source = input.path().join("source.txt");
    fs::write(&source, b"first owner's work").unwrap();
    refused(
        root,
        &[
            "repository",
            "create-file",
            "new.txt",
            source.to_str().unwrap(),
            "--path",
            ".",
        ],
        "LOCKED",
    );
    assert!(!root.join("new.txt").exists());
    let item = create(root, "new.txt", &source, Some(token));
    expire(root, token);
    let replacement = acquire(root, &["new.txt"], "agent-b");
    assert_ne!(replacement, token);
    ok(root, &["lock", "release", token]);
    assert_eq!(
        ok(root, &["lock", "list"])["result"]["locks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fs::write(&source, b"stale overwrite").unwrap();
    refused(
        root,
        &[
            "--lock-token",
            token,
            "repository",
            "save-file",
            &item,
            source.to_str().unwrap(),
            "--path",
            ".",
        ],
        "LOCK_LOST",
    );
    assert_eq!(
        fs::read(root.join("new.txt")).unwrap(),
        b"first owner's work"
    );
    refused(root, &["lock", "renew", token], "LOCK_LOST");
    ok(root, &["lock", "renew", &replacement]);
}

#[test]
fn parallel_saves_preserve_the_owner_and_independent_agents_can_both_commit() {
    let temp = fixture();
    let root = temp.path();
    let inputs = tempfile::tempdir().unwrap();
    let a = inputs.path().join("a.txt");
    let b = inputs.path().join("b.txt");
    fs::write(&a, b"base").unwrap();
    fs::write(&b, b"intruder").unwrap();
    let item = create(root, "shared.txt", &a, None);
    let token = acquire(root, &["shared.txt"], "agent-a");
    fs::write(&a, b"owner's revision").unwrap();
    let out = race(
        root,
        args(&[
            "--lock-token",
            &token,
            "repository",
            "save-file",
            &item,
            a.to_str().unwrap(),
            "--path",
            ".",
        ]),
        args(&[
            "repository",
            "save-file",
            &item,
            b.to_str().unwrap(),
            "--path",
            ".",
        ]),
    );
    assert!(
        out[0].status.success(),
        "{}",
        String::from_utf8_lossy(&out[0].stderr)
    );
    assert_eq!(error_kind(&out[1]), "LOCKED");
    assert_eq!(
        fs::read(root.join("shared.txt")).unwrap(),
        b"owner's revision"
    );
    assert_eq!(fs::read(&b).unwrap(), b"intruder");
    let other = create(root, "independent.txt", &b, None);
    let other_token = acquire(root, &["independent.txt"], "agent-b");
    fs::write(&a, b"A second").unwrap();
    fs::write(&b, b"B second").unwrap();
    let out = race(
        root,
        args(&[
            "--lock-token",
            &token,
            "repository",
            "save-file",
            &item,
            a.to_str().unwrap(),
            "--path",
            ".",
        ]),
        args(&[
            "--lock-token",
            &other_token,
            "repository",
            "save-file",
            &other,
            b.to_str().unwrap(),
            "--path",
            ".",
        ]),
    );
    for out in out {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(fs::read(root.join("shared.txt")).unwrap(), b"A second");
    assert_eq!(fs::read(root.join("independent.txt")).unwrap(), b"B second");
}

#[test]
fn dirty_expired_work_survives_takeover_restore_and_commit_attempts() {
    let temp = fixture();
    let root = temp.path();
    let token = acquire(root, &["new.txt"], "agent-a");
    fs::write(root.join("new.txt"), b"unfinished private work").unwrap();
    expire(root, &token);
    refused(
        root,
        &["lock", "acquire", "new.txt", "--owner", "agent-b"],
        "INVALID",
    );
    refused(root, &["commit", "-m", "steal", "new.txt"], "LOCKED");
    // restore-tree does not support JSON, but must still refuse before deleting.
    let out = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(root)
        .arg("restore-tree")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        fs::read(root.join("new.txt")).unwrap(),
        b"unfinished private work"
    );
    ok(root, &["lock", "break", "new.txt"]);
    assert_eq!(
        fs::read(root.join("new.txt")).unwrap(),
        b"unfinished private work"
    );
    refused(
        root,
        &["--lock-token", &token, "commit", "-m", "stale", "new.txt"],
        "LOCK_LOST",
    );
    ok(root, &["commit", "-m", "recover reviewed work", "new.txt"]);
}

#[test]
fn new_nested_paths_case_aliases_and_atomic_acquisition() {
    let temp = fixture();
    let root = temp.path();
    let token = acquire(root, &["new-dir/file.txt"], "agent-a");
    refused(
        root,
        &["lock", "acquire", "NEW-DIR/FILE.TXT", "--owner", "b"],
        "LOCKED",
    );
    refused(
        root,
        &[
            "lock",
            "acquire",
            "other.txt",
            "new-dir/file.txt",
            "--owner",
            "b",
        ],
        "LOCKED",
    );
    assert_eq!(
        ok(root, &["lock", "list"])["result"]["locks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fs::create_dir(root.join("new-dir")).unwrap();
    fs::write(root.join("new-dir/file.txt"), b"new nested file").unwrap();
    refused(root, &["commit", "-m", "other agent", "new-dir"], "LOCKED");
    ok(
        root,
        &["--lock-token", &token, "commit", "-m", "owner", "new-dir"],
    );
    let other = acquire(root, &["other.txt"], "b");
    fs::write(root.join("other.txt"), b"B work").unwrap();
    refused(
        root,
        &["--lock-token", &token, "commit", "-m", "whole tree"],
        "LOCKED",
    );
    ok(
        root,
        &[
            "--lock-token",
            &other,
            "commit",
            "-m",
            "B only",
            "other.txt",
        ],
    );
    let listed = ok(root, &["lock", "list"]);
    assert!(!listed.to_string().contains(&token));
    refused(
        root,
        &["lock", "acquire", ".synck/config.json", "--owner", "b"],
        "INVALID",
    );
    refused(
        root,
        &["lock", "acquire", "../outside", "--owner", "b"],
        "INVALID",
    );
}

#[tokio::test]
async fn library_edits_restore_and_parent_moves_enforce_reservations() {
    let temp = fixture();
    let root = temp.path();
    fs::create_dir(root.join("dir")).unwrap();
    fs::write(root.join("dir/file.txt"), b"base").unwrap();
    ok(root, &["commit", "-m", "initial"]);
    let token = acquire(root, &["dir/file.txt", "absent.txt"], "agent");
    let mut repo = synck_local::Repository::discover(root).await.unwrap();
    let entries = repo.working_tree().await.unwrap();
    let dir = entries.iter().find(|e| e.path == "dir").unwrap();
    assert!(matches!(
        repo.delete_working_entry(dir).await,
        Err(synck_local::LocalError::Locked { .. })
    ));
    assert!(matches!(
        repo.rename_working_entry(dir, "moved").await,
        Err(synck_local::LocalError::Locked { .. })
    ));
    assert!(matches!(
        repo.create_working_file(None, "absent.txt").await,
        Err(synck_local::LocalError::Locked { .. })
    ));
    assert!(matches!(
        repo.restore(root, Path::new("dir/file.txt"), None).await,
        Err(synck_local::LocalError::Locked { .. })
    ));
    assert!(matches!(
        repo.revert("HEAD", "undo").await,
        Err(synck_local::LocalError::Locked { .. })
    ));
    repo.set_agent_lock_tokens(&[token]).unwrap();
    repo.restore(root, Path::new("dir/file.txt"), None)
        .await
        .unwrap();
    repo.create_working_file(None, "absent.txt").await.unwrap();
}

#[test]
fn coordination_survives_cache_rebuild_and_is_excluded_from_archives() {
    let temp = fixture();
    let root = temp.path();
    let token = acquire(root, &["new.txt"], "agent");
    fs::remove_file(root.join(".synck/repository.local.sqlite3")).unwrap();
    assert_eq!(
        ok(root, &["lock", "list"])["result"]["locks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let output = tempfile::tempdir().unwrap();
    let archive = output.path().join("archive");
    let out = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(root)
        .args(["archive", archive.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!archive.join(".synck/agent-locks.sqlite3").exists());
    ok(root, &["lock", "release", &token]);
    fs::write(root.join(".synck/agent-locks.sqlite3"), b"corrupt database").unwrap();
    fs::write(root.join("new.txt"), b"must not commit").unwrap();
    let out = run(root, &["commit", "-m", "refuse"]);
    assert!(!out.status.success());
    assert_eq!(fs::read(root.join("new.txt")).unwrap(), b"must not commit");
}

#[cfg(unix)]
#[test]
fn reservation_paths_and_database_must_not_follow_symlinks() {
    let temp = fixture();
    let root = temp.path();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
    refused(
        root,
        &["lock", "acquire", "link/new.txt", "--owner", "agent"],
        "INVALID",
    );
    fs::remove_file(root.join("link")).unwrap();
    fs::write(outside.path().join("db"), b"untouched").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("db"),
        root.join(".synck/agent-locks.sqlite3"),
    )
    .unwrap();
    refused(
        root,
        &["lock", "acquire", "new.txt", "--owner", "agent"],
        "INTEGRITY",
    );
    assert_eq!(fs::read(outside.path().join("db")).unwrap(), b"untouched");
}

#[test]
fn sibling_new_files_can_commit_without_claiming_each_others_parent() {
    let temp = fixture();
    let root = temp.path();
    let a = acquire(root, &["folder/a.txt"], "A");
    let b = acquire(root, &["folder/b.txt"], "B");
    fs::create_dir(root.join("folder")).unwrap();
    fs::write(root.join("folder/a.txt"), b"A").unwrap();
    fs::write(root.join("folder/b.txt"), b"B").unwrap();
    let out = race(
        root,
        args(&["--lock-token", &a, "commit", "-m", "A", "folder/a.txt"]),
        args(&["--lock-token", &b, "commit", "-m", "B", "folder/b.txt"]),
    );
    for out in out {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(fs::read(root.join("folder/a.txt")).unwrap(), b"A");
    assert_eq!(fs::read(root.join("folder/b.txt")).unwrap(), b"B");
}

#[test]
fn exports_and_archives_cannot_fill_another_agents_reserved_destination() {
    let temp = fixture();
    let root = temp.path();
    let source = tempfile::NamedTempFile::new().unwrap();
    fs::write(source.path(), b"source").unwrap();
    let item = create(root, "file.txt", source.path(), None);
    let token = acquire(
        root,
        &["reserved.txt", "reserved-tree", "reserved-copy"],
        "A",
    );
    for arguments in [
        vec!["export", "file.txt", "reserved.txt"],
        vec!["export-tree", "HEAD", "reserved-tree"],
        vec!["archive", "reserved-copy"],
        vec!["item", "export", &item, "reserved.txt", "--path", "."],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_synck"))
            .current_dir(root)
            .args(&arguments)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{arguments:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("reserved by A"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    for name in ["reserved.txt", "reserved-tree", "reserved-copy"] {
        assert!(!root.join(name).exists());
    }
    ok(
        root,
        &[
            "--lock-token",
            &token,
            "item",
            "export",
            &item,
            "reserved.txt",
            "--path",
            ".",
        ],
    );
    assert_eq!(fs::read(root.join("reserved.txt")).unwrap(), b"source");
}

#[test]
fn folder_takeover_refuses_ignored_unfinished_files() {
    let temp = fixture();
    let root = temp.path();
    fs::create_dir(root.join("folder")).unwrap();
    fs::write(root.join("folder/tracked.txt"), b"original").unwrap();
    ok(root, &["commit", "-m", "initial"]);
    let token = acquire(root, &["folder"], "A");
    fs::write(root.join("folder/.unfinished"), b"private work").unwrap();
    expire(root, &token);
    // Taking a clean child must not release the folder's dirty siblings.
    refused(
        root,
        &["lock", "acquire", "folder/tracked.txt", "--owner", "B"],
        "LOCKED",
    );
    refused(
        root,
        &["lock", "acquire", "folder", "--owner", "B"],
        "INVALID",
    );
    assert_eq!(
        fs::read(root.join("folder/.unfinished")).unwrap(),
        b"private work"
    );
}
