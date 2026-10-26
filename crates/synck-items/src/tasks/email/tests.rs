use super::*;
use crate::tasks::history::{self, TaskState};
use serde_json::json;
use synck_vcs::types::{EventContent, EventProposal};

fn blob(letter: char, bytes: u64) -> Blob {
    Blob {
        blob_id: BlobId::parse(format!("sha256:{}", letter.to_string().repeat(64))).unwrap(),
        size_bytes: bytes,
    }
}

fn snapshot() -> Snapshot {
    Snapshot {
        captured_at: Utc::now(),
        raw_rfc822: blob('a', 256),
        subject: Some("Selected email".into()),
        internet_message_id: None,
        from: None,
        sender: None,
        to: vec![],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        sent_at: None,
        received_at: None,
        body_markdown: blob('b', 12),
        body_html: Some(blob('c', 18)),
        attachments: vec![Attachment {
            attachment_id: "1".into(),
            filename: "diagram.png".into(),
            mime_type: "image/png".into(),
            content: blob('d', 100),
            is_inline: true,
            content_id: Some("diagram".into()),
        }],
    }
}

fn source() -> Source {
    Source::ProviderOnly {
        generation: 1,
        identity: Identity::Personal {
            owner_id: "alice".into(),
            account_id: Uuid::new_v4(),
            message_id: "provider-message".into(),
        },
    }
}

fn create(source: &Source) -> EventProposal {
    let id = ItemId::new();
    let data = history::TaskData {
        task_list_id: None,
        parent_task_id: None,
        meta: json!({"subject":"Task", "priority":"NORMAL", "created_by":"alice", "created_at":Utc::now().to_rfc3339()}),
    }.normalized().unwrap();
    let mut event: EventProposal = serde_json::from_value(json!({
        "item_id":id, "event_type":"TASK::CREATE",
        "create_item":{"item_type":"TASK", "name":history::internal_name(&id)},
        "set_metadata":data,
    }))
    .unwrap();
    event.set_metadata.as_mut().unwrap()["meta"]["email_source"] = json!(source);
    if let Some(snapshot) = source.snapshot() {
        event.set_content = Some(EventContent::Blob {
            blob_id: snapshot.raw_rfc822.blob_id.to_string(),
        });
    }
    event
}

fn capture(state: &TaskState, source: &Source) -> EventProposal {
    let mut event = create(source);
    event.item_id = Some(state.item_id.clone());
    event.create_item = None;
    event.event_type = "TASK::CAPTURE_EMAIL".into();
    event.base_seq = Some(1);
    let mut data = state.data.clone();
    data.meta["email_source"] = json!(source);
    event.set_metadata = Some(json!(data));
    event
}

#[test]
fn event_blob_closure_is_complete_deduplicated_and_family_scoped() {
    let mut snapshot = snapshot();
    snapshot.attachments.push(Attachment {
        attachment_id: "2".into(),
        filename: "copy.png".into(),
        ..snapshot.attachments[0].clone()
    });
    let saved = Source::Saved {
        generation: 1,
        identity: source().identity().clone(),
        snapshot,
    };
    let expected: Vec<_> = ['a', 'b', 'c', 'd']
        .map(|letter| blob(letter, 0).blob_id.to_string())
        .into();
    for family in ["TASK::CREATE", "TASK::IMPORT", "TASK::CAPTURE_EMAIL"] {
        let mut event = create(&saved);
        event.event_type = family.into();
        if family == "TASK::CAPTURE_EMAIL" {
            event.create_item = None;
            event.base_seq = Some(1);
        }
        assert_eq!(event_blob_references(&event).unwrap(), expected);
        let mut malformed = event.clone();
        malformed.set_metadata = None;
        assert!(event_blob_references(&malformed).is_err());
        malformed = event.clone();
        malformed.set_content = Some(EventContent::Blob {
            blob_id: blob('f', 0).blob_id.to_string(),
        });
        assert!(event_blob_references(&malformed).is_err());
        malformed = event;
        malformed.set_metadata.as_mut().unwrap()["meta"]["email_source"]["snapshot"]["attachments"]
            [0]["content"]["blob_id"] = json!("malformed");
        assert!(event_blob_references(&malformed).is_err());
    }
    assert!(
        event_blob_references(&create(&source()))
            .unwrap()
            .is_empty()
    );
    // A similarly shaped value in an unrelated event is not a retained blob
    // reference or permission to transfer an arbitrary object.
    let mut unrelated = create(&saved);
    unrelated.event_type = "TASK::SET_METADATA".into();
    assert!(event_blob_references(&unrelated).unwrap().is_empty());
}

#[test]
fn capture_is_a_single_immutable_transition_with_an_exact_blob_closure() {
    let original = source();
    let before = history::reduce(None, &create(&original), "alice").unwrap();
    let saved = Source::Saved {
        generation: 1,
        identity: original.identity().clone(),
        snapshot: snapshot(),
    };
    let event = capture(&before, &saved);
    let after = history::reduce(Some(&before), &event, "alice").unwrap();
    assert_eq!(
        from_metadata(&after.data.meta).unwrap(),
        Some(saved.clone())
    );
    assert_eq!(saved.snapshot().unwrap().blobs().count(), 4);
    assert!(
        history::reduce(Some(&after), &event, "alice").is_err(),
        "cannot capture twice"
    );
    let mut downgrade = event.clone();
    downgrade.event_type = "TASK::SET_METADATA".into();
    downgrade.set_content = None;
    downgrade.set_metadata.as_mut().unwrap()["meta"]["email_source"] = json!(original);
    assert!(history::reduce(Some(&after), &downgrade, "alice").is_err());
    let mut comment = event;
    comment.event_type = "TASK::COMMENT::CREATE".into();
    comment.set_metadata = Some(json!({"comment_id":Uuid::new_v4()}));
    comment.set_content = Some(EventContent::Json {
        json: json!({"body":"Comment after capture"}),
    });
    assert_eq!(
        history::reduce(Some(&after), &comment, "bob").unwrap().data,
        after.data
    );
}

#[test]
fn placement_capture_preserves_fields_lifecycle_and_source_identity() {
    let original = source();
    let mut before = history::reduce(None, &create(&original), "alice").unwrap();
    let saved = Source::Saved {
        generation: 1,
        identity: original.identity().clone(),
        snapshot: snapshot(),
    };
    let mut event = capture(&before, &saved);
    event.set_metadata.as_mut().unwrap()["task_list_id"] = json!(ItemId::new());
    let after = history::reduce(Some(&before), &event, "alice").unwrap();
    assert_eq!(from_metadata(&after.data.meta).unwrap(), Some(saved));
    before.lifecycle = history::Lifecycle::Trash;
    assert_eq!(
        history::reduce(Some(&before), &event, "alice")
            .unwrap()
            .lifecycle,
        history::Lifecycle::Trash
    );
    for key in ["created_by", "subject", "assigned_users", "index"] {
        let mut invalid = event.clone();
        invalid.set_metadata.as_mut().unwrap()["meta"][key] = if key == "index" {
            json!(10)
        } else {
            json!("changed")
        };
        assert!(
            history::reduce(Some(&before), &invalid, "alice").is_err(),
            "{key}"
        );
    }
    let mut reparented = event.clone();
    reparented.set_metadata.as_mut().unwrap()["parent_task_id"] = json!(ItemId::new());
    assert!(history::reduce(Some(&before), &reparented, "alice").is_err());
    before.lifecycle = history::Lifecycle::Tombstoned;
    assert!(history::reduce(Some(&before), &event, "alice").is_err());
}

#[test]
fn source_identity_content_and_part_descriptors_are_validated() {
    let original = source();
    let before = history::reduce(None, &create(&original), "alice").unwrap();
    let saved = Source::Saved {
        generation: 1,
        identity: original.identity().clone(),
        snapshot: snapshot(),
    };
    let mut event = capture(&before, &saved);
    event.set_content = Some(EventContent::Blob {
        blob_id: blob('e', 256).blob_id.to_string(),
    });
    assert!(history::reduce(Some(&before), &event, "alice").is_err());
    event.set_content = None;
    assert!(history::validate_envelope(&event).is_err());
    let wrong = Source::Saved {
        generation: 1,
        identity: source().identity().clone(),
        snapshot: snapshot(),
    };
    assert!(history::reduce(Some(&before), &capture(&before, &wrong), "alice").is_err());
    let mut snapshot = snapshot();
    snapshot.attachments.push(snapshot.attachments[0].clone());
    assert!(
        Source::Saved {
            generation: 1,
            identity: original.identity().clone(),
            snapshot
        }
        .validate()
        .is_err()
    );
    let mut value = json!(saved);
    value["snapshot"]["attachments"][0]["content"]["blob_id"] = json!("not-a-hash");
    assert!(from_metadata(&json!({"email_source":value})).is_err());
    assert!(from_metadata(&json!({"email_source":null})).is_err());
    assert_eq!(from_metadata(&json!({})).unwrap(), None);
    let mut value = json!(original);
    for generation in [0, -1] {
        value["generation"] = json!(generation);
        assert!(from_metadata(&json!({"email_source":value})).is_err());
    }
    let mut replacement = json!(saved);
    replacement["generation"] = json!(2);
    let replacement: Source = serde_json::from_value(replacement).unwrap();
    assert!(history::reduce(Some(&before), &capture(&before, &replacement), "alice").is_err());
}

#[test]
fn new_saved_tasks_require_no_hidden_email_item_or_provider_only_intermediate() {
    let saved = Source::Saved {
        generation: 1,
        identity: source().identity().clone(),
        snapshot: snapshot(),
    };
    let event = create(&saved);
    assert!(
        event
            .create_item
            .as_ref()
            .unwrap()
            .parent_folder_id
            .is_none()
    );
    let task = history::reduce(None, &event, "alice").unwrap();
    assert_eq!(from_metadata(&task.data.meta).unwrap(), Some(saved));
}
