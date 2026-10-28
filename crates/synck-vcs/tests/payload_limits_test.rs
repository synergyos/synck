use synck_vcs::{config::CoreConfig, types::*, validate_payload_limits};

fn event() -> EventProposal {
    serde_json::from_value(serde_json::json!({
        "item_id": "01890c6c-1111-7777-8888-aaaaaaaaaaaa",
        "event_type": "CHAT::MESSAGE::CREATE",
        "set_content": {"json": {"body": "a\u{0}🦀"}},
        "base_seq": 1
    }))
    .unwrap()
}

#[test]
fn returns_exact_canonical_payloads_in_item_then_ref_order() {
    let event = event();
    let reference: RefEventProposal = serde_json::from_value(serde_json::json!({
        "owner_item_id": event.item_id,
        "event_type": "FILE_VERSION::CREATE",
        "set_content": {"content_id": ContentId::from_digest([42; 32])},
        "base_ref_seq": 0
    }))
    .unwrap();
    let expected = vec![
        canonical_event_proposal_bytes(&event).unwrap(),
        canonical_ref_event_proposal_bytes(&reference).unwrap(),
    ];
    let bytes = expected.iter().map(Vec::len).sum();
    let mut config = CoreConfig::default();
    config.max_event_payload_bytes = expected.iter().map(Vec::len).max();
    config.max_commit_payload_bytes = Some(bytes);
    assert_eq!(
        validate_payload_limits(&config, &[event.clone()], &[reference.clone()]).unwrap(),
        expected
    );
    config.max_commit_payload_bytes = Some(bytes - 1);
    assert!(
        validate_payload_limits(&config, &[event.clone()], &[reference])
            .unwrap_err()
            .to_string()
            .contains("max_commit_payload_bytes")
    );
    config.max_commit_payload_bytes = None;
    config.max_event_payload_bytes = Some(expected[0].len() - 1);
    assert!(
        validate_payload_limits(&config, &[event], &[])
            .unwrap_err()
            .to_string()
            .contains("max_event_payload_bytes")
    );
}

#[test]
fn defaults_bound_escaped_bytes_not_plaintext_length() {
    let mut event = event();
    event.set_content = Some(EventContent::Json {
        json: serde_json::json!({"body": "\u{0}".repeat(200_000)}),
    });
    assert!(
        validate_payload_limits(&CoreConfig::default(), &[event.clone()], &[])
            .unwrap_err()
            .to_string()
            .contains("max_event_payload_bytes")
    );
    let mut config = CoreConfig::default();
    config.max_event_payload_bytes = None;
    config.max_commit_payload_bytes = None;
    assert_eq!(
        validate_payload_limits(&config, &[event.clone()], &[]).unwrap(),
        vec![canonical_event_proposal_bytes(&event).unwrap()]
    );
    assert!(
        validate_payload_limits(&config, &[], &[])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn lossless_json_rules_remain_enforced() {
    let mut event = event();
    event.set_metadata = Some(serde_json::from_str(r#"{"n":9007199254740993}"#).unwrap());
    assert!(validate_payload_limits(&CoreConfig::default(), &[event], &[]).is_err());
}

#[test]
fn ref_proposals_reject_explicit_initial_sequences() {
    let proposal = serde_json::json!({
        "owner_item_id": "01890c6c-1111-7777-8888-aaaaaaaaaaaa",
        "event_type": "FILE_VERSION::CREATE",
        "base_ref_seq": 0,
        "initial_ref_seq": 18
    });
    assert!(serde_json::from_value::<RefEventProposal>(proposal).unwrap_err()
        .to_string().contains("unknown field"));
}
