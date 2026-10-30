use std::fs;
use std::process::Command;

#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

const CLEAN_STATUS_OUTPUT: &[u8] = b"nothing to commit, working tree clean\n";

#[test]
fn long_paths_survive_commit_rename_and_checkout() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("source");
    let initialized = synck(temporary.path(), &["init", root.to_str().unwrap(), "--author", "author"]);
    assert!(initialized.status.success(), "{:?}", initialized);
    let relative = (0..4).map(|i| format!("{i}{}", "a".repeat(63))).collect::<Vec<_>>().join("/");
    fs::create_dir_all(root.join(&relative)).unwrap();
    fs::write(root.join(&relative).join("before.txt"), b"long path bytes").unwrap();
    let committed = synck(&root, &["commit", "-m", "Long path"]);
    assert!(committed.status.success(), "{:?}", committed);
    fs::rename(root.join(&relative).join("before.txt"), root.join(&relative).join("after.txt")).unwrap();
    let committed = synck(&root, &["commit", "-m", "Rename long path"]);
    assert!(committed.status.success(), "{:?}", committed);
    let status = synck(&root, &["status"]);
    assert!(status.status.success(), "{:?}", status);
    assert_eq!(status.stdout, CLEAN_STATUS_OUTPUT);
    if cfg!(windows) {
        assert!(String::from_utf8_lossy(&status.stderr).contains("after.txt"));
        assert!(String::from_utf8_lossy(&status.stderr).contains("260 UTF-16"));
    } else {
        assert!(status.stderr.is_empty());
    }
    let archive = temporary.path().join("archive");
    let copy = temporary.path().join("copy");
    for (cwd, args) in [
        (root.as_path(), vec!["archive", archive.to_str().unwrap()]),
        (temporary.path(), vec!["repository", "import", archive.to_str().unwrap(), copy.to_str().unwrap()]),
    ] {
        let output = synck(cwd, &args);
        assert!(output.status.success(), "{:?}", output);
    }
    let checkout = synck(&copy, &["repository", "checkout", ".", "--author", "author"]);
    assert!(checkout.status.success(), "{:?}", checkout);
    assert_eq!(fs::read(copy.join(&relative).join("after.txt")).unwrap(), b"long path bytes");
    assert!(!copy.join(&relative).join("before.txt").exists());
    if cfg!(windows) {
        assert!(String::from_utf8_lossy(&checkout.stderr).contains("after.txt"));
    }
}

fn synck(root: &std::path::Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(root)
        .args(arguments)
        .output()
        .unwrap()
}

fn json_stdout(root: &std::path::Path, arguments: &[&str]) -> Value {
    let output = synck(root, arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_clean_status(root: &std::path::Path) {
    let output = synck(root, &["status"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout, CLEAN_STATUS_OUTPUT);
}

fn assert_keys(value: &Value, expected: &[&str]) {
    let object = value.as_object().unwrap();
    let mut actual = object.keys().map(String::as_str).collect::<Vec<_>>();
    let mut expected = expected.to_vec();
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected);
}

fn committed_repository() -> tempfile::TempDir {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join("file.txt"), b"content").unwrap();
    assert!(
        synck(temporary.path(), &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(
        synck(temporary.path(), &["commit", "-m", "initial"])
            .status
            .success()
    );
    temporary
}

#[test]
fn cli_status_reports_a_clean_working_tree() {
    let temporary = tempfile::tempdir().unwrap();
    let initialized = synck(temporary.path(), &["init", "--author", "author"]);
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );

    assert_clean_status(temporary.path());

    let json = json_stdout(temporary.path(), &["status", "--json"]);
    assert_eq!(json["changes"], serde_json::json!([]));
}

#[test]
fn cli_instructions_teach_the_complete_safe_workflow_without_a_repository() {
    let temporary = tempfile::tempdir().unwrap();
    let output = synck(temporary.path(), &["instructions"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());

    let instructions = String::from_utf8(output.stdout).unwrap();
    assert_eq!(instructions, include_str!("../src/instructions.txt"));
    assert!(instructions.contains("synck init [DIR] --author"));
    assert!(!instructions.contains("--batch"));
    assert!(instructions.contains("synck config set-hidden-files track"));
    assert!(instructions.contains("synck recover <path-or-item-id>"));
    assert!(instructions.contains("Never edit anything inside .synck"));
    let help = synck(temporary.path(), &["--help"]);
    assert!(help.status.success());
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("instructions")
    );

    let unsupported = synck(temporary.path(), &["instructions", "--json"]);
    assert!(!unsupported.status.success());
    assert!(unsupported.stdout.is_empty());
    let error: Value = serde_json::from_slice(&unsupported.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "INVALID");
    assert_eq!(
        error["error"]["message"],
        "--json is not supported for this command"
    );
}

#[cfg(unix)]
fn accept_monitor_connection(listener: &UnixListener) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "monitor client did not connect");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot accept monitor client: {error}"),
        }
    }
}

#[cfg(unix)]
fn read_monitor_request(stream: &mut UnixStream) -> Value {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).unwrap();
    let mut payload = vec![0_u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut payload).unwrap();
    serde_json::from_slice(&payload).unwrap()
}

#[cfg(unix)]
fn write_monitor_response(stream: &mut UnixStream, response: Value) {
    let payload = serde_json::to_vec(&response).unwrap();
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&payload).unwrap();
}

#[cfg(unix)]
fn wait_for_monitor_socket_removal(root: &std::path::Path) {
    let socket = root.join(".synck/monitor.sock");
    let deadline = Instant::now() + Duration::from_secs(2);
    while socket.exists() {
        assert!(
            Instant::now() < deadline,
            "monitor socket was not removed after stop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn cli_complete_checkout_monitor_preserves_sparse_boundaries() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    assert!(
        synck(root, &["repository", "init", ".", "--author", "author"])
            .status
            .success()
    );
    fs::create_dir(root.join("selected")).unwrap();
    fs::write(root.join("selected/file.txt"), b"one").unwrap();
    fs::write(root.join("other.txt"), b"other").unwrap();
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());
    assert_eq!(
        json_stdout(root, &["monitor", "status", "--json"])["status"],
        "running"
    );
    assert!(
        synck(
            root,
            &[
                "repository",
                "checkout-selection",
                ".",
                "--include",
                "selected/file.txt"
            ]
        )
        .status
        .success()
    );
    assert_eq!(synck(root, &["status"]).stdout, CLEAN_STATUS_OUTPUT);
    assert_eq!(synck(root, &["status"]).stdout, CLEAN_STATUS_OUTPUT);
    fs::write(root.join("outside.txt"), b"unselected").unwrap();
    assert_eq!(synck(root, &["status"]).stdout, CLEAN_STATUS_OUTPUT);
    fs::write(root.join("selected/file.txt"), b"two").unwrap();
    let changed = synck(root, &["status"]);
    assert!(changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stdout).contains("M selected/file.txt"));
    assert!(synck(root, &["commit", "-m", "edit"]).status.success());
    fs::rename(root.join("selected/file.txt"), root.join("moved.txt")).unwrap();
    for command in [vec!["status"], vec!["commit", "-m", "unsafe move"]] {
        let rejected = synck(root, &command);
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("outside checkout selection"));
    }
    assert!(synck(root, &["monitor", "stop"]).status.success());
}

#[cfg(unix)]
#[test]
fn cli_monitor_tracks_immediate_external_changes_and_stops_cleanly() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    let help = String::from_utf8(synck(root, &["monitor", "--help"]).stdout).unwrap();
    assert!(help.contains("start"));
    assert!(help.contains("status"));
    assert!(help.contains("stop"));

    let initial = synck(root, &["status"]);
    assert!(initial.status.success());
    let running = json_stdout(root, &["monitor", "status", "--json"]);
    assert_eq!(running["status"], "running");
    assert!(running["instance_id"].as_str().is_some());

    fs::create_dir(root.join("Folder")).unwrap();
    fs::write(root.join("Folder/document.txt"), b"one").unwrap();
    let added = String::from_utf8(synck(root, &["status"]).stdout).unwrap();
    assert!(added.contains("A Folder\n"), "{added}");
    assert!(added.contains("A Folder/document.txt\n"), "{added}");
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());

    fs::rename(root.join("Folder"), root.join("Moved")).unwrap();
    fs::write(root.join("Moved/document.txt"), b"two").unwrap();
    let changed = String::from_utf8(synck(root, &["status"]).stdout).unwrap();
    assert!(changed.contains("RN Folder -> Moved\n"), "{changed}");
    assert!(changed.contains("M Moved/document.txt\n"), "{changed}");
    assert!(
        synck(root, &["commit", "-m", "move folder"])
            .status
            .success()
    );

    fs::rename(
        root.join("Moved/document.txt"),
        root.join("Moved/renamed.txt"),
    )
    .unwrap();
    let renamed = String::from_utf8(synck(root, &["status"]).stdout).unwrap();
    assert!(
        renamed.contains("RN Moved/document.txt -> Moved/renamed.txt\n"),
        "{renamed}"
    );
    assert!(
        synck(root, &["commit", "-m", "rename file"])
            .status
            .success()
    );

    let stopped = synck(root, &["monitor", "stop"]);
    assert!(stopped.status.success());
    assert_eq!(
        String::from_utf8(stopped.stdout).unwrap(),
        "monitor stopped\n"
    );
    assert!(synck(root, &["monitor", "start"]).status.success());
    fs::write(root.join("Moved/replacement.tmp"), b"replacement").unwrap();
    fs::rename(
        root.join("Moved/replacement.tmp"),
        root.join("Moved/renamed.txt"),
    )
    .unwrap();
    let replaced = String::from_utf8(synck(root, &["status"]).stdout).unwrap();
    assert!(replaced.contains("M Moved/renamed.txt\n"), "{replaced}");
    assert!(
        synck(root, &["commit", "-m", "atomic save"])
            .status
            .success()
    );

    fs::remove_file(root.join("Moved/renamed.txt")).unwrap();
    fs::remove_dir(root.join("Moved")).unwrap();
    let deleted = String::from_utf8(synck(root, &["status"]).stdout).unwrap();
    assert!(deleted.contains("D Moved/renamed.txt\n"), "{deleted}");
    assert!(deleted.contains("D Moved\n"), "{deleted}");
    assert!(synck(root, &["monitor", "stop"]).status.success());
    assert_eq!(
        String::from_utf8(synck(root, &["monitor", "status"]).stdout).unwrap(),
        "monitor stopped\n"
    );
}

#[cfg(windows)]
#[cfg(unix)]
#[test]
fn cli_monitor_exits_when_its_repository_is_removed() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    fs::create_dir(&root).unwrap();
    assert!(synck(&root, &["init", "--author", "author"]).status.success());
    let config: Value =
        serde_json::from_slice(&fs::read(root.join(".synck/config.json")).unwrap()).unwrap();
    let mut monitor = Command::new(env!("CARGO_BIN_EXE_synck"))
        .args(["__monitor-server", "--root"])
        .arg(&root)
        .args(["--repo-id", config["repo_id"].as_str().unwrap()])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join(".synck/monitor.sock").exists() {
        assert!(Instant::now() < deadline, "monitor did not start");
        std::thread::sleep(Duration::from_millis(10));
    }

    fs::remove_dir_all(&root).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while monitor.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            monitor.kill().unwrap();
            panic!("monitor outlived its repository");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn cli_monitor_starts_tracks_changes_and_stops_cleanly() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let initialized = synck(root, &["init", "--author", "author"]);
    let started = synck(root, &["monitor", "start"]);
    let running = synck(root, &["monitor", "status", "--json"]);
    fs::write(root.join("document.txt"), b"content").unwrap();
    let changed = synck(root, &["status"]);
    let stopped = synck(root, &["monitor", "stop"]);
    let final_status = synck(root, &["monitor", "status"]);

    for output in [
        &initialized,
        &started,
        &running,
        &changed,
        &stopped,
        &final_status,
    ] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let running: Value = serde_json::from_slice(&running.stdout).unwrap();
    assert_eq!(running["status"], "running");
    assert!(running["instance_id"].as_str().is_some());
    assert!(
        String::from_utf8(changed.stdout)
            .unwrap()
            .lines()
            .any(|line| line == "A document.txt")
    );
    assert_eq!(
        String::from_utf8(stopped.stdout).unwrap(),
        "monitor stopped\n"
    );
    assert_eq!(
        String::from_utf8(final_status.stdout).unwrap(),
        "monitor stopped\n"
    );
}

#[cfg(unix)]
#[test]
fn cli_monitor_restarts_scanning_when_ignore_rules_change() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    fs::write(root.join(".synckignore"), b"*.a\n").unwrap();
    fs::write(root.join("file.a"), b"a").unwrap();
    fs::write(root.join("file.b"), b"b").unwrap();
    fs::write(root.join("file.c"), b"c").unwrap();
    assert!(synck(root, &["status"]).status.success());
    assert!(synck(root, &["monitor", "stop"]).status.success());
    wait_for_monitor_socket_removal(root);

    let repo_id = json_stdout(root, &["info", "--json"])["repo_id"]
        .as_str()
        .unwrap()
        .to_string();
    let instance_id = "00000000-0000-4000-8000-000000000001";
    let socket = root.join(".synck/monitor.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server_root = root.to_path_buf();
    let server = std::thread::spawn(move || {
        for step in 0..6 {
            let mut stream = accept_monitor_connection(&listener);
            let request = read_monitor_request(&mut stream);
            assert_eq!(request["version"], 1);
            assert_eq!(request["repo_id"], repo_id);
            let response = match step {
                0 => {
                    assert_eq!(request["operation"], "STATUS");
                    serde_json::json!({
                        "version": 1,
                        "kind": "RUNNING",
                        "instance_id": instance_id,
                        "sequence": 0,
                        "paths": null,
                        "error": null,
                    })
                }
                1 => {
                    assert_eq!(request["operation"], "QUERY");
                    assert!(!request["instance_id"].is_null());
                    fs::write(server_root.join(".synckignore"), b"*.b\n").unwrap();
                    serde_json::json!({
                        "version": 1,
                        "kind": "CHANGES",
                        "instance_id": instance_id,
                        "sequence": 1,
                        "paths": [".synckignore"],
                        "error": null,
                    })
                }
                2 => {
                    assert_eq!(request["operation"], "QUERY");
                    assert!(request["instance_id"].is_null());
                    serde_json::json!({
                        "version": 1,
                        "kind": "RESET",
                        "instance_id": instance_id,
                        "sequence": 1,
                        "paths": null,
                        "error": null,
                    })
                }
                3 => {
                    assert_eq!(request["operation"], "QUERY");
                    assert_eq!(request["instance_id"], instance_id);
                    assert_eq!(request["sequence"], 1);
                    fs::write(server_root.join(".synckignore"), b"*.c\n").unwrap();
                    serde_json::json!({
                        "version": 1,
                        "kind": "CHANGES",
                        "instance_id": instance_id,
                        "sequence": 2,
                        "paths": [".synckignore"],
                        "error": null,
                    })
                }
                4 => {
                    assert_eq!(request["operation"], "QUERY");
                    assert!(request["instance_id"].is_null());
                    serde_json::json!({
                        "version": 1,
                        "kind": "RESET",
                        "instance_id": instance_id,
                        "sequence": 2,
                        "paths": null,
                        "error": null,
                    })
                }
                5 => {
                    assert_eq!(request["operation"], "QUERY");
                    assert_eq!(request["instance_id"], instance_id);
                    assert_eq!(request["sequence"], 2);
                    serde_json::json!({
                        "version": 1,
                        "kind": "CHANGES",
                        "instance_id": instance_id,
                        "sequence": 2,
                        "paths": [],
                        "error": null,
                    })
                }
                _ => unreachable!(),
            };
            write_monitor_response(&mut stream, response);
        }
    });

    let status = synck(root, &["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    server.join().unwrap();
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(status.contains("A file.a\n"), "{status}");
    assert!(status.contains("A file.b\n"), "{status}");
    assert!(!status.contains("file.c"), "{status}");
}

#[cfg(unix)]
#[test]
fn cli_monitor_query_failure_reloads_ignore_rules_without_retrying_endpoint() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    fs::write(root.join(".synckignore"), b"*.a\n").unwrap();
    fs::write(root.join("file.a"), b"a").unwrap();
    fs::write(root.join("file.b"), b"b").unwrap();
    assert!(synck(root, &["status"]).status.success());
    assert!(synck(root, &["monitor", "stop"]).status.success());
    wait_for_monitor_socket_removal(root);

    let repo_id = json_stdout(root, &["info", "--json"])["repo_id"]
        .as_str()
        .unwrap()
        .to_string();
    let socket = root.join(".synck/monitor.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server_root = root.to_path_buf();
    let (status_finished_tx, status_finished_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut status_stream = accept_monitor_connection(&listener);
        let request = read_monitor_request(&mut status_stream);
        assert_eq!(request["operation"], "STATUS");
        assert_eq!(request["repo_id"], repo_id);
        write_monitor_response(
            &mut status_stream,
            serde_json::json!({
                "version": 1,
                "kind": "RUNNING",
                "instance_id": "00000000-0000-4000-8000-000000000001",
                "sequence": 0,
                "paths": null,
                "error": null,
            }),
        );

        let mut query_stream = accept_monitor_connection(&listener);
        let request = read_monitor_request(&mut query_stream);
        assert_eq!(request["operation"], "QUERY");
        assert!(!request["instance_id"].is_null());
        fs::write(server_root.join(".synckignore"), b"*.b\n").unwrap();
        drop(query_stream);

        let mut status_finished = false;
        let mut extra_connections = 0;
        loop {
            if !status_finished {
                status_finished = match status_finished_rx.try_recv() {
                    Ok(()) => true,
                    Err(mpsc::TryRecvError::Empty) => false,
                    Err(mpsc::TryRecvError::Disconnected) => return extra_connections,
                };
            }
            match listener.accept() {
                Ok(_) => extra_connections += 1,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && status_finished => {
                    return extra_connections;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("cannot accept monitor client: {error}"),
            }
        }
    });

    let status = synck(root, &["status"]);
    status_finished_tx.send(()).unwrap();
    let extra_connections = server.join().unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(extra_connections, 0, "client queried the failed endpoint");
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(status.contains("A file.a\n"), "{status}");
    assert!(!status.contains("file.b"), "{status}");
}

#[cfg(unix)]
#[test]
fn cli_monitor_status_reports_unavailable_detail_once() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    let repo_id = json_stdout(root, &["info", "--json"])["repo_id"]
        .as_str()
        .unwrap()
        .to_string();
    let listener = UnixListener::bind(root.join(".synck/monitor.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let mut stream = accept_monitor_connection(&listener);
            let request = read_monitor_request(&mut stream);
            assert_eq!(request["operation"], "STATUS");
            assert_eq!(request["repo_id"], repo_id);
            write_monitor_response(
                &mut stream,
                serde_json::json!({
                    "version": 999,
                    "kind": "RUNNING",
                    "instance_id": "00000000-0000-4000-8000-000000000001",
                    "sequence": 0,
                    "paths": null,
                    "error": null,
                }),
            );
        }
    });

    let plain = synck(root, &["monitor", "status"]);
    assert!(plain.status.success());
    assert!(plain.stderr.is_empty());
    assert_eq!(
        String::from_utf8(plain.stdout).unwrap(),
        "monitor unavailable: unsupported monitor protocol version\n"
    );

    let json = json_stdout(root, &["monitor", "status", "--json"]);
    server.join().unwrap();
    assert_keys(
        &json,
        &["schema_version", "status", "instance_id", "detail"],
    );
    assert_eq!(json["schema_version"], 5);
    assert_eq!(json["status"], "unavailable");
    assert!(json["instance_id"].is_null());
    assert_eq!(json["detail"], "unsupported monitor protocol version");
}

#[cfg(unix)]
#[test]
fn cli_monitor_stop_fails_when_endpoint_is_unavailable() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    let repo_id = json_stdout(root, &["info", "--json"])["repo_id"]
        .as_str()
        .unwrap()
        .to_string();
    let listener = UnixListener::bind(root.join(".synck/monitor.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = accept_monitor_connection(&listener);
        let request = read_monitor_request(&mut stream);
        assert_eq!(request["operation"], "STATUS");
        assert_eq!(request["repo_id"], repo_id);
        write_monitor_response(
            &mut stream,
            serde_json::json!({
                "version": 999,
                "kind": "RUNNING",
                "instance_id": "00000000-0000-4000-8000-000000000001",
                "sequence": 0,
                "paths": null,
                "error": null,
            }),
        );
    });

    let stopped = synck(root, &["monitor", "stop"]);
    server.join().unwrap();
    assert!(!stopped.status.success());
    assert!(stopped.stdout.is_empty());
    assert_eq!(
        String::from_utf8(stopped.stderr).unwrap(),
        "synck: monitor unavailable: unsupported monitor protocol version\n"
    );
}

#[test]
fn cli_selective_commit_leaves_other_changes_uncommitted() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("one.txt"), b"one\n").unwrap();
    fs::write(root.join("two.txt"), b"two\n").unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());

    fs::write(root.join("one.txt"), b"one changed\n").unwrap();
    fs::write(root.join("two.txt"), b"two changed\n").unwrap();
    let committed = synck(root, &["commit", "-m", "only one\ncontext", "one.txt"]);
    assert!(
        committed.status.success(),
        "{}",
        String::from_utf8_lossy(&committed.stderr)
    );
    assert_eq!(
        String::from_utf8(synck(root, &["status"]).stdout).unwrap(),
        "M two.txt\n"
    );
    let shown = String::from_utf8(synck(root, &["show", "HEAD"]).stdout).unwrap();
    assert!(shown.contains("only one\ncontext\n"));
    assert!(shown.ends_with("M one.txt\n"));
    let log = String::from_utf8(synck(root, &["log", "-n", "1"]).stdout).unwrap();
    let lines = log.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    let header = lines[0].split(" | ").collect::<Vec<_>>();
    assert_eq!(header.len(), 5);
    assert_eq!(header[0], "3");
    chrono::DateTime::parse_from_str(header[1], "%Y-%m-%d %H:%M %:z").unwrap();
    assert_eq!(header[2], "author");
    assert_eq!(header[3], "1 change");
    assert_eq!(header[4], "only one context");
    assert_eq!(lines[1], "    M one.txt");
    assert!(!log.contains("sha256:"));

    let oneline = String::from_utf8(synck(root, &["log", "--oneline", "-n", "1"]).stdout).unwrap();
    let fields = oneline.trim_end().split(" | ").collect::<Vec<_>>();
    assert_eq!(fields.len(), 4);
    assert_eq!(fields[0], "3");
    chrono::DateTime::parse_from_str(fields[1], "%Y-%m-%d %H:%M %:z").unwrap();
    assert_eq!(fields[2], "author");
    assert_eq!(fields[3], "only one context");
    assert!(!oneline.contains("sha256:"));

    let rejected = synck(root, &["commit", "-m", "typo", "missing.txt"]);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("selected path does not exist"));
    assert_eq!(
        json_stdout(root, &["log", "--json"])["commits"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn cli_commit_message_is_optional_and_log_delta_is_bounded() {
    let temporary = committed_repository();
    let root = temporary.path();
    for index in 0..101 {
        fs::write(root.join(format!("item-{index:03}.txt")), b"content").unwrap();
    }

    let context = json_stdout(root, &["context", "--json"]);
    assert_keys(
        &context,
        &[
            "schema_version",
            "repository",
            "working_tree",
            "recent_commits",
        ],
    );
    assert_keys(
        &context["repository"],
        &["root", "format_version", "repo_id", "author", "head"],
    );
    assert_keys(
        &context["working_tree"],
        &[
            "clean",
            "changes",
            "changes_total",
            "changes_shown",
            "changes_truncated",
        ],
    );
    assert_eq!(context["repository"]["head"]["repo_seq"], 2);
    assert_eq!(context["working_tree"]["clean"], false);
    assert_eq!(context["working_tree"]["changes_total"], 101);
    assert_eq!(context["working_tree"]["changes_shown"], 100);
    assert_eq!(context["working_tree"]["changes_truncated"], true);
    assert_eq!(
        context["working_tree"]["changes"].as_array().unwrap().len(),
        100
    );
    assert_eq!(context["recent_commits"].as_array().unwrap().len(), 2);
    assert_keys(
        &context["recent_commits"][0],
        &["repo_seq", "commit_id", "created_at", "author", "message"],
    );

    let committed = synck(root, &["commit"]);
    assert!(
        committed.status.success(),
        "{}",
        String::from_utf8_lossy(&committed.stderr)
    );

    let log = String::from_utf8(synck(root, &["log", "-n", "1"]).stdout).unwrap();
    let lines = log.lines().collect::<Vec<_>>();
    assert!(lines[0].starts_with("3 | "), "{log}");
    assert!(lines[0].contains(" | 101 changes (100 shown)"), "{log}");
    assert_eq!(lines[1], "    A item-000.txt");
    assert_eq!(lines[100], "    A item-099.txt");
    assert_eq!(
        lines[101],
        "    Run `synck show 3` for the complete change list."
    );
    assert!(!log.contains("item-100.txt"));

    let log = json_stdout(root, &["log", "-n", "1", "--json"]);
    let commit = &log["commits"][0];
    assert!(commit["message"].is_null());
    assert_eq!(commit["changes"].as_array().unwrap().len(), 100);
    assert_eq!(commit["changes_total"], 101);
    assert_eq!(commit["changes_shown"], 100);
    assert_eq!(commit["changes_truncated"], true);

    let shown = String::from_utf8(synck(root, &["show", "3"]).stdout).unwrap();
    assert!(shown.contains("A item-000.txt\n"));
    assert!(shown.contains("A item-100.txt\n"));

    fs::write(root.join("item-000.txt"), b"changed").unwrap();
    let empty = synck(root, &["commit", "-m", ""]);
    assert_eq!(empty.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&empty.stderr).contains("commit message must not be empty"));
    assert_eq!(
        String::from_utf8(synck(root, &["status"]).stdout).unwrap(),
        "M item-000.txt\n"
    );
}

#[test]
fn cli_log_and_show_events_expose_complete_authoritative_rows() {
    let temporary = committed_repository();
    let root = temporary.path();

    let ordinary = json_stdout(root, &["log", "--limit", "1", "--json"]);
    let ordinary_commit = &ordinary["commits"][0];
    let event_log = json_stdout(root, &["log", "--limit", "1", "--events", "--json"]);
    let event_commit = &event_log["commits"][0];
    assert_keys(
        event_commit,
        &[
            "repo_seq",
            "commit_id",
            "created_at",
            "author",
            "message",
            "changes",
            "changes_total",
            "changes_shown",
            "changes_truncated",
            "event_count",
            "events_digest",
            "events",
        ],
    );
    assert_eq!(event_commit["changes"], ordinary_commit["changes"]);
    assert_eq!(
        event_commit["changes_total"],
        ordinary_commit["changes_total"]
    );
    let events = event_commit["events"].as_array().unwrap();
    assert!(
        event_commit["events_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(
        event_commit["event_count"].as_u64().unwrap() as usize,
        events.len()
    );
    assert!(events.len() > event_commit["changes_total"].as_u64().unwrap() as usize);
    for (index, event) in events.iter().enumerate() {
        assert_keys(
            event,
            &[
                "repo_id",
                "commit_id",
                "repo_seq",
                "commit_event_idx",
                "item_id",
                "item_seq",
                "event_type",
                "payload",
                "event_payload_digest",
                "created_at",
                "parent_folder_id",
                "parent_folder_id_before",
            ],
        );
        assert_eq!(event["commit_event_idx"], index);
        assert_eq!(event["commit_id"], event_commit["commit_id"]);
        assert_eq!(event["repo_seq"], event_commit["repo_seq"]);
        assert_eq!(event["event_type"], event["payload"]["event_type"]);
        assert!(
            event["event_payload_digest"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
    }
    assert!(
        events
            .iter()
            .any(|event| event["event_type"] == "FILE::CREATE")
    );

    let commit_id = event_commit["commit_id"].as_str().unwrap();
    let shown = json_stdout(root, &["show", commit_id, "--events", "--json"]);
    assert_eq!(shown["commit"]["event_count"], event_commit["event_count"]);
    assert_eq!(
        shown["commit"]["events_digest"],
        event_commit["events_digest"]
    );
    assert_eq!(shown["commit"]["events"], event_commit["events"]);

    let human =
        String::from_utf8(synck(root, &["log", "--limit", "1", "--events"]).stdout).unwrap();
    assert!(
        human.lines().next().unwrap().contains(&format!(
            " | {} | {}",
            counted_for_test(
                event_commit["changes_total"].as_u64().unwrap() as usize,
                "change",
                "changes"
            ),
            counted_for_test(events.len(), "event", "events")
        )),
        "{human}"
    );
    assert!(human.contains(" | FILE::CREATE | item "), "{human}");
    assert!(!human.contains("    A file.txt"), "{human}");

    let shown_human = String::from_utf8(synck(root, &["show", "HEAD", "--events"]).stdout).unwrap();
    assert!(shown_human.contains(&format!("    commit {commit_id}\n")));
    assert!(shown_human.contains(" | FILE::CREATE | item "));

    let conflict = synck(root, &["log", "--oneline", "--events"]);
    assert_eq!(conflict.status.code(), Some(2));

    let connection = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    let (original_payload, original_digest): (Vec<u8>, Vec<u8>) = connection
        .query_row(
            "SELECT payload, event_payload_digest FROM vcs_events
             WHERE commit_pk = (SELECT commit_pk FROM vcs_commits ORDER BY repo_seq DESC LIMIT 1)
               AND commit_event_idx = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut proposal: synck_vcs::types::EventProposal =
        serde_json::from_slice(&original_payload).unwrap();
    proposal.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(serde_json::json!({"tampered": true})).unwrap());
    let tampered_payload = synck_vcs::types::canonical_event_proposal_bytes(&proposal).unwrap();
    let tampered_digest = synck_vcs::types::compute_sha256_digest_bytes(&tampered_payload).to_vec();
    connection
        .execute(
            "UPDATE vcs_events SET payload = ?1, event_payload_digest = ?2
             WHERE commit_pk = (SELECT commit_pk FROM vcs_commits ORDER BY repo_seq DESC LIMIT 1)
               AND commit_event_idx = 0",
            rusqlite::params![tampered_payload, tampered_digest],
        )
        .unwrap();
    drop(connection);
    let tampered = synck(root, &["show", "HEAD", "--events"]);
    assert_eq!(tampered.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&tampered.stderr)
            .contains("digest disagrees"),
        "{}",
        String::from_utf8_lossy(&tampered.stderr)
    );

    let connection = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE vcs_events SET payload = ?1, event_payload_digest = ?2
             WHERE commit_pk = (SELECT commit_pk FROM vcs_commits ORDER BY repo_seq DESC LIMIT 1)
               AND commit_event_idx = 0",
            rusqlite::params![original_payload, original_digest],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM vcs_events
             WHERE commit_pk = (SELECT commit_pk FROM vcs_commits ORDER BY repo_seq DESC LIMIT 1)
               AND commit_event_idx = 0",
            [],
        )
        .unwrap();
    drop(connection);
    let incomplete = synck(root, &["show", "HEAD", "--events"]);
    assert_eq!(incomplete.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&incomplete.stderr).contains("event is missing"),
        "{}",
        String::from_utf8_lossy(&incomplete.stderr)
    );
}

fn counted_for_test(count: usize, singular: &str, plural: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural })
}

#[test]
fn cli_context_limits_recent_commit_metadata() {
    let temporary = committed_repository();
    let root = temporary.path();
    for sequence in 3..=7 {
        fs::write(root.join("file.txt"), format!("content {sequence}")).unwrap();
        assert!(
            synck(root, &["commit", "-m", &format!("commit {sequence}")])
                .status
                .success()
        );
    }

    let context = json_stdout(root, &["--json", "context"]);
    assert_eq!(context["working_tree"]["clean"], true);
    assert_eq!(context["working_tree"]["changes_total"], 0);
    let commits = context["recent_commits"].as_array().unwrap();
    assert_eq!(commits.len(), 5);
    assert_eq!(commits[0]["repo_seq"], 7);
    assert_eq!(commits[4]["repo_seq"], 3);

    let text = String::from_utf8(synck(root, &["context"]).stdout).unwrap();
    assert!(text.contains("head_repo_seq 7\n"));
    assert!(text.contains("working_tree clean\n"));
    assert!(text.contains("changes_truncated false\n"));
    assert!(text.contains("recent_commits 5\n"));
}

#[test]
fn selective_commit_includes_required_new_parents() {
    let temporary = committed_repository();
    let root = temporary.path();
    fs::create_dir(root.join("Folder")).unwrap();
    fs::create_dir(root.join("Folder/Nested")).unwrap();
    fs::write(root.join("Folder/Nested/selected.txt"), b"selected\n").unwrap();
    fs::write(root.join("other.txt"), b"other\n").unwrap();

    let output = synck(
        root,
        &[
            "commit",
            "-m",
            "selected nested file",
            "Folder/Nested/selected.txt",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(synck(root, &["status"]).stdout).unwrap(),
        "A other.txt\n"
    );
    let shown = String::from_utf8(synck(root, &["show", "HEAD"]).stdout).unwrap();
    assert!(shown.contains("A Folder\n"));
    assert!(shown.contains("A Folder/Nested\n"));
    assert!(shown.contains("A Folder/Nested/selected.txt\n"));
}

#[test]
fn selective_commit_covers_both_paths_of_a_moved_folder() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("Folder")).unwrap();
    fs::write(root.join("Folder/kept.txt"), b"initial\n").unwrap();
    fs::write(root.join("Folder/removed.txt"), b"remove me\n").unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());

    fs::rename(root.join("Folder"), root.join("Renamed")).unwrap();
    fs::write(root.join("Renamed/kept.txt"), b"changed through old path\n").unwrap();
    // Create before deleting so Linux cannot recycle the deleted inode and make
    // two logically distinct files look like one filesystem-identity rename.
    fs::write(root.join("Renamed/added.txt"), b"added through old path\n").unwrap();
    fs::remove_file(root.join("Renamed/removed.txt")).unwrap();
    let old_path = synck(root, &["commit", "-m", "select old path", "Folder"]);
    assert!(
        old_path.status.success(),
        "{}",
        String::from_utf8_lossy(&old_path.stderr)
    );
    let status = synck(root, &["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(status.stdout, CLEAN_STATUS_OUTPUT);
    let shown = String::from_utf8(synck(root, &["show", "HEAD"]).stdout).unwrap();
    assert!(shown.contains("RN Folder -> Renamed\n"));
    assert!(shown.contains("M Renamed/kept.txt\n"));
    assert!(shown.contains("A Renamed/added.txt\n"));
    assert!(shown.contains("D Folder/removed.txt\n"));
    assert!(!shown.contains("D Folder\n"));
    let log = String::from_utf8(synck(root, &["log", "-n", "1"]).stdout).unwrap();
    assert!(log.contains("    RN Folder -> Renamed\n"));
    assert!(!log.contains("RN Folder/kept.txt -> Renamed/kept.txt"));

    fs::rename(root.join("Renamed"), root.join("Final")).unwrap();
    fs::write(root.join("Final/kept.txt"), b"changed through new path\n").unwrap();
    // Create before deleting so Linux cannot recycle the deleted inode and make
    // two logically distinct files look like one filesystem-identity rename.
    fs::write(root.join("Final/new.txt"), b"added through new path\n").unwrap();
    fs::remove_file(root.join("Final/added.txt")).unwrap();
    let new_path = synck(root, &["commit", "-m", "select new path", "Final"]);
    assert!(
        new_path.status.success(),
        "{}",
        String::from_utf8_lossy(&new_path.stderr)
    );
    let status = synck(root, &["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(status.stdout, CLEAN_STATUS_OUTPUT);
    let shown = String::from_utf8(synck(root, &["show", "HEAD"]).stdout).unwrap();
    assert!(shown.contains("RN Renamed -> Final\n"));
    assert!(shown.contains("M Final/kept.txt\n"));
    assert!(shown.contains("A Final/new.txt\n"));
    assert!(shown.contains("D Renamed/added.txt\n"));
    assert!(!shown.contains("D Renamed\n"));
}

#[test]
fn child_only_history_never_reports_its_live_parent_as_changed() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("F")).unwrap();
    fs::write(root.join("F/a.txt"), b"a\n").unwrap();
    fs::write(root.join("F/b.txt"), b"b\n").unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());

    fs::remove_file(root.join("F/a.txt")).unwrap();
    assert!(
        synck(root, &["commit", "-m", "delete child"])
            .status
            .success()
    );
    let expected_delete = "D F/a.txt\n";
    let shown = String::from_utf8(synck(root, &["show", "HEAD"]).stdout).unwrap();
    assert!(shown.ends_with(expected_delete), "{shown}");
    assert!(!shown.contains("D F\n"), "{shown}");
    let compared = String::from_utf8(synck(root, &["compare", "HEAD~1", "HEAD"]).stdout).unwrap();
    assert_eq!(compared, expected_delete);
    let shown_json = json_stdout(root, &["show", "HEAD", "--json"]);
    assert_eq!(
        shown_json["commit"]["changes"],
        serde_json::json!([{
            "kind": "DELETE",
            "path": "F/a.txt",
            "source_path": null,
            "content_changed": false
        }])
    );
    let log_json = json_stdout(root, &["log", "-n", "1", "--json"]);
    assert_eq!(
        log_json["commits"][0]["changes"],
        shown_json["commit"]["changes"]
    );
    let compared_json = json_stdout(root, &["compare", "HEAD~1", "HEAD", "--json"]);
    assert_eq!(compared_json["changes"], shown_json["commit"]["changes"]);
    let diff = String::from_utf8(synck(root, &["diff", "HEAD~1", "HEAD"]).stdout).unwrap();
    assert!(diff.contains("--- a/F/a.txt\n+++ /dev/null\n"), "{diff}");
    assert!(!diff.contains("a/F\n"), "{diff}");

    fs::write(root.join("F/c.txt"), b"c\n").unwrap();
    assert!(synck(root, &["commit", "-m", "add child"]).status.success());
    let shown = String::from_utf8(synck(root, &["show", "HEAD"]).stdout).unwrap();
    assert!(shown.ends_with("A F/c.txt\n"), "{shown}");
    assert!(!shown.contains("A F\n"), "{shown}");
    assert_eq!(
        String::from_utf8(synck(root, &["compare", "HEAD~1", "HEAD"]).stdout).unwrap(),
        "A F/c.txt\n"
    );
}

#[test]
fn parent_only_move_between_existing_folders_is_in_historical_deltas() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("Src")).unwrap();
    fs::create_dir(root.join("Dst")).unwrap();
    fs::write(root.join("Src/file.txt"), b"content\n").unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());

    fs::rename(root.join("Src/file.txt"), root.join("Dst/file.txt")).unwrap();
    assert_eq!(
        String::from_utf8(synck(root, &["status"]).stdout).unwrap(),
        "MV Src/file.txt -> Dst/file.txt\n"
    );
    assert!(synck(root, &["commit", "-m", "move file"]).status.success());

    let expected = "MV Src/file.txt -> Dst/file.txt\n";
    let shown = String::from_utf8(synck(root, &["show", "HEAD"]).stdout).unwrap();
    assert!(shown.ends_with(expected), "{shown}");
    let log = String::from_utf8(synck(root, &["log", "-n", "1"]).stdout).unwrap();
    assert!(
        log.contains("    MV Src/file.txt -> Dst/file.txt\n"),
        "{log}"
    );
    assert!(log.contains(" | 1 change | move file\n"), "{log}");
    assert!(
        log.ends_with("    MV Src/file.txt -> Dst/file.txt\n"),
        "{log}"
    );
    assert_eq!(
        String::from_utf8(synck(root, &["compare", "HEAD~1", "HEAD"]).stdout).unwrap(),
        expected
    );

    let expected_json = serde_json::json!([{
        "kind": "MOVE",
        "path": "Dst/file.txt",
        "source_path": "Src/file.txt",
        "content_changed": false
    }]);
    let shown_json = json_stdout(root, &["show", "HEAD", "--json"]);
    assert_eq!(shown_json["commit"]["changes"], expected_json);
    let log_json = json_stdout(root, &["log", "-n", "1", "--json"]);
    assert_eq!(log_json["commits"][0]["changes"], expected_json);
    assert_eq!(log_json["commits"][0]["changes_total"], 1);
    let compared_json = json_stdout(root, &["compare", "HEAD~1", "HEAD", "--json"]);
    assert_eq!(compared_json["changes"], expected_json);
}

#[test]
fn observed_move_rename_survives_atomic_save_at_its_destination() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("Src")).unwrap();
    fs::create_dir(root.join("Dst")).unwrap();
    fs::write(root.join("Src/Book1.xlsx"), b"first workbook").unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());

    fs::rename(root.join("Src/Book1.xlsx"), root.join("Dst/Book2.xlsx")).unwrap();
    assert_eq!(
        String::from_utf8(synck(root, &["status"]).stdout).unwrap(),
        "MV+RN Src/Book1.xlsx -> Dst/Book2.xlsx\n"
    );

    fs::write(root.join("Dst/replacement.tmp"), b"second workbook").unwrap();
    fs::remove_file(root.join("Dst/Book2.xlsx")).unwrap();
    fs::rename(
        root.join("Dst/replacement.tmp"),
        root.join("Dst/Book2.xlsx"),
    )
    .unwrap();
    let expected = "M+MV+RN Src/Book1.xlsx -> Dst/Book2.xlsx\n";
    assert_eq!(
        String::from_utf8(synck(root, &["status"]).stdout).unwrap(),
        expected
    );
    let expected_json = serde_json::json!([{
        "kind": "MOVE_RENAME",
        "path": "Dst/Book2.xlsx",
        "source_path": "Src/Book1.xlsx",
        "content_changed": true
    }]);
    assert_eq!(
        json_stdout(root, &["status", "--json"])["changes"],
        expected_json
    );

    assert!(
        synck(root, &["commit", "-m", "move and save"])
            .status
            .success()
    );
    let shown = String::from_utf8(synck(root, &["show", "HEAD"]).stdout).unwrap();
    assert!(shown.ends_with(expected), "{shown}");
    let log = String::from_utf8(synck(root, &["log", "-n", "1"]).stdout).unwrap();
    assert!(log.contains(&format!("    {expected}")), "{log}");
    assert!(log.contains(" | 1 change | move and save\n"), "{log}");
    assert!(log.ends_with(&format!("    {expected}")), "{log}");
    assert_eq!(
        String::from_utf8(synck(root, &["compare", "HEAD~1", "HEAD"]).stdout).unwrap(),
        expected
    );
    let shown_json = json_stdout(root, &["show", "HEAD", "--json"]);
    assert_eq!(shown_json["commit"]["changes"], expected_json);
}

#[test]
fn selective_file_scope_does_not_capture_a_vacated_folder_subtree() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("Occupied")).unwrap();
    fs::write(root.join("document.txt"), b"document\n").unwrap();
    fs::write(root.join("Occupied/tracked.txt"), b"tracked\n").unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());

    fs::rename(root.join("Occupied"), root.join("Relocated")).unwrap();
    fs::rename(root.join("document.txt"), root.join("Occupied")).unwrap();
    fs::write(root.join("Relocated/unrelated.txt"), b"unrelated\n").unwrap();

    let ambiguous = synck(root, &["commit", "-m", "ambiguous", "Occupied"]);
    assert_eq!(ambiguous.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&ambiguous.stderr)
            .contains("reused by another item in accepted or working state")
    );

    let selected = synck(root, &["commit", "-m", "select moved file", "document.txt"]);
    assert_eq!(selected.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&selected.stderr)
            .contains("reused by another item in accepted or working state")
    );
    let status = synck(root, &["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        String::from_utf8(status.stdout).unwrap(),
        "RN document.txt -> Occupied\nRN Occupied -> Relocated\nA Relocated/unrelated.txt\n"
    );
    assert_eq!(
        json_stdout(root, &["log", "--json"])["commits"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn selective_folder_scope_rejects_untracked_endpoint_reuse() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("Folder")).unwrap();
    fs::write(root.join("Folder/tracked.txt"), b"tracked\n").unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());

    fs::rename(root.join("Folder"), root.join("Renamed")).unwrap();
    fs::create_dir(root.join("Folder")).unwrap();
    fs::write(root.join("Folder/unrelated.txt"), b"unrelated\n").unwrap();

    for path in ["Folder", "Renamed"] {
        let rejected = synck(root, &["commit", "-m", "ambiguous", path]);
        assert_eq!(rejected.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&rejected.stderr)
                .contains("reused by another item in accepted or working state")
        );
    }
    let status = synck(root, &["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        String::from_utf8(status.stdout).unwrap(),
        "A Folder\nA Folder/unrelated.txt\nRN Folder -> Renamed\n"
    );
    assert_eq!(
        json_stdout(root, &["log", "--json"])["commits"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn selective_scope_commits_same_path_delete_and_add_together() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("Entry"), b"file\n").unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());

    // Create the replacement before deleting the tracked file so Linux cannot
    // recycle the deleted inode and make two distinct items share an identity.
    fs::create_dir(root.join("Replacement")).unwrap();
    fs::write(root.join("Replacement/child.txt"), b"child\n").unwrap();
    fs::remove_file(root.join("Entry")).unwrap();
    fs::rename(root.join("Replacement"), root.join("Entry")).unwrap();
    let status = synck(root, &["status"]);
    assert!(status.status.success());
    assert_eq!(
        String::from_utf8(status.stdout).unwrap(),
        "D Entry\nA Entry\nA Entry/child.txt\n"
    );
    let status = json_stdout(root, &["status", "--json"]);
    assert_eq!(status["schema_version"], 5);
    assert_eq!(
        status["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|change| (
                change["kind"].as_str().unwrap(),
                change["path"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("DELETE", "Entry"),
            ("ADD", "Entry"),
            ("ADD", "Entry/child.txt")
        ]
    );
    let committed = synck(
        root,
        &[
            "commit",
            "-m",
            "replace file with folder",
            "Entry/child.txt",
        ],
    );
    assert!(
        committed.status.success(),
        "{}",
        String::from_utf8_lossy(&committed.stderr)
    );
    let status = synck(root, &["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(status.stdout, CLEAN_STATUS_OUTPUT);
    let shown = String::from_utf8(synck(root, &["show", "HEAD"]).stdout).unwrap();
    assert_eq!(
        shown.lines().skip(1).collect::<Vec<_>>(),
        vec!["D Entry", "A Entry", "A Entry/child.txt"]
    );
    assert_eq!(
        String::from_utf8(synck(root, &["compare", "HEAD~1", "HEAD"]).stdout).unwrap(),
        "D Entry\nA Entry\nA Entry/child.txt\n"
    );
}

#[test]
fn cli_structural_compare_and_text_diff_use_historical_state() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("Drafts")).unwrap();
    fs::write(root.join("Drafts/note.txt"), b"old line\nkept\n").unwrap();
    fs::write(root.join("Drafts/file.bin"), [0xff, 0x00, 0x01]).unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    let first = synck(root, &["commit", "-m", "initial"]);
    let first = String::from_utf8(first.stdout)
        .unwrap()
        .trim()
        .strip_prefix("committed ")
        .unwrap()
        .to_string();

    fs::rename(root.join("Drafts"), root.join("Final")).unwrap();
    fs::write(root.join("Final/note.txt"), b"new line\nkept\n").unwrap();
    fs::write(root.join("Final/file.bin"), [0xfe, 0x00, 0x02]).unwrap();
    fs::write(root.join("added.txt"), b"added\n").unwrap();
    let second = synck(root, &["commit", "-m", "finalize"]);
    let second = String::from_utf8(second.stdout)
        .unwrap()
        .trim()
        .strip_prefix("committed ")
        .unwrap()
        .to_string();

    let compared = synck(root, &["compare", &first, &second]);
    assert!(
        compared.status.success(),
        "{}",
        String::from_utf8_lossy(&compared.stderr)
    );
    let compared = String::from_utf8(compared.stdout).unwrap();
    assert_eq!(
        compared,
        "RN Drafts -> Final\nM Final/file.bin\nM Final/note.txt\nA added.txt\n"
    );
    assert_eq!(
        String::from_utf8(synck(root, &["compare", "HEAD~1", "HEAD"]).stdout).unwrap(),
        compared
    );
    let compared_json = json_stdout(root, &["compare", "HEAD~1", "HEAD", "--json"]);
    assert_keys(&compared_json, &["schema_version", "from", "to", "changes"]);
    assert_keys(&compared_json["from"], &["repo_seq", "commit_id"]);
    assert_keys(&compared_json["to"], &["repo_seq", "commit_id"]);
    assert_eq!(compared_json["from"]["commit_id"], first);
    assert_eq!(compared_json["to"]["commit_id"], second);
    assert_eq!(compared_json["changes"].as_array().unwrap().len(), 4);

    let diff = synck(root, &["diff", &first, &second]);
    assert!(
        diff.status.success(),
        "{}",
        String::from_utf8_lossy(&diff.stderr)
    );
    let diff = String::from_utf8(diff.stdout).unwrap();
    assert!(diff.contains("Binary files Drafts/file.bin and Final/file.bin differ\n"));
    assert!(diff.contains("--- a/Drafts/note.txt\n+++ b/Final/note.txt\n"));
    assert!(diff.contains("-old line\n+new line\n"));
    assert!(diff.contains("--- /dev/null\n+++ b/added.txt\n"));

    fs::write(root.join("Final/note.txt"), b"working line\nkept\n").unwrap();
    let working = String::from_utf8(synck(root, &["diff"]).stdout).unwrap();
    assert!(working.contains("--- a/Final/note.txt\n+++ b/Final/note.txt\n"));
    assert!(working.contains("-new line\n+working line\n"));

    let invalid = synck(root, &["diff", "HEAD"]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("zero or two revisions"));
}

#[test]
fn historical_comparison_reads_only_changed_file_content() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("changed.txt"), b"before\n").unwrap();
    fs::write(root.join("unchanged.txt"), b"untouched\n").unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());
    fs::write(root.join("changed.txt"), b"after\n").unwrap();
    assert!(synck(root, &["commit", "-m", "edit"]).status.success());
    assert!(synck(root, &["compare", "2", "3"]).status.success());

    let connection = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE vcs_events SET payload = x'7b7d'
             WHERE item_id = (
                 SELECT item_id FROM vcs_items
                 WHERE repo_id = vcs_events.repo_id AND name = 'unchanged.txt'
             ) AND event_type = 'FILE::SET_CONTENT'",
            [],
        )
        .unwrap();
    drop(connection);

    let compared = synck(root, &["compare", "2", "3"]);
    assert!(
        compared.status.success(),
        "{}",
        String::from_utf8_lossy(&compared.stderr)
    );
    assert_eq!(
        String::from_utf8(compared.stdout).unwrap(),
        "M changed.txt\n"
    );

    let diff = synck(root, &["diff", "2", "3"]);
    assert!(
        diff.status.success(),
        "{}",
        String::from_utf8_lossy(&diff.stderr)
    );
    let diff = String::from_utf8(diff.stdout).unwrap();
    assert!(diff.contains("-before\n+after\n"), "{diff}");
    assert!(!diff.contains("unchanged.txt"), "{diff}");
}

#[test]
fn numeric_repository_sequences_are_exact_revisions() {
    let temporary = committed_repository();
    let root = temporary.path();
    let db = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    db.execute("UPDATE vcs_repo_activity_heads SET next_repo_seq = 4", [])
        .unwrap();
    let before: (i64, i64) = db
        .query_row(
            "SELECT (SELECT count(*) FROM vcs_commits), (SELECT count(*) FROM vcs_events)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    fs::write(root.join("file.txt"), b"changed").unwrap();
    let pending = synck(
        root,
        &["--json", "commit", "-m", "accepted pending publication"],
    );
    // Acceptance succeeded; the CLI reports the separate publication failure.
    assert!(!pending.status.success());
    let error: Value = serde_json::from_slice(&pending.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "ACCEPTED_NEEDS_RECOVERY");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("publication counter") && message.contains("repository finalize"),
        "{message}"
    );
    let accepted_id: Vec<u8> = db
        .query_row(
            "SELECT commit_id FROM vcs_commits WHERE repo_seq IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let accepted_id = synck_vcs::types::commit_id_from_digest_bytes(&accepted_id).unwrap();
    assert!(message.contains(accepted_id.as_str()), "{message}");
    let after: (i64, i64) = db
        .query_row(
            "SELECT (SELECT count(*) FROM vcs_commits), (SELECT count(*) FROM vcs_events)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(after.0, before.0 + 1);
    assert!(after.1 > before.1);
    let repaired = synck(root, &["repository", "finalize", "."]);
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    let published: Vec<u8> = db
        .query_row(
            "SELECT commit_id FROM vcs_commits WHERE repo_seq=3",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        synck_vcs::types::commit_id_from_digest_bytes(&published).unwrap(),
        accepted_id
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM vcs_commits", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        after.0
    );
    assert_eq!(
        db.query_row(
            "SELECT next_repo_seq FROM vcs_repo_activity_heads",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        4
    );
    let compared = synck(root, &["compare", "2", "3"]);
    assert!(
        compared.status.success(),
        "{}",
        String::from_utf8_lossy(&compared.stderr)
    );
    assert_eq!(String::from_utf8(compared.stdout).unwrap(), "M file.txt\n");
    assert!(synck(root, &["--json", "log"]).status.success());
    assert!(synck(root, &["fsck"]).status.success());
    let missing = synck(root, &["compare", "2", "4"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("revision does not exist: 4"));
    for invalid in ["0", "01", "+1", "-1", " 1", "1 ", "9223372036854775808"] {
        assert!(
            !synck(root, &["compare", "2", invalid]).status.success(),
            "revision {invalid:?}"
        );
    }
}

#[test]
fn finalize_repairs_counter_and_publishes_previously_accepted_commit() {
    let temporary = committed_repository();
    let root = temporary.path();
    fs::write(root.join("file.txt"), b"accepted").unwrap();
    assert!(synck(root, &["commit", "-m", "accepted"]).status.success());
    let db = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    let accepted: Vec<u8> = db
        .query_row(
            "SELECT commit_id FROM vcs_commits WHERE repo_seq=3",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let checkpoint: Vec<u8> = db
        .query_row(
            "SELECT checkpoint FROM vcs_checkpoints WHERE repo_seq=3",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // Recreate an older writer's accepted-but-unpublished state and bad counter.
    db.execute_batch("BEGIN IMMEDIATE; DELETE FROM vcs_checkpoints WHERE repo_seq=3; UPDATE vcs_commits SET repo_seq=NULL WHERE repo_seq=3; UPDATE vcs_repo_activity_heads SET next_repo_seq=7; COMMIT;").unwrap();
    fs::write(root.join("file.txt"), b"unsaved working edit").unwrap();
    assert!(!synck(root, &["fsck"]).status.success());
    for _ in 0..2 {
        let finalized = synck(root, &["repository", "finalize", "."]);
        assert!(
            finalized.status.success(),
            "{}",
            String::from_utf8_lossy(&finalized.stderr)
        );
    }
    let restored: (Vec<u8>, Vec<u8>) = db
        .query_row(
            "SELECT commit_id,checkpoint FROM vcs_checkpoints WHERE repo_seq=3",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(restored, (accepted, checkpoint));
    assert_eq!(
        db.query_row("SELECT count(*) FROM vcs_commits", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        fs::read(root.join("file.txt")).unwrap(),
        b"unsaved working edit"
    );
    assert!(synck(root, &["--json", "log"]).status.success());
    assert!(synck(root, &["fsck"]).status.success());
}

#[test]
fn show_and_revert_refresh_checkout_history() {
    let temporary = committed_repository();
    let root = temporary.path();
    fs::write(root.join("file.txt"), b"changed").unwrap();
    assert!(
        synck(root, &["commit", "-m", "third sequence"])
            .status
            .success()
    );

    let shown = synck(root, &["show", "3"]);
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert!(
        String::from_utf8(shown.stdout)
            .unwrap()
            .ends_with("M file.txt\n")
    );

    let reverted = synck(root, &["revert", "3", "-m", "revert third sequence"]);
    assert!(
        reverted.status.success(),
        "{}",
        String::from_utf8_lossy(&reverted.stderr)
    );
    assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"content");
    assert_clean_status(root);
}

#[test]
fn historical_state_authenticates_the_pointed_file_event() {
    let temporary = committed_repository();
    let root = temporary.path();
    fs::write(root.join("file.txt"), b"changed").unwrap();
    assert!(synck(root, &["commit", "-m", "change"]).status.success());
    assert!(synck(root, &["compare", "2", "3"]).status.success());
    let connection = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE vcs_events SET payload = x'7b7d'
             WHERE event_type = 'FILE::SET_CONTENT'",
            [],
        )
        .unwrap();
    drop(connection);

    let output = synck(root, &["compare", "2", "3"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("historical content event digest disagrees"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_repository_information_and_author_configuration() {
    let temporary = committed_repository();
    let root = temporary.path();

    let info = json_stdout(root, &["info", "--json"]);
    assert_keys(
        &info,
        &[
            "schema_version",
            "root",
            "format_version",
            "repo_id",
            "author",
            "head",
        ],
    );
    assert_eq!(info["schema_version"], 5);
    assert_eq!(info["format_version"], 7);
    assert_eq!(info["author"], "author");
    assert_eq!(info["head"]["repo_seq"], 2);
    assert_keys(info.get("head").unwrap(), &["repo_seq", "commit_id"]);

    let author = json_stdout(root, &["--json", "config", "author"]);
    assert_keys(&author, &["schema_version", "author"]);
    assert_eq!(author["author"], "author");

    let updated = json_stdout(root, &["config", "set-author", "second author", "--json"]);
    assert_keys(&updated, &["schema_version", "author"]);
    assert_eq!(updated["author"], "second author");
    assert_eq!(
        String::from_utf8(synck(root, &["config", "author"]).stdout).unwrap(),
        "author second author\n"
    );

    fs::write(root.join("file.txt"), b"next").unwrap();
    assert!(synck(root, &["commit", "-m", "next"]).status.success());
    let log = json_stdout(root, &["log", "--json"]);
    assert_eq!(log["commits"][0]["author"], "second author");
    assert_eq!(log["commits"][1]["author"], "author");

    let rejected = synck(root, &["--json", "config", "set-author", ""]);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(rejected.stdout.is_empty());
    let error: Value = serde_json::from_slice(&rejected.stderr).unwrap();
    assert_keys(&error, &["schema_version", "error"]);
    assert_keys(error.get("error").unwrap(), &["kind", "message"]);
    assert_eq!(error["error"]["kind"], "INVALID");
    assert_eq!(
        json_stdout(root, &["config", "author", "--json"])["author"],
        "second author"
    );

    let hidden = json_stdout(root, &["config", "hidden-files", "--json"]);
    assert_keys(&hidden, &["schema_version", "hidden_files"]);
    assert_eq!(hidden["hidden_files"], "ignore");
    fs::write(root.join(".agent-notes"), b"notes").unwrap();
    assert!(
        !String::from_utf8(synck(root, &["status"]).stdout)
            .unwrap()
            .contains(".agent-notes")
    );

    let updated = json_stdout(root, &["config", "set-hidden-files", "track", "--json"]);
    assert_keys(&updated, &["schema_version", "hidden_files"]);
    assert_eq!(updated["hidden_files"], "track");
    assert!(
        String::from_utf8(synck(root, &["status"]).stdout)
            .unwrap()
            .contains("A .agent-notes")
    );

    assert!(
        synck(root, &["config", "set-hidden-files", "ignore"])
            .status
            .success()
    );
    assert_eq!(
        String::from_utf8(synck(root, &["config", "hidden-files"]).stdout).unwrap(),
        "hidden-files ignore\n"
    );
}

#[test]
fn cli_json_inspection_contract() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("Documents")).unwrap();
    fs::write(root.join("Documents/file.txt"), b"before").unwrap();
    assert!(synck(root, &["init", "--author", "owner"]).status.success());
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());

    fs::write(root.join("Documents/file.txt"), b"after").unwrap();
    let status = json_stdout(root, &["status", "--json"]);
    assert_keys(&status, &["schema_version", "changes"]);
    assert_keys(
        &status["changes"][0],
        &["kind", "path", "source_path", "content_changed"],
    );
    assert_eq!(status["changes"][0]["kind"], "MODIFY");
    assert!(status["changes"][0]["source_path"].is_null());
    assert_eq!(status["changes"][0]["content_changed"], true);
    assert!(synck(root, &["commit", "-m", "update"]).status.success());

    let log = json_stdout(root, &["--json", "log"]);
    assert_keys(&log, &["schema_version", "commits"]);
    assert_keys(
        &log["commits"][0],
        &[
            "repo_seq",
            "commit_id",
            "created_at",
            "author",
            "message",
            "changes",
            "changes_total",
            "changes_shown",
            "changes_truncated",
        ],
    );
    assert_eq!(log["commits"][0]["changes_total"], 1);
    assert_eq!(log["commits"][0]["changes_shown"], 1);
    assert_eq!(log["commits"][0]["changes_truncated"], false);
    assert_eq!(log["commits"][0]["changes"][0]["kind"], "MODIFY");
    let commit_id = log["commits"][0]["commit_id"].as_str().unwrap();

    let show = json_stdout(root, &["show", commit_id, "--json"]);
    assert_keys(&show, &["schema_version", "commit"]);
    assert_keys(
        &show["commit"],
        &[
            "repo_seq",
            "commit_id",
            "created_at",
            "author",
            "message",
            "changes",
        ],
    );
    let history = json_stdout(root, &["history", "Documents/file.txt", "--json"]);
    assert_keys(&history, &["schema_version", "commits"]);
    assert_keys(
        &history["commits"][0],
        &[
            "repo_seq",
            "commit_id",
            "created_at",
            "author",
            "message",
            "changes",
        ],
    );

    let versions = json_stdout(root, &["versions", "Documents/file.txt", "--json"]);
    assert_keys(&versions, &["schema_version", "versions"]);
    assert_keys(
        &versions["versions"][0],
        &["item_seq", "commit_id", "created_at", "blob_id", "size"],
    );
    let named = json_stdout(root, &["version", "list", "Documents/file.txt", "--json"]);
    assert_keys(&named, &["schema_version", "versions"]);
    assert_keys(
        &named["versions"][0],
        &[
            "label",
            "version_id",
            "lifecycle_state",
            "kind",
            "label_source",
            "description",
            "source_item_seq",
            "version_set_seq",
            "created_at",
            "blob_id",
            "size",
        ],
    );

    let storage = json_stdout(root, &["storage", "--json"]);
    assert_keys(
        &storage,
        &[
            "schema_version",
            "referenced_objects",
            "referenced_bytes",
            "unreachable_objects",
            "unreachable_bytes",
        ],
    );
    let fsck_output = synck(root, &["fsck", "--json"]);
    assert!(fsck_output.status.success());
    assert!(fsck_output.stderr.is_empty());
    let fsck: Value = serde_json::from_slice(&fsck_output.stdout).unwrap();
    assert_eq!(fsck, json_stdout(root, &["repository", "verify"]));

    fs::remove_file(root.join("Documents/file.txt")).unwrap();
    assert!(synck(root, &["commit", "-m", "delete"]).status.success());
    let deleted = json_stdout(root, &["deleted", "--json"]);
    assert_keys(&deleted, &["schema_version", "items"]);
    assert_keys(
        &deleted["items"][0],
        &[
            "item_id",
            "item_type",
            "path",
            "repo_seq",
            "commit_id",
            "deleted_at",
        ],
    );
}

#[test]
fn cli_json_errors_and_exit_codes_are_stable() {
    let temporary = committed_repository();
    let root = temporary.path();

    let usage = synck(root, &["--json", "not-a-command"]);
    assert_eq!(usage.status.code(), Some(2));
    assert!(usage.stdout.is_empty());
    let usage: Value = serde_json::from_slice(&usage.stderr).unwrap();
    assert_eq!(usage["schema_version"], 5);
    assert_eq!(usage["error"]["kind"], "USAGE");

    let unsupported = synck(root, &["restore-tree", "--json"]);
    assert_eq!(unsupported.status.code(), Some(1));
    assert!(unsupported.stdout.is_empty());
    let unsupported: Value = serde_json::from_slice(&unsupported.stderr).unwrap();
    assert_eq!(unsupported["schema_version"], 5);
    assert_eq!(unsupported["error"]["kind"], "INVALID");
    assert_eq!(
        unsupported["error"]["message"],
        "--json is not supported for this command"
    );

    let runtime = synck(root, &["--json", "show", "missing"]);
    assert_eq!(runtime.status.code(), Some(1));
    assert!(runtime.stdout.is_empty());
    let runtime: Value = serde_json::from_slice(&runtime.stderr).unwrap();
    assert_eq!(runtime["schema_version"], 5);
    assert_eq!(runtime["error"]["kind"], "INVALID");
}

#[test]
fn cli_json_commit_and_revert_results_identify_accepted_history() {
    let temporary = committed_repository();
    let root = temporary.path();
    let clean = json_stdout(root, &["--json", "commit"]);
    assert_eq!(
        clean,
        serde_json::json!({"schema_version": 5, "commit_id": null})
    );

    fs::write(root.join("file.txt"), b"updated").unwrap();
    fs::write(root.join("other.txt"), b"unselected").unwrap();
    let committed = json_stdout(root, &["commit", "file.txt", "-m", "selected", "--json"]);
    assert_keys(&committed, &["schema_version", "commit_id"]);
    assert_eq!(committed["schema_version"], 5);
    let shown = json_stdout(root, &["show", "HEAD", "--json"]);
    assert_eq!(committed["commit_id"], shown["commit"]["commit_id"]);
    assert_eq!(shown["commit"]["changes"].as_array().unwrap().len(), 1);

    let remainder = json_stdout(root, &["commit", "-m", "remainder", "--json"]);
    assert_keys(&remainder, &["schema_version", "commit_id"]);
    let shown = json_stdout(root, &["show", "HEAD", "--json"]);
    assert_eq!(remainder["commit_id"], shown["commit"]["commit_id"]);
    assert_eq!(
        json_stdout(root, &["commit", "--json"]),
        serde_json::json!({"schema_version": 5, "commit_id": null})
    );

    let reverted = json_stdout(root, &["revert", "HEAD", "-m", "undo", "--json"]);
    assert_keys(&reverted, &["schema_version", "commit_id"]);
    assert_eq!(reverted["schema_version"], 5);
    let shown = json_stdout(root, &["show", "HEAD", "--json"]);
    assert_eq!(reverted["commit_id"], shown["commit"]["commit_id"]);
    assert_ne!(reverted["commit_id"], remainder["commit_id"]);
    assert!(!root.join("other.txt").exists());

    for arguments in [
        vec!["commit", "missing.txt", "--json"],
        vec!["revert", "missing", "-m", "invalid", "--json"],
    ] {
        let output = synck(root, &arguments);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["schema_version"], 5);
        assert_eq!(error["error"]["kind"], "INVALID");
    }
}

#[test]
fn cli_json_diff_preserves_patches_and_non_text_results() {
    let temporary = committed_repository();
    let root = temporary.path();
    assert_eq!(
        json_stdout(root, &["diff", "--json"]),
        serde_json::json!({"schema_version": 5, "entries": []})
    );
    fs::remove_file(root.join("file.txt")).unwrap();
    fs::write(root.join("added.txt"), b"quoted \"text\"\n").unwrap();
    fs::write(root.join("binary.bin"), [0xff, 0x00]).unwrap();
    fs::File::create(root.join("large.txt"))
        .unwrap()
        .set_len(synck_local::TEXT_DIFF_MAX_BYTES + 1)
        .unwrap();
    let diff = json_stdout(root, &["diff", "--json"]);
    assert_keys(&diff, &["schema_version", "entries"]);
    assert_eq!(diff["schema_version"], 5);
    let entries = diff["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 4);
    for entry in entries {
        assert_keys(entry, &["old_path", "new_path", "kind", "patch"]);
    }
    let added = entries
        .iter()
        .find(|entry| entry["new_path"] == "added.txt")
        .unwrap();
    assert_eq!(added["old_path"], Value::Null);
    assert_eq!(added["kind"], "PATCH");
    assert!(
        added["patch"]
            .as_str()
            .unwrap()
            .contains("+quoted \"text\"\n")
    );
    let deleted = entries
        .iter()
        .find(|entry| entry["old_path"] == "file.txt")
        .unwrap();
    assert_eq!(deleted["new_path"], Value::Null);
    assert_eq!(deleted["kind"], "PATCH");
    assert!(deleted["patch"].as_str().unwrap().contains("-content\n"));
    for (path, kind) in [("binary.bin", "BINARY"), ("large.txt", "TOO_LARGE")] {
        let entry = entries
            .iter()
            .find(|entry| entry["new_path"] == path)
            .unwrap();
        assert_eq!(entry["old_path"], Value::Null);
        assert_eq!(entry["kind"], kind);
        assert_eq!(entry["patch"], Value::Null);
    }

    json_stdout(root, &["commit", "-m", "changes", "--json"]);
    assert_eq!(
        json_stdout(root, &["diff", "HEAD~1", "HEAD", "--json"]),
        diff
    );
    let failed = synck(root, &["diff", "HEAD", "--json"]);
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty());
    let error: Value = serde_json::from_slice(&failed.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "INVALID");
}

#[test]
fn cli_named_version_round_trip() {
    let temporary = committed_repository();
    let root = temporary.path();
    let initial = synck(root, &["version", "list", "file.txt"]);
    assert!(
        initial.status.success(),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    let initial = String::from_utf8(initial.stdout).unwrap();
    let initial_fields = initial.split_whitespace().collect::<Vec<_>>();
    assert_eq!(initial_fields[0], "0.01");
    assert_eq!(initial_fields[2], "LIVE");
    assert_eq!(initial_fields[3], "MINOR");
    let initial_id = initial_fields[1].to_string();

    fs::write(root.join("file.txt"), b"second").unwrap();
    assert!(synck(root, &["commit", "-m", "second"]).status.success());
    let created = synck(
        root,
        &[
            "version",
            "create",
            "file.txt",
            "--major",
            "--description",
            "client copy",
            "-m",
            "name client copy",
        ],
    );
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(synck(root, &["compare", "3", "4"]).stdout.is_empty());
    assert!(synck(root, &["diff", "3", "4"]).stdout.is_empty());
    let version_log = String::from_utf8(synck(root, &["log", "-n", "1"]).stdout).unwrap();
    assert!(version_log.contains(" | 0 changes | name client copy\n"));
    assert_eq!(version_log.lines().count(), 1);
    let listed = String::from_utf8(synck(root, &["version", "list", "file.txt"]).stdout).unwrap();
    let second = listed.lines().nth(1).unwrap();
    assert!(second.starts_with("1.00 "));
    assert!(second.ends_with("client copy"));
    let second_id = second.split_whitespace().nth(1).unwrap().to_string();

    assert!(
        synck(
            root,
            &[
                "version",
                "set-description",
                "file.txt",
                &second_id,
                "--description",
                "corrected",
                "-m",
                "correct context",
            ],
        )
        .status
        .success()
    );
    assert!(
        synck(
            root,
            &["version", "demote", "file.txt", &second_id, "-m", "demote",],
        )
        .status
        .success()
    );
    assert!(
        String::from_utf8(synck(root, &["version", "list", "file.txt"]).stdout)
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("0.02 ")
    );
    assert!(
        synck(
            root,
            &[
                "version", "promote", "file.txt", &second_id, "-m", "promote",
            ],
        )
        .status
        .success()
    );
    assert!(
        String::from_utf8(synck(root, &["version", "list", "file.txt"]).stdout)
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("1.00 ")
    );

    assert!(
        synck(root, &["version", "restore", "file.txt", &initial_id],)
            .status
            .success()
    );
    assert_eq!(fs::read(root.join("file.txt")).unwrap(), b"content");
    assert!(synck(root, &["restore-tree"]).status.success());
    assert!(
        synck(
            root,
            &[
                "version",
                "tombstone",
                "file.txt",
                &second_id,
                "-m",
                "remove label",
            ],
        )
        .status
        .success()
    );
    let all =
        String::from_utf8(synck(root, &["version", "list", "file.txt", "--all"]).stdout).unwrap();
    assert!(all.contains(" TOMBSTONED "));
    assert!(synck(root, &["fsck"]).status.success());
}

#[test]
fn cli_restore_revert_and_historical_export_surfaces() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("Folder")).unwrap();
    fs::write(root.join("Folder/document.bin"), b"first").unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    let initial = synck(root, &["commit", "-m", "initial"]);
    let initial = String::from_utf8(initial.stdout)
        .unwrap()
        .trim()
        .strip_prefix("committed ")
        .unwrap()
        .to_string();

    fs::write(root.join("Folder/document.bin"), b"dirty").unwrap();
    fs::write(root.join("added.bin"), b"added").unwrap();
    let restored = synck(root, &["restore-tree"]);
    assert!(restored.status.success());
    assert_eq!(
        String::from_utf8(restored.stdout).unwrap(),
        "restored working tree\n"
    );
    assert_eq!(
        fs::read(root.join("Folder/document.bin")).unwrap(),
        b"first"
    );
    assert!(!root.join("added.bin").exists());

    fs::write(root.join("Folder/document.bin"), b"second").unwrap();
    let edited = synck(root, &["commit", "-m", "edit"]);
    let edited = String::from_utf8(edited.stdout)
        .unwrap()
        .trim()
        .strip_prefix("committed ")
        .unwrap()
        .to_string();
    let reverted = synck(root, &["revert", &edited, "-m", "revert edit"]);
    assert!(
        reverted.status.success(),
        "{}",
        String::from_utf8_lossy(&reverted.stderr)
    );
    assert_eq!(
        fs::read(root.join("Folder/document.bin")).unwrap(),
        b"first"
    );

    let destination = temporary.path().join("historical");
    let exported = synck(
        root,
        &[
            "export-tree",
            &initial,
            destination.to_str().unwrap(),
            "--folder",
            "Folder",
        ],
    );
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert_eq!(
        fs::read(destination.join("document.bin")).unwrap(),
        b"first"
    );
    fs::remove_dir_all(&destination).unwrap();
}

fn assert_fsck_integrity_failure(root: &std::path::Path, message: &str) {
    let output = synck(root, &["fsck"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn install_unreachable_object(root: &std::path::Path, bytes: &[u8]) {
    let digest = Sha256::digest(bytes).to_vec();
    let connection = rusqlite::Connection::open(root.join(".synck/object-store.sqlite3")).unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys = ON; BEGIN IMMEDIATE")
        .unwrap();
    connection
        .execute(
            "INSERT INTO chunks(digest, raw_size, codec, payload) VALUES (?1, ?2, 'RAW', ?3)",
            rusqlite::params![&digest, bytes.len() as i64, bytes],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO blobs(digest, size_bytes, complete) VALUES (?1, ?2, 1)",
            rusqlite::params![&digest, bytes.len() as i64],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO blob_chunks(blob_digest, ordinal, chunk_digest) VALUES (?1, 0, ?1)",
            [&digest],
        )
        .unwrap();
    connection.execute_batch("COMMIT").unwrap();
}

fn corrupt_first_raw_chunk(database: &std::path::Path) {
    let connection = rusqlite::Connection::open(database).unwrap();
    let (digest, mut payload): (Vec<u8>, Vec<u8>) = connection
        .query_row(
            "SELECT digest, payload FROM chunks WHERE codec = 'RAW' ORDER BY digest LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    payload[0] ^= 0x5a;
    connection
        .execute(
            "UPDATE chunks SET payload = ?1 WHERE digest = ?2",
            rusqlite::params![payload, digest],
        )
        .unwrap();
}

#[test]
fn storage_and_gc_print_exact_object_totals() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );

    let ignore_size = fs::metadata(root.join(".synckignore")).unwrap().len();
    let output = synck(root, &["storage"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "referenced_objects 1\nreferenced_bytes {ignore_size}\nunreachable_objects 0\nunreachable_bytes 0\n"
        )
    );

    fs::write(root.join("file"), b"content").unwrap();
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());
    let output = synck(root, &["storage"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "referenced_objects 2\nreferenced_bytes {}\nunreachable_objects 0\nunreachable_bytes 0\n",
            ignore_size + 7
        )
    );

    let orphan_size = 6;
    install_unreachable_object(root, b"orphan");
    let output = synck(root, &["storage"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "referenced_objects 2\nreferenced_bytes {}\nunreachable_objects 1\nunreachable_bytes {orphan_size}\n",
            ignore_size + 7
        )
    );

    let output = synck(root, &["gc"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("removed_objects 1\nremoved_bytes {orphan_size}\n")
    );
    let output = synck(root, &["storage"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "referenced_objects 2\nreferenced_bytes {}\nunreachable_objects 0\nunreachable_bytes 0\n",
            ignore_size + 7
        )
    );
}

#[test]
fn storage_optimize_help_and_json_contract_are_explicit() {
    let temporary = tempfile::tempdir().unwrap();
    let help = synck(temporary.path(), &["storage", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("optimize"));

    let rejected = synck(temporary.path(), &["--json", "storage", "optimize"]);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("--json is not supported for this command")
    );
    assert!(!temporary.path().join(".synck").exists());
}

#[test]
fn cli_smoke_and_fsck_exit_status() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join("file.txt"), b"content").unwrap();
    fs::write(temporary.path().join(".DS_Store"), b"metadata").unwrap();
    fs::write(temporary.path().join("._file.txt"), b"metadata").unwrap();
    fs::write(temporary.path().join(".private-notes"), b"hidden").unwrap();
    fs::create_dir(temporary.path().join(".obsidian")).unwrap();
    fs::write(
        temporary.path().join(".obsidian/workspace.json"),
        b"metadata",
    )
    .unwrap();
    fs::create_dir(temporary.path().join("__MACOSX")).unwrap();
    fs::write(temporary.path().join("__MACOSX/archive"), b"metadata").unwrap();
    fs::write(temporary.path().join("Thumbs.db"), b"metadata").unwrap();
    fs::write(temporary.path().join("ehthumbs.db"), b"metadata").unwrap();
    fs::write(temporary.path().join("ehthumbs_vista.db"), b"metadata").unwrap();
    fs::write(temporary.path().join("desktop.ini"), b"metadata").unwrap();
    fs::create_dir(temporary.path().join("$RECYCLE.BIN")).unwrap();
    fs::write(temporary.path().join("$RECYCLE.BIN/recycled"), b"metadata").unwrap();
    fs::create_dir(temporary.path().join("System Volume Information")).unwrap();
    fs::write(
        temporary.path().join("System Volume Information/index"),
        b"metadata",
    )
    .unwrap();
    fs::write(temporary.path().join("~$draft.docx"), b"temporary").unwrap();

    let output = synck(temporary.path(), &["init", "--author", "author"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let ignore = fs::read_to_string(temporary.path().join(".synckignore")).unwrap();
    for rule in ["~$*", "*.swp", "node_modules/", "__pycache__/", "Thumbs.db"] {
        assert!(ignore.contains(rule));
    }
    // Ordinary folder names in knowledge work stay visible.
    for rule in ["target/", "dist/", "build/"] {
        assert!(!ignore.lines().any(|line| line == rule), "{rule}");
    }
    let initialized = String::from_utf8(synck(temporary.path(), &["show", "HEAD"]).stdout).unwrap();
    assert!(initialized.ends_with("A .synckignore\n"));
    let output = synck(temporary.path(), &["status"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "A file.txt\n");
    let output = synck(temporary.path(), &["commit", "-m", "initial"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = synck(temporary.path(), &["fsck"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["commits"]
            .as_u64()
            .is_some()
    );

    corrupt_first_raw_chunk(&temporary.path().join(".synck/object-store.sqlite3"));
    let output = synck(temporary.path(), &["fsck"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("chunk digest mismatch"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_init_preserves_an_existing_synckignore() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join(".synckignore"), b"*.private\n").unwrap();
    fs::write(temporary.path().join("notes.private"), b"private").unwrap();

    let output = synck(temporary.path(), &["init", "--author", "author"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read(temporary.path().join(".synckignore")).unwrap(),
        b"*.private\n"
    );
    assert_clean_status(temporary.path());
    let log = String::from_utf8(synck(temporary.path(), &["log", "--oneline"]).stdout).unwrap();
    assert!(log.contains("Initialize Synck repository"));
}

#[test]
fn cli_default_synckignore_excludes_dot_names_at_every_depth() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join(".root-hidden"), b"hidden").unwrap();
    fs::create_dir_all(temporary.path().join("visible/.hidden-directory")).unwrap();
    fs::write(
        temporary.path().join("visible/.hidden-directory/state"),
        b"hidden",
    )
    .unwrap();
    fs::write(temporary.path().join("visible/.hidden-file"), b"hidden").unwrap();
    fs::write(temporary.path().join("visible/document.txt"), b"visible").unwrap();

    let initialized = synck(temporary.path(), &["init", "--author", "author"]);
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    assert_eq!(
        String::from_utf8(synck(temporary.path(), &["status"]).stdout).unwrap(),
        "A visible\nA visible/document.txt\n"
    );
}

#[test]
fn cli_revert_head_appends_inverse_commit_and_validates_inputs() {
    let temporary = committed_repository();
    let invalid = synck(
        temporary.path(),
        &["revert", "head", "-m", "invalid revision"],
    );
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid commit ID"));

    fs::write(temporary.path().join("file.txt"), b"dirty").unwrap();
    let dirty = synck(temporary.path(), &["revert", "HEAD", "-m", "reject dirty"]);
    assert!(!dirty.status.success());
    assert!(String::from_utf8_lossy(&dirty.stderr).contains("clean working tree"));
    fs::write(temporary.path().join("file.txt"), b"content").unwrap();

    let reverted = synck(
        temporary.path(),
        &["revert", "HEAD", "-m", "remove initial import"],
    );
    assert!(
        reverted.status.success(),
        "{}",
        String::from_utf8_lossy(&reverted.stderr)
    );
    assert!(String::from_utf8_lossy(&reverted.stdout).starts_with("committed sha256:"));
    assert!(!temporary.path().join("file.txt").exists());
    assert_clean_status(temporary.path());
    assert_eq!(
        String::from_utf8(synck(temporary.path(), &["log", "--oneline"]).stdout)
            .unwrap()
            .lines()
            .count(),
        3
    );
    let deleted = String::from_utf8(synck(temporary.path(), &["deleted"]).stdout).unwrap();
    assert!(deleted.contains(" FILE "));
    assert!(deleted.ends_with(" file.txt\n"));
    assert!(synck(temporary.path(), &["fsck"]).status.success());
}

#[test]
fn cli_exports_committed_file_bytes_without_creating_history() {
    let repository = committed_repository();
    let output = tempfile::tempdir().unwrap();
    let destination = output.path().join("exported.txt");

    let exported = synck(
        repository.path(),
        &["export", "file.txt", destination.to_str().unwrap()],
    );

    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert_eq!(fs::read(&destination).unwrap(), b"content");
    assert_eq!(
        String::from_utf8(exported.stdout).unwrap(),
        format!("exported file.txt to {}\n", destination.to_string_lossy())
    );
    assert_clean_status(repository.path());
    assert_eq!(
        String::from_utf8(synck(repository.path(), &["log", "--oneline"]).stdout)
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn cli_mv_preserves_document_history() {
    let temporary = tempfile::tempdir().unwrap();
    fs::create_dir(temporary.path().join("documents")).unwrap();
    fs::write(temporary.path().join("documents/draft.txt"), b"first").unwrap();
    assert!(
        synck(temporary.path(), &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(
        synck(temporary.path(), &["commit", "-m", "initial"])
            .status
            .success()
    );
    fs::write(temporary.path().join("documents/draft.txt"), b"second").unwrap();
    assert!(
        synck(temporary.path(), &["commit", "-m", "second"])
            .status
            .success()
    );

    let output = synck(
        temporary.path(),
        &[
            "mv",
            "documents/draft.txt",
            "final.txt",
            "-m",
            "finalize document",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("committed sha256:"));
    assert!(!temporary.path().join("documents/draft.txt").exists());
    assert_eq!(
        fs::read(temporary.path().join("final.txt")).unwrap(),
        b"second"
    );
    assert_clean_status(temporary.path());
    let versions = synck(temporary.path(), &["versions", "final.txt"]);
    assert!(versions.status.success());
    assert_eq!(
        String::from_utf8(versions.stdout).unwrap().lines().count(),
        2
    );
    assert!(synck(temporary.path(), &["fsck"]).status.success());

    let recovery = tempfile::tempdir().unwrap();
    let archive = recovery.path().join("archive");
    let restored = recovery.path().join("restored");
    assert!(
        synck(temporary.path(), &["archive", archive.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        synck(
            recovery.path(),
            &[
                "repository",
                "import",
                archive.to_str().unwrap(),
                restored.to_str().unwrap(),
            ],
        )
        .status
        .success()
    );
    assert!(
        synck(
            &restored,
            &["repository", "checkout", ".", "--author", "author"]
        )
        .status
        .success()
    );
    assert_eq!(fs::read(restored.join("final.txt")).unwrap(), b"second");
    assert!(!restored.join("documents/draft.txt").exists());
    assert_eq!(
        String::from_utf8(synck(&restored, &["versions", "final.txt"]).stdout)
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn cli_status_reports_filesystem_rename() {
    let temporary = committed_repository();
    fs::rename(
        temporary.path().join("file.txt"),
        temporary.path().join("renamed.txt"),
    )
    .unwrap();

    let output = synck(temporary.path(), &["status"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "RN file.txt -> renamed.txt\n"
    );
    assert!(
        synck(temporary.path(), &["commit", "-m", "rename"])
            .status
            .success()
    );
    assert_clean_status(temporary.path());
}

#[test]
fn cli_status_colors_only_markers_when_color_is_forced() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("deleted.txt"), b"deleted").unwrap();
    fs::write(root.join("modified.txt"), b"before").unwrap();
    fs::write(root.join("moved.txt"), b"moved").unwrap();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(synck(root, &["commit", "-m", "initial"]).status.success());
    let log = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(root)
        .args(["log", "--limit", "1"])
        .env("CLICOLOR_FORCE", "1")
        .env_remove("NO_COLOR")
        .output()
        .unwrap();
    assert!(log.status.success());
    assert!(log.stderr.is_empty());
    assert!(
        String::from_utf8(log.stdout)
            .unwrap()
            .starts_with("\u{1b}[1m2\u{1b}[0m | ")
    );
    let clean = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(root)
        .args(["context"])
        .env("CLICOLOR_FORCE", "1")
        .env_remove("NO_COLOR")
        .output()
        .unwrap();
    assert!(
        String::from_utf8(clean.stdout)
            .unwrap()
            .contains("working_tree \u{1b}[1m\u{1b}[92mclean\u{1b}[0m\n")
    );

    fs::write(root.join("added.txt"), b"added").unwrap();
    fs::remove_file(root.join("deleted.txt")).unwrap();
    fs::write(root.join("modified.txt"), b"after").unwrap();
    fs::rename(root.join("moved.txt"), root.join("renamed.txt")).unwrap();
    fs::write(root.join("renamed.txt"), b"moved and modified").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(root)
        .args(["status"])
        .env("CLICOLOR_FORCE", "1")
        .env_remove("NO_COLOR")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "\u{1b}[1m\u{1b}[94mA\u{1b}[0m    added.txt\n",
            "\u{1b}[1m\u{1b}[91mD\u{1b}[0m    deleted.txt\n",
            "\u{1b}[1m\u{1b}[93mM\u{1b}[0m    modified.txt\n",
            "\u{1b}[1m\u{1b}[93mM+RN\u{1b}[0m moved.txt -> renamed.txt\n",
        )
    );
}

#[cfg(unix)]
#[test]
fn reused_allocation_identity_is_delete_plus_add() {
    use std::os::unix::fs::MetadataExt;

    let temporary = committed_repository();
    let root = temporary.path();
    fs::remove_file(root.join("file.txt")).unwrap();
    fs::write(root.join("unrelated.bin"), b"unrelated bytes").unwrap();
    let metadata = fs::metadata(root.join("unrelated.bin")).unwrap();
    let connection = rusqlite::Connection::open(root.join(".synck/repository.local.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE local_worktree_identities
             SET device_id = ?1, file_id = ?2, birth_seconds = ?3, birth_nanos = 0",
            rusqlite::params![
                metadata.dev().to_be_bytes().as_slice(),
                metadata.ino().to_be_bytes().as_slice(),
                i64::MIN + 1,
            ],
        )
        .unwrap();
    drop(connection);

    assert_eq!(
        String::from_utf8(synck(root, &["status"]).stdout).unwrap(),
        "D file.txt\nA unrelated.bin\n"
    );
    assert_eq!(
        json_stdout(root, &["status", "--json"])["changes"],
        serde_json::json!([
            {"kind": "DELETE", "path": "file.txt", "source_path": null, "content_changed": false},
            {"kind": "ADD", "path": "unrelated.bin", "source_path": null, "content_changed": false}
        ])
    );
    assert!(
        synck(root, &["commit", "-m", "replace file"])
            .status
            .success()
    );
    let deleted = String::from_utf8(synck(root, &["deleted"]).stdout).unwrap();
    assert!(deleted.ends_with(" file.txt\n"), "{deleted}");
    let history = String::from_utf8(synck(root, &["history", "unrelated.bin"]).stdout).unwrap();
    assert!(history.contains("A unrelated.bin\n"), "{history}");
    assert!(!history.contains("file.txt"), "{history}");
}

#[test]
fn cli_lists_and_recovers_deleted_items() {
    let temporary = committed_repository();
    fs::remove_file(temporary.path().join("file.txt")).unwrap();
    assert!(
        synck(temporary.path(), &["commit", "-m", "delete file"])
            .status
            .success()
    );

    let deleted = synck(temporary.path(), &["deleted"]);
    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    let row = String::from_utf8(deleted.stdout).unwrap();
    let fields = row.trim_end().splitn(6, ' ').collect::<Vec<_>>();
    assert_eq!(fields.len(), 6);
    assert_eq!(fields[1], "FILE");
    assert_eq!(fields[5], "file.txt");
    assert!(fields[3].starts_with("sha256:"));

    let recovered = synck(
        temporary.path(),
        &["recover", fields[0], "-m", "recover file"],
    );
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(String::from_utf8_lossy(&recovered.stdout).starts_with("committed sha256:"));
    assert_eq!(
        fs::read(temporary.path().join("file.txt")).unwrap(),
        b"content"
    );
    assert!(synck(temporary.path(), &["deleted"]).stdout.is_empty());
    assert_clean_status(temporary.path());
    assert!(
        synck(temporary.path(), &["history", "file.txt"])
            .status
            .success()
    );
    assert!(synck(temporary.path(), &["fsck"]).status.success());
}

#[test]
fn cli_show_and_history_render_structural_changes() {
    let temporary = tempfile::tempdir().unwrap();
    fs::create_dir(temporary.path().join("documents")).unwrap();
    fs::write(temporary.path().join("documents/draft.txt"), b"first").unwrap();
    assert!(
        synck(temporary.path(), &["init", "--author", "author"])
            .status
            .success()
    );
    assert!(
        synck(temporary.path(), &["commit", "-m", "initial"])
            .status
            .success()
    );
    fs::rename(
        temporary.path().join("documents/draft.txt"),
        temporary.path().join("documents/final.txt"),
    )
    .unwrap();
    let committed = synck(temporary.path(), &["commit", "-m", "rename"]);
    assert!(committed.status.success());
    let commit_id = String::from_utf8(committed.stdout)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();

    let shown = synck(temporary.path(), &["show", &commit_id]);
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let shown = String::from_utf8(shown.stdout).unwrap();
    assert!(shown.lines().next().unwrap().contains(&commit_id));
    assert!(shown.ends_with("RN documents/draft.txt -> documents/final.txt\n"));

    let history = synck(temporary.path(), &["history", "documents/final.txt"]);
    assert!(
        history.status.success(),
        "{}",
        String::from_utf8_lossy(&history.stderr)
    );
    let history = String::from_utf8(history.stdout).unwrap();
    assert_eq!(history.lines().count(), 4);
    assert!(history.contains("A documents/draft.txt\n"));
    assert!(history.contains("RN documents/draft.txt -> documents/final.txt\n"));

    let invalid = synck(temporary.path(), &["show", "invalid"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid commit ID"));
}

#[test]
fn fsck_reports_noncanonical_commit_timestamp() {
    let temporary = committed_repository();
    let connection =
        rusqlite::Connection::open(temporary.path().join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute("UPDATE vcs_commits SET created_at = 'not-a-timestamp'", [])
        .unwrap();
    drop(connection);

    assert_fsck_integrity_failure(temporary.path(), "created_at is invalid");
}

#[test]
fn fsck_json_reports_integrity_errors() {
    let temporary = committed_repository();
    let connection =
        rusqlite::Connection::open(temporary.path().join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute("UPDATE vcs_commits SET created_at = 'not-a-timestamp'", [])
        .unwrap();
    drop(connection);

    let output = synck(temporary.path(), &["fsck", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let report: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(report["error"]["kind"], "INTEGRITY");
    assert!(
        report["error"]["message"]
            .as_str()
            .unwrap()
            .contains("created_at is invalid")
    );
}

#[test]
fn fsck_bounds_integrity_diagnostics() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    assert!(
        synck(root, &["init", "--author", "author"])
            .status
            .success()
    );
    for index in 0..101 {
        fs::create_dir(root.join(format!("folder-{index}"))).unwrap();
    }
    assert!(synck(root, &["commit", "-m", "folders"]).status.success());
    let db = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    db.execute("UPDATE vcs_items SET created_at='invalid'", [])
        .unwrap();
    drop(db);
    let output = synck(root, &["fsck"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("additional finding(s) omitted"));
}

#[test]
fn fsck_reports_non_uuidv7_commit_key() {
    let temporary = committed_repository();
    let connection =
        rusqlite::Connection::open(temporary.path().join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             BEGIN IMMEDIATE;
             UPDATE vcs_events
             SET commit_pk = '00000000-0000-4000-8000-000000000000'
             WHERE commit_pk = (SELECT commit_pk FROM vcs_commits WHERE repo_seq = 2);
             UPDATE vcs_ref_events
             SET commit_pk = '00000000-0000-4000-8000-000000000000'
             WHERE commit_pk = (SELECT commit_pk FROM vcs_commits WHERE repo_seq = 2);
             UPDATE vcs_commits
             SET commit_pk = '00000000-0000-4000-8000-000000000000'
             WHERE repo_seq = 2;
             COMMIT;",
        )
        .unwrap();
    drop(connection);

    assert_fsck_integrity_failure(temporary.path(), "invalid commit key");
}

#[test]
fn fsck_reports_invalid_commit_format_despite_writer_constraints() {
    let temporary = committed_repository();
    let connection =
        rusqlite::Connection::open(temporary.path().join(".synck/repository.sqlite3")).unwrap();
    assert!(
        connection
            .execute("UPDATE vcs_commits SET format_version = 2147483648", [])
            .is_err()
    );
    // Simulate damaged storage, bypassing the writer's format constraint.
    connection.execute_batch("PRAGMA ignore_check_constraints = ON; UPDATE vcs_commits SET format_version = 2147483648;").unwrap();
    drop(connection);
    assert_fsck_integrity_failure(temporary.path(), "outside the signed 32-bit range");
}

#[test]
fn fsck_reports_schema_drift_as_integrity_failure() {
    let temporary = committed_repository();
    let connection =
        rusqlite::Connection::open(temporary.path().join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute_batch("DROP INDEX idx_vcs_items_unique_name_key")
        .unwrap();
    drop(connection);

    assert_fsck_integrity_failure(temporary.path(), "schema definition");
}

#[test]
fn fsck_rebuild_state_index_replaces_corrupt_derived_nodes() {
    let temporary = committed_repository();
    assert!(synck(temporary.path(), &["fsck", "--rebuild-state-index"]).status.success());
    let database = temporary.path().join(".synck/repository.state.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    let state_root: Vec<u8> = connection
        .query_row(
            "SELECT state_root FROM vcs_state_projection_heads",
            [],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE vcs_state_nodes SET payload = x'00' WHERE node_hash = ?1",
            [&state_root],
        )
        .unwrap();
    drop(connection);

    let broken = synck(temporary.path(), &["fsck"]);
    assert!(broken.status.success(), "a cache cannot invalidate canonical history: {}", String::from_utf8_lossy(&broken.stderr));

    let rebuilt = synck(temporary.path(), &["fsck", "--rebuild-state-index"]);
    assert!(
        rebuilt.status.success(),
        "{}",
        String::from_utf8_lossy(&rebuilt.stderr)
    );
    assert!(
        serde_json::from_slice::<Value>(&rebuilt.stdout).unwrap()["commits"]
            .as_u64()
            .is_some()
    );
}

#[test]
fn fsck_ignores_unreachable_state_nodes() {
    let temporary = committed_repository();
    assert!(synck(temporary.path(), &["fsck", "--rebuild-state-index"]).status.success());
    let baseline = json_stdout(temporary.path(), &["fsck"]);
    let database = temporary.path().join(".synck/repository.state.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    let repo_id: String = connection
        .query_row(
            "SELECT repo_id FROM vcs_state_projection_heads",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let payload = b"unreachable and deliberately not a state-node encoding";
    let digest: [u8; 32] = Sha256::digest(payload).into();
    connection
        .execute(
            "INSERT INTO vcs_state_nodes(repo_id, node_hash, payload)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![repo_id, digest.as_slice(), payload],
        )
        .unwrap();
    drop(connection);

    let output = synck(temporary.path(), &["fsck"]);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        baseline
    );
}

#[test]
fn missing_state_head_discards_residual_rows_and_reprojects() {
    let temporary = committed_repository();
    assert!(synck(temporary.path(), &["fsck", "--rebuild-state-index"]).status.success());
    let database = temporary.path().join(".synck/repository.state.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute("DELETE FROM vcs_state_projection_heads", [])
        .unwrap();
    drop(connection);

    let output = synck(temporary.path(), &["fsck", "--rebuild-state-index"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let connection = rusqlite::Connection::open(database).unwrap();
    let (heads, roots): (i64, i64) = connection
        .query_row(
            "SELECT
                (SELECT count(*) FROM vcs_state_projection_heads),
                (SELECT count(*) FROM vcs_state_roots)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((heads, roots), (1, 2));
}

#[test]
fn fsck_reports_stale_repository_activity_head() {
    let temporary = committed_repository();
    let connection =
        rusqlite::Connection::open(temporary.path().join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute("UPDATE vcs_repo_activity_heads SET next_repo_seq = 1", [])
        .unwrap();
    drop(connection);

    assert_fsck_integrity_failure(
        temporary.path(),
        "next sequence 1 does not follow maximum committed sequence 2",
    );
}

#[test]
fn fsck_reports_missing_repository_activity_head() {
    let temporary = committed_repository();
    let connection =
        rusqlite::Connection::open(temporary.path().join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute("DELETE FROM vcs_repo_activity_heads", [])
        .unwrap();
    drop(connection);

    assert_fsck_integrity_failure(
        temporary.path(),
        "activity head is missing after sequence 2",
    );
}

#[test]
fn fsck_rejects_an_allocator_ahead_of_history() {
    let temporary = committed_repository();
    let connection =
        rusqlite::Connection::open(temporary.path().join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute("UPDATE vcs_repo_activity_heads SET next_repo_seq = 4", [])
        .unwrap();
    drop(connection);

    assert_fsck_integrity_failure(
        temporary.path(),
        "repository allocator does not follow canonical history",
    );
}

#[test]
fn history_readers_reject_repository_sequence_gaps() {
    let temporary = committed_repository();
    let root = temporary.path();
    let db = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    let (id, time): (Vec<u8>, String) = db
        .query_row(
            "SELECT commit_id,created_at FROM vcs_commits WHERE repo_seq=2",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let previous: Vec<u8> = db
        .query_row(
            "SELECT checkpoint FROM vcs_checkpoints WHERE repo_seq=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let id = synck_vcs::types::commit_id_from_digest_bytes(&id).unwrap();
    let checkpoint = synck_vcs::checkpoint::digest(
        &previous.try_into().unwrap(),
        3,
        &id,
        synck_vcs_sqlite::parse_timestamp(&time).unwrap(),
    )
    .unwrap();
    // Keep the digest correct: the missing position itself must be rejected.
    db.execute("UPDATE vcs_commits SET repo_seq=3 WHERE repo_seq=2", [])
        .unwrap();
    db.execute(
        "UPDATE vcs_checkpoints SET repo_seq=3,checkpoint=?1 WHERE repo_seq=2",
        [checkpoint.as_slice()],
    )
    .unwrap();
    db.execute("UPDATE vcs_repo_activity_heads SET next_repo_seq=4", [])
        .unwrap();
    drop(db);
    for args in [
        vec!["log"],
        vec!["--json", "log"],
        vec!["show", "3"],
        vec!["fsck"],
    ] {
        let output = synck(root, &args);
        assert!(!output.status.success(), "{args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("consecutive") || error.contains("checkpoint placement"),
            "{error}"
        );
    }
}

#[test]
fn history_reads_refresh_stale_checkout_cache() {
    for args in [
        vec!["--json", "log"],
        vec!["show", "HEAD", "--events", "--json"],
        vec!["compare", "2", "3"],
        vec!["history", "file.txt"],
    ] {
        let temporary = committed_repository();
        let root = temporary.path();
        fs::write(root.join("file.txt"), b"changed").unwrap();
        let committed = synck(root, &["commit", "-m", "second"]);
        assert!(committed.status.success(), "{:?}", committed);
        let output = synck(root, &args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if args.contains(&"--json") {
            serde_json::from_slice::<Value>(&output.stdout).unwrap();
        }
        let db = rusqlite::Connection::open(root.join(".synck/repository.state.sqlite3")).unwrap();
        let applied: i64 = db
            .query_row(
                "SELECT applied_repo_seq FROM vcs_state_projection_heads",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(applied, 3, "{args:?}");
    }
}

#[cfg(unix)]
#[test]
fn fix_names_initializes_and_commits_selected_renamed_paths() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("folder?")).unwrap();
    fs::write(root.join("folder?/NUL.txt"), b"initial").unwrap();
    let output = synck(root, &["init", "--author", "author", "--fix-names"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stderr).contains("folder_/_NUL.txt"));
    assert_eq!(fs::read(root.join("folder_/_NUL.txt")).unwrap(), b"initial");
    fs::write(root.join("bad?name"), b"selected").unwrap();
    let output = synck(root, &["commit", "--fix-names", "bad?name", "-m", "selected", "--json"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["renamed"][0]["from"], "bad?name");
    assert_eq!(value["renamed"][0]["to"], "bad_name");
    assert!(value["commit_id"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(fs::read(root.join("bad_name")).unwrap(), b"selected");
    assert!(synck(root, &["status"]).status.success());
}

#[test]
fn commit_format_limits_survive_portable_writes_copy_and_fsck() {
    use synck_vcs::limits::*;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    assert!(synck(temp.path(), &["init", root.to_str().unwrap(), "--author", "author"]).status.success());
    fs::write(root.join("file.txt"), b"one").unwrap();
    let message = "é".repeat(MAX_MESSAGE_BYTES / 2);
    let output = synck(&root, &["commit", "-m", &message]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let source = temp.path().join("input");
    fs::write(&source, b"two").unwrap();
    let output = synck(&root, &["repository", "create-file", "two.txt", source.to_str().unwrap(), "--path", ".", "-m", &message]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let archive = temp.path().join("archive");
    let copy = temp.path().join("copy");
    for args in [vec!["fsck"], vec!["archive", archive.to_str().unwrap()],
        vec!["repository", "import", archive.to_str().unwrap(), copy.to_str().unwrap()],
        vec!["repository", "verify", copy.to_str().unwrap()]] {
        let output = synck(&root, &args);
        assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }
    fs::write(root.join("file.txt"), b"edited").unwrap();
    let output = synck(&root, &["commit", "-m", &(message+"x")]);
    assert!(!output.status.success());

    let db = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    for (field, maximum) in [("message",MAX_MESSAGE_BYTES), ("author_id",MAX_ACTOR_ID_BYTES),
        ("committer_id",MAX_ACTOR_ID_BYTES), ("operation_id",MAX_OPERATION_ID_BYTES)] {
        let original: Option<String> = db.query_row(&format!("SELECT {field} FROM vcs_commits ORDER BY repo_seq DESC LIMIT 1"), [], |r| r.get(0)).unwrap();
        db.execute(&format!("UPDATE vcs_commits SET {field}=?1 WHERE repo_seq=(SELECT max(repo_seq) FROM vcs_commits)"), ["x".repeat(maximum+1)]).unwrap();
        for args in [vec!["log"], vec!["fsck"]] {
            let output = synck(&root, &args);
            assert!(!output.status.success(), "{field}: {args:?}");
            let output = format!("{}{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
            assert!(output.contains("format byte limit"), "{field}: {output}");
        }
        db.execute(&format!("UPDATE vcs_commits SET {field}=?1 WHERE repo_seq=(SELECT max(repo_seq) FROM vcs_commits)"), [original]).unwrap();
    }
}
