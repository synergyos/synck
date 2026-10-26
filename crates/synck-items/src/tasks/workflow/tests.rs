use super::*;
use serde_json::{Value, json};
use synck_vcs::types::ItemId;

fn task(priority: &str, status: &str) -> TaskData {
    TaskData {
        task_list_id: Some(ItemId::new()),
        parent_task_id: Some(ItemId::new()),
        meta: json!({
            "subject": "Email follow-up: this name is not provenance",
            "description": "Keep the original description",
            "priority": priority, "status": status,
            "assigned_users": ["alice"], "assigned_groups": ["team"],
            "created_by": "bob", "created_at": "2026-09-01T10:30:00Z",
            "index": 17, "column_values": {"unknown": {"retained": true}},
            "due": {"kind": "DATETIME", "at": "2026-10-25T02:30:00+02:00", "timezone": "Europe/Madrid"},
            "email_source": {"retained": "opaque source metadata"},
            "legacy_task": {"retained": "opaque import metadata"}
        }),
    }
}

#[test]
fn current_normalization_has_explicit_defaults_and_never_accepts_legacy_codes() {
    let meta = json!({"subject":"Review"});
    let current = crate::tasks::normalize_current_fields(&meta).unwrap();
    assert_eq!(current.priority, "NORMAL");
    assert_eq!(current.status, "TODO");
    assert_eq!(
        crate::tasks::normalize_history_fields(&meta)
            .unwrap()
            .priority,
        "P1"
    );
    for priority in ["URGENT", "HIGH", "NORMAL", "LOW"] {
        for status in [
            "TODO",
            "IN_PROGRESS",
            "BLOCKED",
            "FOLLOW_UP",
            "DONE",
            "CANCELED",
        ] {
            let input = json!({"subject":"Review", "priority":priority, "status":status});
            let current = crate::tasks::normalize_current_fields(&input).unwrap();
            let historical = crate::tasks::normalize_history_fields(&input).unwrap();
            assert_eq!(current.metadata_json, historical.metadata_json);
            assert_eq!(current.priority, priority);
            assert_eq!(current.status, status);
        }
    }
    for value in [
        json!("P0"),
        json!("P1"),
        json!("P2"),
        json!("P3"),
        json!(" NORMAL"),
        Value::Null,
        json!(4),
    ] {
        assert!(
            crate::tasks::normalize_current_fields(&json!({"subject":"Review","priority":value}))
                .is_err()
        );
    }
    for value in [json!("OPEN"), json!(" TODO"), Value::Null, json!(4)] {
        assert!(
            crate::tasks::normalize_current_fields(&json!({"subject":"Review","status":value}))
                .is_err()
        );
    }
    assert!(
        crate::tasks::normalize_fields(
            &json!({"subject":"Review","priority":"P1","status":"FOLLOW_UP"})
        )
        .is_err()
    );
}

#[test]
fn legacy_matrix_preserves_explicit_status_and_all_unrelated_data() {
    for (old_priority, expected_priority) in [
        ("P0", "URGENT"),
        ("P1", "NORMAL"),
        ("P2", "NORMAL"),
        ("P3", "LOW"),
    ] {
        for old_status in ["TODO", "IN_PROGRESS", "BLOCKED", "DONE", "CANCELED"] {
            for compose in [false, true] {
                let before = task(old_priority, old_status);
                let original = before.clone();
                let conversion = convert_legacy(&before, compose).unwrap();
                let mut expected = original.clone();
                expected.meta["priority"] = json!(expected_priority);
                expected.meta["status"] = json!(if old_status == "TODO"
                    && (old_priority == "P2" || compose)
                {
                    "FOLLOW_UP"
                } else {
                    old_status
                });
                assert_eq!(
                    conversion.data, expected,
                    "{old_priority}/{old_status}/{compose}"
                );
                assert_eq!(before, original, "conversion must not mutate its source");
                assert_eq!(
                    conversion.ambiguous_follow_up,
                    old_priority == "P2" && matches!(old_status, "IN_PROGRESS" | "BLOCKED")
                );
            }
        }
    }
}

#[test]
fn source_or_subject_alone_never_establishes_compose_intent() {
    let before = task("P1", "TODO");
    assert_eq!(
        convert_legacy(&before, false).unwrap().data.meta["status"],
        "TODO"
    );
    assert_eq!(
        convert_legacy(&before, true).unwrap().data.meta["status"],
        "FOLLOW_UP"
    );
}

#[test]
fn conversion_keeps_null_date_only_and_exact_timed_due_values() {
    for due in [
        Value::Null,
        json!({"kind": "DATE", "date": "2026-10-25"}),
        json!({"kind": "DATETIME", "at": "2026-10-25T02:30:00+02:00", "timezone": "Europe/Madrid"}),
        json!({"kind": "DATETIME", "at": "2026-10-25T02:30:00+01:00", "timezone": "Europe/Madrid"}),
    ] {
        let mut before = task("P2", "TODO");
        before.meta["due"] = due.clone();
        before.meta["due_at"] = json!("2026-10-25T00:30:00.000Z");
        let after = convert_legacy(&before, false).unwrap().data;
        assert_eq!(after.meta["due"], due);
        assert_eq!(after.meta["due_at"], before.meta["due_at"]);
    }
}

#[test]
fn malformed_or_already_converted_heads_fail_without_guessing() {
    for (field, values) in [
        (
            "priority",
            vec![
                Value::Null,
                json!(3),
                json!(""),
                json!(" P2"),
                json!("P4"),
                json!("URGENT"),
                json!("HIGH"),
                json!("NORMAL"),
                json!("LOW"),
            ],
        ),
        (
            "status",
            vec![
                Value::Null,
                json!(3),
                json!(""),
                json!(" TODO"),
                json!("OPEN"),
                json!("FOLLOW_UP"),
            ],
        ),
    ] {
        for value in values {
            let mut before = task("P1", "TODO");
            before.meta[field] = value;
            assert!(
                convert_legacy(&before, false).is_err(),
                "{field}: {}",
                before.meta[field]
            );
        }
        let mut before = task("P1", "TODO");
        before.meta.as_object_mut().unwrap().remove(field);
        assert!(convert_legacy(&before, false).is_err());
    }
    for meta in [Value::Null, json!([]), json!("invalid")] {
        let mut before = task("P1", "TODO");
        before.meta = meta;
        assert!(convert_legacy(&before, false).is_err());
    }
    let after = convert_legacy(&task("P2", "TODO"), false).unwrap().data;
    assert!(
        convert_legacy(&after, false).is_err(),
        "resume must recover a receipt, not map twice"
    );
}

#[test]
fn historical_normalization_is_not_the_migration_or_current_default() {
    let fields = crate::tasks::normalize_fields(&json!({"subject": "Legacy default"})).unwrap();
    assert_eq!(fields.priority, "P1");
    assert_eq!(fields.status, "TODO");
    for priority in ["P0", "P1", "P2", "P3"] {
        let fields = crate::tasks::normalize_fields(&json!({
            "subject": "Legacy read", "priority": priority, "status": "TODO"
        }))
        .unwrap();
        assert_eq!(fields.priority, priority);
        assert_eq!(fields.status, "TODO");
    }
}

#[test]
fn current_vocabulary_round_trips_and_has_explicit_order_and_closed_states() {
    for (wire, status, open) in [
        ("TODO", Status::Todo, true),
        ("IN_PROGRESS", Status::InProgress, true),
        ("BLOCKED", Status::Blocked, true),
        ("FOLLOW_UP", Status::FollowUp, true),
        ("DONE", Status::Done, false),
        ("CANCELED", Status::Canceled, false),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), json!(wire));
        assert_eq!(
            serde_json::from_value::<Status>(json!(wire)).unwrap(),
            status
        );
        assert_eq!(status.is_open(), open);
    }
    for (wire, priority, rank) in [
        ("URGENT", Priority::Urgent, 0),
        ("HIGH", Priority::High, 1),
        ("NORMAL", Priority::Normal, 2),
        ("LOW", Priority::Low, 3),
    ] {
        assert_eq!(serde_json::to_value(priority).unwrap(), json!(wire));
        assert_eq!(
            serde_json::from_value::<Priority>(json!(wire)).unwrap(),
            priority
        );
        assert_eq!(priority.rank(), rank);
    }
    for legacy in ["P0", "P1", "P2", "P3", "normal", " NORMAL"] {
        assert!(serde_json::from_value::<Priority>(json!(legacy)).is_err());
    }
    for invalid in ["OPEN", "CLOSED", "todo", "WAITING", " TODO"] {
        assert!(serde_json::from_value::<Status>(json!(invalid)).is_err());
    }
}
