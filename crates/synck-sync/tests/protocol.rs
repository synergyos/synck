use chrono::{TimeZone, Utc};
use synck_sync::*;
use synck_vcs::types::*;
use uuid::Uuid;

fn repository() -> RepositoryIdentity {
    RepositoryIdentity {
        repo_id: RepoId::parse("019a2900-0000-7000-8000-000000000001").unwrap(),
        generation: Uuid::parse_str("019a2900-0000-7000-8000-000000000002").unwrap(),
    }
}

fn request() -> SubmitRequest {
    serde_json::from_value(serde_json::json!({
        "repository": repository(),
        "proposal": {
            "author_id": "owner", "committer_id": "device", "operation_id": "op:1",
            "events": [{
                "item_id": "019a2900-0000-7000-8000-000000000003",
                "event_type": "FILE_CREATE",
                "create_item": {"item_type": "FILE", "name": "hello.txt"},
                "set_content": {"bytes": format!("sha256:{}", "ab".repeat(32))}
            }]
        }
    }))
    .unwrap()
}

fn digest(bytes: [u8; 32]) -> Sha256Digest {
    Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(bytes)).unwrap()
}

fn page() -> HistoryPage {
    let request = request();
    let validated = validate_submit(&request).unwrap();
    let created_at = Utc.timestamp_opt(1_793_145_600, 123_456_789).unwrap();
    let payload = canonical_event_proposal_bytes(&request.proposal.events[0]).unwrap();
    let row = EventRow {
        repo_id: request.repository.repo_id.clone(),
        item_id: request.proposal.events[0].item_id.clone().unwrap(),
        item_seq: 1,
        event_type: "FILE_CREATE".into(),
        commit_id: validated.commit_id.clone(),
        commit_event_idx: 0,
        repo_seq: Some(5),
        event_payload_digest: digest(compute_sha256_digest_bytes(&payload)),
        payload,
        metadata: None,
        created_at,
        parent_folder_id: None,
        parent_folder_id_before: None,
    };
    let checkpoint = digest(
        synck_vcs::checkpoint::digest(
            &synck_vcs::checkpoint::GENESIS,
            5,
            &validated.commit_id,
            created_at,
        )
        .unwrap(),
    );
    let commit = CommitRow {
        repo_id: request.repository.repo_id.clone(),
        commit_id: validated.commit_id,
        repo_seq: Some(5),
        format_version: COMMIT_FORMAT_VERSION,
        import_job_id: None,
        author_id: request.proposal.author_id,
        committer_id: request.proposal.committer_id,
        message: None,
        occurred_at: None,
        created_at,
        event_count: 1,
        events_digest: validated.events_digest,
    };
    HistoryPage {
        after: Cursor::genesis(repository()),
        commits: vec![PublishedCommit {
            commit,
            events: vec![CommitEventRow::Item(row)],
            checkpoint: checkpoint.clone(),
        }],
        next: Cursor {
            repository: repository(),
            repo_seq: 5,
            checkpoint,
        },
        has_more: false,
    }
}

#[test]
fn stable_identity_and_required_object_closure() {
    let original = request();
    let first = validate_submit(&original).unwrap();
    let wire = serde_json::to_vec(&original).unwrap();
    let replay: SubmitRequest = serde_json::from_slice(&wire).unwrap();
    let second = validate_submit(&replay).unwrap();
    assert_eq!(first.commit_id, second.commit_id);
    assert_eq!(first.request_digest, second.request_digest);
    assert_eq!(first.required_objects.len(), 1);
    let mut different = original;
    different.proposal.operation_id = Some("op:2".into());
    let different = validate_submit(&different).unwrap();
    assert_eq!(first.commit_id, different.commit_id);
    assert_ne!(first.request_digest, different.request_digest);
}

#[test]
fn generation_and_guards_bind_request_but_not_commit_content() {
    let mut request = request();
    let first = validate_submit(&request).unwrap();
    request.repository.generation = Uuid::now_v7();
    let changed = validate_submit(&request).unwrap();
    assert_ne!(first.request_digest, changed.request_digest);
    assert_eq!(first.commit_id, changed.commit_id);
    request.head_guards.push(ItemHead {
        item_id: ItemId::new(),
        head_seq: 3,
    });
    assert_ne!(
        changed.request_digest,
        validate_submit(&request).unwrap().request_digest
    );
}

#[test]
fn reject_unstable_and_ambiguous_proposals() {
    let mut changed = request();
    changed.proposal.operation_id = None;
    assert!(validate_submit(&changed).is_err());
    changed = request();
    changed.proposal.events[0].item_id = None;
    assert!(validate_submit(&changed).is_err());
    changed = request();
    changed.proposal.events[0].base_seq = Some(1);
    assert!(validate_submit(&changed).is_err());
    changed = request();
    changed.proposal.events[0].create_item = None;
    assert!(validate_submit(&changed).is_err());
    changed = request();
    changed.head_guards.push(ItemHead {
        item_id: ItemId::new(),
        head_seq: 0,
    });
    assert!(validate_submit(&changed).is_err());
    changed = request();
    changed.proposal.events = vec![changed.proposal.events[0].clone(); MAX_EVENTS + 1];
    assert!(validate_submit(&changed).is_err());
}

#[test]
fn verify_publication_and_valid_sequence_gaps() {
    let page = page();
    assert_eq!(verify_history_page(&page, &page.after).unwrap(), page.next);
    let empty = HistoryPage {
        after: page.next.clone(),
        next: page.next.clone(),
        commits: vec![],
        has_more: false,
    };
    assert_eq!(
        verify_history_page(&empty, &empty.after).unwrap(),
        page.next
    );
}

#[test]
fn reject_tampering_replay_and_wrong_history() {
    let mut changed = page();
    changed.commits[0].commit.created_at += chrono::Duration::seconds(1);
    assert!(verify_history_page(&changed, &changed.after).is_err());
    changed = page();
    if let CommitEventRow::Item(row) = &mut changed.commits[0].events[0] {
        row.payload.push(b' ');
    }
    assert!(verify_history_page(&changed, &changed.after).is_err());
    changed = page();
    changed.commits[0].events.clear();
    assert!(verify_history_page(&changed, &changed.after).is_err());
    changed = page();
    changed.next.repo_seq += 1;
    assert!(verify_history_page(&changed, &changed.after).is_err());
    changed = page();
    let mut previous = changed.after.clone();
    previous.repository.generation = Uuid::now_v7();
    assert!(matches!(
        verify_history_page(&changed, &previous),
        Err(ProtocolError::Generation)
    ));
    changed = page();
    let mut previous = changed.after.clone();
    previous.repo_seq = 1;
    assert!(verify_history_page(&changed, &previous).is_err());
}

#[test]
fn recovered_receipt_must_match_exact_pending_operation() {
    let request = request();
    let validated = validate_submit(&request).unwrap();
    let mut receipt = Receipt {
        operation_id: request.proposal.operation_id.clone().unwrap(),
        request_digest: validated.request_digest,
        commit_id: validated.commit_id,
        accepted_at: Utc::now(),
        published: None,
    };
    receipt.validate_for(&request).unwrap();
    receipt.operation_id = "other".into();
    assert!(receipt.validate_for(&request).is_err());
    receipt.operation_id = request.proposal.operation_id.clone().unwrap();
    receipt.published = Some(Cursor::genesis(repository()));
    assert!(receipt.validate_for(&request).is_err());
}

#[test]
fn top_level_wire_values_reject_unknown_fields() {
    let mut value = serde_json::to_value(request()).unwrap();
    value["extension"] = serde_json::json!(true);
    assert!(serde_json::from_value::<SubmitRequest>(value).is_err());
    let mut empty = page();
    empty.commits.clear();
    empty.next = empty.after.clone();
    empty.has_more = true;
    assert!(verify_history_page(&empty, &empty.after).is_err());
}

#[test]
fn independent_wire_vector_matches_canonical_hashes() {
    let vector: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/sync.json")).unwrap();
    let request: SubmitRequest = serde_json::from_value(vector["request"].clone()).unwrap();
    let validated = validate_submit(&request).unwrap();
    assert_eq!(
        validated.commit_id.as_str(),
        vector["commit_id"].as_str().unwrap()
    );
    assert_eq!(
        validated.events_digest.as_str(),
        vector["events_digest"].as_str().unwrap()
    );
    assert_eq!(
        validated.request_digest.as_str(),
        vector["request_digest"].as_str().unwrap()
    );
    assert_eq!(
        canonical_event_proposal_bytes(&request.proposal.events[0]).unwrap(),
        vector["canonical_event_utf8"].as_str().unwrap().as_bytes()
    );
}
