use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::http::{Method, StatusCode};
use synck_local::server::Server;
use synck_local::server::tokens::{self, Access};
use synck_sync::{
    Capabilities, ItemHead, ObjectRequest, PageAck, ProposalPage, ProposalRecord, Receipt, Refusal,
    SubmitResult, decode,
};
use synck_vcs::types::ItemId;

use super::common::{
    create_file, metadata_object, object_id, pack, refusal, send, submit_result, upload, wire,
};

#[tokio::test]
async fn proposals_stage_check_objects_and_return_one_receipt() {
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("state");
    synck_local::server::init(&state).unwrap();
    let root = parent.path().join("personal");
    let repo_id = synck_local::init(&root, "owner").await.unwrap();
    let writer = tokens::add(&state, "laptop", Access::Write, &[repo_id.clone()]).unwrap();
    let reader = tokens::add(&state, "phone", Access::Read, &[repo_id.clone()]).unwrap();
    let router =
        synck_local::server::router(Arc::new(Server::open(&state, &[root.clone()]).unwrap()));
    let sync = format!("/v2/repositories/{}/sync", repo_id.as_str());

    let (_, body) = send(
        &router,
        Method::GET,
        &format!("{sync}/capabilities"),
        &reader,
        vec![],
    )
    .await;
    let repository = decode::<Capabilities>(&body).unwrap().latest.repository;
    let (a, event) = create_file(&repo_id, "a.txt", b"hello");
    let (metadata_id, metadata) = metadata_object(&event);
    let objects = format!("{sync}/objects");
    let (pages, request) = upload(&repository, "op-1", vec![ProposalRecord::Item(event)]);
    let page_uri = format!("{sync}/uploads/op-1/pages/0");
    let page = wire(&pages[0]);
    let submission = wire(&request);

    let (status, body) = send(
        &router,
        Method::POST,
        &format!("{sync}/proposals"),
        &writer,
        submission.clone(),
    )
    .await;
    assert_eq!(
        (status, refusal(&body)),
        (StatusCode::GONE, Refusal::UploadExpired)
    );
    assert_eq!(
        send(&router, Method::PUT, &page_uri, &reader, page.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, body) = send(&router, Method::PUT, &page_uri, &writer, page.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let ack: PageAck = decode(&body).unwrap();
    assert_eq!(ack.next_offset, 1);
    // A retry gets the same acknowledgement; other contents at that offset do not.
    let (status, body) = send(&router, Method::PUT, &page_uri, &writer, page).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decode::<PageAck>(&body).unwrap().expires_at, ack.expires_at);
    let (_, other) = create_file(&repo_id, "other.txt", b"hello");
    let mut changed = pages[0].clone();
    changed.records = vec![ProposalRecord::Item(other)];
    let changed = wire(&changed);
    assert_eq!(
        send(&router, Method::PUT, &page_uri, &writer, changed)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );

    let (status, body) = send(
        &router,
        Method::POST,
        &format!("{sync}/proposals"),
        &writer,
        submission.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let Refusal::ObjectsMissing(missing) = refusal(&body) else {
        panic!("not OBJECTS_MISSING");
    };
    assert_eq!(missing.len(), 2);
    assert!(missing.contains(&metadata_id));
    // Metadata arrives as an object and is installed when the commit is.
    assert_eq!(
        send(
            &router,
            Method::PUT,
            &objects,
            &writer,
            pack(&[(metadata_id.clone(), &metadata)])
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );

    let hello = object_id(b"hello");
    for refused in [pack(&[(hello.clone(), b"other")]), Vec::new()] {
        let (status, body) = send(&router, Method::PUT, &objects, &writer, refused).await;
        assert_eq!(
            (status, refusal(&body)),
            (StatusCode::BAD_REQUEST, Refusal::Invalid)
        );
    }
    assert_eq!(
        send(
            &router,
            Method::PUT,
            &objects,
            &reader,
            pack(&[(hello.clone(), b"hello")])
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &router,
            Method::PUT,
            &objects,
            &writer,
            pack(&[(hello.clone(), b"hello")])
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let fetch = |ids: Vec<synck_vcs::types::ObjectId>| {
        send(
            &router,
            Method::POST,
            &objects,
            &reader,
            wire(&ObjectRequest { objects: ids }),
        )
    };
    let (status, body) = fetch(vec![hello.clone()]).await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, pack(&[(hello.clone(), b"hello")]))
    );
    let (status, body) = fetch(vec![hello.clone(), object_id(b"absent")]).await;
    assert_eq!(
        (status, refusal(&body)),
        (StatusCode::NOT_FOUND, Refusal::ObjectMissing)
    );
    // A trailing byte is refused like any malformed message.
    let mut trailing = submission.clone();
    trailing.push(0);
    let (status, body) = send(
        &router,
        Method::POST,
        &format!("{sync}/proposals"),
        &writer,
        trailing,
    )
    .await;
    assert_eq!(
        (status, refusal(&body)),
        (StatusCode::BAD_REQUEST, Refusal::Invalid)
    );

    let (status, body) = send(
        &router,
        Method::POST,
        &format!("{sync}/proposals"),
        &writer,
        submission.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let SubmitResult::Accepted { receipt } = submit_result(&body) else {
        panic!("not accepted");
    };
    receipt.validate_for(&request).unwrap();
    assert_eq!(receipt.published.as_ref().unwrap().repo_seq, 2);
    // Its metadata is served from history now.
    let (status, body) = fetch(vec![metadata_id.clone()]).await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, pack(&[(metadata_id, &metadata)]))
    );
    // Retried after its pages are gone, the submission gets the same receipt.
    let (_, body) = send(
        &router,
        Method::POST,
        &format!("{sync}/proposals"),
        &writer,
        submission,
    )
    .await;
    let SubmitResult::Accepted { receipt: again } = submit_result(&body) else {
        panic!("retry not accepted");
    };
    assert_eq!(again.commit_id, receipt.commit_id);
    let (status, body) = send(
        &router,
        Method::GET,
        &format!("{sync}/receipts/op-1"),
        &reader,
        vec![],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let stored: Receipt = decode(&body).unwrap();
    stored.validate_for(&request).unwrap();
    assert_eq!(stored.accepted_at, receipt.accepted_at);
    assert_eq!(
        send(
            &router,
            Method::GET,
            &format!("{sync}/receipts/op-2"),
            &reader,
            vec![]
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, body) = send(
        &router,
        Method::GET,
        &format!("{sync}/capabilities"),
        &reader,
        vec![],
    )
    .await;
    assert_eq!(decode::<Capabilities>(&body).unwrap().latest.repo_seq, 2);

    // A head guard that no longer holds is a conflict, never a merge.
    let (b, event) = create_file(&repo_id, "b.txt", b"hello");
    let (metadata_id, metadata) = metadata_object(&event);
    assert_eq!(
        send(
            &router,
            Method::PUT,
            &objects,
            &writer,
            pack(&[(metadata_id, &metadata)])
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let guard = ItemHead {
        item_id: a.clone(),
        head_seq: 5,
    };
    let (pages, request) = upload(
        &repository,
        "op-2",
        vec![ProposalRecord::Item(event), ProposalRecord::Guard(guard)],
    );
    for page in &pages {
        let uri = format!("{sync}/uploads/op-2/pages/{}", page.offset);
        assert_eq!(
            send(&router, Method::PUT, &uri, &writer, wire(page))
                .await
                .0,
            StatusCode::OK
        );
    }
    let (_, body) = send(
        &router,
        Method::POST,
        &format!("{sync}/proposals"),
        &writer,
        wire(&request),
    )
    .await;
    let SubmitResult::Conflict { items } = submit_result(&body) else {
        panic!("not a conflict");
    };
    let heads: BTreeMap<ItemId, i64> = items
        .into_iter()
        .map(|head| (head.item_id, head.head_seq))
        .collect();
    assert_eq!(heads, BTreeMap::from([(a, 1), (b, 0)]));
}

#[tokio::test]
async fn an_upload_continues_after_a_restart() {
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("state");
    synck_local::server::init(&state).unwrap();
    let root = parent.path().join("personal");
    let repo_id = synck_local::init(&root, "owner").await.unwrap();
    let writer = tokens::add(&state, "laptop", Access::Write, &[repo_id.clone()]).unwrap();
    let sync = format!("/v2/repositories/{}/sync", repo_id.as_str());
    let open =
        || synck_local::server::router(Arc::new(Server::open(&state, &[root.clone()]).unwrap()));

    let router = open();
    let (_, body) = send(
        &router,
        Method::GET,
        &format!("{sync}/capabilities"),
        &writer,
        vec![],
    )
    .await;
    let repository = decode::<Capabilities>(&body).unwrap().latest.repository;
    let events = ["c.txt", "d.txt"].map(|name| create_file(&repo_id, name, b"two files").1);
    let metadata = events.each_ref().map(metadata_object);
    let objects = [
        (metadata[0].0.clone(), metadata[0].1.as_slice()),
        (metadata[1].0.clone(), metadata[1].1.as_slice()),
        (object_id(b"two files"), b"two files"),
    ];
    assert_eq!(
        send(
            &router,
            Method::PUT,
            &format!("{sync}/objects"),
            &writer,
            pack(&objects)
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let records = events.map(ProposalRecord::Item).to_vec();
    let (pages, request) = upload(&repository, "op-1", records);
    let put = |router: Router, page: ProposalPage| {
        let uri = format!("{sync}/uploads/op-1/pages/{}", page.offset);
        let writer = writer.clone();
        async move {
            send(&router, Method::PUT, &uri, &writer, wire(&page))
                .await
                .0
        }
    };
    assert_eq!(put(router.clone(), pages[0].clone()).await, StatusCode::OK);
    drop(router);

    let router = open();
    assert_eq!(put(router.clone(), pages[1].clone()).await, StatusCode::OK);
    let (status, body) = send(
        &router,
        Method::POST,
        &format!("{sync}/proposals"),
        &writer,
        wire(&request),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(matches!(
        submit_result(&body),
        SubmitResult::Accepted { .. }
    ));
}

#[tokio::test]
async fn repository_head_fences_new_child_phantoms_and_replays_accepted_requests() {
    use synck_sync::{RepositoryIdentity, SubmitRequest};
    async fn stage(
        router: &Router,
        sync: &str,
        writer: &str,
        repository: &RepositoryIdentity,
        operation: &str,
        name: &str,
        head: Option<u64>,
    ) -> SubmitRequest {
        let (_, event) = create_file(&repository.repo_id, name, b"body");
        let (metadata_id, metadata) = metadata_object(&event);
        let (pages, mut request) = upload(repository, operation, vec![ProposalRecord::Item(event)]);
        request.repository_head = head;
        let objects = pack(&[(metadata_id, &metadata), (object_id(b"body"), b"body")]);
        assert_eq!(
            send(
                router,
                Method::PUT,
                &format!("{sync}/objects"),
                writer,
                objects
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        for page in pages {
            assert_eq!(
                send(
                    router,
                    Method::PUT,
                    &format!("{sync}/uploads/{operation}/pages/{}", page.offset),
                    writer,
                    wire(&page)
                )
                .await
                .0,
                StatusCode::OK
            );
        }
        request
    }
    async fn submit(
        router: &Router,
        sync: &str,
        writer: &str,
        request: &SubmitRequest,
    ) -> SubmitResult {
        let (status, body) = send(
            router,
            Method::POST,
            &format!("{sync}/proposals"),
            writer,
            wire(request),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        submit_result(&body)
    }
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("state");
    synck_local::server::init(&state).unwrap();
    let root = parent.path().join("repository");
    let repo_id = synck_local::init(&root, "owner").await.unwrap();
    let writer = tokens::add(&state, "writer", Access::Write, &[repo_id.clone()]).unwrap();
    let router = synck_local::server::router(Arc::new(Server::open(&state, &[root]).unwrap()));
    let sync = format!("/v2/repositories/{repo_id}/sync");
    let (_, raw) = send(
        &router,
        Method::GET,
        &format!("{sync}/capabilities"),
        &writer,
        vec![],
    )
    .await;
    let caps: Capabilities = decode(&raw).unwrap();
    let repository = caps.latest.repository.clone();
    let stale = stage(
        &router,
        &sync,
        &writer,
        &repository,
        "stale-absence",
        "parent-change",
        Some(caps.latest.repo_seq),
    )
    .await;
    let child = stage(
        &router,
        &sync,
        &writer,
        &repository,
        "new-child",
        "child",
        None,
    )
    .await;
    let SubmitResult::Accepted {
        receipt: child_receipt,
    } = submit(&router, &sync, &writer, &child).await
    else {
        panic!("child rejected")
    };
    assert!(matches!(
        submit(&router, &sync, &writer, &stale).await,
        SubmitResult::Conflict { .. }
    ));
    let mut rebound = stale.clone();
    rebound.repository_head = Some(child_receipt.published.as_ref().unwrap().repo_seq);
    let (status, body) = send(
        &router,
        Method::POST,
        &format!("{sync}/proposals"),
        &writer,
        wire(&rebound),
    )
    .await;
    assert_eq!(
        (status, refusal(&body)),
        (StatusCode::CONFLICT, Refusal::OperationReused)
    );
    let guarded = stage(
        &router,
        &sync,
        &writer,
        &repository,
        "guarded",
        "guarded",
        Some(child_receipt.published.unwrap().repo_seq),
    )
    .await;
    let SubmitResult::Accepted { receipt } = submit(&router, &sync, &writer, &guarded).await else {
        panic!("guarded request rejected")
    };
    let next = stage(&router, &sync, &writer, &repository, "later", "later", None).await;
    assert!(matches!(
        submit(&router, &sync, &writer, &next).await,
        SubmitResult::Accepted { .. }
    ));
    let SubmitResult::Accepted { receipt: replay } =
        submit(&router, &sync, &writer, &guarded).await
    else {
        panic!("accepted retry hit stale head fence")
    };
    assert_eq!(replay.commit_id, receipt.commit_id);
    assert_eq!(replay.accepted_at, receipt.accepted_at);
}
