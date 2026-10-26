use super::*;
use serde_json::json;

fn create() -> EventProposal {
    let id = ItemId::new();
    let data = TaskData {
        task_list_id: None,
        parent_task_id: None,
        meta: json!({
            "subject": "Review", "created_by": "alice", "created_at": "2026-09-03T12:00:00Z",
            "assigned_users": ["bob"], "assigned_groups": ["bob"]
        }),
    }
    .normalized()
    .unwrap();
    serde_json::from_value(json!({
        "item_id": id, "event_type": "TASK::CREATE", "set_metadata": data,
        "create_item": {"item_type": "TASK", "name": internal_name(&id)}
    }))
    .unwrap()
}

fn state() -> TaskState {
    reduce(None, &create(), "alice").unwrap()
}

fn event(state: &TaskState, verb: &str) -> EventProposal {
    serde_json::from_value(json!({
        "item_id": state.item_id, "event_type": format!("TASK::{verb}"), "base_seq": 1
    }))
    .unwrap()
}

fn edit(state: &TaskState, patch: Value) -> EventProposal {
    let mut data = state.data.clone();
    data.meta
        .as_object_mut()
        .unwrap()
        .extend(patch.as_object().unwrap().clone());
    let mut event = event(state, "SET_METADATA");
    event.set_metadata = Some(serde_json::to_value(data.normalized().unwrap()).unwrap());
    event
}

#[test]
fn workflow_migration_is_one_way_and_cannot_edit_other_fields() {
    for priority in ["P0", "P1", "P2", "P3"] {
        for status in ["TODO", "IN_PROGRESS", "BLOCKED", "DONE", "CANCELED"] {
            for lifecycle in [Lifecycle::Live, Lifecycle::Trash] {
                let mut before = state();
                before.lifecycle = lifecycle;
                before.data.meta["priority"] = json!(priority);
                before.data.meta["status"] = json!(status);
                for compose in [false, true] {
                    let after = crate::tasks::workflow::convert_legacy(&before.data, compose)
                        .unwrap()
                        .data;
                    let mut migration = event(&before, "MIGRATE_WORKFLOW");
                    migration.set_metadata = Some(json!(after));
                    let actual = reduce(Some(&before), &migration, "maintenance").unwrap();
                    assert_eq!(actual.data, after);
                    assert_eq!(actual.lifecycle, lifecycle);
                    assert!(reduce(Some(&actual), &migration, "maintenance").is_err());
                    let mut tombstone = before.clone();
                    tombstone.lifecycle = Lifecycle::Tombstoned;
                    assert!(reduce(Some(&tombstone), &migration, "maintenance").is_err());
                    for key in [
                        "subject",
                        "description",
                        "created_by",
                        "created_at",
                        "due",
                        "index",
                        "assigned_users",
                        "assigned_groups",
                        "column_values",
                    ] {
                        let mut invalid = migration.clone();
                        invalid.set_metadata.as_mut().unwrap()["meta"][key] = json!("changed");
                        assert!(
                            reduce(Some(&before), &invalid, "maintenance").is_err(),
                            "{key}"
                        );
                    }
                    let mut invalid = migration.clone();
                    invalid.set_metadata.as_mut().unwrap()["task_list_id"] = json!(ItemId::new());
                    assert!(reduce(Some(&before), &invalid, "maintenance").is_err());
                    migration.event_type = "TASK::SET_METADATA".into();
                    assert!(reduce(Some(&before), &migration, "alice").is_err());
                    let mut reverse = event(&actual, "SET_METADATA");
                    reverse.set_metadata = Some(json!(before.data));
                    assert!(reduce(Some(&actual), &reverse, "alice").is_err());
                }
            }
        }
    }
}

#[test]
fn personal_retirement_has_one_narrow_effect_for_live_and_trash() {
    for lifecycle in [Lifecycle::Live, Lifecycle::Trash] {
        for creator in ["alice", "bob"] {
            let mut before = state();
            before.lifecycle = lifecycle;
            before.data.task_list_id = Some(ItemId::new());
            before.data.meta["assigned_users"] = json!([]);
            before.data.meta["assigned_groups"] = json!([]);
            before.data.meta["created_by"] = json!(creator);
            let after = retired_personal_data(&before.data, "alice").unwrap();
            let mut retirement = event(&before, "RETIRE_PERSONAL_LIST");
            retirement.set_metadata = Some(json!(after));
            let actual = reduce(Some(&before), &retirement, "alice").unwrap();
            assert_eq!(actual.lifecycle, lifecycle);
            assert_eq!(actual.data.task_list_id, None);
            assert_eq!(
                actual.data.meta["assigned_users"],
                if creator == "alice" {
                    json!([])
                } else {
                    json!(["alice"])
                }
            );
            for key in [
                "subject",
                "created_by",
                "created_at",
                "index",
                "priority",
                "status",
                "due",
            ] {
                let mut invalid = retirement.clone();
                invalid.set_metadata.as_mut().unwrap()["meta"][key] = json!("changed");
                assert!(reduce(Some(&before), &invalid, "alice").is_err(), "{key}");
            }
            let mut tombstone = before.clone();
            tombstone.lifecycle = Lifecycle::Tombstoned;
            assert!(reduce(Some(&tombstone), &retirement, "alice").is_err());
            assert!(reduce(Some(&actual), &retirement, "alice").is_err());
        }
    }
    let mut before = state();
    before.data.task_list_id = Some(ItemId::new());
    let after = retired_personal_data(&before.data, "alice").unwrap();
    assert_eq!(
        after.meta, before.data.meta,
        "existing assignees are immutable"
    );
}

#[test]
fn task_identity_is_uuidv7_and_does_not_depend_on_subject_or_list() {
    let first = state();
    let second = state();
    assert_eq!(
        uuid::Uuid::parse_str(first.item_id.as_str())
            .unwrap()
            .get_version_num(),
        7
    );
    assert_ne!(first.item_id, second.item_id);
    assert_eq!(first.data.meta["subject"], second.data.meta["subject"]);
    assert!(first.data.task_list_id.is_none());
    let invalid_id = uuid::Uuid::new_v4().to_string();
    let mut wire = serde_json::to_value(create()).unwrap();
    wire["item_id"] = json!(invalid_id);
    assert!(serde_json::from_value::<EventProposal>(wire).is_err());
}

#[test]
fn creation_rejects_tree_placement_and_wrong_internal_identity() {
    let mut event = create();
    event.create_item.as_mut().unwrap().parent_folder_id = Some(ItemId::new());
    assert!(reduce(None, &event, "alice").is_err());
    let mut event = create();
    event.create_item.as_mut().unwrap().name = "Review".into();
    assert!(reduce(None, &event, "alice").is_err());
    let mut event = create();
    event.item_id = None;
    assert!(reduce(None, &event, "alice").is_err());
    let event = create();
    let state = reduce(None, &event, "alice").unwrap();
    assert!(reduce(Some(&state), &event, "alice").is_err());
    assert!(reduce(None, &event, "mallory").is_err());
}

#[test]
fn field_edits_preserve_identity_context_and_creator() {
    let before = state();
    let edit = edit(&before, json!({"subject": "Updated", "status": "DONE"}));
    let after = reduce(Some(&before), &edit, "bob").unwrap();
    assert_eq!(after.item_id, before.item_id);
    assert_eq!(after.data.task_list_id, before.data.task_list_id);
    assert_eq!(after.lifecycle, Lifecycle::Live);
    assert_eq!(after.data.meta["created_by"], "alice");
    assert_eq!(after.data.meta["status"], "DONE");
}

#[test]
fn field_events_cannot_change_creator_or_context() {
    let before = state();
    for patch in [
        json!({"created_by": "bob"}),
        json!({"created_at": "2027-01-01T00:00:00Z"}),
    ] {
        assert!(reduce(Some(&before), &edit(&before, patch), "bob").is_err());
    }
    let mut change = edit(&before, json!({}));
    change.set_metadata.as_mut().unwrap()["task_list_id"] = json!(ItemId::new());
    assert!(reduce(Some(&before), &change, "alice").is_err());
    change.set_metadata.as_mut().unwrap()["task_list_id"] = Value::Null;
    change.set_metadata.as_mut().unwrap()["parent_task_id"] = json!(ItemId::new());
    assert!(reduce(Some(&before), &change, "alice").is_err());
}

#[test]
fn context_event_preserves_fields_and_cannot_self_parent() {
    let before = state();
    let mut data = before.data.clone();
    data.task_list_id = Some(ItemId::new());
    data.parent_task_id = Some(ItemId::new());
    let mut change = event(&before, "SET_CONTEXT");
    change.set_metadata = Some(serde_json::to_value(&data).unwrap());
    let after = reduce(Some(&before), &change, "alice").unwrap();
    assert_eq!(after.data.meta, before.data.meta);
    assert_eq!(after.data.task_list_id, data.task_list_id);
    data.meta["status"] = json!("DONE");
    change.set_metadata = Some(serde_json::to_value(data).unwrap());
    assert!(reduce(Some(&before), &change, "alice").is_err());
    let mut data = before.data.clone();
    data.parent_task_id = Some(before.item_id.clone());
    change.set_metadata = Some(serde_json::to_value(data).unwrap());
    assert!(reduce(Some(&before), &change, "alice").is_err());
}

#[test]
fn lifecycle_is_independent_of_status_and_tombstone_is_terminal() {
    let before = state();
    let completed = reduce(
        Some(&before),
        &edit(&before, json!({"status": "DONE"})),
        "bob",
    )
    .unwrap();
    let mut trash = event(&completed, "TRASH");
    trash.set_in_trash = Some(true);
    let trashed = reduce(Some(&completed), &trash, "bob").unwrap();
    assert_eq!(trashed.lifecycle, Lifecycle::Trash);
    assert_eq!(trashed.data.meta["status"], "DONE");
    assert!(
        reduce(
            Some(&trashed),
            &edit(&trashed, json!({"status": "TODO"})),
            "bob"
        )
        .is_err()
    );
    let mut restore = event(&trashed, "RESTORE");
    restore.set_in_trash = Some(false);
    let restored = reduce(Some(&trashed), &restore, "bob").unwrap();
    assert_eq!(restored, completed);
    assert!(reduce(Some(&restored), &restore, "bob").is_err());
    let mut tombstone = event(&restored, "TOMBSTONE");
    tombstone.set_tombstoned = Some(true);
    let deleted = reduce(Some(&restored), &tombstone, "bob").unwrap();
    assert_eq!(deleted.lifecycle, Lifecycle::Tombstoned);
    assert!(reduce(Some(&deleted), &restore, "bob").is_err());
    assert!(reduce(Some(&deleted), &tombstone, "bob").is_err());
}

#[test]
fn comment_does_not_change_fields_or_provenance_and_uses_byte_limits() {
    let before = state();
    let mut comment = event(&before, "COMMENT::CREATE");
    comment.set_metadata = Some(json!({"comment_id": uuid::Uuid::new_v4()}));
    comment.set_content = Some(EventContent::Json {
        json: json!({"body": "é".repeat(MAX_COMMENT_BYTES / 2)}),
    });
    assert_eq!(reduce(Some(&before), &comment, "bob").unwrap(), before);
    for body in ["   ".to_owned(), "é".repeat(MAX_COMMENT_BYTES / 2 + 1)] {
        comment.set_content = Some(EventContent::Json {
            json: json!({"body": body}),
        });
        assert!(reduce(Some(&before), &comment, "bob").is_err());
    }
}

#[test]
fn referenced_comment_envelope_is_valid_but_reduction_requires_its_body() {
    let before = state();
    let mut comment = event(&before, "COMMENT::CREATE");
    comment.set_metadata = Some(json!({"comment_id": uuid::Uuid::new_v4()}));
    comment.set_content = Some(EventContent::Content {
        content_id: synck_vcs::types::ContentId::parse(format!("sha256:{}", "a".repeat(64)))
            .unwrap(),
    });
    validate_envelope(&comment).unwrap();
    assert!(reduce(Some(&before), &comment, "bob").is_err());
}

#[test]
fn rejects_noncanonical_fields_unknown_envelopes_and_cross_task_replay() {
    let before = state();
    let mut change = edit(&before, json!({}));
    change.set_metadata.as_mut().unwrap()["meta"]["assigned_users"] = json!(["bob", "bob"]);
    assert!(reduce(Some(&before), &change, "alice").is_err());
    let mut change = edit(&before, json!({}));
    change.set_metadata.as_mut().unwrap()["unknown"] = json!(1);
    assert!(reduce(Some(&before), &change, "alice").is_err());
    let mut change = edit(&before, json!({}));
    change.item_id = Some(ItemId::new());
    assert!(reduce(Some(&before), &change, "alice").is_err());
    for key in [
        "set_parent",
        "set_name",
        "set_in_trash",
        "set_tombstoned",
        "set_is_federated",
        "set_federation_stream_id",
    ] {
        let mut wire = serde_json::to_value(edit(&before, json!({}))).unwrap();
        wire[key] = match key {
            "set_parent" => Value::Null,
            "set_name" | "set_federation_stream_id" => json!("invalid"),
            _ => json!(true),
        };
        let malformed: EventProposal = serde_json::from_value(wire).unwrap();
        assert!(reduce(Some(&before), &malformed, "alice").is_err(), "{key}");
    }
}
