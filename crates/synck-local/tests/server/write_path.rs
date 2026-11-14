use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::http::{Method, StatusCode};
use serde_json::Value;
use synck_local::server::Server;
use synck_local::server::tokens::{self, Access};
use synck_sync::{
    Capabilities, ItemHead, PageAck, ProposalPage, ProposalRecord, Receipt, SubmitResult,
};
use synck_vcs::types::ItemId;

use super::common::{create_file, metadata_object, object_id, send, status_of, upload};

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
    let sync = format!("/v1/repositories/{}/sync", repo_id.as_str());

    let (_, body) = send(
        &router,
        Method::GET,
        &format!("{sync}/capabilities"),
        &reader,
        vec![],
    )
    .await;
    let repository = serde_json::from_slice::<Capabilities>(&body)
        .unwrap()
        .repository;
    let (a, event) = create_file(&repo_id, "a.txt", b"hello");
    let (metadata_id, metadata) = metadata_object(&event);
    let metadata_uri = format!("{sync}/objects/{}", metadata_id.as_str());
    let (pages, request) = upload(&repository, "op-1", vec![ProposalRecord::Item(event)]);
    let page_uri = format!("{sync}/uploads/op-1/pages/0");
    let page = serde_json::to_vec(&pages[0]).unwrap();
    let submission = serde_json::to_vec(&request).unwrap();

    let (status, body) = send(
        &router,
        Method::POST,
        &format!("{sync}/proposals"),
        &writer,
        submission.clone(),
    )
    .await;
    assert_eq!(
        (status, status_of(&body).as_str()),
        (StatusCode::GONE, "UPLOAD_EXPIRED")
    );
    assert_eq!(
        send(&router, Method::PUT, &page_uri, &reader, page.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, body) = send(&router, Method::PUT, &page_uri, &writer, page.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let ack: PageAck = serde_json::from_slice(&body).unwrap();
    assert_eq!(ack.next_offset, 1);
    // A retry gets the same acknowledgement; other contents at that offset do not.
    let (status, body) = send(&router, Method::PUT, &page_uri, &writer, page).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<PageAck>(&body).unwrap().expires_at,
        ack.expires_at
    );
    let (_, other) = create_file(&repo_id, "other.txt", b"hello");
    let mut changed = pages[0].clone();
    changed.records = vec![ProposalRecord::Item(other)];
    let changed = serde_json::to_vec(&changed).unwrap();
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
    let missing: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(missing["status"], "OBJECTS_MISSING");
    assert_eq!(missing["objects"].as_array().unwrap().len(), 2);
    assert!(
        missing["objects"]
            .as_array()
            .unwrap()
            .contains(&Value::from(metadata_id.as_str()))
    );
    // Metadata arrives as an object and is installed when the commit is.
    assert_eq!(
        send(
            &router,
            Method::PUT,
            &metadata_uri,
            &writer,
            metadata.clone()
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );

    let object_uri = format!("{sync}/objects/{}", object_id(b"hello").as_str());
    assert_eq!(
        send(
            &router,
            Method::PUT,
            &object_uri,
            &writer,
            b"other".to_vec()
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &router,
            Method::PUT,
            &object_uri,
            &writer,
            b"hello".to_vec()
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (status, body) = send(&router, Method::GET, &object_uri, &reader, vec![]).await;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, &b"hello"[..]));
    let absent = format!("{sync}/objects/{}", object_id(b"absent").as_str());
    let (status, body) = send(&router, Method::GET, &absent, &reader, vec![]).await;
    assert_eq!(
        (status, status_of(&body).as_str()),
        (StatusCode::NOT_FOUND, "OBJECT_MISSING")
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
    let SubmitResult::Accepted { receipt } = serde_json::from_slice(&body).unwrap() else {
        panic!("not accepted: {}", String::from_utf8_lossy(&body));
    };
    receipt.validate_for(&request).unwrap();
    assert_eq!(receipt.published.as_ref().unwrap().repo_seq, 2);
    // Its metadata is served from history now.
    let (status, body) = send(&router, Method::GET, &metadata_uri, &reader, vec![]).await;
    assert_eq!((status, body), (StatusCode::OK, metadata));
    // Retried after its pages are gone, the submission gets the same receipt.
    let (_, body) = send(
        &router,
        Method::POST,
        &format!("{sync}/proposals"),
        &writer,
        submission,
    )
    .await;
    let SubmitResult::Accepted { receipt: again } = serde_json::from_slice(&body).unwrap() else {
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
    let stored: Receipt = serde_json::from_slice(&body).unwrap();
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
    assert_eq!(
        serde_json::from_slice::<Capabilities>(&body)
            .unwrap()
            .latest
            .repo_seq,
        2
    );

    // A head guard that no longer holds is a conflict, never a merge.
    let (b, event) = create_file(&repo_id, "b.txt", b"hello");
    let (metadata_id, metadata) = metadata_object(&event);
    let uri = format!("{sync}/objects/{}", metadata_id.as_str());
    assert_eq!(
        send(&router, Method::PUT, &uri, &writer, metadata).await.0,
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
            send(
                &router,
                Method::PUT,
                &uri,
                &writer,
                serde_json::to_vec(page).unwrap()
            )
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
        serde_json::to_vec(&request).unwrap(),
    )
    .await;
    let SubmitResult::Conflict { items } = serde_json::from_slice(&body).unwrap() else {
        panic!("not a conflict: {}", String::from_utf8_lossy(&body));
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
    let sync = format!("/v1/repositories/{}/sync", repo_id.as_str());
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
    let repository = serde_json::from_slice::<Capabilities>(&body)
        .unwrap()
        .repository;
    let events = ["c.txt", "d.txt"].map(|name| create_file(&repo_id, name, b"two files").1);
    for event in &events {
        let (id, bytes) = metadata_object(event);
        let uri = format!("{sync}/objects/{}", id.as_str());
        assert_eq!(
            send(&router, Method::PUT, &uri, &writer, bytes).await.0,
            StatusCode::NO_CONTENT
        );
    }
    let records = events.map(ProposalRecord::Item).to_vec();
    let (pages, request) = upload(&repository, "op-1", records);
    let put = |router: Router, page: ProposalPage| {
        let uri = format!("{sync}/uploads/op-1/pages/{}", page.offset);
        let writer = writer.clone();
        async move {
            send(
                &router,
                Method::PUT,
                &uri,
                &writer,
                serde_json::to_vec(&page).unwrap(),
            )
            .await
            .0
        }
    };
    let object_uri = format!("{sync}/objects/{}", object_id(b"two files").as_str());
    assert_eq!(
        send(
            &router,
            Method::PUT,
            &object_uri,
            &writer,
            b"two files".to_vec()
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(put(router.clone(), pages[0].clone()).await, StatusCode::OK);
    drop(router);

    let router = open();
    assert_eq!(put(router.clone(), pages[1].clone()).await, StatusCode::OK);
    let (status, body) = send(
        &router,
        Method::POST,
        &format!("{sync}/proposals"),
        &writer,
        serde_json::to_vec(&request).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(matches!(
        serde_json::from_slice(&body).unwrap(),
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
        for (id, bytes) in [
            (metadata_id, metadata),
            (object_id(b"body"), b"body".to_vec()),
        ] {
            assert_eq!(
                send(
                    router,
                    Method::PUT,
                    &format!("{sync}/objects/{id}"),
                    writer,
                    bytes
                )
                .await
                .0,
                StatusCode::NO_CONTENT
            );
        }
        for page in pages {
            assert_eq!(
                send(
                    router,
                    Method::PUT,
                    &format!("{sync}/uploads/{operation}/pages/{}", page.offset),
                    writer,
                    serde_json::to_vec(&page).unwrap()
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
            serde_json::to_vec(request).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("state");
    synck_local::server::init(&state).unwrap();
    let root = parent.path().join("repository");
    let repo_id = synck_local::init(&root, "owner").await.unwrap();
    let writer = tokens::add(&state, "writer", Access::Write, &[repo_id.clone()]).unwrap();
    let router = synck_local::server::router(Arc::new(Server::open(&state, &[root]).unwrap()));
    let sync = format!("/v1/repositories/{repo_id}/sync");
    let (_, raw) = send(
        &router,
        Method::GET,
        &format!("{sync}/capabilities"),
        &writer,
        vec![],
    )
    .await;
    let caps: Capabilities = serde_json::from_slice(&raw).unwrap();
    let stale = stage(
        &router,
        &sync,
        &writer,
        &caps.repository,
        "stale-absence",
        "parent-change",
        Some(caps.latest.repo_seq),
    )
    .await;
    let child = stage(
        &router,
        &sync,
        &writer,
        &caps.repository,
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
    assert_eq!(
        send(
            &router,
            Method::POST,
            &format!("{sync}/proposals"),
            &writer,
            serde_json::to_vec(&rebound).unwrap()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let guarded = stage(
        &router,
        &sync,
        &writer,
        &caps.repository,
        "guarded",
        "guarded",
        Some(child_receipt.published.unwrap().repo_seq),
    )
    .await;
    let SubmitResult::Accepted { receipt } = submit(&router, &sync, &writer, &guarded).await else {
        panic!("guarded request rejected")
    };
    let next = stage(
        &router,
        &sync,
        &writer,
        &caps.repository,
        "later",
        "later",
        None,
    )
    .await;
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
