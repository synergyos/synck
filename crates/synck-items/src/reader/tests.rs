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
fn email_readers_expose_attachments_and_metadata_replacement() {
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
fn hosted_only_native_domains_are_explicitly_unsupported() {
    for kind in [
        "TASK",
        "TASK_LIST",
        "POST",
        "TABLE",
        "DATABASE",
        "CALENDAR",
        "CALENDAR_EVENT",
        "TIME_SETTINGS",
        "TIME_BOOK",
        "TIME_ENTRY",
        "CALL",
        "CHAT_ORGANIZATION",
        "FEDERATED_MOUNT",
    ] {
        let id = ItemId::new();
        let mut reader = ItemReader::new(id.clone());
        let mut first = event(&id, &format!("{kind}::CREATE"), json!({}), None);
        first.create_item = Some(CreateItem {
            item_type: kind.into(),
            name: "unsupported".into(),
            parent_folder_id: None,
        });
        let result = reader.apply(
            &first,
            ReadContext {
                item_seq: 1,
                created_at: now(),
                occurred_at: None,
                author: "author",
                committer: "author",
            },
        );
        assert!(
            matches!(result, Err(ReadError::Unsupported { .. })),
            "{kind}: {result:?}"
        );
    }
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
fn owner_refs_keep_uuid_identity_and_independent_positions() {
    let (mut reader, owner) = create("FILE");
    let version_id = Uuid::now_v7();
    let reference = synck_file_folder::file_version_set::automatic_initial_ref_event(
        &synck_vcs::types::RepoId::new(), &owner, &version_id.to_string(),
        0, 2, 1, A.into(), Some(7),
    ).unwrap();
    reader.apply_ref(&reference, RefReadContext {
        ref_seq: 1, created_at: now(), author: "author", committer: "committer",
    }).unwrap();
    let update: synck_vcs::types::RefEventProposal = serde_json::from_value(json!({
        "owner_item_id":owner,"event_type":"FILE_VERSION::SET_DESCRIPTION","base_ref_seq":1,
        "set_metadata":{"file_item_id":owner,"version_id":version_id,"description":"Reviewed"}
    })).unwrap();
    reader.apply_ref(&update, RefReadContext {
        ref_seq: 2, created_at: now(), author: "editor", committer: "committer",
    }).unwrap();
    let snapshot = reader.snapshot().unwrap();
    assert_eq!(snapshot.projection.latest_item_seq, 1);
    assert_eq!(snapshot.latest_ref_seq, 2);
    assert_eq!(snapshot.subitems.len(), 1);
    let version = &snapshot.subitems[0];
    assert_eq!(version.identity, SubitemIdentity::Uuid(version_id));
    assert_eq!(version.created_position.seq, 1);
    assert_eq!(version.latest_position.seq, 2);
    assert_eq!(version.body.as_deref(), Some("Reviewed"));
}
