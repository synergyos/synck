//! HTTPS clones against a real server on loopback, through the `synck` binary.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use serde_json::Value;
use synck_local::server::tokens::{self, Access};
use synck_local::server::{Server, router};
use tokio::sync::oneshot;

pub(super) struct Running {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.stop.take().map(|stop| stop.send(()));
        let _ = self.thread.take().map(std::thread::JoinHandle::join);
    }
}

/// Serve on `address` from a thread of its own, since the client blocks.
pub(super) fn start(state: &Path, roots: &[PathBuf], address: SocketAddr) -> (Running, SocketAddr) {
    let listener = std::net::TcpListener::bind(address).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = Server::open(state, roots).unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, router(Arc::new(server)))
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
    });
    (Running { stop: Some(stop), thread: Some(thread) }, address)
}

pub(super) fn synck(directory: &Path, token: &str, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(directory)
        .env("SYNCK_TOKEN", token)
        .args(arguments)
        .output()
        .unwrap()
}

pub(super) fn ok(directory: &Path, token: &str, arguments: &[&str]) -> String {
    let output = synck(directory, token, arguments);
    assert!(output.status.success(), "{arguments:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

pub(super) fn json(directory: &Path, token: &str, arguments: &[&str]) -> Value {
    let mut arguments = arguments.to_vec();
    arguments.insert(0, "--json");
    serde_json::from_str(&ok(directory, token, &arguments)).unwrap()
}

pub(super) struct Fixture {
    pub(super) parent: tempfile::TempDir,
    pub(super) state: PathBuf,
    pub(super) origin: PathBuf,
    pub(super) url: String,
    pub(super) writer: String,
    pub(super) running: Running,
}

pub(super) async fn fixture() -> Fixture {
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("state");
    synck_local::server::init(&state).unwrap();
    let origin = parent.path().join("origin");
    let repo_id = synck_local::init(&origin, "owner").await.unwrap();
    std::fs::write(origin.join("brief.txt"), "first draft\n").unwrap();
    ok(&origin, "", &["commit", "-m", "Add the brief"]);
    let writer = tokens::add(&state, "laptop", Access::Write, &[repo_id.clone()]).unwrap();
    let (running, address) = start(&state, &[origin.clone()], "127.0.0.1:0".parse().unwrap());
    let url = format!("http://{address}/v1/repositories/{}", repo_id.as_str());
    Fixture { parent, state, origin, url, writer, running }
}

impl Fixture {
    /// The server's own address, before any proxy.
    pub(super) fn address(&self) -> SocketAddr {
        self.url["http://".len()..].split('/').next().unwrap().parse().unwrap()
    }

    /// Stop the server once its connections finish.
    pub(super) fn stop(&mut self) {
        drop(std::mem::replace(&mut self.running, Running { stop: None, thread: None }));
    }

    pub(super) fn restart(&mut self, address: SocketAddr) {
        self.running = start(&self.state, &[self.origin.clone()], address).0;
    }

    pub(super) fn clone(&self, name: &str, hold: bool) -> PathBuf {
        let destination = self.parent.path().join(name);
        let mut arguments = vec!["clone", &self.url, destination.to_str().unwrap(), "--author", name];
        if hold {
            arguments.push("--hold");
        }
        ok(self.parent.path(), &self.writer, &arguments);
        destination
    }
}

#[tokio::test]
async fn https_clones_send_hold_push_and_lease() {
    let fixture = fixture().await;
    let token = &fixture.writer;
    let laptop = fixture.clone("laptop", false);
    assert_eq!(std::fs::read_to_string(laptop.join("brief.txt")).unwrap(), "first draft\n");
    let phone = fixture.clone("phone", true);

    // Sent at once, with its commit ID.
    std::fs::write(laptop.join("notes.txt"), "from the laptop\n").unwrap();
    let commit_id = json(&laptop, token, &["commit", "-m", "Add notes"])["commit_id"].clone();
    ok(&fixture.origin, "", &["pull"]);
    assert_eq!(std::fs::read_to_string(fixture.origin.join("notes.txt")).unwrap(), "from the laptop\n");
    let head = json(&fixture.origin, "", &["info"])["head"]["commit_id"].clone();
    assert_eq!(head, commit_id);

    // Held until pushed.
    ok(&phone, token, &["pull"]);
    std::fs::write(phone.join("brief.txt"), "second draft\n").unwrap();
    ok(&phone, token, &["commit", "-m", "Revise the brief"]);
    assert_eq!(json(&phone, token, &["status"])["held_commits"].as_array().unwrap().len(), 1);
    ok(&laptop, token, &["pull"]);
    assert_eq!(std::fs::read_to_string(laptop.join("brief.txt")).unwrap(), "first draft\n");
    ok(&phone, token, &["push"]);
    assert!(json(&phone, token, &["status"])["held_commits"].as_array().unwrap().is_empty());
    ok(&laptop, token, &["pull"]);
    assert_eq!(std::fs::read_to_string(laptop.join("brief.txt")).unwrap(), "second draft\n");
    ok(&laptop, token, &["fsck"]);

    // A server lease fences every clone and the served repository's own tree.
    let lease = json(&laptop, token, &["lock", "acquire", "notes.txt", "--owner", "agent-a"]);
    let lease_token = lease["result"]["token"].as_str().unwrap().to_string();
    let listed = json(&phone, token, &["lock", "list"]);
    assert_eq!(listed["result"]["locks"][0]["owner"], "agent-a");
    ok(&phone, token, &["pull"]);
    std::fs::write(phone.join("notes.txt"), "edited without the lease\n").unwrap();
    ok(&phone, token, &["commit", "-m", "Edit notes"]);
    let refused = synck(&phone, token, &["--json", "push"]);
    assert!(!refused.status.success());
    let error: Value = serde_json::from_slice(&refused.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "LOCKED");
    ok(&phone, token, &["--lock-token", &lease_token, "push"]);
    assert_eq!(json(&laptop, token, &["lock", "release", &lease_token])["result"]["released"], 1);

    // Read tokens read.
    let repo_id = fixture.url.rsplit('/').next().unwrap();
    let reader = tokens::add(
        &fixture.state,
        "viewer",
        Access::Read,
        &[synck_vcs::types::RepoId::parse(repo_id).unwrap()],
    )
    .unwrap();
    let viewer = fixture.parent.path().join("viewer");
    ok(fixture.parent.path(), &reader, &["clone", &fixture.url, viewer.to_str().unwrap(), "--author", "viewer"]);
    assert_eq!(std::fs::read_to_string(viewer.join("notes.txt")).unwrap(), "edited without the lease\n");
}

#[tokio::test]
async fn commits_wait_while_the_server_is_unreachable() {
    let mut fixture = fixture().await;
    let token = fixture.writer.clone();
    let laptop = fixture.clone("laptop", false);
    let address = fixture.address();
    fixture.stop();

    std::fs::write(laptop.join("offline.txt"), "written on a train\n").unwrap();
    let output = synck(&laptop, &token, &["commit", "-m", "Offline"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("held here, not sent"));
    assert_eq!(json(&laptop, &token, &["status"])["held_commits"].as_array().unwrap().len(), 1);
    let failed = synck(&laptop, &token, &["--json", "push"]);
    let error: Value = serde_json::from_slice(&failed.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "UNREACHABLE");

    fixture.restart(address);
    ok(&laptop, &token, &["push"]);
    ok(&fixture.origin, "", &["pull"]);
    assert_eq!(std::fs::read_to_string(fixture.origin.join("offline.txt")).unwrap(), "written on a train\n");
}

#[tokio::test]
async fn an_offline_commit_the_server_overtook_is_withdrawn_and_kept() {
    let mut fixture = fixture().await;
    let token = fixture.writer.clone();
    let laptop = fixture.clone("laptop", false);
    let address = fixture.address();
    fixture.stop();

    std::fs::write(laptop.join("notes.txt"), "notes on a train\n").unwrap();
    let clean = json(&laptop, &token, &["commit", "-m", "Notes"])["commit_id"].clone();
    std::fs::write(laptop.join("brief.txt"), "brief on a train\n").unwrap();
    let mine = json(&laptop, &token, &["commit", "-m", "Brief"])["commit_id"].clone();
    std::fs::write(fixture.origin.join("brief.txt"), "brief in the office\n").unwrap();
    ok(&fixture.origin, "", &["commit", "-m", "Office brief"]);
    fixture.restart(address);

    // The commit before the conflicting one stays held and is sent with its ID.
    let pulled = json(&laptop, &token, &["pull"]);
    assert_eq!(pulled["withdrawn"], serde_json::json!(["w1"]));
    assert_eq!(json(&laptop, &token, &["status"])["held_commits"], serde_json::json!([clean]));
    assert_eq!(json(&laptop, &token, &["withdrawn"])["withdrawn"][0]["commit_id"], mine);

    // Kept with the office's version, the train's bytes survive this clone's gc.
    std::fs::write(laptop.join("brief.txt"), "brief in the office\n").unwrap();
    ok(&laptop, &token, &["resolve", "brief.txt"]);
    assert_eq!(json(&laptop, &token, &["push"])["pushed"], serde_json::json!([clean]));
    ok(&laptop, &token, &["gc"]);
    ok(&laptop, &token, &["withdrawn", "export", "w1", "brief.txt", "../train.txt"]);
    assert_eq!(std::fs::read_to_string(fixture.parent.path().join("train.txt")).unwrap(), "brief on a train\n");
    ok(&laptop, &token, &["fsck"]);
}
