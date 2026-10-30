//! Kill a real `synck` at every filesystem mutation of an operation, then check
//! that the repository verifies and that repeating the operation finishes it.
//!
//! This covers process death, where the kernel keeps every completed write. It
//! does not model power loss. Run with:
//! `cargo test -p synck-local --test crash_points -- --ignored`.
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

const SIGKILL: i32 = 9;

fn interposer() -> &'static Path {
    static LIBRARY: OnceLock<PathBuf> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        let output = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("libcrash_points");
        let status = Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string()))
            .args(["--edition=2024", "--crate-type=cdylib", "-Cpanic=abort", "-o"])
            .arg(&output)
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/crash_points/interposer.rs"))
            .status()
            .unwrap();
        assert!(status.success());
        output
    })
}

fn synck(root: &Path, args: &[&str], crash_at: Option<usize>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_synck"));
    command.current_dir(root).args(args);
    if let Some(point) = crash_at {
        let variable = if cfg!(target_os = "macos") { "DYLD_INSERT_LIBRARIES" } else { "LD_PRELOAD" };
        command.env(variable, interposer()).env("SYNCK_CRASH_AT", point.to_string());
    }
    command.output().unwrap()
}

fn ok(root: &Path, args: &[&str]) -> String {
    let output = synck(root, args, None);
    assert!(
        output.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn commit_count(root: &Path) -> usize {
    let log = ok(root, &["log", "--json", "--limit", "1000"]);
    serde_json::from_str::<serde_json::Value>(&log).unwrap()["commits"].as_array().unwrap().len()
}

/// Every working file, outside repository metadata.
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().unwrap() == ".synck" {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(path.strip_prefix(root).unwrap().to_path_buf(), fs::read(&path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn copy(from: &Path, to: &Path) {
    let status = Command::new("cp").arg("-Rp").arg(from).arg(to).status().unwrap();
    assert!(status.success());
}

/// A repository with history, ready for the operation under test.
fn base(root: &Path) {
    fs::create_dir(root).unwrap();
    ok(root, &["init", "--author", "tester", "--storage-mode", "WHOLE_FILE"]);
    fs::create_dir(root.join("folder")).unwrap();
    fs::write(root.join("folder/keep.txt"), "keep\n").unwrap();
    fs::write(root.join("edit.txt"), "before\n").unwrap();
    fs::write(root.join("remove.txt"), "remove\n").unwrap();
    ok(root, &["commit", "-m", "first"]);
    fs::write(root.join("edit.txt"), "second\n").unwrap();
    ok(root, &["commit", "-m", "second"]);
}

fn scenario(prepare: impl Fn(&Path), args: &[&str]) {
    scenario_with(
        |root| {
            prepare(root);
            args.iter().map(ToString::to_string).collect()
        },
    );
}

fn scenario_with(prepare: impl Fn(&Path) -> Vec<String>) {
    let directory = tempfile::tempdir().unwrap();
    let prepared = directory.path().join("prepared");
    base(&prepared);
    let args = prepare(&prepared);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let args = args.as_slice();
    let commits_before = commit_count(&prepared);

    let finished = directory.path().join("finished");
    copy(&prepared, &finished);
    ok(&finished, args);
    let expected_commits = commit_count(&finished);
    let expected_tree = tree(&finished);
    ok(&finished, &["fsck"]);

    let mut point = 1;
    loop {
        let crashed = directory.path().join("crashed");
        copy(&prepared, &crashed);
        let output = synck(&crashed, args, Some(point));
        if output.status.success() {
            assert_eq!(output.status.signal(), None);
            break;
        }
        assert_eq!(
            output.status.signal(),
            Some(SIGKILL),
            "point {point}: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let context = format!("{args:?} killed at mutation {point}");
        // Verification never repairs, so it names the recovery it needs.
        let fsck = synck(&crashed, &["fsck"], None);
        if String::from_utf8_lossy(&fsck.stderr).contains("explicit finalize is required") {
            ok(&crashed, &["repository", "finalize", "."]);
            let fsck = synck(&crashed, &["fsck"], None);
            assert!(fsck.status.success(), "{context}: {}", String::from_utf8_lossy(&fsck.stderr));
        } else {
            assert!(fsck.status.success(), "{context}: {}", String::from_utf8_lossy(&fsck.stderr));
        }
        let commits = commit_count(&crashed);
        assert!(
            commits == commits_before || commits == expected_commits,
            "{context}: {commits} commits, {commits_before} before, {expected_commits} after"
        );
        if commits == commits_before {
            let retry = synck(&crashed, args, None);
            assert!(retry.status.success(), "{context}: {}", String::from_utf8_lossy(&retry.stderr));
        }
        assert_eq!(commit_count(&crashed), expected_commits, "{context}");
        assert_eq!(tree(&crashed), expected_tree, "{context}");
        let fsck = synck(&crashed, &["fsck"], None);
        assert!(fsck.status.success(), "{context}: {}", String::from_utf8_lossy(&fsck.stderr));

        fs::remove_dir_all(&crashed).unwrap();
        point += 1;
    }
    assert!(point > 5, "{args:?} reached only {point} mutations, so the interposer is not loaded");
    eprintln!("{args:?}: {} mutations", point - 1);
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn commit_survives_a_kill_at_every_mutation() {
    scenario(
        |root| {
            fs::write(root.join("edit.txt"), "third\n").unwrap();
            fs::write(root.join("add.txt"), "add\n").unwrap();
            fs::remove_file(root.join("remove.txt")).unwrap();
        },
        &["commit", "-m", "third"],
    );
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn move_survives_a_kill_at_every_mutation() {
    scenario(|_| {}, &["mv", "-m", "moved", "edit.txt", "folder/moved.txt"]);
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn revert_survives_a_kill_at_every_mutation() {
    scenario(|_| {}, &["revert", "-m", "undone", "HEAD"]);
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn restore_tree_survives_a_kill_at_every_mutation() {
    scenario(
        |root| {
            fs::write(root.join("edit.txt"), "discarded\n").unwrap();
            fs::remove_file(root.join("remove.txt")).unwrap();
        },
        &["restore-tree"],
    );
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn storage_optimize_survives_a_kill_at_every_mutation() {
    scenario(|_| {}, &["storage", "optimize"]);
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn gc_survives_a_kill_at_every_mutation() {
    scenario(|_| {}, &["gc"]);
}

/// The first argument to run, against a copy of a repository, until it stops being
/// killed. `check` sees the killed copy and decides what must hold.
fn each_kill(root: &Path, run: &[&str], check: impl Fn(&Path, &str)) {
    let mut point = 1;
    loop {
        let parent = tempfile::tempdir().unwrap();
        let crashed = parent.path().join("crashed");
        copy(root, &crashed);
        let output = synck(&crashed, run, Some(point));
        if output.status.success() {
            break;
        }
        assert_eq!(
            output.status.signal(),
            Some(SIGKILL),
            "point {point}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        check(&crashed, &format!("{run:?} killed at mutation {point}"));
        point += 1;
    }
    assert!(point > 5, "{run:?} reached only {point} mutations, so the interposer is not loaded");
    eprintln!("{run:?}: {} mutations", point - 1);
}

fn succeeds(root: &Path, args: &[&str], context: &str) {
    let output = synck(root, args, None);
    assert!(
        output.status.success(),
        "{context}: {args:?}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A copy is published whole or not at all, and the source is untouched.
fn copy_survives(args: &[&str]) {
    let directory = tempfile::tempdir().unwrap();
    let prepared = directory.path().join("prepared");
    base(&prepared);
    let commits = commit_count(&prepared);
    each_kill(&prepared, args, |crashed, context| {
        succeeds(crashed, &["fsck"], context);
        assert_eq!(commit_count(crashed), commits, "{context}");
        let destination = crashed.join("../copy");
        if !destination.exists() {
            succeeds(crashed, args, context);
        }
        succeeds(crashed, &["repository", "verify", "../copy"], context);
    });
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn archive_survives_a_kill_at_every_mutation() {
    copy_survives(&["archive", "../copy"]);
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn import_survives_a_kill_at_every_mutation() {
    copy_survives(&["repository", "import", ".", "../copy"]);
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn init_survives_a_kill_at_every_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let empty = directory.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let args = ["init", "--author", "tester"];
    each_kill(&empty, &args, |crashed, context| {
        if !crashed.join(".synck").exists() || !synck(crashed, &["fsck"], None).status.success() {
            succeeds(crashed, &args, context);
        }
        succeeds(crashed, &["fsck"], context);
        assert_eq!(commit_count(crashed), 1, "{context}");
    });
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn checkout_survives_a_kill_at_every_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    base(&source);
    let expected = tree(&source);
    let follower = directory.path().join("follower");
    ok(&source, &["repository", "import", ".", follower.to_str().unwrap()]);
    let args = ["repository", "checkout", ".", "--author", "reader"];
    each_kill(&follower, &args, |crashed, context| {
        succeeds(crashed, &args, context);
        assert_eq!(tree(crashed), expected, "{context}");
        succeeds(crashed, &["fsck"], context);
    });
}

const OPERATION: &str = "019a2900-0000-4000-8000-000000000001";

#[test]
#[ignore = "kills a real process at every mutation"]
fn create_file_survives_a_kill_at_every_mutation() {
    scenario(
        |root| fs::write(root.join("../source.txt"), "new\n").unwrap(),
        &["repository", "create-file", "new.txt", "../source.txt", "-m", "add", "--operation-id", OPERATION],
    );
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn save_file_survives_a_kill_at_every_mutation() {
    scenario_with(|root| {
        fs::write(root.join("../source.txt"), "saved\n").unwrap();
        let listed: serde_json::Value = serde_json::from_str(&ok(root, &["item", "list"])).unwrap();
        let entry = listed["items"].as_array().unwrap().iter().find(|i| i["name"] == "edit.txt");
        let item = entry.unwrap()["item_id"].as_str().unwrap().to_string();
        ["repository", "save-file", &item, "../source.txt", "-m", "save", "--operation-id", OPERATION]
            .map(str::to_string)
            .to_vec()
    });
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn import_email_survives_a_kill_at_every_mutation() {
    scenario(
        |root| {
            let email = "From: a@example.com\r\nTo: b@example.com\r\nSubject: Hello\r\n\r\nBody\r\n";
            fs::write(root.join("../message.eml"), email).unwrap();
        },
        &["repository", "import-email", "../message.eml", "-m", "mail"],
    );
}
