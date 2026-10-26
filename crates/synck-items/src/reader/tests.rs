use super::*;
use serde_json::json;
use synck_vcs::types::CreateItem;

const A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
fn now() -> DateTime<Utc> {
    "2026-10-25T12:00:00Z".parse().unwrap()
}
fn event(id: &ItemId, ty: &str, metadata: Value, content: Option<EventContent>) -> EventProposal {
    serde_json::from_value(
        json!({"item_id":id,"event_type":ty,"set_metadata":metadata,"set_content":content}),
    )
    .unwrap()
}
fn apply(reader: &mut ItemReader, event: &EventProposal, author: &str) {
    reader
        .apply(
            event,
            ReadContext {
                item_seq: reader.latest_item_seq() + 1,
                created_at: now(),
                occurred_at: None,
                author,
                committer: author,
            },
        )
        .unwrap();
}
fn create(kind: &str) -> (ItemReader, ItemId) {
    let id = ItemId::new();
    let mut reader = ItemReader::new(id.clone());
    let mut first = event(&id, &format!("{kind}::CREATE"), json!({}), None);
    first.create_item = Some(CreateItem {
        item_type: kind.into(),
        name: "Example".into(),
        parent_folder_id: None,
    });
    apply(&mut reader, &first, "alice");
    (reader, id)
}
fn json_body(body: &str) -> Option<EventContent> {
    Some(EventContent::Json {
        json: json!({"body":body}),
    })
}
fn row<'a>(snapshot: &'a ReadableItem, family: &str, seq: i64) -> &'a ReadableSubitem {
    snapshot
        .subitems
        .iter()
        .find(|row| row.family == family && row.created_position.seq == seq)
        .unwrap()
}

#[test]
fn messages_keep_creation_attribution_replies_raw_text_and_replacement_metadata() {
    let (mut reader, id) = create("CHAT");
    apply(
        &mut reader,
        &event(
            &id,
            "CHAT::MESSAGE::CREATE",
            json!({"meta":{"sender_label":"Alice","reply_to_item_seq":17,"keep":true}}),
            json_body(" original\0🦀 "),
        ),
        "alice",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "CHAT::MESSAGE::SET_CONTENT",
            json!({"item_seq":2,"meta":{"changed":true}}),
            json_body("edited\0body"),
        ),
        "editor",
    );
    let snapshot = reader.snapshot().unwrap();
    let message = row(&snapshot, "CHAT::MESSAGE", 2);
    assert_eq!(message.sender_id.as_deref(), Some("alice"));
    assert_eq!(message.sender_label.as_deref(), Some("Alice"));
    assert_eq!(message.reply_to_item_seq, Some(17));
    assert_eq!(message.body.as_deref(), Some("edited\0body"));
    assert_eq!(message.metadata, json!({"changed":true}));
    assert_eq!(message.content, json!({"body":"edited\0body"}));
    assert_eq!(message.created_position.seq, 2);
    assert_eq!(message.latest_position.seq, 3);
    for (verb, state) in [
        ("TRASH", SubitemLifecycle::Trash),
        ("RESTORE", SubitemLifecycle::Live),
        ("TOMBSTONE", SubitemLifecycle::Tombstoned),
    ] {
        apply(
            &mut reader,
            &event(
                &id,
                &format!("CHAT::MESSAGE::{verb}"),
                json!({"item_seq":2}),
                None,
            ),
            "editor",
        );
        assert_eq!(
            row(&reader.snapshot().unwrap(), "CHAT::MESSAGE", 2).lifecycle,
            state
        );
    }
}

#[test]
fn reactions_keep_reactor_and_attachment_kind_survives_nonobject_metadata_replacement() {
    let (mut reader, id) = create("CHAT");
    apply(
        &mut reader,
        &event(
            &id,
            "CHAT::MESSAGE::CREATE",
            Value::Null,
            json_body("message"),
        ),
        "alice",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "CHAT::MESSAGE.REACTION::CREATE",
            json!({"parent_item_seq":2}),
            Some(EventContent::Json {
                json: json!({"emoji":" 👍 "}),
            }),
        ),
        "bob",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "CHAT::MESSAGE.REACTION::SET_CONTENT",
            json!({"item_seq":3}),
            Some(EventContent::Json {
                json: json!({"emoji":" ✅ "}),
            }),
        ),
        "editor",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "CHAT::MESSAGE.ATTACHMENT::CREATE",
            json!({"parent_item_seq":2,"meta":{"kind":"EMBEDDED","snapshot_name":"photo.png"},"content":{"size":12}}),
            Some(EventContent::Blob { blob_id: A.into() }),
        ),
        "alice",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "CHAT::MESSAGE.ATTACHMENT::SET_METADATA",
            json!({"item_seq":5,"meta":["retained"]}),
            None,
        ),
        "editor",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "CHAT::MESSAGE.ATTACHMENT::SET_CONTENT",
            json!({"item_seq":5}),
            Some(EventContent::Blob { blob_id: B.into() }),
        ),
        "editor",
    );
    let snapshot = reader.snapshot().unwrap();
    let reaction = row(&snapshot, "CHAT::MESSAGE.REACTION", 3);
    assert_eq!(reaction.sender_id.as_deref(), Some("bob"));
    assert_eq!(reaction.emoji.as_deref(), Some("✅"));
    let attachment = row(&snapshot, "CHAT::MESSAGE.ATTACHMENT", 5);
    assert_eq!(attachment.attachment_kind.as_deref(), Some("EMBEDDED"));
    assert_eq!(attachment.metadata, json!(["retained"]));
    assert_eq!(attachment.blob_id.as_deref(), Some(B));
    assert_eq!(attachment.size, Some(12));
}

#[test]
fn folder_removal_detaches_attachments_and_rehomes_native_file_names() {
    let (mut reader, id) = create("CHAT");
    let folder = Uuid::now_v7();
    let file = Uuid::now_v7();
    apply(
        &mut reader,
        &event(
            &id,
            "CHAT::FILE_FOLDER::CREATE",
            json!({"meta":{"folder_id":folder,"name":"Documents"}}),
            None,
        ),
        "alice",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "CHAT::FILE::CREATE",
            json!({"meta":{"file_id":file,"name":"contract.pdf","parent_folder_id":folder}}),
            None,
        ),
        "alice",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "CHAT::MESSAGE.ATTACHMENT::CREATE",
            json!({"parent_item_seq":10,"meta":{"kind":"EMBEDDED","chat_file_folder_id":folder}}),
            Some(EventContent::Blob { blob_id: A.into() }),
        ),
        "alice",
    );
    apply(
        &mut reader,
        &event(&id, "CHAT::FILE_FOLDER::TRASH", json!({"item_seq":2}), None),
        "alice",
    );
    let snapshot = reader.snapshot().unwrap();
    let file = row(&snapshot, "CHAT::FILE", 3);
    assert_eq!(file.name.as_deref(), Some("contract (moved-3).pdf"));
    assert_eq!(file.parent_folder_id, None);
    assert_eq!(
        file.metadata["v2_projection_moved_from_folder_id"],
        json!(folder)
    );
    let attachment = row(&snapshot, "CHAT::MESSAGE.ATTACHMENT", 4);
    assert_eq!(attachment.parent_folder_id, None);
    assert!(attachment.metadata.get("chat_file_folder_id").is_none());
}

#[test]
fn legacy_task_reorder_renumbers_siblings_and_preserves_creator_metadata() {
    let (mut reader, id) = create("TASK_LIST");
    for subject in ["First", "Second", "Third"] {
        apply(
            &mut reader,
            &event(
                &id,
                "TASK_LIST::TASK::CREATE",
                json!({"meta":{"subject":subject}}),
                None,
            ),
            "alice",
        );
    }
    apply(
        &mut reader,
        &event(
            &id,
            "TASK_LIST::TASK::SET_METADATA",
            json!({"item_seq":4,"before_item_seq":2,"meta":{"subject":"Moved","created_by":"forged"}}),
            None,
        ),
        "bob",
    );
    let snapshot = reader.snapshot().unwrap();
    assert_eq!(
        row(&snapshot, "TASK_LIST::TASK", 4).metadata["index"],
        json!(0)
    );
    assert_eq!(
        row(&snapshot, "TASK_LIST::TASK", 2).metadata["index"],
        json!(1)
    );
    assert_eq!(
        row(&snapshot, "TASK_LIST::TASK", 3).metadata["index"],
        json!(2)
    );
    assert_eq!(
        row(&snapshot, "TASK_LIST::TASK", 4).metadata["created_by"],
        json!("alice")
    );
    apply(
        &mut reader,
        &event(
            &id,
            "TASK_LIST::TASK.COMMENT::CREATE",
            json!({"parent_item_seq":4,"meta":{"comment_id":Uuid::now_v7()}}),
            json_body(" note "),
        ),
        "bob",
    );
    assert_eq!(
        row(&reader.snapshot().unwrap(), "TASK_LIST::TASK.COMMENT", 6)
            .body
            .as_deref(),
        Some("note")
    );
    apply(
        &mut reader,
        &event(
            &id,
            "TASK_LIST::TASK::TOMBSTONE",
            json!({"item_seq":4}),
            None,
        ),
        "alice",
    );
    assert!(
        reader
            .snapshot()
            .unwrap()
            .subitems
            .iter()
            .all(|row| row.family != "TASK_LIST::TASK.COMMENT")
    );
}

#[test]
fn table_records_keep_data_version_and_comment_identity_through_edits() {
    let (mut reader, id) = create("TABLE");
    let record = Uuid::now_v7();
    let comment = Uuid::now_v7();
    apply(
        &mut reader,
        &event(
            &id,
            "TABLE::RECORD::CREATE",
            json!({"meta":{"record_id":record}}),
            Some(EventContent::Json {
                json: json!({"data":{"name":"Acme","amount":7},"version":4}),
            }),
        ),
        "alice",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "TABLE::RECORD::SET_CONTENT",
            json!({"item_seq":2}),
            Some(EventContent::Json {
                json: json!({"display_value":"Renamed"}),
            }),
        ),
        "bob",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "TABLE::RECORD.COMMENT::CREATE",
            json!({"parent_item_seq":2,"meta":{"record_id":record,"comment_id":comment}}),
            json_body(" comment "),
        ),
        "bob",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "TABLE::RECORD.COMMENT::SET_CONTENT",
            json!({"item_seq":4}),
            json_body(" revised "),
        ),
        "bob",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "TABLE::RECORD.COMMENT::TRASH",
            json!({"item_seq":4}),
            None,
        ),
        "bob",
    );
    let snapshot = reader.snapshot().unwrap();
    let record = row(&snapshot, "TABLE::RECORD", 2);
    assert_eq!(record.data, Some(json!({"name":"Acme","amount":7})));
    assert_eq!(record.record_version, Some(4));
    assert_eq!(record.display_value.as_deref(), Some("Renamed"));
    let comment = row(&snapshot, "TABLE::RECORD.COMMENT", 4);
    assert_eq!(comment.stable_id, Some(comment_id(&comment.metadata)));
    assert_eq!(comment.body.as_deref(), Some("revised"));
    assert_eq!(comment.lifecycle, SubitemLifecycle::Tombstoned);
}
fn comment_id(meta: &Value) -> Uuid {
    Uuid::parse_str(meta["comment_id"].as_str().unwrap()).unwrap()
}

#[test]
fn post_and_email_readers_expose_bodies_comments_assets_and_metadata_replacement() {
    let (mut reader, id) = create("POST");
    apply(
        &mut reader,
        &event(
            &id,
            "POST::SET_METADATA",
            json!({"title":"News","body_markdown_blob_id":A}),
            None,
        ),
        "alice",
    );
    let comment = Uuid::now_v7();
    let asset = Uuid::now_v7();
    apply(
        &mut reader,
        &event(
            &id,
            "POST::COMMENT::CREATE",
            json!({"comment_id":comment}),
            Some(EventContent::Json {
                json: json!({"body_markdown":"hello\0world"}),
            }),
        ),
        "bob",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "POST::ASSET::CREATE",
            json!({"asset_id":asset,"blob_id":B,"filename":"photo.png","size":10}),
            None,
        ),
        "alice",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "POST::ASSET::TOMBSTONE",
            json!({"asset_id":asset}),
            None,
        ),
        "alice",
    );
    apply(
        &mut reader,
        &event(&id, "POST::COMMENT::TRASH", json!({"item_seq":3}), None),
        "bob",
    );
    let snapshot = reader.snapshot().unwrap();
    assert_eq!(snapshot.post.unwrap().body_blob_id.as_deref(), Some(A));
    assert!(
        snapshot
            .subitems
            .iter()
            .any(|row| row.stable_id == Some(comment)
                && row.body.as_deref() == Some("hello\0world")
                && row.lifecycle == SubitemLifecycle::Trash)
    );
    assert!(
        snapshot
            .subitems
            .iter()
            .any(|row| row.stable_id == Some(asset)
                && row.blob_id.as_deref() == Some(B)
                && row.lifecycle == SubitemLifecycle::Tombstoned)
    );
    let (mut reader, id) = create("EMAIL");
    apply(
        &mut reader,
        &event(
            &id,
            "EMAIL::ATTACHMENT::CREATE",
            json!({"meta":{"filename":"document.pdf"}}),
            Some(EventContent::Blob { blob_id: A.into() }),
        ),
        "alice",
    );
    apply(
        &mut reader,
        &event(
            &id,
            "EMAIL::ATTACHMENT::SET_METADATA",
            json!({"item_seq":2,"meta":{"name":"renamed"}}),
            None,
        ),
        "bob",
    );
    assert_eq!(
        row(&reader.snapshot().unwrap(), "EMAIL::ATTACHMENT", 2).metadata,
        json!({"name":"renamed"})
    );
}

#[test]
fn native_domains_use_existing_typed_reducers_and_remain_serializable() {
    let cases = [
        (
            "CALENDAR",
            json!({"title":"Calendar","color":"#336699","description":null,"archived":false}),
        ),
        (
            "CALENDAR_EVENT",
            json!({"calendar_item_id":ItemId::new(),"case_item_id":null,"title":"Meeting","time":{"kind":"TIMED","start":"2026-10-25T12:00:00Z","end":"2026-10-25T13:00:00Z","time_zone":"UTC"},"location":null,"meeting_url":null,"organizer_user_id":"alice","state":"SCHEDULED","attendees":[],"invitation_counter":0,"reminders":{"counter":0,"entries":[]},"reminder_schedule_generation":0}),
        ),
        (
            "TIME_SETTINGS",
            json!({"currency":"EUR","offices":[],"practices":[]}),
        ),
        (
            "TIME_BOOK",
            json!({"user_id":"alice","display_name":"Alice","hourly_rate_minor":null,"office_id":null,"practice_id":null,"active_entry_id":null}),
        ),
        (
            "TIME_ENTRY",
            json!({"book_id":ItemId::new(),"user_id":"alice","display_name":"Alice","draft":{"work_date":"2026-10-25","time_zone":"UTC","seconds":600,"description":"Review","assignment":{"kind":"INTERNAL"},"classification":"NONBILLABLE","office_id":null,"practice_id":null,"hourly_rate_minor":null,"rate_override":false},"status":"DRAFT","currency":"EUR","value_minor":0,"intervals":[]}),
        ),
        (
            "CALL",
            json!({"kind":"CREATE","creation":{"creator_id":"alice","created_at":"2026-10-25T12:00:00Z","expires_at":"2026-10-25T13:00:00Z","origin":{"kind":"STANDALONE","recipient_id":"bob"},"media":"AUDIO","transport":"GROUP_LIVEKIT"}}),
        ),
    ];
    for (kind, meta) in cases {
        let id = ItemId::new();
        let mut reader = ItemReader::new(id.clone());
        let mut first = event(&id, &format!("{kind}::CREATE"), meta, None);
        let name = match kind {
            "CALL" => calls::history::internal_name(&id),
            "CALENDAR" | "CALENDAR_EVENT" => native_calendars::history::internal_name(kind, &id),
            _ => format!("{}-{id}", kind.to_ascii_lowercase()),
        };
        first.create_item = Some(CreateItem {
            item_type: kind.into(),
            name,
            parent_folder_id: None,
        });
        apply(&mut reader, &first, "alice");
        let snapshot = reader.snapshot().unwrap();
        assert!(!matches!(snapshot.native, NativeState::Generic), "{kind}");
        serde_json::to_value(snapshot).unwrap();
    }
    let id = ItemId::new();
    let mut reader = ItemReader::new(id.clone());
    let data = tasks::history::TaskData {
        task_list_id: None,
        parent_task_id: None,
        meta: json!({"subject":"Review","created_by":"alice","created_at":"2026-10-25T12:00:00Z"}),
    }
    .normalized()
    .unwrap();
    let mut first = event(
        &id,
        "TASK::CREATE",
        serde_json::to_value(data).unwrap(),
        None,
    );
    first.create_item = Some(CreateItem {
        item_type: "TASK".into(),
        name: tasks::history::internal_name(&id),
        parent_folder_id: None,
    });
    apply(&mut reader, &first, "alice");
    let mut comment = event(
        &id,
        "TASK::COMMENT::CREATE",
        json!({"comment_id":Uuid::now_v7()}),
        json_body("task note"),
    );
    comment.base_seq = Some(1);
    apply(&mut reader, &comment, "bob");
    assert!(matches!(
        reader.snapshot().unwrap().native,
        NativeState::Task { .. }
    ));
    assert_eq!(
        reader.snapshot().unwrap().subitems[0].body.as_deref(),
        Some("task note")
    );
}

#[test]
fn errors_poison_only_the_typed_reader_and_do_not_modify_source_events() {
    let (mut reader, id) = create("CHAT");
    let unknown = event(
        &id,
        "CHAT::FUTURE_DOMAIN_CHANGE",
        json!({"retained":"opaque"}),
        None,
    );
    let before = synck_vcs::types::canonical_event_proposal_bytes(&unknown).unwrap();
    assert!(matches!(
        reader.apply(
            &unknown,
            ReadContext {
                item_seq: 2,
                created_at: now(),
                occurred_at: None,
                author: "alice",
                committer: "alice"
            }
        ),
        Err(ReadError::Unsupported { .. })
    ));
    assert!(reader.snapshot().is_err());
    assert_eq!(
        synck_vcs::types::canonical_event_proposal_bytes(&unknown).unwrap(),
        before
    );
    assert!(
        crate::references::event_references(&unknown)
            .unwrap()
            .blobs
            .is_empty()
    );
}

#[test]
fn version_sidecars_and_owner_refs_keep_uuid_identity_and_separate_ledger_positions() {
    let repo = RepoId::new();
    let file = ItemId::new();
    let sidecar_id = synck_file_folder::file_version_set::item_id(&repo, &file).unwrap();
    let version = Uuid::now_v7();
    let mut sidecar = ItemReader::for_repository(repo.clone(), sidecar_id.clone());
    apply(
        &mut sidecar,
        &synck_file_folder::file_version_set::create_event(sidecar_id.clone(), &file),
        "alice",
    );
    let mut version_event = synck_file_folder::file_version_set::version_event(
        sidecar_id.clone(),
        "FILE_VERSION_SET::VERSION_CREATE",
        &file,
        &version.to_string(),
        json!({"kind":"MAJOR","order_key":1,"description":"Original","content":{"size":12}})
            .as_object()
            .unwrap()
            .clone(),
        Some(EventContent::Blob { blob_id: A.into() }),
    );
    apply(&mut sidecar, &version_event, "alice");
    version_event.event_type = "FILE_VERSION_SET::VERSION_SET_DESCRIPTION".into();
    version_event.set_content = None;
    version_event.set_metadata.as_mut().unwrap()["description"] = json!("Described");
    apply(&mut sidecar, &version_event, "alice");
    let sidecar = sidecar.snapshot().unwrap();
    assert_eq!(sidecar.subitems[0].version_label.as_deref(), Some("1.0"));
    let mut reader = ItemReader::for_repository(repo, file.clone());
    let mut first = event(
        &file,
        "FILE::CREATE",
        Value::Null,
        Some(EventContent::Blob { blob_id: A.into() }),
    );
    first.create_item = Some(CreateItem {
        item_type: "FILE".into(),
        name: "document.txt".into(),
        parent_folder_id: None,
    });
    apply(&mut reader, &first, "alice");
    let baseline = RefEventProposal {
        owner_item_id: file.clone(),
        event_type: "FILE_VERSION::LEGACY_BASELINE".into(),
        set_metadata: Some(
            json!({"legacy_sidecar_item_id":sidecar_id,"legacy_sidecar_head_seq":3,"legacy_sidecar_head_event_digest":A}),
        ),
        set_content: None,
        base_ref_seq: 0,
        initial_ref_seq: Some(3),
    };
    reader
        .apply_ref(
            &baseline,
            RefReadContext {
                ref_seq: 3,
                created_at: now(),
                author: "alice",
                committer: "alice",
            },
        )
        .unwrap();
    assert!(matches!(reader.snapshot(), Err(ReadError::NeedsHistory(_))));
    reader.seed_legacy_versions(&sidecar).unwrap();
    let edit = RefEventProposal {
        owner_item_id: file,
        event_type: "FILE_VERSION::SET_DESCRIPTION".into(),
        set_metadata: Some(
            json!({"file_item_id":reader.item_id,"version_id":version,"description":"New description"}),
        ),
        set_content: None,
        base_ref_seq: 3,
        initial_ref_seq: None,
    };
    reader
        .apply_ref(
            &edit,
            RefReadContext {
                ref_seq: 4,
                created_at: now(),
                author: "alice",
                committer: "alice",
            },
        )
        .unwrap();
    let snapshot = reader.snapshot().unwrap();
    let version = &snapshot.subitems[0];
    assert_eq!(version.body.as_deref(), Some("New description"));
    assert_eq!(version.stable_id, Some(version_id(&version.metadata)));
    assert!(matches!(
        version.created_position.ledger,
        HistoryLedger::Item
    ));
    assert!(matches!(version.latest_position.ledger, HistoryLedger::Ref));
    assert_eq!(snapshot.projection.latest_item_seq, 1);
    assert_eq!(snapshot.latest_ref_seq, 4);
}
fn version_id(meta: &Value) -> Uuid {
    Uuid::parse_str(meta["version_id"].as_str().unwrap()).unwrap()
}

#[test]
fn contacts_expose_full_payload_and_contact_lifecycle_without_private_curation() {
    let id = ItemId::new();
    let contact = Uuid::now_v7();
    let mut reader = ItemReader::new(id.clone());
    let mut first = event(
        &id,
        "TABLE::CREATE",
        json!({"system_registry":true,"system_registry_key":"CONTACTS","contact_scope":"REPO"}),
        None,
    );
    first.create_item = Some(CreateItem {
        item_type: "TABLE".into(),
        name: "Contacts".into(),
        parent_folder_id: None,
    });
    apply(&mut reader, &first, "alice");
    let payload = json!({"schema_version":1,"contact_id":contact,"display_name":"Ada Example","sort_name":"Example, Ada","scope":"REPO","curation_state":"CURATED","lifecycle_state":"LIVE","notes":"Retained context","emails":[{"email":"ada@example.test"}]});
    apply(
        &mut reader,
        &event(
            &id,
            "TABLE::RECORD::CREATE",
            json!({"meta":{"record_id":contact}}),
            Some(EventContent::Json {
                json: json!({"data":payload}),
            }),
        ),
        "alice",
    );
    let snapshot = reader.snapshot().unwrap();
    let record = row(&snapshot, "TABLE::RECORD", 2);
    assert_eq!(record.name.as_deref(), Some("Ada Example"));
    assert_eq!(record.body.as_deref(), Some("Retained context"));
    assert_eq!(
        record.data.as_ref().unwrap()["emails"][0]["email"],
        json!("ada@example.test")
    );
    apply(
        &mut reader,
        &event(&id, "TABLE::RECORD::TRASH", json!({"item_seq":2}), None),
        "alice",
    );
    assert_eq!(
        row(&reader.snapshot().unwrap(), "TABLE::RECORD", 2)
            .record_lifecycle
            .as_deref(),
        Some("ARCHIVED")
    );
    apply(
        &mut reader,
        &event(&id, "TABLE::RECORD::RESTORE", json!({"item_seq":2}), None),
        "alice",
    );
    assert_eq!(
        row(&reader.snapshot().unwrap(), "TABLE::RECORD", 2)
            .record_lifecycle
            .as_deref(),
        Some("LIVE")
    );
}

#[test]
fn data_only_file_edits_carry_previous_resource_activation() {
    let (mut reader, id) = create("FILE");
    apply(
        &mut reader,
        &event(
            &id,
            "FILE::SET_CONTENT",
            json!({"content":{"size":5}}),
            Some(EventContent::Blob { blob_id: A.into() }),
        ),
        "alice",
    );
    let fork = RefEventProposal {
        owner_item_id: id.clone(),
        event_type: "FILE_RESOURCE_FORK::SET".into(),
        set_metadata: Some(json!({"primary_item_seq":2,"primary_commit_event_idx":0,"size":7})),
        set_content: Some(EventContent::Blob { blob_id: B.into() }),
        base_ref_seq: 0,
        initial_ref_seq: None,
    };
    reader
        .apply_ref(
            &fork,
            RefReadContext {
                ref_seq: 1,
                created_at: now(),
                author: "alice",
                committer: "alice",
            },
        )
        .unwrap();
    apply(
        &mut reader,
        &event(
            &id,
            "FILE::SET_CONTENT",
            json!({"content":{"size":8}}),
            Some(EventContent::Blob { blob_id: B.into() }),
        ),
        "alice",
    );
    let snapshot = reader.snapshot().unwrap();
    assert_eq!(snapshot.latest_ref_seq, 1);
    assert_eq!(snapshot.projection.content_item_seq, Some(3));
    let fork = snapshot.resource_fork.unwrap();
    assert_eq!(fork.primary_item_seq, 2);
    assert!(
        matches!(fork.fork,crate::file_resources::ResourceFork::Present(ref blob) if blob.blob_id.as_str()==B&&blob.size==7)
    );
}

#[test]
fn supplemental_links_and_workflow_annotations_do_not_hide_primary_content() {
    let (mut reader, id) = create("FILE");
    apply(
        &mut reader,
        &event(
            &id,
            "FILE::SET_CONTENT",
            Value::Null,
            Some(EventContent::Blob { blob_id: A.into() }),
        ),
        "alice",
    );
    let link = json!({"link_key":"RELATED","database_key":"DB","table_key":"PEOPLE","record_id":Uuid::now_v7(),"display_value":"Ada"});
    apply(
        &mut reader,
        &event(&id, "ITEM::DATA_LINK::PUT", link.clone(), None),
        "alice",
    );
    assert_eq!(
        reader.snapshot().unwrap().data_links[0].display_value,
        "Ada"
    );
    apply(
        &mut reader,
        &event(
            &id,
            "FILE::REVISION_SUPPRESS",
            json!({"content_item_seq":2,"reason":"retained"}),
            None,
        ),
        "alice",
    );
    let snapshot = reader.snapshot().unwrap();
    assert_eq!(snapshot.projection.content_blob_id.as_deref(), Some(A));
    assert_eq!(snapshot.ancillary_events.len(), 1);
    assert_eq!(snapshot.presentation_warnings.len(), 1);
    apply(
        &mut reader,
        &event(&id, "ITEM::DATA_LINK::DELETE", link, None),
        "alice",
    );
    assert!(reader.snapshot().unwrap().data_links.is_empty());
    let (mut folder, id) = create("FOLDER");
    apply(
        &mut folder,
        &event(
            &id,
            "FOLDER::WORKFLOW_RUNTIME",
            json!({"kind":"RUN_LOG","retained":{"steps":[]}}),
            None,
        ),
        "alice",
    );
    assert_eq!(
        folder.snapshot().unwrap().ancillary_events[0]
            .metadata
            .as_ref()
            .unwrap()["kind"],
        json!("RUN_LOG")
    );
}

#[test]
fn translated_sidecar_root_retains_provenance_without_executing_or_requiring_private_services() {
    let (mut reader, id) = create("FILE");
    let sidecar = ItemId::new();
    let marker = RefEventProposal {
        owner_item_id: id.clone(),
        event_type: "FILE_VERSION::LEGACY_SIDECAR_ROOT".into(),
        set_metadata: Some(
            json!({"file_item_id":id,"legacy_sidecar_item_id":sidecar,"legacy_sidecar_head_seq":3,"legacy_sidecar_head_event_digest":A,"legacy_sidecar_root_event_digest":B}),
        ),
        set_content: None,
        base_ref_seq: 0,
        initial_ref_seq: None,
    };
    reader
        .apply_ref(
            &marker,
            RefReadContext {
                ref_seq: 1,
                created_at: now(),
                author: "alice",
                committer: "alice",
            },
        )
        .unwrap();
    let snapshot = reader.snapshot().unwrap();
    assert_eq!(snapshot.legacy_sidecar_roots[0].item_id, sidecar);
    assert_eq!(snapshot.legacy_sidecar_roots[0].root_event_digest, B);
    assert_eq!(reader.required_sidecars().count(), 0);
}

#[test]
fn legacy_inline_and_current_ref_versions_share_one_label_sequence() {
    let repo = RepoId::new();
    let file = ItemId::new();
    let mut reader = ItemReader::for_repository(repo, file.clone());
    let mut first = event(
        &file,
        "FILE::CREATE",
        Value::Null,
        Some(EventContent::Blob { blob_id: A.into() }),
    );
    first.create_item = Some(CreateItem {
        item_type: "FILE".into(),
        name: "doc.txt".into(),
        parent_folder_id: None,
    });
    apply(&mut reader, &first, "alice");
    apply(
        &mut reader,
        &event(
            &file,
            "FILE::VERSION::CREATE",
            json!({"meta":{"kind":"MAJOR","order_key":1024}}),
            Some(EventContent::Blob { blob_id: A.into() }),
        ),
        "alice",
    );
    let reference = RefEventProposal {
        owner_item_id: file.clone(),
        event_type: "FILE_VERSION::CREATE".into(),
        set_metadata: Some(
            json!({"file_item_id":file,"version_id":Uuid::now_v7(),"operation_id":"new-version","kind":"MAJOR","label_source":"INTERNAL","order_key":2048}),
        ),
        set_content: Some(EventContent::Blob { blob_id: A.into() }),
        base_ref_seq: 0,
        initial_ref_seq: None,
    };
    reader
        .apply_ref(
            &reference,
            RefReadContext {
                ref_seq: 1,
                created_at: now(),
                author: "alice",
                committer: "alice",
            },
        )
        .unwrap();
    let snapshot = reader.snapshot().unwrap();
    assert_eq!(
        snapshot
            .subitems
            .iter()
            .find(|row| row.family == "FILE::VERSION")
            .unwrap()
            .version_label
            .as_deref(),
        Some("1.0")
    );
    assert_eq!(
        snapshot
            .subitems
            .iter()
            .find(|row| row.family == "FILE_VERSION")
            .unwrap()
            .version_label
            .as_deref(),
        Some("2.0")
    );
}
