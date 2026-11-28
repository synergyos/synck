//! Helpers shared by the route tests.
#![allow(dead_code)]

use std::collections::BTreeMap;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use sha2::{Digest as _, Sha256};
use synck_file_folder::{BlobRef, Intent, IntentProposal, Location};
use synck_sync::{
    CommitMetadata, ProposalPage, ProposalRecord, ProposalVerifier, Refusal, RepositoryIdentity,
    SubmitRequest, SubmitResult, Wire,
};
use synck_vcs::metadata::MetadataRef;
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

pub fn wire<T: Wire>(value: &T) -> Vec<u8> {
    synck_sync::encode(value).unwrap()
}

pub fn refusal(body: &[u8]) -> Refusal {
    synck_sync::decode(body).unwrap()
}

pub fn submit_result(body: &[u8]) -> SubmitResult {
    synck_sync::decode(body).unwrap()
}

/// An object pack: per object, its raw digest, its size as a big-endian
/// `u64`, and its bytes.
pub fn pack(objects: &[(ObjectId, &[u8])]) -> Vec<u8> {
    let mut pack = Vec::new();
    for (id, bytes) in objects {
        pack.extend(id.as_bytes());
        pack.extend((bytes.len() as u64).to_be_bytes());
        pack.extend(*bytes);
    }
    pack
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

/// Give a file an item key, so its event carries a metadata object, which a
/// client uploads like content. A format 8 FILE event carries metadata only
/// when the item has keys besides its content.
pub fn with_metadata(event: &mut EventProposal) -> (ObjectId, Vec<u8>) {
    let metadata = MetadataRef::new(serde_json::json!({"note": "from a client"})).unwrap();
    let object = metadata.payload().unwrap();
    let found = (object.id(), object.payload().to_vec());
    event.set_metadata = Some(metadata);
    found
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
