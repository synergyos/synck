//! A repository built from the language-neutral vector and read as any other.
use serde_json::Value;
use std::path::Path;
use std::process::Command;

const VECTOR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/spec/vectors/repository");
const SCHEMA: &str = include_str!("../../../docs/spec/history.sql");

fn build(root: &Path) {
    let control = root.join(".synck");
    std::fs::create_dir_all(control.join("objects")).unwrap();
    std::fs::write(control.join("lock"), "").unwrap();
    let vector = Path::new(VECTOR);
    std::fs::copy(vector.join("config.json"), control.join("config.json")).unwrap();
    for entry in std::fs::read_dir(vector.join("objects")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), control.join("objects").join(entry.file_name())).unwrap();
    }
    let database = rusqlite::Connection::open(control.join("repository.sqlite3")).unwrap();
    database
        .execute_batch(
            "PRAGMA application_id = 1398361675; PRAGMA user_version = 7; PRAGMA journal_mode = WAL;",
        )
        .unwrap();
    database.execute_batch(SCHEMA).unwrap();
    database
        .execute_batch(&std::fs::read_to_string(vector.join("rows.sql")).unwrap())
        .unwrap();
}

fn synck(root: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(root)
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn repository_vector_verifies_and_reads() {
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(Path::new(VECTOR).join("expected.json")).unwrap())
            .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repository");
    std::fs::create_dir(&root).unwrap();
    build(&root);

    let commits = expected["commits"].as_array().unwrap();
    let last = commits.last().unwrap();
    let report = synck(&root, &["repository", "verify", "."]);
    assert_eq!(report["terminal_repo_seq"], last["repo_seq"], "{report}");
    assert_eq!(report["state_digest"], last["state_digest"], "{report}");
    assert_eq!(report["missing_objects"], serde_json::json!([]), "{report}");
    assert_eq!(report["missing_metadata"], serde_json::json!([]), "{report}");

    let log = synck(&root, &["repository", "log", "."]);
    let listed: Vec<_> = log["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|commit| commit["commit_id"].clone())
        .collect();
    let mut wanted: Vec<_> = commits.iter().map(|c| c["commit_id"].clone()).collect();
    wanted.reverse();
    assert_eq!(listed, wanted);

    synck(&root, &["repository", "checkout", ".", "--author", "reader"]);
    assert_eq!(std::fs::read(root.join("notes.txt")).unwrap(), b"two\n");
    assert!(root.join("docs").is_dir());
    assert!(!root.join("scratch.txt").exists());
}
