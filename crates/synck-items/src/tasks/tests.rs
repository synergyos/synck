use super::*;
use serde_json::{Value, json};

#[test]
fn assignment_subjects_preserve_the_legacy_serde_contract() {
    assert_eq!(
        serde_json::from_value::<AssignmentSubject>(serde_json::json!("invalid"))
            .unwrap_err()
            .to_string(),
        "invalid type: string \"invalid\", expected struct TaskFollower"
    );
    let subject = AssignmentSubject {
        kind: AssignmentSubjectKind::User,
        id: "example".into(),
    };
    assert_eq!(
        serde_json::to_value(subject).unwrap(),
        serde_json::json!({"kind": "USER", "id": "example"})
    );
}

#[test]
fn normalizes_defaults_without_dropping_retained_metadata() {
    let input = json!({
        "subject": "  Review the contract  ",
        "description": "  Context  ",
        "column_values": {"legacy_column": {"retained": true}},
        "mailcal_follow_up": {"message_id": "opaque"},
        "created_by": "original-author",
        "created_at": "2026-09-03T10:00:00Z"
    });
    let fields = normalize_fields(&input).unwrap();
    assert_eq!(fields.subject, "Review the contract");
    assert_eq!(fields.priority, "P1");
    assert_eq!(fields.status, "TODO");
    assert_eq!(fields.metadata_json["description"], "Context");
    assert_eq!(fields.metadata_json["index"], 0);
    assert_eq!(fields.metadata_json["assigned_users"], json!([]));
    assert_eq!(fields.metadata_json["assigned_groups"], json!([]));
    assert_eq!(fields.metadata_json["due_at"], Value::Null);
    assert!(fields.metadata_json.get("due").is_none());
    for key in [
        "column_values",
        "mailcal_follow_up",
        "created_by",
        "created_at",
    ] {
        assert_eq!(fields.metadata_json[key], input[key]);
    }
    assert_eq!(input["subject"], "  Review the contract  ");
}

#[test]
fn retained_assignees_keep_first_seen_order_and_legacy_leniency() {
    let fields = normalize_fields(&json!({
        "subject": "Work",
        "assigned_users": [" b ", null, "a", "b", 17, ""],
        "assigned_groups": [" reviewers ", "reviewers", "", false, "legal"]
    }))
    .unwrap();
    assert_eq!(fields.assigned_users, ["b", "a"]);
    assert_eq!(fields.assigned_groups, ["reviewers", "legal"]);
    assert_eq!(fields.metadata_json["assigned_users"], json!(["b", "a"]));
    assert_eq!(
        fields.metadata_json["assigned_groups"],
        json!(["reviewers", "legal"])
    );
}

#[test]
fn date_only_does_not_become_midnight_or_a_timezone() {
    let fields = normalize_fields(&json!({
        "subject": "Deadline",
        "due": {"kind": "DATE", "date": "2026-10-25"},
        "due_at": "2025-01-01T00:00:00Z"
    }))
    .unwrap();
    assert_eq!(fields.due_on.unwrap().to_string(), "2026-10-25");
    assert_eq!(fields.due_at, None);
    assert_eq!(fields.due_timezone, None);
    assert_eq!(fields.metadata_json["due_at"], Value::Null);
}

#[test]
fn timed_due_preserves_the_instant_and_editing_timezone() {
    let fields = normalize_fields(&json!({
        "subject": "Call",
        "due": {
            "kind": "DATETIME", "at": "2026-10-25T01:30:00+02:00",
            "timezone": "Europe/Madrid"
        }
    }))
    .unwrap();
    assert_eq!(
        fields.due_at.unwrap().to_rfc3339(),
        "2026-10-24T23:30:00+00:00"
    );
    assert_eq!(fields.due_on, None);
    assert_eq!(fields.due_timezone.as_deref(), Some("Europe/Madrid"));
    assert_eq!(
        fields.metadata_json["due"]["at"],
        "2026-10-25T01:30:00+02:00"
    );
}

#[test]
fn legacy_timestamp_is_not_rewritten_as_a_typed_due() {
    let fields = normalize_fields(&json!({
        "subject": "Legacy", "due_at": "2026-07-10T00:00:00Z"
    }))
    .unwrap();
    assert!(fields.due_at.is_some());
    assert_eq!(fields.due_on, None);
    assert_eq!(fields.due_timezone, None);
    assert!(fields.metadata_json.get("due").is_none());
}

#[test]
fn explicit_null_due_clears_a_legacy_timestamp() {
    let fields = normalize_fields(&json!({
        "subject": "No deadline", "due": null, "due_at": "invalid old value"
    }))
    .unwrap();
    assert_eq!(fields.due_at, None);
    assert_eq!(fields.metadata_json["due"], Value::Null);
    assert_eq!(fields.metadata_json["due_at"], Value::Null);
}

#[test]
fn every_supported_priority_and_status_remains_valid() {
    for priority in ["P0", "P1", "P2", "P3"] {
        for status in ["TODO", "IN_PROGRESS", "BLOCKED", "DONE", "CANCELED"] {
            let fields = normalize_fields(&json!({
                "subject": "Work", "priority": priority, "status": status
            }))
            .unwrap();
            assert_eq!(fields.priority, priority);
            assert_eq!(fields.status, status);
        }
    }
}

#[test]
fn normalization_is_idempotent_for_retained_task_shapes() {
    for input in [
        json!({"subject": " Work "}),
        json!({"subject": "Work", "assigned_users": [" a ", "a"], "index": 999}),
        json!({"subject": "Work", "due_at": "2026-09-03T12:15:00+02:00"}),
        json!({"subject": "Work", "due": {"kind": "DATE", "date": "2026-09-03"}}),
        json!({"subject": "Work", "column_values": {"unknown": [1, 2]}}),
    ] {
        let first = normalize_fields(&input).unwrap().metadata_json;
        assert_eq!(normalize_fields(&first).unwrap().metadata_json, first);
    }
}

#[test]
fn defaulting_retained_nonstring_status_does_not_change_during_extraction() {
    // Typed command validation is separate from the historical projection parser.
    let fields =
        normalize_fields(&json!({"subject": "Old task", "status": 7, "priority": null})).unwrap();
    assert_eq!(fields.status, "TODO");
    assert_eq!(fields.priority, "P1");
}
