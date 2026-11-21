use std::sync::Arc;

use axum::Router;
use axum::http::{Method, StatusCode};
use synck_local::server::Server;
use synck_local::server::tokens::{self, Access};
use synck_sync::{
    Capabilities, Cursor, HistoryPage, HistoryPosition, HistoryRequest, HistoryVerifier, Lease,
    LeasePaths, LeaseRenewal, LeaseRequest, LeaseToken, Leases, ProposalRecord, Refusal, Released,
    RepositoryIdentity, SubmitResult, Wire, decode,
};
use synck_vcs::types::RepoId;

use super::common::{
    create_file, metadata_object, object_id, pack, refusal, send, send_with, submit_result, upload,
    wire,
};

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
    let sync = format!("/v2/repositories/{}/sync", repo_id.as_str());
    let (_, body) = send(
        &router,
        Method::GET,
        &format!("{sync}/capabilities"),
        &reader,
        vec![],
    )
    .await;
    let capabilities = decode(&body).unwrap();
    Fixture {
        _parent: parent,
        router,
        repo_id,
        sync,
        writer,
        reader,
        capabilities,
    }
}

impl Fixture {
    fn repository(&self) -> &RepositoryIdentity {
        &self.capabilities.latest.repository
    }

    /// Upload and submit one new file at the top level.
    async fn create(
        &self,
        operation_id: &str,
        name: &str,
        lock_tokens: &[&str],
    ) -> (StatusCode, Vec<u8>) {
        let (_, event) = create_file(&self.repo_id, name, name.as_bytes());
        let (metadata_id, metadata) = metadata_object(&event);
        let objects = pack(&[
            (object_id(name.as_bytes()), name.as_bytes()),
            (metadata_id, &metadata),
        ]);
        let uri = format!("{}/objects", self.sync);
        send(&self.router, Method::PUT, &uri, &self.writer, objects).await;
        let (pages, request) = upload(
            self.repository(),
            operation_id,
            vec![ProposalRecord::Item(event)],
        );
        let page_uri = format!("{}/uploads/{operation_id}/pages/0", self.sync);
        send(
            &self.router,
            Method::PUT,
            &page_uri,
            &self.writer,
            wire(&pages[0]),
        )
        .await;
        let headers: Vec<(&str, &str)> = lock_tokens
            .iter()
            .map(|token| ("synck-lock-token", *token))
            .collect();
        let uri = format!("{}/proposals", self.sync);
        send_with(
            &self.router,
            Method::POST,
            &uri,
            &self.writer,
            &headers,
            wire(&request),
        )
        .await
    }

    async fn post<T: Wire>(&self, suffix: &str, token: &str, value: &T) -> (StatusCode, Vec<u8>) {
        let uri = format!("{}/{suffix}", self.sync);
        send(&self.router, Method::POST, &uri, token, wire(value)).await
    }
}

fn history_request(completed: Cursor, limit: usize) -> HistoryRequest {
    HistoryRequest {
        after: HistoryPosition {
            completed,
            pending: None,
        },
        limit,
    }
}

#[tokio::test]
async fn history_pages_verify_from_genesis_to_the_published_head() {
    let fixture = fixture().await;
    for (operation_id, name) in [("op-1", "a.txt"), ("op-2", "b.txt")] {
        assert_eq!(
            fixture.create(operation_id, name, &[]).await.0,
            StatusCode::OK
        );
    }
    let (_, body) = send(
        &fixture.router,
        Method::GET,
        &format!("{}/capabilities", fixture.sync),
        &fixture.reader,
        vec![],
    )
    .await;
    let latest = decode::<Capabilities>(&body).unwrap().latest;
    assert_eq!(latest.repo_seq, 3);
    let genesis = Cursor::genesis(fixture.repository().clone(), false).unwrap();

    // One page holds the whole history.
    let (status, body) = fixture
        .post(
            "history",
            &fixture.reader,
            &history_request(genesis.clone(), 512),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let page: HistoryPage = decode(&body).unwrap();
    assert!(!page.has_more);
    let mut verifier = HistoryVerifier::new(genesis.clone()).unwrap();
    assert_eq!(verifier.push(&page).unwrap(), Some(latest.clone()));

    // One event per page reaches the same head.
    let mut verifier = HistoryVerifier::new(genesis.clone()).unwrap();
    let mut pages = 0;
    loop {
        let request = HistoryRequest {
            after: verifier.position().clone(),
            limit: 1,
        };
        let (_, body) = fixture.post("history", &fixture.reader, &request).await;
        let page: HistoryPage = decode(&body).unwrap();
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
    let (status, body) = fixture
        .post("history", &fixture.reader, &history_request(forked, 512))
        .await;
    assert_eq!(
        (status, refusal(&body)),
        (StatusCode::CONFLICT, Refusal::Checkpoint)
    );
}

#[tokio::test]
async fn leases_fence_remote_commits_until_released() {
    let fixture = fixture().await;
    let request = LeaseRequest {
        paths: vec!["a.txt".into(), "docs".into()],
        owner: "agent-a".into(),
        ttl: 600,
    };
    assert_eq!(
        fixture.post("leases", &fixture.reader, &request).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, body) = fixture.post("leases", &fixture.writer, &request).await;
    assert_eq!(status, StatusCode::OK);
    let lease: Lease = decode(&body).unwrap();
    assert_eq!(lease.locks.len(), 2);

    let (_, body) = send(
        &fixture.router,
        Method::GET,
        &format!("{}/leases", fixture.sync),
        &fixture.reader,
        vec![],
    )
    .await;
    let listed: Leases = decode(&body).unwrap();
    assert_eq!(listed.locks, lease.locks);
    assert!(
        !body
            .windows(lease.token.len())
            .any(|bytes| bytes == lease.token.as_bytes())
    );

    let other = LeaseRequest {
        paths: vec!["docs/notes.txt".into()],
        owner: "agent-b".into(),
        ttl: 600,
    };
    let (status, body) = fixture.post("leases", &fixture.writer, &other).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let Refusal::Locked { owner, .. } = refusal(&body) else {
        panic!("not LOCKED");
    };
    assert_eq!(owner, "agent-a");

    // A commit to a reserved path needs the lease's token; others are free.
    let (status, body) = fixture.create("op-1", "a.txt", &[]).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(matches!(refusal(&body), Refusal::Locked { .. }));
    assert_eq!(fixture.create("op-2", "b.txt", &[]).await.0, StatusCode::OK);
    let (status, body) = fixture.create("op-3", "a.txt", &[&lease.token]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(matches!(
        submit_result(&body),
        SubmitResult::Accepted { .. }
    ));

    let renewal = LeaseRenewal {
        token: lease.token.clone(),
        ttl: 60,
    };
    assert_eq!(
        fixture
            .post("leases/renew", &fixture.writer, &renewal)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let token = LeaseToken {
        token: lease.token.clone(),
    };
    let (_, body) = fixture
        .post("leases/release", &fixture.writer, &token)
        .await;
    assert_eq!(decode::<Released>(&body).unwrap().released, 2);
    let (_, body) = fixture
        .post("leases/release", &fixture.writer, &token)
        .await;
    assert_eq!(decode::<Released>(&body).unwrap().released, 0);
    let (status, body) = fixture
        .post("leases/renew", &fixture.writer, &renewal)
        .await;
    assert_eq!(
        (status, refusal(&body)),
        (StatusCode::CONFLICT, Refusal::LockLost)
    );
    let (status, body) = fixture.create("op-4", "c.txt", &[&lease.token]).await;
    assert_eq!(
        (status, refusal(&body)),
        (StatusCode::CONFLICT, Refusal::LockLost)
    );

    let (_, body) = fixture.post("leases", &fixture.writer, &other).await;
    decode::<Lease>(&body).unwrap();
    let (_, body) = fixture
        .post(
            "leases/break",
            &fixture.writer,
            &LeasePaths {
                paths: vec!["docs".into()],
            },
        )
        .await;
    assert_eq!(decode::<Released>(&body).unwrap().released, 1);
}
