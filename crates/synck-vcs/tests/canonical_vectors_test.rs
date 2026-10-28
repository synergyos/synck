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
        hasher.update([0]);
        hasher.update(event_hash);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(digest))
        .expect("internal sha256 digest construction must be valid")
}

fn compute_commit_id_for_events(
    author_id: &str,
    message: Option<&str>,
    _parent_commit_id: Option<&CommitId>,
    event_bytes: &[Vec<u8>],
) -> (String, String, String) {
    let events_digest = compute_events_digest(event_bytes);
    let commit_bytes = canonical_commit_bytes_for_hash(
        &author_id.to_string(),
        &author_id.to_string(),
        None,
        message,
        None,
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
        "sha256:ce66c68cbf6cdf5e4671923aadb6c3010cf06cd37b79165df945386fda5cf078"
    );
    assert_eq!(
        commit_id,
        "sha256:5832264e0fedaacd515fd4f6c4fbbac249ceb5aa223c98a1d1042d6f27408b87"
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
        "sha256:8039be6671e20d856866b03828312241c0a2768ad3a66b9263e976984a066e8c";
    let expected_commit_json = "{\"author_id\":\"author-1\",\"committer_id\":\"author-1\",\"event_count\":1,\"events_digest\":\"sha256:8039be6671e20d856866b03828312241c0a2768ad3a66b9263e976984a066e8c\",\"format_version\":6,\"import_job_id\":null,\"message\":\"first\",\"object_type\":\"vcs_commit\",\"occurred_at\":null}";
    let expected_commit_id =
        "sha256:bf9e962d3e2a09bc9203e6213bf0a08ee38a750c93c6c284741958381fc0aca8";

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
        "sha256:53cc9b22551f9939dab4178acf10a06a6ad5c870560dc1d488fd32b71d776189";
    let expected_commit_json = "{\"author_id\":\"author-2\",\"committer_id\":\"author-2\",\"event_count\":1,\"events_digest\":\"sha256:53cc9b22551f9939dab4178acf10a06a6ad5c870560dc1d488fd32b71d776189\",\"format_version\":6,\"import_job_id\":null,\"message\":null,\"object_type\":\"vcs_commit\",\"occurred_at\":null}";
    let expected_commit_id =
        "sha256:7d874225c9ef5e8c1eda2231e7aef4aa5186b91576732d1c49cea499b3471ce4";

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
        "sha256:8c21e2a50d74ab0c8865672bed3c1f78cdf0dff36219abeed8983a5cf420a188";
    let expected_commit_json = "{\"author_id\":\"author-3\",\"committer_id\":\"author-3\",\"event_count\":1,\"events_digest\":\"sha256:8c21e2a50d74ab0c8865672bed3c1f78cdf0dff36219abeed8983a5cf420a188\",\"format_version\":6,\"import_job_id\":null,\"message\":null,\"object_type\":\"vcs_commit\",\"occurred_at\":null}";
    let expected_commit_id =
        "sha256:0983533cb3fd10db144bc8b423ae8a2623f338302e3edba14d75c76a6d43a008";

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
        "sha256:8b914cddbd62473d48fc866cd970d452732fb28b4ff678b77a7c720b2944f536";
    let expected_commit_json = "{\"author_id\":\"author-4\",\"committer_id\":\"author-4\",\"event_count\":1,\"events_digest\":\"sha256:8b914cddbd62473d48fc866cd970d452732fb28b4ff678b77a7c720b2944f536\",\"format_version\":6,\"import_job_id\":null,\"message\":null,\"object_type\":\"vcs_commit\",\"occurred_at\":null}";
    let expected_commit_id =
        "sha256:d047f52aeed915f37985aba31aa49857094f3b5732186bbc5a52397533aa364c";

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
        "sha256:7382152c11bc7032bf6a95624f72b761a1623e80640cf809dc917e682bf2c0af";
    let expected_commit_json = "{\"author_id\":\"author-5\",\"committer_id\":\"author-5\",\"event_count\":1,\"events_digest\":\"sha256:7382152c11bc7032bf6a95624f72b761a1623e80640cf809dc917e682bf2c0af\",\"format_version\":6,\"import_job_id\":null,\"message\":\"federate\",\"object_type\":\"vcs_commit\",\"occurred_at\":null}";
    let expected_commit_id =
        "sha256:2fbfe58370d3cd024ecb29c25a35662f6bdbe478e80de1f20a19841f780e070d";

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
        "sha256:f8056ab5392df252b7cac165bf5ba55a99519aea73575a918400899c51432e80";
    let expected_commit_json = "{\"author_id\":\"author-9\",\"committer_id\":\"author-9\",\"event_count\":1,\"events_digest\":\"sha256:f8056ab5392df252b7cac165bf5ba55a99519aea73575a918400899c51432e80\",\"format_version\":6,\"import_job_id\":null,\"message\":null,\"object_type\":\"vcs_commit\",\"occurred_at\":null}";
    let expected_commit_id =
        "sha256:f6cac05475ad8246712f07fe0969f9f98e60f13bd300ddddce3fe08d80fe33e7";

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
        "sha256:26dcd38286c4e4d8a939e7f8443436f39041384cbded8eb791e96680aab1acc2";
    let expected_commit_json = "{\"author_id\":\"author-10\",\"committer_id\":\"author-10\",\"event_count\":1,\"events_digest\":\"sha256:26dcd38286c4e4d8a939e7f8443436f39041384cbded8eb791e96680aab1acc2\",\"format_version\":6,\"import_job_id\":null,\"message\":null,\"object_type\":\"vcs_commit\",\"occurred_at\":null}";
    let expected_commit_id =
        "sha256:3c2bf9e7e50e8e3284b622ead97bfcb8605c6e38edd4f0c05955eeadf1429505";

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
        "sha256:85ace6c4f7d4618d1b6e6e6abf076159f6bb07b9ecff23e6733863c148be146f";
    let expected_commit_json = "{\"author_id\":\"author-11\",\"committer_id\":\"author-11\",\"event_count\":1,\"events_digest\":\"sha256:85ace6c4f7d4618d1b6e6e6abf076159f6bb07b9ecff23e6733863c148be146f\",\"format_version\":6,\"import_job_id\":null,\"message\":\"unfederate\",\"object_type\":\"vcs_commit\",\"occurred_at\":null}";
    let expected_commit_id =
        "sha256:038a3342cde0b19c99c2c2c5b2cd4d404686bcecc72f6eb3938a92eea8933642";

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
        "sha256:d1b2e92d203c33d40d086c82406efd175584b843b70229de31d7a4517162d8b9";
    let expected_commit_json = "{\"author_id\":\"author-7\",\"committer_id\":\"author-7\",\"event_count\":1,\"events_digest\":\"sha256:d1b2e92d203c33d40d086c82406efd175584b843b70229de31d7a4517162d8b9\",\"format_version\":6,\"import_job_id\":null,\"message\":null,\"object_type\":\"vcs_commit\",\"occurred_at\":null}";
    let expected_commit_id =
        "sha256:b578fdb641778b0a70f3f7cd2b45b08970acad5e6ee433bbdbe4e4a9e0ce63f4";

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
        "sha256:f7ffd4f76c39384f08bad6e650ef2bd0fd67e0b79084d63aee660ee75be3d935";
    let expected_commit_json = "{\"author_id\":\"author-8\",\"committer_id\":\"author-8\",\"event_count\":1,\"events_digest\":\"sha256:f7ffd4f76c39384f08bad6e650ef2bd0fd67e0b79084d63aee660ee75be3d935\",\"format_version\":6,\"import_job_id\":null,\"message\":\"big\",\"object_type\":\"vcs_commit\",\"occurred_at\":null}";
    let expected_commit_id =
        "sha256:e71742447c1ab27f91dc9aacaa51933e710e12d41ff031c494e28934ab336e73";

    assert_eq!(event_hash, expected_event_hash);
    assert_eq!(events_digest, expected_events_digest);
    assert_eq!(commit_json, expected_commit_json);
    assert_eq!(commit_id, expected_commit_id);
}
