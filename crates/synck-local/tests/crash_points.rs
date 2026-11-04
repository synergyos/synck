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
    base_in("WHOLE_FILE", root);
}

fn base_in(mode: &str, root: &Path) {
    fs::create_dir(root).unwrap();
    ok(root, &["init", "--author", "tester", "--storage-mode", mode]);
    fs::create_dir(root.join("folder")).unwrap();
    fs::write(root.join("folder/keep.txt"), "keep\n").unwrap();
    fs::write(root.join("edit.txt"), "before\n").unwrap();
    fs::write(root.join("remove.txt"), "remove\n").unwrap();
    ok(root, &["commit", "-m", "first"]);
    fs::write(root.join("edit.txt"), "second\n").unwrap();
    ok(root, &["commit", "-m", "second"]);
}

/// Whatever a kill left, the repository can collect, and nothing stays behind.
fn assert_clean_metadata(root: &Path, context: &str) {
    let gc = synck(root, &["gc"], None);
    assert!(gc.status.success(), "{context}: {}", String::from_utf8_lossy(&gc.stderr));
    let names = |path: &str| -> Vec<String> {
        fs::read_dir(root.join(path))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect()
    };
    assert_eq!(names(".synck/tmp"), Vec::<String>::new(), "{context}");
    let stray = names(".synck")
        .into_iter()
        .filter(|name| name.ends_with(".json") && name != "config.json" && name != "checkout.json")
        .collect::<Vec<_>>();
    assert_eq!(stray, Vec::<String>::new(), "{context}");
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
    scenario_in("WHOLE_FILE", prepare);
}

fn scenario_in(mode: &str, prepare: impl Fn(&Path) -> Vec<String>) {
    let directory = tempfile::tempdir().unwrap();
    let prepared = directory.path().join("prepared");
    base_in(mode, &prepared);
    let args = prepare(&prepared);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let args = args.as_slice();
    let commits_before = commit_count(&prepared);

    let finished = directory.path().join("finished");
    copy(&prepared, &finished);
    ok(&finished, args);
    let expected_commits = commit_count(&finished);
    let expected_tree = tree(&finished);
    let expected_status = ok(&finished, &["status", "--json"]);
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
        // The working tree is based on the commit it made, not left behind it.
        assert_eq!(ok(&crashed, &["status", "--json"]), expected_status, "{context}");
        let fsck = synck(&crashed, &["fsck"], None);
        assert!(fsck.status.success(), "{context}: {}", String::from_utf8_lossy(&fsck.stderr));
        assert_clean_metadata(&crashed, &context);

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

/// A writer reads checkout state, so a kill must not leave one under its old author.
#[test]
#[ignore = "kills a real process at every mutation"]
fn checkout_of_a_writer_survives_a_kill_at_every_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    base(&source);
    let expected = tree(&source);
    let writer = directory.path().join("writer");
    ok(&source, &["repository", "import", ".", writer.to_str().unwrap()]);
    ok(&writer, &["repository", "activate", ".", "--author", "before", "--source-retired"]);
    let args = ["repository", "checkout", ".", "--author", "reader"];
    each_kill(&writer, &args, |crashed, context| {
        succeeds(crashed, &args, context);
        assert_eq!(tree(crashed), expected, "{context}");
        succeeds(crashed, &["fsck"], context);
    });
}

/// A kill before enabling commits leaves a follower, and activating that makes a writer
/// with nothing of the abandoned checkout.
#[test]
#[ignore = "kills a real process at every mutation"]
fn activating_after_a_killed_checkout_leaves_a_working_repository() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    base(&source);
    let follower = directory.path().join("follower");
    ok(&source, &["repository", "import", ".", follower.to_str().unwrap()]);
    let args = ["repository", "checkout", ".", "--author", "reader"];
    each_kill(&follower, &args, |crashed, context| {
        succeeds(crashed, &["repository", "activate", ".", "--author", "writer", "--source-retired"], context);
        succeeds(crashed, &["repository", "verify", "."], context);
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

/// A kill can land after acceptance and before sequencing, and a caller is told to
/// repeat the identical call: the replay alone must leave a verifying repository.
#[test]
#[ignore = "kills a real process at every mutation"]
fn replaying_an_operation_finishes_a_killed_acceptance() {
    let directory = tempfile::tempdir().unwrap();
    let prepared = directory.path().join("prepared");
    base(&prepared);
    let source = directory.path().join("source.txt");
    fs::write(&source, "new\n").unwrap();
    let args = ["repository", "create-file", "new.txt", source.to_str().unwrap(), "-m", "add", "--operation-id", OPERATION];
    each_kill(&prepared, &args, |crashed, context| {
        succeeds(crashed, &args, context);
        succeeds(crashed, &["fsck"], context);
    });
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn replaying_a_save_finishes_a_killed_acceptance() {
    let directory = tempfile::tempdir().unwrap();
    let prepared = directory.path().join("prepared");
    base(&prepared);
    let source = directory.path().join("source.txt");
    fs::write(&source, "saved\n").unwrap();
    let listed: serde_json::Value = serde_json::from_str(&ok(&prepared, &["item", "list"])).unwrap();
    let entry = listed["items"].as_array().unwrap().iter().find(|i| i["name"] == "edit.txt");
    let item = entry.unwrap()["item_id"].as_str().unwrap().to_string();
    let args = ["repository", "save-file", &item, source.to_str().unwrap(), "-m", "save", "--operation-id", OPERATION];
    each_kill(&prepared, &args, |crashed, context| {
        succeeds(crashed, &args, context);
        succeeds(crashed, &["fsck"], context);
    });
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn replaying_an_email_import_finishes_a_killed_acceptance() {
    let directory = tempfile::tempdir().unwrap();
    let prepared = directory.path().join("prepared");
    base(&prepared);
    let source = directory.path().join("message.eml");
    fs::write(&source, "From: a@example.com\r\nTo: b@example.com\r\nSubject: Hello\r\n\r\nBody\r\n").unwrap();
    let args = ["repository", "import-email", source.to_str().unwrap(), "-m", "mail", "--operation-id", OPERATION];
    each_kill(&prepared, &args, |crashed, context| {
        succeeds(crashed, &args, context);
        succeeds(crashed, &["fsck"], context);
    });
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

/// Bytes that split into several chunks, so a kill can land between them.
fn chunked_content() -> Vec<u8> {
    let mut state = 1u32;
    (0..300 * 1024)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn chunked_commit_survives_a_kill_at_every_mutation() {
    scenario_in("CHUNKED", |root| {
        fs::write(root.join("large.bin"), chunked_content()).unwrap();
        fs::write(root.join("edit.txt"), "third\n").unwrap();
        ["commit", "-m", "third"].map(str::to_string).to_vec()
    });
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn chunked_create_file_survives_a_kill_at_every_mutation() {
    scenario_in("CHUNKED", |root| {
        fs::write(root.join("../source.bin"), chunked_content()).unwrap();
        ["repository", "create-file", "large.bin", "../source.bin", "-m", "add", "--operation-id", OPERATION]
            .map(str::to_string)
            .to_vec()
    });
}

#[test]
#[ignore = "kills a real process at every mutation"]
fn chunked_gc_survives_a_kill_at_every_mutation() {
    scenario_in("CHUNKED", |root| {
        fs::write(root.join("large.bin"), chunked_content()).unwrap();
        ok(root, &["commit", "-m", "large"]);
        ["gc"].map(str::to_string).to_vec()
    });
}

/// A held commit to a file the origin changed too. Wherever a kill stops the
/// pull that takes it back into the working files, the clone verifies and
/// the next pull ends where an uninterrupted one does.
#[test]
#[ignore = "kills a real process at every mutation"]
fn held_pull_survives_a_kill_at_every_mutation() {
    let directory = tempfile::tempdir().unwrap();
    // A clone names its origin by absolute path, so every run puts the pair
    // back in the same place.
    let work = directory.path().join("work");
    let saved = directory.path().join("saved");
    let (origin, held) = (work.join("origin"), work.join("held"));
    fs::create_dir(&work).unwrap();
    base(&origin);
    ok(&work, &["clone", "origin", "held", "--hold"]);
    fs::write(held.join("edit.txt"), "held\n").unwrap();
    ok(&held, &["commit", "-m", "held"]);
    fs::write(origin.join("edit.txt"), "origin\n").unwrap();
    ok(&origin, &["commit", "-m", "origin"]);
    copy(&work, &saved);
    ok(&held, &["pull"]);
    let expected = tree(&held);

    let mut point = 1;
    loop {
        fs::remove_dir_all(&work).unwrap();
        copy(&saved, &work);
        let output = synck(&held, &["pull"], Some(point));
        if output.status.success() {
            break;
        }
        let context = format!("held pull killed at mutation {point}");
        assert_eq!(output.status.signal(), Some(SIGKILL), "{context}: {}", String::from_utf8_lossy(&output.stderr));
        let fsck = synck(&held, &["fsck"], None);
        assert!(fsck.status.success(), "{context}: {}", String::from_utf8_lossy(&fsck.stderr));
        ok(&held, &["pull"]);
        assert_eq!(tree(&held), expected, "{context}");
        let status: serde_json::Value = serde_json::from_str(&ok(&held, &["status", "--json"])).unwrap();
        assert_eq!(status["held_commits"], serde_json::json!([]), "{context}");
        assert_eq!(status["conflicts"][0]["path"], "edit.txt", "{context}");
        point += 1;
    }
    assert!(point > 5, "the pull reached only {point} mutations, so the interposer is not loaded");
    eprintln!("held pull: {} mutations", point - 1);
}
