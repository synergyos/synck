//! Two or more HTTPS clones on one server, with failures put where they hurt:
//! a server restart between upload pages, and a client killed after the
//! server accepted its commit but before the receipt reached it.

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::clients::{fixture, json, ok, synck};

/// Armed: the pattern and how many matching requests still pass.
type Armed = Arc<Mutex<Option<(&'static [u8], usize)>>>;

/// Forwards connections to `upstream`. Once armed with a pattern, it swallows
/// the response to a request containing it: the server handles the request
/// and the client never hears back.
struct Proxy {
    address: SocketAddr,
    armed: Armed,
    swallowed: Arc<AtomicBool>,
}

impl Proxy {
    fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let armed: Armed = Arc::new(Mutex::new(None));
        let swallowed = Arc::new(AtomicBool::new(false));
        let (armed_all, swallowed_all) = (armed.clone(), swallowed.clone());
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                // A stopped server is a refused connection, as it would be.
                let Ok(server) = TcpStream::connect(upstream) else {
                    continue;
                };
                let mute = Arc::new(AtomicBool::new(false));
                let (armed, request_mute) = (armed_all.clone(), mute.clone());
                let (from, to) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                std::thread::spawn(move || {
                    forward(from, to, |chunk| {
                        let mut armed = armed.lock().unwrap();
                        if let Some((pattern, pass)) = armed.as_mut()
                            && chunk.windows(pattern.len()).any(|bytes| bytes == *pattern)
                        {
                            if *pass == 0 {
                                *armed = None;
                                request_mute.store(true, Ordering::SeqCst);
                            } else {
                                *pass -= 1;
                            }
                        }
                        true
                    })
                });
                let swallowed = swallowed_all.clone();
                std::thread::spawn(move || {
                    forward(server, client, |_| {
                        if mute.load(Ordering::SeqCst) {
                            swallowed.store(true, Ordering::SeqCst);
                            return false;
                        }
                        true
                    })
                });
            }
        });
        Self {
            address,
            armed,
            swallowed,
        }
    }

    /// Swallow the response to the request after `pass` matching ones.
    fn arm(&self, pattern: &'static [u8], pass: usize) {
        self.swallowed.store(false, Ordering::SeqCst);
        *self.armed.lock().unwrap() = Some((pattern, pass));
    }

    /// Wait until the server has answered the armed request.
    fn wait_swallowed(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self.swallowed.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the armed request never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Copy `from` to `to` while `pass` lets each chunk through.
fn forward(mut from: TcpStream, mut to: TcpStream, mut pass: impl FnMut(&[u8]) -> bool) {
    let mut buffer = vec![0; 64 * 1024];
    while let Ok(count) = from.read(&mut buffer) {
        if count == 0 || (pass(&buffer[..count]) && to.write_all(&buffer[..count]).is_err()) {
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

fn spawn(directory: &Path, token: &str, arguments: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_synck"))
        .current_dir(directory)
        .env("SYNCK_TOKEN", token)
        .args(arguments)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn kill(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

fn held(directory: &Path, token: &str) -> usize {
    json(directory, token, &["status"])["held_commits"]
        .as_array()
        .map_or(0, Vec::len)
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn error_kind(directory: &Path, token: &str, arguments: &[&str]) -> String {
    let mut arguments = arguments.to_vec();
    arguments.insert(0, "--json");
    let output = synck(directory, token, &arguments);
    assert!(!output.status.success(), "{arguments:?} succeeded");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    error["error"]["kind"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn clones_commit_different_items_and_meet_over_one() {
    let fixture = fixture().await;
    let token = &fixture.writer;
    let laptop = fixture.clone("laptop", false);
    let phone = fixture.clone("phone", false);
    let draft = fixture.clone("draft", true);

    // Different items: each commit lands, built on the other clone's.
    std::fs::write(laptop.join("a.txt"), "from the laptop\n").unwrap();
    ok(&laptop, token, &["commit", "-m", "Add a"]);
    std::fs::write(phone.join("b.txt"), "from the phone\n").unwrap();
    ok(&phone, token, &["commit", "-m", "Add b"]);
    assert_eq!(read(phone.join("a.txt")), "from the laptop\n");
    ok(&fixture.origin, "", &["pull"]);
    assert_eq!(read(fixture.origin.join("a.txt")), "from the laptop\n");
    assert_eq!(read(fixture.origin.join("b.txt")), "from the phone\n");

    // One item from two clones: the later commit stops, and pull merges.
    std::fs::write(laptop.join("brief.txt"), "the laptop's draft\n").unwrap();
    ok(&laptop, token, &["commit", "-m", "Laptop draft"]);
    std::fs::write(phone.join("brief.txt"), "the phone's draft\n").unwrap();
    assert_eq!(
        error_kind(&phone, token, &["commit", "-m", "Phone draft"]),
        "CONFLICT"
    );
    ok(&phone, token, &["pull"]);
    assert!(read(phone.join("brief.txt")).contains("<<<<<<< ours"));
    assert!(
        !json(&phone, token, &["status"])["conflicts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    std::fs::write(phone.join("brief.txt"), "both drafts, merged\n").unwrap();
    ok(&phone, token, &["resolve", "brief.txt"]);
    ok(&phone, token, &["commit", "-m", "Merge the drafts"]);
    assert_eq!(held(&phone, token), 0);
    ok(&fixture.origin, "", &["pull"]);
    assert_eq!(
        read(fixture.origin.join("brief.txt")),
        "both drafts, merged\n"
    );

    // A held commit to an item the server changed since: push merges it
    // back into the working files and sends nothing until it is committed.
    std::fs::write(draft.join("brief.txt"), "a held draft\n").unwrap();
    ok(&draft, token, &["commit", "-m", "Held draft"]);
    assert!(ok(&draft, token, &["info"]).starts_with("clone of http://"));
    assert!(ok(&draft, token, &["status"]).contains("1 commit waiting to be sent"));
    let head = json(&fixture.origin, "", &["info"])["head"].clone();
    assert_eq!(error_kind(&draft, token, &["push"]), "CONFLICT");
    assert_eq!(held(&draft, token), 0);
    assert!(read(draft.join("brief.txt")).contains("a held draft"));
    assert_eq!(json(&fixture.origin, "", &["info"])["head"], head);
    std::fs::write(draft.join("brief.txt"), "merged with the held draft\n").unwrap();
    ok(&draft, token, &["resolve", "brief.txt"]);
    ok(&draft, token, &["commit", "-m", "Merge the held draft"]);
    ok(&draft, token, &["push"]);
    ok(&fixture.origin, "", &["pull"]);
    assert_eq!(
        read(fixture.origin.join("brief.txt")),
        "merged with the held draft\n"
    );
    ok(&draft, token, &["fsck"]);
}

#[tokio::test]
async fn an_upload_resumes_after_the_server_restarts() {
    let mut fixture = fixture().await;
    let address = fixture.address();
    let proxy = Proxy::start(address);
    fixture.url = fixture
        .url
        .replace(&address.to_string(), &proxy.address.to_string());
    let token = fixture.writer.clone();
    let laptop = fixture.clone("laptop", true);
    // A folder and 513 files: two pages, at offsets 0 and 512.
    std::fs::create_dir(laptop.join("notes")).unwrap();
    for index in 0..513 {
        std::fs::write(laptop.join(format!("notes/{index}.txt")), "a note\n").unwrap();
    }
    ok(&laptop, &token, &["commit", "-m", "Add the notes"]);

    proxy.arm(b"/pages/512 HTTP/1.1", 0);
    let push = spawn(&laptop, &token, &["push"]);
    proxy.wait_swallowed();
    kill(push);
    fixture.stop();
    let state = fixture.state.join("state.sqlite3");
    let staged = || -> i64 {
        rusqlite::Connection::open(&state)
            .unwrap()
            .query_row("SELECT count(*) FROM upload_pages", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(staged(), 2);

    fixture.restart(address);
    ok(&laptop, &token, &["push"]);
    assert_eq!(held(&laptop, &token), 0);
    assert_eq!(staged(), 0);
    ok(&fixture.origin, "", &["pull"]);
    assert_eq!(
        std::fs::read_dir(fixture.origin.join("notes"))
            .unwrap()
            .count(),
        513
    );
    ok(&fixture.origin, "", &["fsck"]);
}

#[tokio::test]
async fn a_client_killed_before_its_receipt_finds_its_commit_accepted() {
    let mut fixture = fixture().await;
    let proxy = Proxy::start(fixture.address());
    fixture.url = fixture
        .url
        .replace(&fixture.address().to_string(), &proxy.address.to_string());
    let token = fixture.writer.clone();
    let phone = fixture.clone("phone", true);
    std::fs::write(phone.join("brief.txt"), "second draft\n").unwrap();
    ok(&phone, &token, &["commit", "-m", "Revise the brief"]);

    // The first submission answers OBJECTS_MISSING; the second is accepted.
    proxy.arm(b"/sync/proposals HTTP/1.1", 1);
    let push = spawn(&phone, &token, &["push"]);
    proxy.wait_swallowed();
    kill(push);
    // Accepted, though the clone never heard so.
    ok(&fixture.origin, "", &["pull"]);
    assert_eq!(read(fixture.origin.join("brief.txt")), "second draft\n");
    assert_eq!(held(&phone, &token), 1);

    ok(&phone, &token, &["push"]);
    assert_eq!(held(&phone, &token), 0);
    let log = ok(&fixture.origin, "", &["log"]);
    assert_eq!(log.matches("Revise the brief").count(), 1);
}
