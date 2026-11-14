//! Helpers shared by the route tests.
#![allow(dead_code)]

use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use synck_file_folder::{BlobRef, Intent, IntentProposal, Location};
use synck_sync::{
    CommitMetadata, ProposalPage, ProposalRecord, ProposalVerifier, RepositoryIdentity,
    SubmitRequest,
};
use synck_vcs::types::{EventProposal, ItemId, ObjectId, RepoId};
use tower::ServiceExt as _;

pub async fn send(
    router: &Router,
    method: Method,
    uri: &str,
    token: &str,
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    send_with(router, method, uri, token, &[], body).await
}

pub async fn send_with(
    router: &Router,
    method: Method,
    uri: &str,
    token: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request.body(Body::from(body)).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    (
        status,
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}

pub fn status_of(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body).unwrap()["status"]
        .as_str()
        .unwrap()
        .to_string()
}

pub fn create_file(repo_id: &RepoId, name: &str, bytes: &[u8]) -> (ItemId, EventProposal) {
    let item_id = ItemId::new();
    let materialized = synck_file_folder::validate_and_materialize(
        repo_id,
        &IntentProposal {
            message: None,
            intents: vec![Intent::CreateFile {
                item_id: item_id.clone(),
                location: Location {
                    parent_item_id: None,
                    name: name.into(),
                },
                content: BlobRef {
                    blob_id: object_id(bytes),
                    size: bytes.len() as u64,
                },
            }],
        },
        &BTreeMap::new(),
    )
    .unwrap();
    let [event] = <[EventProposal; 1]>::try_from(materialized.events).unwrap();
    (item_id, event)
}

/// The metadata object an event names, which a client uploads like content.
pub fn metadata_object(event: &EventProposal) -> (ObjectId, Vec<u8>) {
    let metadata = event.set_metadata.as_ref().unwrap().payload().unwrap();
    (metadata.id(), metadata.payload().to_vec())
}

pub fn object_id(bytes: &[u8]) -> ObjectId {
    ObjectId::from_digest(Sha256::digest(bytes).into())
}

/// One record per page, and the request a client would build from them.
pub fn upload(
    repository: &RepositoryIdentity,
    operation_id: &str,
    records: Vec<ProposalRecord>,
) -> (Vec<ProposalPage>, SubmitRequest) {
    let expires = chrono::Utc::now() + chrono::TimeDelta::days(1);
    let mut verifier =
        ProposalVerifier::new(repository.clone(), operation_id.into(), expires).unwrap();
    let mut pages = Vec::new();
    for (offset, record) in records.into_iter().enumerate() {
        let page = ProposalPage {
            repository: repository.clone(),
            operation_id: operation_id.into(),
            offset: offset as u64,
            records: vec![record],
        };
        verifier.push(&page, chrono::Utc::now()).unwrap();
        pages.push(page);
    }
    let request = verifier
        .request(CommitMetadata {
            author_id: "owner".into(),
            committer_id: "laptop".into(),
            message: Some("from a client".into()),
            occurred_at: None,
        })
        .unwrap();
    (pages, request)
}
