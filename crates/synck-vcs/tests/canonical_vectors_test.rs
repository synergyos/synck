use sha2::Digest;
use synck_vcs::errors::CoreError;
use synck_vcs::types::{
    CommitId, CreateItem, EventContent, EventProposal, ItemId, Sha256Digest,
    canonical_commit_bytes_for_hash, canonical_event_proposal_bytes, compute_commit_id,
    compute_sha256_digest_bytes, sha256_prefixed_hex_from_digest_bytes,
};

fn compute_events_digest(event_bytes: &[Vec<u8>]) -> Sha256Digest {
    let mut hasher = sha2::Sha256::new();
    for bytes in event_bytes {
        let event_hash = compute_sha256_digest_bytes(bytes);
        hasher.update(event_hash);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(digest))
        .expect("internal sha256 digest construction must be valid")
}

fn compute_commit_id_for_events(
    author_id: &str,
    message: Option<&str>,
    parent_commit_id: Option<&CommitId>,
    event_bytes: &[Vec<u8>],
) -> (String, String, String) {
    let events_digest = compute_events_digest(event_bytes);
    let commit_bytes = canonical_commit_bytes_for_hash(
        &author_id.to_string(),
        message,
        parent_commit_id,
        event_bytes.len(),
        &events_digest,
    )
    .expect("canonical commit bytes");
    let commit_json =
        String::from_utf8(commit_bytes.clone()).expect("commit bytes must be UTF-8 JSON");
    let commit_id = compute_commit_id(&commit_bytes);
    (
        events_digest.as_str().to_string(),
        commit_json,
        commit_id.as_str().to_string(),
    )
}

#[test]
fn canonical_vector_content_reference_does_not_change_commit_format() {
    let json = r#"{"base_seq":1,"event_type":"CHAT::MESSAGE::CREATE","item_id":"01890c6c-1111-7777-8888-aaaaaaaaaaaa","set_content":{"content_id":"sha256:2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a"}}"#;
    let event: EventProposal = serde_json::from_str(json).unwrap();
    let bytes = canonical_event_proposal_bytes(&event).unwrap();
    assert_eq!(bytes, json.as_bytes());
    let (digest, _, commit_id) = compute_commit_id_for_events("human", None, None, &[bytes]);
    assert_eq!(
        digest,
        "sha256:e9fa79a66dfde68c71ce717f200e07b435352b52467ece0124b4aedc6c6ee341"
    );
    assert_eq!(
        commit_id,
        "sha256:8f35cfde7189b20d9898d135f08691b079d8db1a60332a4bfb4d9e259aa19ae7"
    );
}

#[test]
fn canonical_vector_create_item() {
    let event = EventProposal {
        item_id: Some(ItemId::parse("01890c6c-1111-7777-8888-aaaaaaaaaaaa").unwrap()),
        event_type: "ITEM_CREATE".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: Some(CreateItem {
            item_type: "FILE".to_string(),
            parent_folder_id: None,
            name: "doc.txt".to_string(),
        }),
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: None,
    };

    let event_bytes = canonical_event_proposal_bytes(&event).unwrap();
    let event_json = String::from_utf8(event_bytes.clone()).unwrap();
    let event_hash = format!(
        "sha256:{}",
        hex::encode(compute_sha256_digest_bytes(&event_bytes))
    );

    let (events_digest, commit_json, commit_id) =
        compute_commit_id_for_events("author-1", Some("first"), None, &[event_bytes]);

    let expected_event_json = "{\"create_item\":{\"item_type\":\"FILE\",\"name\":\"doc.txt\"},\"event_type\":\"ITEM_CREATE\",\"item_id\":\"01890c6c-1111-7777-8888-aaaaaaaaaaaa\"}";
    let expected_event_hash =
        "sha256:c4450657eb41f5b4f2b44290b459265c68bd1ae2ce2489cc2d140b69ffdb6505";
    let expected_events_digest =
        "sha256:4fb901d0e362bb6fa5a8f01da2b49a84d5852a4a7553236810bd35f41168783a";
    let expected_commit_json = "{\"author_id\":\"author-1\",\"event_count\":1,\"events_digest\":\"sha256:4fb901d0e362bb6fa5a8f01da2b49a84d5852a4a7553236810bd35f41168783a\",\"format_version\":2,\"message\":\"first\",\"object_type\":\"vcs_commit\",\"parent_commit_id\":null}";
    let expected_commit_id =
        "sha256:02e539da96333d0576c331db0b23dcaa2dd136ef76d212bfae5828b62b8554d3";

    assert_eq!(event_json, expected_event_json);
    assert_eq!(event_hash, expected_event_hash);
    assert_eq!(events_digest, expected_events_digest);
    assert_eq!(commit_json, expected_commit_json);
    assert_eq!(commit_id, expected_commit_id);
}

#[test]
fn canonical_vector_update_text() {
    let event = EventProposal {
        item_id: Some(ItemId::parse("01890c6c-2222-7777-8888-bbbbbbbbbbbb").unwrap()),
        event_type: "UPDATE".to_string(),
        set_metadata: Some(serde_json::json!({"k":"v","n":1})),
        set_content: Some(EventContent::Text {
            text: "Hello".to_string(),
        }),
        create_item: None,
        set_parent: None,
        set_name: Some("New.txt".to_string()),
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(1),
    };

    let event_bytes = canonical_event_proposal_bytes(&event).unwrap();
    let event_json = String::from_utf8(event_bytes.clone()).unwrap();
    let event_hash = format!(
        "sha256:{}",
        hex::encode(compute_sha256_digest_bytes(&event_bytes))
    );

    let (events_digest, commit_json, commit_id) =
        compute_commit_id_for_events("author-2", None, None, &[event_bytes]);

    let expected_event_json = "{\"base_seq\":1,\"event_type\":\"UPDATE\",\"item_id\":\"01890c6c-2222-7777-8888-bbbbbbbbbbbb\",\"set_content\":{\"text\":\"Hello\"},\"set_metadata\":{\"k\":\"v\",\"n\":1},\"set_name\":\"New.txt\"}";
    let expected_event_hash =
        "sha256:2c775fe5631c3baaa7799d6b0eef75c3db61afe7ff6bfed3d8412b1d364bb4b6";
    let expected_events_digest =
        "sha256:53995897b30b2aace948af74c353c9d013009e38685adf76378082c09add5522";
    let expected_commit_json = "{\"author_id\":\"author-2\",\"event_count\":1,\"events_digest\":\"sha256:53995897b30b2aace948af74c353c9d013009e38685adf76378082c09add5522\",\"format_version\":2,\"message\":null,\"object_type\":\"vcs_commit\",\"parent_commit_id\":null}";
    let expected_commit_id =
        "sha256:67d1115d6c65e56a32fbf5f41a108248a04c8ed40d28f77ba0e08fa86aec68bd";

    assert_eq!(event_json, expected_event_json);
    assert_eq!(event_hash, expected_event_hash);
    assert_eq!(events_digest, expected_events_digest);
    assert_eq!(commit_json, expected_commit_json);
    assert_eq!(commit_id, expected_commit_id);
}

#[test]
fn canonical_vector_set_parent_move_to_root() {
    let event = EventProposal {
        item_id: Some(ItemId::parse("01890c6c-3333-7777-8888-cccccccccccc").unwrap()),
        event_type: "MOVE".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: Some(None),
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(1),
    };

    let event_bytes = canonical_event_proposal_bytes(&event).unwrap();
    let event_json = String::from_utf8(event_bytes.clone()).unwrap();
    let event_hash = format!(
        "sha256:{}",
        hex::encode(compute_sha256_digest_bytes(&event_bytes))
    );

    let (events_digest, commit_json, commit_id) =
        compute_commit_id_for_events("author-3", None, None, &[event_bytes]);

    let expected_event_json = "{\"base_seq\":1,\"event_type\":\"MOVE\",\"item_id\":\"01890c6c-3333-7777-8888-cccccccccccc\",\"set_parent\":null}";
    let expected_event_hash =
        "sha256:0ff22bb475411d497b0048d222c65bcfaf2832edfdfe432770c6dedf9d8a3ddb";
    let expected_events_digest =
        "sha256:29ed11b6ce464187e7238a09c18c818212a98a736bb733a02a187e62462d14fd";
    let expected_commit_json = "{\"author_id\":\"author-3\",\"event_count\":1,\"events_digest\":\"sha256:29ed11b6ce464187e7238a09c18c818212a98a736bb733a02a187e62462d14fd\",\"format_version\":2,\"message\":null,\"object_type\":\"vcs_commit\",\"parent_commit_id\":null}";
    let expected_commit_id =
        "sha256:635555157653ef8355d2455c177ebd00ec79a35458517042c565e566e883cd3b";

    assert_eq!(event_json, expected_event_json);
    assert_eq!(event_hash, expected_event_hash);
    assert_eq!(events_digest, expected_events_digest);
    assert_eq!(commit_json, expected_commit_json);
    assert_eq!(commit_id, expected_commit_id);
}

#[test]
fn canonical_vector_set_parent_move_under_folder() {
    let event = EventProposal {
        item_id: Some(ItemId::parse("01890c6c-4444-7777-8888-dddddddddddd").unwrap()),
        event_type: "MOVE".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: Some(Some(
            ItemId::parse("01890c6c-5555-7777-8888-eeeeeeeeeeee").unwrap(),
        )),
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(1),
    };

    let event_bytes = canonical_event_proposal_bytes(&event).unwrap();
    let event_json = String::from_utf8(event_bytes.clone()).unwrap();
    let event_hash = format!(
        "sha256:{}",
        hex::encode(compute_sha256_digest_bytes(&event_bytes))
    );

    let (events_digest, commit_json, commit_id) =
        compute_commit_id_for_events("author-4", None, None, &[event_bytes]);

    let expected_event_json = "{\"base_seq\":1,\"event_type\":\"MOVE\",\"item_id\":\"01890c6c-4444-7777-8888-dddddddddddd\",\"set_parent\":\"01890c6c-5555-7777-8888-eeeeeeeeeeee\"}";
    let expected_event_hash =
        "sha256:1fbea02a66f32238b3475294f2b5bc11f1cf99a183cb5f124d1b02879f1e41b5";
    let expected_events_digest =
        "sha256:cafb4b2f65714f20c032a6ae1bdd70cc9c69d124d169bfdda52461f380b9c76b";
    let expected_commit_json = "{\"author_id\":\"author-4\",\"event_count\":1,\"events_digest\":\"sha256:cafb4b2f65714f20c032a6ae1bdd70cc9c69d124d169bfdda52461f380b9c76b\",\"format_version\":2,\"message\":null,\"object_type\":\"vcs_commit\",\"parent_commit_id\":null}";
    let expected_commit_id =
        "sha256:a0c459189f453209ca822a90cf1a1b28222fd8f35604a1194a3e394b57058c1f";

    assert_eq!(event_json, expected_event_json);
    assert_eq!(event_hash, expected_event_hash);
    assert_eq!(events_digest, expected_events_digest);
    assert_eq!(commit_json, expected_commit_json);
    assert_eq!(commit_id, expected_commit_id);
}

#[test]
fn canonical_vector_federation_transition_federate() {
    let event = EventProposal {
        item_id: Some(ItemId::parse("01890c6c-6666-7777-8888-ffffffffffff").unwrap()),
        event_type: "FEDERATE".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: Some(true),
        set_federation_stream_id: Some("fid-1".to_string()),
        base_seq: Some(1),
    };

    let event_bytes = canonical_event_proposal_bytes(&event).unwrap();
    let event_json = String::from_utf8(event_bytes.clone()).unwrap();
    let event_hash = format!(
        "sha256:{}",
        hex::encode(compute_sha256_digest_bytes(&event_bytes))
    );

    let (events_digest, commit_json, commit_id) =
        compute_commit_id_for_events("author-5", Some("federate"), None, &[event_bytes]);

    let expected_event_json = "{\"base_seq\":1,\"event_type\":\"FEDERATE\",\"item_id\":\"01890c6c-6666-7777-8888-ffffffffffff\",\"set_federation_stream_id\":\"fid-1\",\"set_is_federated\":true}";
    let expected_event_hash =
        "sha256:61bc1b9f48d5c2d4555753c9f714d0dd525ee16e75c7c1b3ecfeeeac2b0b4a40";
    let expected_events_digest =
        "sha256:bdaea83d60782abce8cb3661ec459b7553d6bf35097199907c7e25cad273cbd8";
    let expected_commit_json = "{\"author_id\":\"author-5\",\"event_count\":1,\"events_digest\":\"sha256:bdaea83d60782abce8cb3661ec459b7553d6bf35097199907c7e25cad273cbd8\",\"format_version\":2,\"message\":\"federate\",\"object_type\":\"vcs_commit\",\"parent_commit_id\":null}";
    let expected_commit_id =
        "sha256:84435d2a404dd284b0491330acb156a4bc85416742ffad845d49b47f57992910";

    assert_eq!(event_json, expected_event_json);
    assert_eq!(event_hash, expected_event_hash);
    assert_eq!(events_digest, expected_events_digest);
    assert_eq!(commit_json, expected_commit_json);
    assert_eq!(commit_id, expected_commit_id);
}

#[test]
fn canonical_vector_rejects_lossy_integer_in_metadata() {
    let event = EventProposal {
        item_id: Some(ItemId::parse("01890c6c-7777-7777-8888-aaaaaaaaaaaa").unwrap()),
        event_type: "NUMBERS".to_string(),
        set_metadata: Some(serde_json::json!({
            "i": 9007199254740993_i64,
            "f": 0.1,
            "n": -12.5,
            "z": 0
        })),
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(1),
    };

    let error = canonical_event_proposal_bytes(&event).unwrap_err();
    assert!(matches!(
        error,
        CoreError::InvalidProposal(message)
            if message == "JSON integer 9007199254740993 cannot be represented by RFC 8785 without changing its value; encode it as a string"
    ));
}

#[test]
fn canonical_vector_json_numbers_exponent_and_negative_zero() {
    let event = EventProposal {
        item_id: Some(ItemId::parse("01890c6c-aaaa-7777-8888-aaaaaaaaaaaa").unwrap()),
        event_type: "NUMBERS2".to_string(),
        set_metadata: Some(serde_json::json!({
            "neg_zero": -0.0,
            "exp": 1e-7,
        })),
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(1),
    };

    let event_bytes = canonical_event_proposal_bytes(&event).unwrap();
    let event_json = String::from_utf8(event_bytes.clone()).unwrap();
    let event_hash = format!(
        "sha256:{}",
        hex::encode(compute_sha256_digest_bytes(&event_bytes))
    );

    let (events_digest, commit_json, commit_id) =
        compute_commit_id_for_events("author-9", None, None, &[event_bytes]);

    let expected_event_json = "{\"base_seq\":1,\"event_type\":\"NUMBERS2\",\"item_id\":\"01890c6c-aaaa-7777-8888-aaaaaaaaaaaa\",\"set_metadata\":{\"exp\":1e-7,\"neg_zero\":0}}";
    let expected_event_hash =
        "sha256:f7e3c53aff4c0a9e6f47a4183b7b9ebc9a9acc02fe9e417c17d290d9e7806e98";
    let expected_events_digest =
        "sha256:6579d303b82504165b6ad6fa0ef47891da6a70b82ebb45ccc5d432d04d5abe1e";
    let expected_commit_json = "{\"author_id\":\"author-9\",\"event_count\":1,\"events_digest\":\"sha256:6579d303b82504165b6ad6fa0ef47891da6a70b82ebb45ccc5d432d04d5abe1e\",\"format_version\":2,\"message\":null,\"object_type\":\"vcs_commit\",\"parent_commit_id\":null}";
    let expected_commit_id =
        "sha256:ebcfa0ed2e7b1bf37339adf8630c8d1f23d4c17a6ae454bb2df398d39671fdc0";

    assert_eq!(event_json, expected_event_json);
    assert_eq!(event_hash, expected_event_hash);
    assert_eq!(events_digest, expected_events_digest);
    assert_eq!(commit_json, expected_commit_json);
    assert_eq!(commit_id, expected_commit_id);
}

#[test]
fn canonical_vector_unicode_control_chars_and_separators() {
    let event = EventProposal {
        item_id: Some(ItemId::parse("01890c6c-bbbb-7777-8888-bbbbbbbbbbbb").unwrap()),
        event_type: "UNICODE_ESC".to_string(),
        set_metadata: Some(serde_json::json!({
            "nul": "a\u{0000}b",
            "line_sep": "\u{2028}",
            "para_sep": "\u{2029}",
            "ws": "a\tb\nc",
        })),
        set_content: Some(EventContent::Text {
            text: "x\u{0000}y\u{2028}z".to_string(),
        }),
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(1),
    };

    let event_bytes = canonical_event_proposal_bytes(&event).unwrap();
    let event_json = String::from_utf8(event_bytes.clone()).unwrap();
    let event_hash = format!(
        "sha256:{}",
        hex::encode(compute_sha256_digest_bytes(&event_bytes))
    );

    let (events_digest, commit_json, commit_id) =
        compute_commit_id_for_events("author-10", None, None, &[event_bytes]);

    let expected_event_json = "{\"base_seq\":1,\"event_type\":\"UNICODE_ESC\",\"item_id\":\"01890c6c-bbbb-7777-8888-bbbbbbbbbbbb\",\"set_content\":{\"text\":\"x\\u0000y\u{2028}z\"},\"set_metadata\":{\"line_sep\":\"\u{2028}\",\"nul\":\"a\\u0000b\",\"para_sep\":\"\u{2029}\",\"ws\":\"a\\tb\\nc\"}}";
    let expected_event_hash =
        "sha256:5a125fc06464b548a84db7d67a1206d5f009bbc8680e102fe990e415c20d52e4";
    let expected_events_digest =
        "sha256:238b0e26b591d0f30a820675852ccbf1f49e0786b9f0fe1bacb9a0ce59f0b545";
    let expected_commit_json = "{\"author_id\":\"author-10\",\"event_count\":1,\"events_digest\":\"sha256:238b0e26b591d0f30a820675852ccbf1f49e0786b9f0fe1bacb9a0ce59f0b545\",\"format_version\":2,\"message\":null,\"object_type\":\"vcs_commit\",\"parent_commit_id\":null}";
    let expected_commit_id =
        "sha256:23aec1bec7e8677cdd8bd1bd545f7b2d98d1daeb4c67b40cdb3bc33fb7bc298a";

    assert_eq!(event_json, expected_event_json);
    assert_eq!(event_hash, expected_event_hash);
    assert_eq!(events_digest, expected_events_digest);
    assert_eq!(commit_json, expected_commit_json);
    assert_eq!(commit_id, expected_commit_id);
}

#[test]
fn canonical_vector_federation_transition_unfederate() {
    let event = EventProposal {
        item_id: Some(ItemId::parse("01890c6c-cccc-7777-8888-cccccccccccc").unwrap()),
        event_type: "UNFEDERATE".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: Some(false),
        set_federation_stream_id: None,
        base_seq: Some(1),
    };

    let event_bytes = canonical_event_proposal_bytes(&event).unwrap();
    let event_json = String::from_utf8(event_bytes.clone()).unwrap();
    let event_hash = format!(
        "sha256:{}",
        hex::encode(compute_sha256_digest_bytes(&event_bytes))
    );

    let (events_digest, commit_json, commit_id) =
        compute_commit_id_for_events("author-11", Some("unfederate"), None, &[event_bytes]);

    let expected_event_json = "{\"base_seq\":1,\"event_type\":\"UNFEDERATE\",\"item_id\":\"01890c6c-cccc-7777-8888-cccccccccccc\",\"set_is_federated\":false}";
    let expected_event_hash =
        "sha256:741290999d66ba4877a1b3f9abb059cca66c9609d9a0c573967d0cd383469d8c";
    let expected_events_digest =
        "sha256:80d414401391f7c20af0cba77154e02b72416c1f31a28fe10160461acf5e826d";
    let expected_commit_json = "{\"author_id\":\"author-11\",\"event_count\":1,\"events_digest\":\"sha256:80d414401391f7c20af0cba77154e02b72416c1f31a28fe10160461acf5e826d\",\"format_version\":2,\"message\":\"unfederate\",\"object_type\":\"vcs_commit\",\"parent_commit_id\":null}";
    let expected_commit_id =
        "sha256:f1f380ce38a9c8db92fbef8613c87701bec72d4bdf5da386fd21891bf5f36e80";

    assert_eq!(event_json, expected_event_json);
    assert_eq!(event_hash, expected_event_hash);
    assert_eq!(events_digest, expected_events_digest);
    assert_eq!(commit_json, expected_commit_json);
    assert_eq!(commit_id, expected_commit_id);
}

#[test]
fn canonical_vector_unicode_metadata_and_text() {
    let event = EventProposal {
        item_id: Some(ItemId::parse("01890c6c-8888-7777-8888-bbbbbbbbbbbb").unwrap()),
        event_type: "UNICODE".to_string(),
        set_metadata: Some(serde_json::json!({
            "emoji": "😺",
            "combining": "e\u{0301}",
            "jp": "ねこ"
        })),
        set_content: Some(EventContent::Text {
            text: "e\u{0301} 😺 ねこ".to_string(),
        }),
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(1),
    };

    let event_bytes = canonical_event_proposal_bytes(&event).unwrap();
    let event_json = String::from_utf8(event_bytes.clone()).unwrap();
    let event_hash = format!(
        "sha256:{}",
        hex::encode(compute_sha256_digest_bytes(&event_bytes))
    );

    let (events_digest, commit_json, commit_id) =
        compute_commit_id_for_events("author-7", None, None, &[event_bytes]);

    let expected_event_json = "{\"base_seq\":1,\"event_type\":\"UNICODE\",\"item_id\":\"01890c6c-8888-7777-8888-bbbbbbbbbbbb\",\"set_content\":{\"text\":\"é 😺 ねこ\"},\"set_metadata\":{\"combining\":\"é\",\"emoji\":\"😺\",\"jp\":\"ねこ\"}}";
    let expected_event_hash =
        "sha256:67c52e35b4f57df0c91d24ec733dadf45eec79a23559d10bc0efb56bc17656e6";
    let expected_events_digest =
        "sha256:75590b63837e92a20690340f158b7ac03e04f6534dea2f69f2fb3e819e000f14";
    let expected_commit_json = "{\"author_id\":\"author-7\",\"event_count\":1,\"events_digest\":\"sha256:75590b63837e92a20690340f158b7ac03e04f6534dea2f69f2fb3e819e000f14\",\"format_version\":2,\"message\":null,\"object_type\":\"vcs_commit\",\"parent_commit_id\":null}";
    let expected_commit_id =
        "sha256:3f0d3d1d06dc918989db0faa779c6b33b1ab58ff7ed0c1e27a6ca3771445d586";

    assert_eq!(event_json, expected_event_json);
    assert_eq!(event_hash, expected_event_hash);
    assert_eq!(events_digest, expected_events_digest);
    assert_eq!(commit_json, expected_commit_json);
    assert_eq!(commit_id, expected_commit_id);
}

#[test]
fn canonical_vector_large_payload_digest_stability() {
    let big = "abcd".repeat(16 * 1024); // 64 KiB

    let event = EventProposal {
        item_id: Some(ItemId::parse("01890c6c-9999-7777-8888-cccccccccccc").unwrap()),
        event_type: "BIG".to_string(),
        set_metadata: Some(serde_json::json!({
            "note": "large payload vector",
            "len": big.len()
        })),
        set_content: Some(EventContent::Text { text: big }),
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: Some(1),
    };

    let event_bytes = canonical_event_proposal_bytes(&event).unwrap();
    let event_hash = format!(
        "sha256:{}",
        hex::encode(compute_sha256_digest_bytes(&event_bytes))
    );

    let (events_digest, commit_json, commit_id) =
        compute_commit_id_for_events("author-8", Some("big"), None, &[event_bytes]);

    let expected_event_hash =
        "sha256:294b710e8f11be6330acb7440ea71356cc156a8a6e2c0c9c94d3c9b8d5c747b7";
    let expected_events_digest =
        "sha256:761dc37f48725c11e3d2b845d16f33f5e7d06e20569313a2ab1db1afa9994d3b";
    let expected_commit_json = "{\"author_id\":\"author-8\",\"event_count\":1,\"events_digest\":\"sha256:761dc37f48725c11e3d2b845d16f33f5e7d06e20569313a2ab1db1afa9994d3b\",\"format_version\":2,\"message\":\"big\",\"object_type\":\"vcs_commit\",\"parent_commit_id\":null}";
    let expected_commit_id =
        "sha256:db6e2c756842d46ee1015d13dc14e0d42d8bb4105a5a7260635bff809c47f46a";

    assert_eq!(event_hash, expected_event_hash);
    assert_eq!(events_digest, expected_events_digest);
    assert_eq!(commit_json, expected_commit_json);
    assert_eq!(commit_id, expected_commit_id);
}
