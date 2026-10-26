use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synck_file_folder::{ChangeGroup, Intent, canonical_operation_bytes, compute_operation_id};
use synck_remote_protocol::*;
use synck_vcs::errors::{CoreError, StorageError};
use synck_vcs::types::{BlobId, CommitId, ItemId, RepoId, Sha256Digest};

const DISCOVERY: &str = include_str!("fixtures/v1/discovery.json");
const REPOSITORIES: &str = include_str!("fixtures/v1/repositories.json");
const CAPTURES: &str = include_str!("fixtures/v1/captures.json");
const MANIFEST_PAGES: &str = include_str!("fixtures/v1/manifest_pages.json");
const CHANGE_PAGES: &str = include_str!("fixtures/v1/change_pages.json");
const BLOB_TRANSFERS: &str = include_str!("fixtures/v1/blob_transfers.json");
const OPERATIONS: &str = include_str!("fixtures/v1/operations.json");
const ERRORS: &str = include_str!("fixtures/v1/errors.json");
const OPERATION_IDS: &str = include_str!("fixtures/v1/operation_ids.json");

fn fixture(source: &str) -> Value {
    serde_json::from_str(source).unwrap()
}

fn decode_value<T: DeserializeOwned + Serialize>(value: &Value) -> T {
    let bytes = serde_json::to_vec(value).unwrap();
    let decoded: T = decode_json(&bytes).unwrap();
    let encoded = encode_json(&decoded).unwrap();
    decode_json(&encoded).unwrap()
}

fn repo_id() -> RepoId {
    RepoId::parse("0198e8fa-664e-7e80-b134-94ea7ecec437").unwrap()
}

fn operation_id() -> Sha256Digest {
    let operations = fixture(OPERATIONS);
    Sha256Digest::parse(operations["operation_id"].as_str().unwrap()).unwrap()
}

fn indexed_repo_id(index: usize) -> RepoId {
    RepoId::parse(format!("0198e8fa-664e-7e80-b134-{index:012x}")).unwrap()
}

fn indexed_item_id(index: usize) -> ItemId {
    ItemId::parse(format!("0198e901-d208-7670-ae29-{index:012x}")).unwrap()
}

fn indexed_blob_id(index: usize) -> BlobId {
    BlobId::parse(format!("sha256:{index:064x}")).unwrap()
}

fn indexed_commit_id(index: usize) -> CommitId {
    CommitId::parse(format!("sha256:{index:064x}")).unwrap()
}

#[test]
fn normative_discovery_and_repository_examples_validate() {
    let discovery: DiscoveryResponse = decode_json(DISCOVERY.as_bytes()).unwrap();
    validate_discovery(&discovery).unwrap();
    let mut bounded_discovery = discovery.clone();
    bounded_discovery.limits.max_blob_bytes = 1;
    validate_discovery(&bounded_discovery).unwrap();
    bounded_discovery.limits.max_blob_bytes = 0;
    assert_eq!(
        validate_discovery(&bounded_discovery).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(discovery.limits.max_lifecycle_descendants, 9_999);

    let repositories: RepositoryPage = decode_json(REPOSITORIES.as_bytes()).unwrap();
    validate_repository_page(&repositories).unwrap();
    assert_eq!(repositories.repositories[0].name, "Legal");

    let default_query = RepositoryListQuery {
        limit: None,
        cursor: None,
    };
    validate_repository_list_query(&default_query).unwrap();
    assert_eq!(default_query.effective_limit(), 256);
    assert_eq!(
        validate_repository_list_query(&RepositoryListQuery {
            limit: Some(0),
            cursor: None,
        })
        .unwrap_err()
        .code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn normative_capture_and_manifest_examples_validate() {
    let captures = fixture(CAPTURES);
    let clone_request: CaptureRequest = decode_value(&captures["clone_request"]);
    let pull_request: CaptureRequest = decode_value(&captures["pull_request"]);
    let clone_response: CaptureResponse = decode_value(&captures["clone_response"]);
    let pull_response: CaptureResponse = decode_value(&captures["pull_response"]);
    validate_capture_response(&clone_request, &clone_response).unwrap();
    validate_capture_response(&pull_request, &pull_response).unwrap();

    let pages = fixture(MANIFEST_PAGES);
    let _: PageRequest = decode_value(&pages["request"]);
    let response: ManifestPageResponse = decode_value(&pages["response"]);
    validate_manifest_page(&clone_response.target_checkpoint, &response).unwrap();
    assert_eq!(response.entries.len(), 3);
}

#[test]
fn product_only_and_group_change_pages_advance_exact_checkpoints() {
    let fixture = fixture(CHANGE_PAGES);
    let _: PageRequest = decode_value(&fixture["request"]);
    let from: Checkpoint = serde_json::from_value(fixture["from_checkpoint"].clone()).unwrap();
    let target: Checkpoint = serde_json::from_value(fixture["target_checkpoint"].clone()).unwrap();
    let product_only: ChangePageResponse = decode_value(&fixture["product_only_response"]);
    validate_change_page(&repo_id(), &from, &target, &product_only).unwrap();
    assert!(product_only.groups.is_empty());
    assert_eq!(product_only.page_through_checkpoint.repo_seq, 31);

    let group_page: ChangePageResponse = decode_value(&fixture["group_response"]);
    validate_change_page(&repo_id(), &from, &target, &group_page).unwrap();
    assert_eq!(group_page.groups.len(), 1);
    assert_eq!(group_page.page_through_checkpoint, target);
}

#[test]
fn change_page_rejects_an_oversized_complete_response() {
    let fixture = fixture(CHANGE_PAGES);
    let from: Checkpoint = serde_json::from_value(fixture["from_checkpoint"].clone()).unwrap();
    let mut group: ChangeGroup =
        serde_json::from_value(fixture["group_response"]["groups"][0].clone()).unwrap();
    group.message = Some("x".repeat(MAX_JSON_BYTES / 2));
    let mut second = group.clone();
    group.repo_seq = from.repo_seq as i64 + 1;
    group.commit_id = indexed_commit_id(1);
    second.repo_seq = from.repo_seq as i64 + 2;
    second.commit_id = indexed_commit_id(2);
    let target = Checkpoint {
        repo_seq: from.repo_seq + 2,
        tip_commit_id: Some(second.commit_id.clone()),
    };
    let page = ChangePageResponse {
        protocol_version: ProtocolVersion,
        profile: Profile,
        target_checkpoint: target.clone(),
        page_through_checkpoint: target.clone(),
        groups: vec![group, second],
        next_cursor: None,
    };

    assert_eq!(
        validate_change_page(&repo_id(), &from, &target, &page)
            .unwrap_err()
            .code,
        ErrorCode::JsonTooLarge
    );
}

#[test]
fn have_and_need_use_one_bound_evidence_shape() {
    let fixture = fixture(BLOB_TRANSFERS);
    let request: HaveNeedRequest = decode_value(&fixture["request"]);
    let have: HaveNeedResponse = decode_value(&fixture["have_response"]);
    let need: HaveNeedResponse = decode_value(&fixture["need_response"]);
    validate_have_need_response(&repo_id(), &request, &have).unwrap();
    validate_have_need_response(&repo_id(), &request, &need).unwrap();
    assert_eq!(have.results[0].state, HaveNeedState::Have);
    assert_eq!(need.results[0].state, HaveNeedState::Need);
    assert_ne!(have.results[0].proof, need.results[0].proof);
    assert_eq!(request.operation_id, operation_id());

    let valid = &fixture["valid_range"];
    let range = parse_single_byte_range(
        valid["header"].as_str().unwrap(),
        valid["object_size"].as_u64().unwrap(),
    )
    .unwrap();
    assert_eq!(range.start, valid["start"].as_u64().unwrap());
    assert_eq!(range.end, valid["end"].as_u64().unwrap());
    for invalid in fixture["invalid_ranges"].as_array().unwrap() {
        assert_eq!(
            parse_single_byte_range(invalid.as_str().unwrap(), 5)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRange
        );
    }
}

#[test]
fn operation_request_and_receipt_reuse_canonical_profile_types() {
    let operations = fixture(OPERATIONS);
    let request: OperationRequest = decode_value(&operations["request"]);
    let receipt: OperationReceipt = decode_value(&operations["receipt"]);
    let normalized = validate_operation_request(&repo_id(), &operation_id(), &request).unwrap();
    assert_eq!(normalized, request.intents);
    validate_operation_receipt(&repo_id(), &operation_id(), &receipt).unwrap();

    let change_pages = fixture(CHANGE_PAGES);
    assert_eq!(
        serde_json::to_value(&receipt.group).unwrap(),
        change_pages["group_response"]["groups"][0]
    );
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationVector {
    repo_id: RepoId,
    message: Option<String>,
    intents: Vec<Intent>,
    canonical_hex: String,
    operation_id: Sha256Digest,
}

#[test]
fn operation_id_vectors_lock_bytes_and_hashes() {
    let vectors: Vec<OperationVector> = serde_json::from_str(OPERATION_IDS).unwrap();
    for vector in vectors {
        let bytes =
            canonical_operation_bytes(&vector.repo_id, vector.message.as_deref(), &vector.intents)
                .unwrap();
        let actual_hex = hex(&bytes);
        let actual_id =
            compute_operation_id(&vector.repo_id, vector.message.as_deref(), &vector.intents)
                .unwrap();
        assert_eq!(
            actual_hex, vector.canonical_hex,
            "canonical_hex={actual_hex}"
        );
        assert_eq!(actual_id, vector.operation_id, "operation_id={actual_id}");
    }
}

#[test]
fn strict_json_rejects_version_shape_and_canonicality_errors() {
    let cases = [
        (
            br#"{"protocol_version":1,"protocol_version":1,"repositories":[],"next_cursor":null}"#.as_slice(),
            ErrorCode::InvalidRequest,
        ),
        (
            br#"{"protocol_version":1,"repositories":[],"next_cursor":null,"extra":true}"#,
            ErrorCode::InvalidRequest,
        ),
        (
            br#"{"protocol_version":2,"repositories":[],"next_cursor":null}"#,
            ErrorCode::UnsupportedProtocolVersion,
        ),
        (
            br#"{"protocol_version":1.0,"repositories":[],"next_cursor":null}"#,
            ErrorCode::InvalidRequest,
        ),
        (
            br#"{"repositories":[],"next_cursor":null}"#,
            ErrorCode::UnsupportedProtocolVersion,
        ),
        (
            br#"{"protocol_version":1,"repositories":[],"next_cursor":null} trailing"#,
            ErrorCode::InvalidRequest,
        ),
        (
            br#"{"protocol_version":1,"repositories":[{"repo_id":"0198E8FA-664E-7E80-B134-94EA7ECEC437","name":"Legal"}],"next_cursor":null}"#,
            ErrorCode::InvalidIdentifier,
        ),
        (
            br#"{"protocol_version":1,"repositories":[],"next_cursor":null,"unused":1.5}"#,
            ErrorCode::InvalidRequest,
        ),
    ];
    for (bytes, code) in cases {
        assert_eq!(decode_json::<RepositoryPage>(bytes).unwrap_err().code, code);
    }

    for local_only in ["WORKTREE_DIRTY", "CHECKOUT_INVALID"] {
        let body = format!(
            r#"{{"protocol_version":1,"error":{{"code":"{local_only}","retryable":false}}}}"#
        );
        assert_eq!(
            decode_json::<ErrorEnvelope>(body.as_bytes())
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }

    let operations = fixture(OPERATIONS);
    let mut receipt = operations["receipt"].clone();
    receipt["group"]["changes"][0]["event"]["set_metadata"]["content"]["size"] = json!(1.5);
    assert_eq!(
        decode_json::<OperationReceipt>(&serde_json::to_vec(&receipt).unwrap())
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn checkpoint_cursor_group_and_receipt_bindings_fail_closed() {
    assert!(
        validate_checkpoint(&Checkpoint {
            repo_seq: 0,
            tip_commit_id: Some(CommitId::parse(format!("sha256:{}", "a".repeat(64))).unwrap()),
        })
        .is_err()
    );

    let change_fixture = fixture(CHANGE_PAGES);
    let from: Checkpoint =
        serde_json::from_value(change_fixture["from_checkpoint"].clone()).unwrap();
    let target: Checkpoint =
        serde_json::from_value(change_fixture["target_checkpoint"].clone()).unwrap();
    let mut page: ChangePageResponse = decode_value(&change_fixture["group_response"]);
    page.target_checkpoint.repo_seq = 41;
    assert_eq!(
        validate_change_page(&repo_id(), &from, &target, &page)
            .unwrap_err()
            .code,
        ErrorCode::CursorBindingMismatch
    );

    let operations = fixture(OPERATIONS);
    let mut receipt: OperationReceipt = decode_value(&operations["receipt"]);
    receipt.operation_id = Sha256Digest::parse(format!("sha256:{}", "9".repeat(64))).unwrap();
    assert_eq!(
        validate_operation_receipt(&repo_id(), &operation_id(), &receipt)
            .unwrap_err()
            .code,
        ErrorCode::CursorBindingMismatch
    );
}

#[test]
fn canonical_path_identifiers_and_head_only_groups_are_validated() {
    assert_eq!(
        parse_canonical_repo_id("0198E8FA-664E-7E80-B134-94EA7ECEC437")
            .unwrap_err()
            .code,
        ErrorCode::InvalidIdentifier
    );
    assert_eq!(
        parse_canonical_sha256(
            "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        )
        .unwrap_err()
        .code,
        ErrorCode::InvalidIdentifier
    );

    let group_page = fixture(CHANGE_PAGES);
    let mut group: ChangeGroup =
        serde_json::from_value(group_page["group_response"]["groups"][0].clone()).unwrap();
    let mut product_only_head = group.item_heads[0].clone();
    product_only_head.item_id =
        synck_vcs::types::ItemId::parse("0198e901-d208-7670-ae29-6834da5adfc3").unwrap();
    group.item_heads.push(product_only_head);
    validate_change_group(&repo_id(), &group).unwrap();

    group.changes.clear();
    validate_change_group(&repo_id(), &group).unwrap();
}

#[test]
fn operation_identity_excludes_refreshable_blob_evidence() {
    let operations = fixture(OPERATIONS);
    let mut request: OperationRequest = decode_value(&operations["request"]);
    validate_operation_request(&repo_id(), &operation_id(), &request).unwrap();
    request.blob_proofs[0] = OpaqueValue::parse("refreshed-proof").unwrap();
    validate_operation_request(&repo_id(), &operation_id(), &request).unwrap();

    request.message = Some("different operation".to_string());
    assert_eq!(
        validate_operation_request(&repo_id(), &operation_id(), &request)
            .unwrap_err()
            .code,
        ErrorCode::OperationIdMismatch
    );

    request.message = Some("x".repeat(synck_file_folder::MAX_MESSAGE_BYTES + 1));
    assert_eq!(
        validate_operation_request(&repo_id(), &operation_id(), &request)
            .unwrap_err()
            .code,
        ErrorCode::MessageTooLarge
    );
}

#[test]
fn projected_group_byte_limit_is_exact_and_inclusive() {
    let fixture = fixture(CHANGE_PAGES);
    let expected_limit = fixture["max_projected_group_bytes"].as_u64().unwrap() as usize;
    assert_eq!(expected_limit, synck_file_folder::MAX_PROJECTED_GROUP_BYTES);

    let mut group = ChangeGroup {
        repo_id: repo_id(),
        repo_seq: 1,
        commit_id: CommitId::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
        message: Some(String::new()),
        accepted_at: "2026-08-20T12:00:00Z".parse().unwrap(),
        changes: Vec::new(),
        item_heads: Vec::new(),
    };
    let base = serde_json_canonicalizer::to_vec(&serde_json::to_value(&group).unwrap())
        .unwrap()
        .len();
    group.message = Some("x".repeat(expected_limit - base));
    validate_projected_group_size(&group).unwrap();
    group.message.as_mut().unwrap().push('x');
    assert_eq!(
        validate_projected_group_size(&group).unwrap_err().code,
        ErrorCode::ProjectedGroupTooLarge
    );
}

#[test]
fn error_fixtures_have_exact_status_and_retryability() {
    let fixtures: Vec<Value> = serde_json::from_str(ERRORS).unwrap();
    for value in fixtures {
        let envelope: ErrorEnvelope = decode_value(&value);
        envelope.validate().unwrap();
        assert_eq!(envelope.error.retryable, envelope.error.code.retryable());
    }

    for (code, status, retryable) in error_contract() {
        assert_eq!(code.http_status(), status);
        assert_eq!(code.retryable(), retryable);
        let envelope = ErrorEnvelope::new(code);
        assert_eq!(
            decode_json::<ErrorEnvelope>(&encode_json(&envelope).unwrap()).unwrap(),
            envelope
        );
    }
    assert_eq!(ErrorCode::Unauthenticated.http_status(), 401);
    assert_eq!(ErrorCode::RepositoryForbidden.http_status(), 403);
}

fn error_contract() -> Vec<(ErrorCode, u16, bool)> {
    use ErrorCode::*;
    vec![
        (UnsupportedProtocolVersion, 400, false),
        (InvalidRequest, 400, false),
        (InvalidIdentifier, 400, false),
        (InvalidName, 400, false),
        (InvalidContent, 400, false),
        (EmptyIntent, 400, false),
        (MessageTooLarge, 400, false),
        (OperationIdMismatch, 400, false),
        (InvalidCursor, 400, false),
        (CursorBindingMismatch, 400, false),
        (InvalidBlobProof, 400, false),
        (InvalidRange, 400, false),
        (Unauthenticated, 401, false),
        (RepositoryForbidden, 403, false),
        (BlobNotFound, 404, false),
        (ItemNotFound, 409, false),
        (ItemAlreadyExists, 409, false),
        (ItemTombstoned, 409, false),
        (BaseSeqMismatch, 409, false),
        (ParentNotFound, 409, false),
        (ParentNotFolder, 409, false),
        (ParentNotLive, 409, false),
        (NameConflict, 409, false),
        (CycleDetected, 409, false),
        (NoOp, 409, false),
        (ConflictingIntent, 409, false),
        (LifecycleMismatch, 409, false),
        (SubtreeChanged, 409, false),
        (ReservedWorktreePath, 409, false),
        (BlobProofRequired, 409, true),
        (OperationOwnershipMismatch, 409, false),
        (CursorExpired, 410, false),
        (JsonTooLarge, 413, false),
        (TooManyIntents, 413, false),
        (TooManyEvents, 413, false),
        (SubtreeTooLarge, 413, false),
        (ProjectedGroupTooLarge, 413, false),
        (BlobTooLarge, 413, false),
        (RepositoryIneligible, 422, false),
        (RepositoryCorrupt, 500, false),
        (InternalError, 500, false),
        (ServerUnavailable, 503, true),
        (OperationPending, 503, true),
    ]
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[test]
fn constructed_shape_validation_covers_manifest_and_evidence_failures() {
    let manifest_fixture = fixture(MANIFEST_PAGES);
    let mut manifest: ManifestPageResponse = decode_value(&manifest_fixture["response"]);
    manifest.entries[0].content = Some(manifest.entries[1].content.clone().unwrap());
    assert_eq!(
        validate_manifest_page(&manifest.target_checkpoint.clone(), &manifest)
            .unwrap_err()
            .code,
        ErrorCode::InvalidContent
    );

    let blob_fixture = fixture(BLOB_TRANSFERS);
    let request: HaveNeedRequest = decode_value(&blob_fixture["request"]);
    let mut response: HaveNeedResponse = decode_value(&blob_fixture["need_response"]);
    response.results[0].upload.as_mut().unwrap().method = "POST".to_string();
    assert_eq!(
        validate_have_need_response(&repo_id(), &request, &response)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );

    let mut response: HaveNeedResponse = decode_value(&blob_fixture["need_response"]);
    response.results[0].upload.as_mut().unwrap().path = format!(
        "/synck/v1/repositories/{}/blob-uploads/opaque-upload-id",
        RepoId::parse("0198e8fa-664e-7e80-b134-94ea7ecec438").unwrap()
    );
    assert_eq!(
        validate_have_need_response(&repo_id(), &request, &response)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );

    let malformed_retry = json!({
        "protocol_version": 1,
        "error": {"code":"SERVER_UNAVAILABLE","retryable":false}
    });
    let envelope: ErrorEnvelope = decode_value(&malformed_retry);
    assert_eq!(
        envelope.validate().unwrap_err().code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn json_opaque_and_timestamp_bounds_are_exact() {
    assert!(OpaqueValue::parse("x".repeat(MAX_OPAQUE_BYTES)).is_ok());
    assert_eq!(
        OpaqueValue::parse("x".repeat(MAX_OPAQUE_BYTES + 1))
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );

    let oversized = vec![b' '; MAX_JSON_BYTES + 1];
    assert_eq!(
        decode_json::<RepositoryPage>(&oversized).unwrap_err().code,
        ErrorCode::JsonTooLarge
    );

    let transfers = fixture(BLOB_TRANSFERS);
    let mut response = transfers["have_response"].clone();
    response["results"][0]["expires_at"] = json!("2026-08-20T14:00:00+02:00");
    assert_eq!(
        decode_json::<HaveNeedResponse>(&serde_json::to_vec(&response).unwrap())
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn collection_and_blob_limits_are_exact() {
    let mut repositories: RepositoryPage = decode_json(REPOSITORIES.as_bytes()).unwrap();
    let repository = repositories.repositories[0].clone();
    repositories.repositories = (0..MAX_REPOSITORIES_PER_PAGE)
        .map(|index| RepositorySummary {
            repo_id: indexed_repo_id(index),
            name: repository.name.clone(),
        })
        .collect();
    validate_repository_page(&repositories).unwrap();
    repositories.repositories.push(RepositorySummary {
        repo_id: indexed_repo_id(MAX_REPOSITORIES_PER_PAGE),
        name: repository.name,
    });
    assert_eq!(
        validate_repository_page(&repositories).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    repositories.repositories.truncate(1);
    repositories.repositories[0].name = "x".repeat(MAX_REPOSITORY_NAME_BYTES);
    validate_repository_page(&repositories).unwrap();
    repositories.repositories[0].name.push('x');
    assert_eq!(
        validate_repository_page(&repositories).unwrap_err().code,
        ErrorCode::InvalidRequest
    );

    let manifest_fixture = fixture(MANIFEST_PAGES);
    let mut manifest: ManifestPageResponse = decode_value(&manifest_fixture["response"]);
    let entry = manifest.entries[1].clone();
    manifest.entries = (0..MAX_MANIFEST_ENTRIES_PER_PAGE)
        .map(|index| ManifestEntry {
            item_id: indexed_item_id(index),
            ..entry.clone()
        })
        .collect();
    validate_manifest_page(&manifest.target_checkpoint.clone(), &manifest).unwrap();
    manifest.entries.push(ManifestEntry {
        item_id: indexed_item_id(MAX_MANIFEST_ENTRIES_PER_PAGE),
        ..entry
    });
    assert_eq!(
        validate_manifest_page(&manifest.target_checkpoint.clone(), &manifest)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );

    let transfer_fixture = fixture(BLOB_TRANSFERS);
    let mut request: HaveNeedRequest = decode_value(&transfer_fixture["request"]);
    let blob = request.blobs[0].clone();
    request.blobs = (0..MAX_BLOB_DECLARATIONS)
        .map(|index| BlobRef {
            blob_id: indexed_blob_id(index),
            size: blob.size,
        })
        .collect();
    validate_have_need_request(&request).unwrap();
    request.blobs.push(BlobRef {
        blob_id: indexed_blob_id(MAX_BLOB_DECLARATIONS),
        size: blob.size,
    });
    assert_eq!(
        validate_have_need_request(&request).unwrap_err().code,
        ErrorCode::InvalidRequest
    );
    request.blobs.truncate(1);
    request.blobs[0].size = synck_file_folder::MAX_SAFE_JSON_INTEGER;
    validate_have_need_request(&request).unwrap();
    request.blobs[0].size += 1;
    assert_eq!(
        validate_have_need_request(&request).unwrap_err().code,
        ErrorCode::BlobTooLarge
    );

    let change_fixture = fixture(CHANGE_PAGES);
    let from: Checkpoint =
        serde_json::from_value(change_fixture["from_checkpoint"].clone()).unwrap();
    let group: ChangeGroup =
        serde_json::from_value(change_fixture["group_response"]["groups"][0].clone()).unwrap();
    let mut groups = Vec::with_capacity(MAX_COMMITS_PER_CHANGE_PAGE + 1);
    for offset in 1..=MAX_COMMITS_PER_CHANGE_PAGE {
        groups.push(ChangeGroup {
            repo_seq: (from.repo_seq + offset as u64) as i64,
            commit_id: indexed_commit_id(offset),
            ..group.clone()
        });
    }
    let mut target = Checkpoint {
        repo_seq: from.repo_seq + MAX_COMMITS_PER_CHANGE_PAGE as u64,
        tip_commit_id: Some(indexed_commit_id(MAX_COMMITS_PER_CHANGE_PAGE)),
    };
    let mut page = ChangePageResponse {
        protocol_version: ProtocolVersion,
        profile: Profile,
        target_checkpoint: target.clone(),
        page_through_checkpoint: target.clone(),
        groups,
        next_cursor: None,
    };
    validate_change_page(&repo_id(), &from, &target, &page).unwrap();
    page.groups.push(ChangeGroup {
        repo_seq: (target.repo_seq + 1) as i64,
        commit_id: indexed_commit_id(MAX_COMMITS_PER_CHANGE_PAGE + 1),
        ..group
    });
    target.repo_seq += 1;
    target.tip_commit_id = Some(indexed_commit_id(MAX_COMMITS_PER_CHANGE_PAGE + 1));
    page.target_checkpoint = target.clone();
    page.page_through_checkpoint = target.clone();
    assert_eq!(
        validate_change_page(&repo_id(), &from, &target, &page)
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn core_error_mapping_is_typed_and_fail_closed() {
    let repo_id = repo_id();
    let item_id = indexed_item_id(1);
    let exact = [
        (
            CoreError::ItemNotFound {
                repo_id: repo_id.clone(),
                item_id: item_id.clone(),
            },
            ErrorCode::ItemNotFound,
        ),
        (
            CoreError::ItemTombstoned {
                repo_id: repo_id.clone(),
                item_id: item_id.clone(),
            },
            ErrorCode::ItemTombstoned,
        ),
        (
            CoreError::ItemAlreadyExists {
                repo_id: repo_id.clone(),
                item_id: item_id.clone(),
            },
            ErrorCode::ItemAlreadyExists,
        ),
        (
            CoreError::ParentNotLive {
                repo_id: repo_id.clone(),
                parent_item_id: item_id.clone(),
            },
            ErrorCode::ParentNotLive,
        ),
        (
            CoreError::NameConflict {
                repo_id: repo_id.clone(),
                parent_folder_id: None,
                name: "conflict".to_string(),
            },
            ErrorCode::NameConflict,
        ),
        (
            CoreError::BaseSeqMismatch {
                repo_id: repo_id.clone(),
                item_id: item_id.clone(),
                expected: 1,
                found: 2,
            },
            ErrorCode::BaseSeqMismatch,
        ),
        (
            CoreError::SubtreeTooLarge {
                repo_id: repo_id.clone(),
                max_descendants: 9_999,
            },
            ErrorCode::SubtreeTooLarge,
        ),
        (
            CoreError::SubtreeChanged {
                repo_id: repo_id.clone(),
                root_item_id: item_id.clone(),
            },
            ErrorCode::SubtreeChanged,
        ),
        (
            CoreError::ItemIsFederated {
                repo_id: repo_id.clone(),
                item_id: item_id.clone(),
            },
            ErrorCode::RepositoryIneligible,
        ),
        (
            CoreError::OperationAlreadyApplied {
                repo_id: repo_id.clone(),
                operation_id: "operation".to_string(),
                commit_id: indexed_commit_id(1),
            },
            ErrorCode::InternalError,
        ),
        (
            CoreError::StorageError(StorageError::Overloaded {
                detail: "queue full".to_string(),
            }),
            ErrorCode::ServerUnavailable,
        ),
        (
            CoreError::StorageError(StorageError::UniqueViolation {
                detail: "name conflict text must not be parsed".to_string(),
                constraint: None,
            }),
            ErrorCode::InternalError,
        ),
        (
            CoreError::InvalidProposal("BASE_SEQ_MISMATCH text must not be parsed".to_string()),
            ErrorCode::InternalError,
        ),
    ];
    for (error, expected) in exact {
        assert_eq!(core_error_code(&error), expected);
        assert_eq!(ValidationError::from(error).code, expected);
    }
}
