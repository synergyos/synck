use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;
use synck_local::server::tokens::{self, Access};
use synck_local::server::{Error, Server};
use synck_sync::Capabilities;
use tower::ServiceExt as _;

async fn get(router: &Router, uri: &str, token: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut request = Request::get(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (status, response.into_body().collect().await.unwrap().to_bytes().to_vec())
}

#[tokio::test]
async fn capabilities_need_a_granted_token_and_revocation_applies_at_once() {
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("state");
    synck_local::server::init(&state).unwrap();
    let personal = parent.path().join("personal");
    let work = parent.path().join("work");
    let personal_id = synck_local::init(&personal, "owner").await.unwrap();
    let work_id = synck_local::init(&work, "owner").await.unwrap();
    let laptop = tokens::add(&state, "laptop", Access::Write, &[personal_id.clone()]).unwrap();

    assert!(matches!(
        Server::open(&state, &[personal.clone(), personal.clone()]),
        Err(Error::Invalid(_))
    ));
    let server = Server::open(&state, &[personal.clone(), work.clone()]).unwrap();
    assert!(matches!(Server::open(&state, &[]), Err(Error::Locked(_))));
    let router = synck_local::server::router(Arc::new(server));

    let uri = format!("/v1/repositories/{}/sync/capabilities", personal_id.as_str());
    let (status, body) = get(&router, &uri, Some(&laptop)).await;
    assert_eq!(status, StatusCode::OK);
    let capabilities: Capabilities = serde_json::from_slice(&body).unwrap();
    capabilities.validate().unwrap();
    assert_eq!(capabilities.repository.repo_id, personal_id);
    assert_eq!(capabilities.latest.repo_seq, 1);

    assert_eq!(get(&router, &uri, None).await.0, StatusCode::UNAUTHORIZED);
    // Change one character of the secret to another.
    let swapped = if &laptop[40..41] == "A" { "B" } else { "A" };
    let forged = format!("{}{swapped}{}", &laptop[..40], &laptop[41..]);
    assert_eq!(get(&router, &uri, Some(&forged)).await.0, StatusCode::UNAUTHORIZED);
    let work_uri = format!("/v1/repositories/{}/sync/capabilities", work_id.as_str());
    assert_eq!(get(&router, &work_uri, Some(&laptop)).await.0, StatusCode::NOT_FOUND);
    assert_eq!(get(&router, "/v1/unknown", Some(&laptop)).await.0, StatusCode::NOT_FOUND);

    assert!(tokens::revoke(&state, "laptop").unwrap());
    assert_eq!(get(&router, &uri, Some(&laptop)).await.0, StatusCode::UNAUTHORIZED);
}
