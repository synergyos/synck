use std::sync::Arc;

use axum::Router;
use axum::http::{Method, StatusCode};
use serde_json::Value;
use synck_local::server::Server;
use synck_local::server::tokens::{self, Access};
use synck_sync::{
    Capabilities, Cursor, HistoryPage, HistoryPosition, HistoryRequest, HistoryVerifier, Lease,
    Leases, ProposalRecord, Released, RepositoryIdentity, SubmitResult,
};
use synck_vcs::types::RepoId;

use super::common::{create_file, object_id, send, send_with, status_of, upload};

struct Fixture {
    _parent: tempfile::TempDir,
    router: Router,
    repo_id: RepoId,
    sync: String,
    writer: String,
    reader: String,
    capabilities: Capabilities,
}

async fn fixture() -> Fixture {
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("state");
    synck_local::server::init(&state).unwrap();
    let root = parent.path().join("personal");
    let repo_id = synck_local::init(&root, "owner").await.unwrap();
    let writer = tokens::add(&state, "laptop", Access::Write, &[repo_id.clone()]).unwrap();
    let reader = tokens::add(&state, "phone", Access::Read, &[repo_id.clone()]).unwrap();
    let router = synck_local::server::router(Arc::new(Server::open(&state, &[root]).unwrap()));
    let sync = format!("/v1/repositories/{}/sync", repo_id.as_str());
    let (_, body) = send(&router, Method::GET, &format!("{sync}/capabilities"), &reader, vec![]).await;
    let capabilities = serde_json::from_slice(&body).unwrap();
    Fixture { _parent: parent, router, repo_id, sync, writer, reader, capabilities }
}

impl Fixture {
    fn repository(&self) -> &RepositoryIdentity {
        &self.capabilities.repository
    }

    /// Upload and submit one new file at the top level.
    async fn create(&self, operation_id: &str, name: &str, lock_tokens: &[&str]) -> (StatusCode, Vec<u8>) {
        let (_, event) = create_file(&self.repo_id, name, name.as_bytes());
        let object = format!("{}/objects/{}", self.sync, object_id(name.as_bytes()).as_str());
        send(&self.router, Method::PUT, &object, &self.writer, name.as_bytes().to_vec()).await;
        let (pages, request) = upload(self.repository(), operation_id, vec![ProposalRecord::Item(event)]);
        let page_uri = format!("{}/uploads/{operation_id}/pages/0", self.sync);
        send(&self.router, Method::PUT, &page_uri, &self.writer, serde_json::to_vec(&pages[0]).unwrap()).await;
        let headers: Vec<(&str, &str)> = lock_tokens.iter().map(|token| ("synck-lock-token", *token)).collect();
        let uri = format!("{}/proposals", self.sync);
        send_with(&self.router, Method::POST, &uri, &self.writer, &headers, serde_json::to_vec(&request).unwrap()).await
    }

    async fn post<T: serde::Serialize>(&self, suffix: &str, token: &str, value: &T) -> (StatusCode, Vec<u8>) {
        let uri = format!("{}/{suffix}", self.sync);
        send(&self.router, Method::POST, &uri, token, serde_json::to_vec(value).unwrap()).await
    }
}

fn history_request(completed: Cursor, limit: usize) -> HistoryRequest {
    HistoryRequest {
        after: HistoryPosition { completed, pending: None },
        limit,
    }
}

#[tokio::test]
async fn history_pages_verify_from_genesis_to_the_published_head() {
    let fixture = fixture().await;
    for (operation_id, name) in [("op-1", "a.txt"), ("op-2", "b.txt")] {
        assert_eq!(fixture.create(operation_id, name, &[]).await.0, StatusCode::OK);
    }
    let (_, body) = send(&fixture.router, Method::GET, &format!("{}/capabilities", fixture.sync), &fixture.reader, vec![]).await;
    let latest = serde_json::from_slice::<Capabilities>(&body).unwrap().latest;
    assert_eq!(latest.repo_seq, 3);
    let genesis = Cursor::genesis(fixture.repository().clone(), false).unwrap();

    // One page holds the whole history.
    let (status, body) = fixture.post("history", &fixture.reader, &history_request(genesis.clone(), 512)).await;
    assert_eq!(status, StatusCode::OK);
    let page: HistoryPage = serde_json::from_slice(&body).unwrap();
    assert!(!page.has_more);
    let mut verifier = HistoryVerifier::new(genesis.clone()).unwrap();
    assert_eq!(verifier.push(&page).unwrap(), Some(latest.clone()));

    // One event per page reaches the same head.
    let mut verifier = HistoryVerifier::new(genesis.clone()).unwrap();
    let mut pages = 0;
    loop {
        let request = HistoryRequest { after: verifier.position().clone(), limit: 1 };
        let (_, body) = fixture.post("history", &fixture.reader, &request).await;
        let page: HistoryPage = serde_json::from_slice(&body).unwrap();
        verifier.push(&page).unwrap();
        pages += 1;
        if !page.has_more {
            break;
        }
    }
    assert!(pages >= 3);
    assert_eq!(verifier.position().completed, latest);

    let mut forked = latest.clone();
    forked.checkpoint = genesis.checkpoint.clone();
    let (status, body) = fixture.post("history", &fixture.reader, &history_request(forked, 512)).await;
    assert_eq!((status, status_of(&body).as_str()), (StatusCode::CONFLICT, "CHECKPOINT"));
}

#[tokio::test]
async fn leases_fence_remote_commits_until_released() {
    let fixture = fixture().await;
    let request = serde_json::json!({ "paths": ["a.txt", "docs"], "owner": "agent-a", "ttl": 600 });
    assert_eq!(fixture.post("leases", &fixture.reader, &request).await.0, StatusCode::FORBIDDEN);
    let (status, body) = fixture.post("leases", &fixture.writer, &request).await;
    assert_eq!(status, StatusCode::OK);
    let lease: Lease = serde_json::from_slice(&body).unwrap();
    assert_eq!(lease.locks.len(), 2);

    let (_, body) = send(&fixture.router, Method::GET, &format!("{}/leases", fixture.sync), &fixture.reader, vec![]).await;
    let listed: Leases = serde_json::from_slice(&body).unwrap();
    assert_eq!(listed.locks, lease.locks);
    assert!(!String::from_utf8_lossy(&body).contains(&lease.token));

    let other = serde_json::json!({ "paths": ["docs/notes.txt"], "owner": "agent-b", "ttl": 600 });
    let (status, body) = fixture.post("leases", &fixture.writer, &other).await;
    let refused: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!((status, refused["status"].as_str(), refused["owner"].as_str()), (StatusCode::CONFLICT, Some("LOCKED"), Some("agent-a")));

    // A commit to a reserved path needs the lease's token; others are free.
    let (status, body) = fixture.create("op-1", "a.txt", &[]).await;
    assert_eq!((status, status_of(&body).as_str()), (StatusCode::CONFLICT, "LOCKED"));
    assert_eq!(fixture.create("op-2", "b.txt", &[]).await.0, StatusCode::OK);
    let (status, body) = fixture.create("op-3", "a.txt", &[&lease.token]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(matches!(serde_json::from_slice(&body).unwrap(), SubmitResult::Accepted { .. }));

    let renewal = serde_json::json!({ "token": lease.token, "ttl": 60 });
    assert_eq!(fixture.post("leases/renew", &fixture.writer, &renewal).await.0, StatusCode::NO_CONTENT);
    let token = serde_json::json!({ "token": lease.token });
    let (_, body) = fixture.post("leases/release", &fixture.writer, &token).await;
    assert_eq!(serde_json::from_slice::<Released>(&body).unwrap().released, 2);
    let (_, body) = fixture.post("leases/release", &fixture.writer, &token).await;
    assert_eq!(serde_json::from_slice::<Released>(&body).unwrap().released, 0);
    let (status, body) = fixture.post("leases/renew", &fixture.writer, &renewal).await;
    assert_eq!((status, status_of(&body).as_str()), (StatusCode::CONFLICT, "LOCK_LOST"));
    let (status, body) = fixture.create("op-4", "c.txt", &[&lease.token]).await;
    assert_eq!((status, status_of(&body).as_str()), (StatusCode::CONFLICT, "LOCK_LOST"));

    let (_, body) = fixture.post("leases", &fixture.writer, &other).await;
    serde_json::from_slice::<Lease>(&body).unwrap();
    let (_, body) = fixture.post("leases/break", &fixture.writer, &serde_json::json!({ "paths": ["docs"] })).await;
    assert_eq!(serde_json::from_slice::<Released>(&body).unwrap().released, 1);
}
