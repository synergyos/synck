use synck_vcs::{limits::*, types::*, validate_payload_limits};

fn event() -> EventProposal {
    serde_json::from_value(serde_json::json!({
        "item_id": "01890c6c-1111-7777-8888-aaaaaaaaaaaa",
        "event_type": "CHAT::MESSAGE::CREATE", "base_seq": 1
    }))
    .unwrap()
}

#[test]
fn format_payload_limit_checks_exact_canonical_bytes() {
    let mut event = event();
    let overhead = canonical_event_proposal_bytes(&event).unwrap().len() - event.event_type.len();
    event.event_type = "x".repeat(MAX_EVENT_PAYLOAD_BYTES - overhead);
    assert_eq!(
        canonical_event_proposal_bytes(&event).unwrap().len(),
        MAX_EVENT_PAYLOAD_BYTES
    );
    event.event_type.push('x');
    assert!(
        canonical_event_proposal_bytes(&event)
            .unwrap_err()
            .to_string()
            .contains("event payload")
    );
    event.event_type = "\0".repeat(200_000);
    assert!(canonical_event_proposal_bytes(&event).is_err());
}

#[test]
fn format_has_no_commit_event_or_aggregate_payload_cap() {
    let event = event();
    let reference: RefEventProposal = serde_json::from_value(serde_json::json!({
        "owner_item_id": event.item_id, "event_type": "FILE_VERSION::CREATE", "base_ref_seq": 0
    }))
    .unwrap();
    let expected = vec![
        canonical_event_proposal_bytes(&event).unwrap(),
        canonical_ref_event_proposal_bytes(&reference).unwrap(),
    ];
    assert_eq!(
        validate_payload_limits(&[event.clone()], &[reference]).unwrap(),
        expected
    );
    let mut large = event.clone();
    large.event_type = "CHAT::".to_owned() + &"x".repeat(512 * 1024);
    assert_eq!(
        validate_payload_limits(&vec![large; 10], &[])
            .unwrap()
            .len(),
        10
    );
    assert_eq!(
        validate_payload_limits(&vec![event; 10_001], &[])
            .unwrap()
            .len(),
        10_001
    );
    assert!(validate_payload_limits(&[], &[]).unwrap().is_empty());
}

#[test]
fn format_metadata_limits_are_fixed_utf8_bytes() {
    let author = "é".repeat(MAX_ACTOR_ID_BYTES / 2);
    let message = "é".repeat(MAX_MESSAGE_BYTES / 2);
    let operation = "é".repeat(MAX_OPERATION_ID_BYTES / 2);
    validate_commit_metadata(&author, &author, Some(&message), Some(&operation)).unwrap();
    for result in [
        validate_commit_metadata(&(author.clone() + "x"), &author, None, None),
        validate_commit_metadata(&author, &(author.clone() + "x"), None, None),
        validate_commit_metadata(&author, &author, Some(&(message.clone() + "x")), None),
        validate_commit_metadata(&author, &author, None, Some(&(operation.clone() + "x"))),
        validate_commit_metadata("", &author, None, None),
        validate_commit_metadata(&author, "", None, None),
        validate_commit_metadata(&author, &author, None, Some(" ")),
    ] {
        assert!(result.is_err());
    }
    let digest = Sha256Digest::parse(format!("sha256:{}", "00".repeat(32))).unwrap();
    assert!(
        canonical_stored_commit_bytes_for_hash(
            COMMIT_FORMAT_VERSION,
            &author,
            &author,
            None,
            Some(&(message + "x")),
            None,
            1,
            &digest
        )
        .is_err()
    );
}

#[test]
fn format_name_limit_applies_to_readable_canonical_events() {
    let mut event = event();
    event.set_name = Some("x".repeat(MAX_NAME_BYTES));
    assert!(canonical_event_proposal_bytes(&event).is_ok());
    event.set_name.as_mut().unwrap().push('x');
    assert!(canonical_event_proposal_bytes(&event).is_err());
    assert!(synck_vcs::naming::compute_name_key(event.set_name.as_ref().unwrap()).is_err());
}

#[test]
fn bundle_reader_enforces_metadata_and_operation_limits() {
    let mut record = serde_json::json!({"type":"commit", "commit_id":format!("sha256:{}", "00".repeat(32)),
        "checkpoint":format!("sha256:{}", "00".repeat(32)), "repo_seq":1,
        "operation_id":"x".repeat(MAX_OPERATION_ID_BYTES), "import_job_id":null,
        "format_version":COMMIT_FORMAT_VERSION, "author_id":"a", "committer_id":"c",
        "message":"x".repeat(MAX_MESSAGE_BYTES), "occurred_at":null,
        "created_at":"2026-10-28T00:00:00Z", "event_count":1, "events_digest":format!("sha256:{}", "00".repeat(32))});
    let read = |record: &serde_json::Value| {
        synck_vcs::bundle::read_record(
            &mut std::io::Cursor::new(serde_json::to_vec(record).unwrap()),
            synck_vcs::bundle::BUNDLE_FORMAT_VERSION,
        )
    };
    assert!(read(&record).is_ok());
    for (field, maximum) in [
        ("operation_id", MAX_OPERATION_ID_BYTES),
        ("message", MAX_MESSAGE_BYTES),
        ("author_id", MAX_ACTOR_ID_BYTES),
        ("committer_id", MAX_ACTOR_ID_BYTES),
    ] {
        let saved = record[field].clone();
        record[field] = serde_json::json!("x".repeat(maximum + 1));
        assert!(read(&record).is_err(), "{field}");
        record[field] = saved;
    }
}

#[test]
fn lossless_json_rules_remain_enforced() {
    assert!(
        synck_vcs::metadata::MetadataRef::new(
            serde_json::from_str(r#"{"n":9007199254740993}"#).unwrap()
        )
        .is_err()
    );
}

#[test]
fn ref_proposals_reject_explicit_initial_sequences() {
    let proposal = serde_json::json!({"owner_item_id": "01890c6c-1111-7777-8888-aaaaaaaaaaaa",
        "event_type": "FILE_VERSION::CREATE", "base_ref_seq":0, "initial_ref_seq":18});
    assert!(serde_json::from_value::<RefEventProposal>(proposal).is_err());
}
