//! Immutable provenance for a maintenance-imported task. Local activity/read and
//! follower state are remapped separately; they never enter replicated metadata.
use super::history::{Lifecycle, TaskData};
use super::{AssignmentSubject, AssignmentSubjectKind, TaskError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use synck_vcs::types::{ItemId, Sha256Digest};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacySource {
    pub task_list_id: ItemId,
    pub task_item_seq: i64,
    pub create_event_digest: Sha256Digest,
    pub through_item_seq: i64,
    pub through_event_digest: Sha256Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub subject: AssignmentSubject,
    pub assigned_by: String,
    pub assigned_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub source: LegacySource,
    pub lifecycle: Lifecycle,
    pub assignments: Vec<Assignment>,
    pub updated_by: String,
    pub updated_at: DateTime<Utc>,
}

fn invalid(message: &str) -> TaskError {
    TaskError::Invalid(message.into())
}

pub fn from_metadata(meta: &serde_json::Value) -> Result<Option<Baseline>, TaskError> {
    meta.get("legacy_task")
        .map(|value| {
            serde_json::from_value(value.clone())
                .map_err(|_| invalid("invalid imported task provenance"))
        })
        .transpose()
}

impl Baseline {
    /// Core creates live items. Maintenance completes a non-live baseline with
    /// the ordinary lifecycle event at sequence two, before serving can resume.
    pub fn completes_lifecycle(&self, item_seq: i64, event_type: &str) -> bool {
        item_seq == 2
            && matches!(
                (self.lifecycle, event_type),
                (Lifecycle::Trash, "TASK::TRASH") | (Lifecycle::Tombstoned, "TASK::TOMBSTONE")
            )
    }

    pub fn validate(&self, data: &TaskData) -> Result<(), TaskError> {
        if self.source.task_item_seq <= 0
            || self.source.through_item_seq < self.source.task_item_seq
        {
            return Err(invalid("invalid legacy task history boundary"));
        }
        if data.task_list_id.as_ref() != Some(&self.source.task_list_id) {
            return Err(invalid("task import must retain its source list"));
        }
        let actor_valid =
            |actor: &str| !actor.is_empty() && actor.trim() == actor && !actor.contains('\0');
        if !actor_valid(&self.updated_by)
            || self
                .assignments
                .iter()
                .any(|a| !actor_valid(&a.assigned_by))
        {
            return Err(invalid("imported provenance requires valid actors"));
        }
        let subjects: Vec<_> = self.assignments.iter().map(|a| a.subject.clone()).collect();
        super::validate_assignment_subjects(&subjects)?;
        // A baseline must account for every retained assignment exactly once.
        // It must not rewrite historical eligibility using today's directory.
        let fields = super::normalize_fields(&data.meta)?;
        if subjects.len() != fields.assigned_users.len() + fields.assigned_groups.len()
            || subjects.iter().any(|subject| match subject.kind {
                AssignmentSubjectKind::User => !fields.assigned_users.contains(&subject.id),
                AssignmentSubjectKind::Group => !fields.assigned_groups.contains(&subject.id),
            })
        {
            return Err(invalid(
                "imported assignment provenance does not match task assignments",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::history::{self, Lifecycle};
    use serde_json::json;
    use synck_vcs::types::EventProposal;

    fn imported() -> EventProposal {
        let id = ItemId::new();
        let list = ItemId::new();
        let digest = format!("sha256:{}", "a".repeat(64));
        let data = TaskData {
            task_list_id: Some(list.clone()), parent_task_id: None,
            meta: json!({"subject":"Retained task","created_by":"original-creator","created_at":"2020-01-02T03:04:05Z","assigned_users":["bob"],
                "legacy_task":{"source":{"task_list_id":list,"task_item_seq":3,"create_event_digest":digest,"through_item_seq":20,"through_event_digest":digest},"lifecycle":"LIVE",
                "assignments":[{"subject":{"kind":"USER","id":"bob"},"assigned_by":"original-assigner","assigned_at":"2020-02-03T04:05:06Z"}],
                "updated_by":"original-editor","updated_at":"2020-03-04T05:06:07Z"}}),
        }.normalized().unwrap();
        serde_json::from_value(json!({"item_id":id,"event_type":"TASK::IMPORT","create_item":{"item_type":"TASK","name":history::internal_name(&id)},"set_metadata":data})).unwrap()
    }

    #[test]
    fn import_preserves_creator_and_assignments_before_lifecycle_completion() {
        for (trash, tombstone, expected) in [
            (None, None, Lifecycle::Live),
            (Some(true), None, Lifecycle::Trash),
            (None, Some(true), Lifecycle::Tombstoned),
        ] {
            let mut event = imported();
            event.set_metadata.as_mut().unwrap()["meta"]["legacy_task"]["lifecycle"] =
                serde_json::to_value(expected).unwrap();
            let state = history::reduce(None, &event, "migration-author").unwrap();
            assert_eq!(state.lifecycle, Lifecycle::Live);
            assert_eq!(state.data.meta["created_by"], "original-creator");
            assert_eq!(state.data.meta["created_at"], "2020-01-02T03:04:05Z");
            assert_eq!(
                from_metadata(&state.data.meta)
                    .unwrap()
                    .unwrap()
                    .assignments[0]
                    .assigned_by,
                "original-assigner"
            );
            if expected != Lifecycle::Live {
                event.event_type = if trash.is_some() {
                    "TASK::TRASH"
                } else {
                    "TASK::TOMBSTONE"
                }
                .into();
                event.create_item = None;
                event.set_metadata = None;
                event.base_seq = Some(1);
                event.set_in_trash = trash;
                event.set_tombstoned = tombstone;
                let baseline = from_metadata(&state.data.meta).unwrap().unwrap();
                assert!(baseline.completes_lifecycle(2, &event.event_type));
                assert!(!baseline.completes_lifecycle(3, &event.event_type));
                assert_eq!(
                    history::reduce(Some(&state), &event, "migration-author")
                        .unwrap()
                        .lifecycle,
                    expected
                );
            }
        }
    }

    #[test]
    fn import_requires_a_complete_and_unambiguous_legacy_binding() {
        for change in 0..8 {
            let mut event = imported();
            let data = event.set_metadata.as_mut().unwrap();
            match change {
                0 => {
                    data["meta"].as_object_mut().unwrap().remove("legacy_task");
                }
                1 => data["meta"]["legacy_task"]["source"]["task_item_seq"] = json!(0),
                2 => data["meta"]["legacy_task"]["source"]["through_item_seq"] = json!(2),
                3 => data["meta"]["legacy_task"]["source"]["create_event_digest"] = json!("bad"),
                4 => data["task_list_id"] = json!(ItemId::new()),
                5 => data["meta"]["legacy_task"]["assignments"] = json!([]),
                6 => {
                    let first = data["meta"]["legacy_task"]["assignments"][0].clone();
                    data["meta"]["legacy_task"]["assignments"]
                        .as_array_mut()
                        .unwrap()
                        .push(first);
                }
                7 => data["meta"]["followers"] = json!([]),
                _ => unreachable!(),
            }
            assert!(
                history::reduce(None, &event, "migration-author").is_err(),
                "case {change}"
            );
        }
        for (trash, tombstone) in [
            (Some(true), None),
            (None, Some(true)),
            (Some(true), Some(true)),
        ] {
            let mut event = imported();
            event.set_in_trash = trash;
            event.set_tombstoned = tombstone;
            assert!(history::reduce(None, &event, "migration-author").is_err());
        }
    }

    #[test]
    fn ordinary_commands_cannot_introduce_replace_or_remove_import_provenance() {
        let mut event = imported();
        event.event_type = "TASK::CREATE".into();
        assert!(history::reduce(None, &event, "original-creator").is_err());
        event.event_type = "TASK::IMPORT".into();
        let state = history::reduce(None, &event, "migration-author").unwrap();
        assert!(history::reduce(Some(&state), &event, "migration-author").is_err());
        let mut changed = event.clone();
        changed.event_type = "TASK::SET_METADATA".into();
        changed.create_item = None;
        changed.base_seq = Some(1);
        changed.set_metadata.as_mut().unwrap()["meta"]["assigned_users"] = json!([]);
        // Current assignment changes do not rewrite the historical baseline.
        let next = history::reduce(Some(&state), &changed, "editor").unwrap();
        assert_eq!(
            next.data.meta["legacy_task"],
            state.data.meta["legacy_task"]
        );
        changed.set_metadata.as_mut().unwrap()["meta"]
            .as_object_mut()
            .unwrap()
            .remove("legacy_task");
        assert!(history::reduce(Some(&state), &changed, "editor").is_err());
    }
}
