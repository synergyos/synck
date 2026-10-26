use super::*;

#[test]
fn event_dates_fit_the_four_digit_calendar_contract() {
    for year in [0, 10000, -5000] {
        let date = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
        let end = date.succ_opt().unwrap();
        assert!(
            EventTime::AllDay {
                start_date: date,
                end_date: end,
                time_zone: "UTC".into()
            }
            .validate()
            .is_err()
        );
        assert!(
            EventTime::Timed {
                start: date.and_hms_opt(0, 0, 0).unwrap().and_utc(),
                end: end.and_hms_opt(0, 0, 0).unwrap().and_utc(),
                time_zone: "UTC".into()
            }
            .validate()
            .is_err()
        );
    }
    for year in [1, 9999] {
        let date = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
        assert!(
            EventTime::AllDay {
                start_date: date,
                end_date: date.succ_opt().unwrap(),
                time_zone: "UTC".into()
            }
            .validate()
            .is_ok()
        );
    }
}

#[test]
fn timed_precision_matches_durable_reminder_timestamps() {
    for (fraction, valid) in [("123456", true), ("123456000", true), ("123456001", false)] {
        let time = EventTime::Timed {
            start: instant(&format!("2026-09-03T12:00:00.{fraction}Z")),
            end: instant("2026-09-03T13:00:00Z"),
            time_zone: "UTC".into(),
        };
        assert_eq!(time.validate().is_ok(), valid);
    }
    assert!(
        EventTime::Timed {
            start: instant("2016-12-31T23:59:60Z"),
            end: instant("2017-01-01T00:01:00Z"),
            time_zone: "UTC".into()
        }
        .validate()
        .is_err()
    );
}

fn instant(value: &str) -> DateTime<Utc> {
    value.parse().unwrap()
}

fn event() -> CalendarEvent {
    CalendarEvent {
        event_type_id: None,
        occurrence_details: Vec::new(),
        occurrence_descriptions: Vec::new(),
        canceled_from: None,
        split_from: None,
        split_to: None,
        external: None,
        calendar_item_id: ItemId::new(),
        case_item_id: None,
        attachment_item_ids: Vec::new(),
        title: "Project X closing".into(),
        time: EventTime::Timed {
            start: instant("2026-09-03T12:00:00Z"),
            end: instant("2026-09-03T13:00:00Z"),
            time_zone: "Europe/Madrid".into(),
        },
        location: None,
        meeting_url: None,
        organizer_user_id: "organizer".into(),
        state: EventState::Scheduled,
        attendees: Vec::new(),
        invitation_counter: 0,
        reminders: Reminders::default(),
        reminder_schedule_generation: 0,
        recurrence: None,
        exceptions: vec![],
        occurrence_responses: vec![],
        occurrence_attendees: vec![],
    }
}

#[test]
fn legacy_occurrence_cancel_replays_with_its_historical_series_order() {
    use crate::external_scheduling::state::{
        ExternalParticipantSource, ExternalScheduling, ReviewedExternalAction, apply_reviewed,
    };

    let mut event = event();
    event
        .set_schedule(
            event.time.clone(),
            Some(recurrence::Recurrence {
                rule: "FREQ=DAILY;COUNT=2".into(),
                rdates: Vec::new(),
                exdates: Vec::new(),
                timezone: None,
            }),
            Vec::new(),
        )
        .unwrap();
    let stamp = instant("2026-09-10T10:00:00Z");
    event.external = Some(ExternalScheduling::ExternalOrganizer {
        uid: "meeting@example.test".into(),
        organizer: "host@external.test".into(),
        sequence: 1,
        dtstamp: stamp,
        participants: [
            ("organizer", "organizer@example.test"),
            ("second", "second@example.test"),
        ]
        .into_iter()
        .map(|(user_id, recipient)| ExternalParticipantSource {
            user_id: user_id.into(),
            mailbox_id: uuid::Uuid::new_v4(),
            recipient: recipient.into(),
            invitation_id: uuid::Uuid::now_v7(),
            source_digest: "ab".repeat(32),
            sequence: 1,
            dtstamp: stamp,
            last_action: None,
            last_action_occurrence: None,
            occurrence_orders: Vec::new(),
        })
        .collect(),
    });
    let mut scoped_event = event.clone();
    let original_start = instant("2026-09-04T12:00:00Z");
    let command_id = uuid::Uuid::new_v4();
    let legacy_json = serde_json::json!({
        "kind": "CANCEL",
        "original_start": original_start,
        "sequence": 1,
        "dtstamp": stamp,
        "command_id": command_id,
    });
    let action: ReviewedExternalAction = serde_json::from_value(legacy_json).unwrap();

    apply_reviewed(&mut event, "organizer", &action).unwrap();
    let after_first = event.clone();
    apply_reviewed(&mut event, "organizer", &action).unwrap();
    assert_eq!(event, after_first);
    assert!(matches!(
        action,
        ReviewedExternalAction::Cancel {
            occurrence_order: None,
            ..
        }
    ));

    let scoped_cancel = |invitation_id| ReviewedExternalAction::Cancel {
        original_start: Some(original_start),
        sequence: Some(1),
        dtstamp: Some(stamp),
        command_id: Some(uuid::Uuid::new_v4()),
        occurrence_order: Some(crate::external_scheduling::state::ExternalOccurrenceOrder {
            original_start,
            invitation_id,
            source_digest: "cd".repeat(32),
            sequence: 1,
            dtstamp: stamp,
        }),
    };
    apply_reviewed(
        &mut scoped_event,
        "organizer",
        &scoped_cancel(uuid::Uuid::now_v7()),
    )
    .unwrap();
    let after_first_recipient = scoped_event.clone();
    apply_reviewed(
        &mut scoped_event,
        "second",
        &scoped_cancel(uuid::Uuid::now_v7()),
    )
    .unwrap();
    assert_eq!(scoped_event, after_first_recipient);
}

fn attendees(users: &[&str]) -> Vec<AttendeeInput> {
    users
        .iter()
        .map(|id| AttendeeInput {
            user_id: (*id).into(),
            role: AttendeeRole::Required,
        })
        .collect()
}

#[test]
fn repository_external_history_accepts_only_scheduling_and_canonical_rsvp_changes() {
    use crate::external_scheduling::state::{ExternalParticipantSource, ExternalScheduling};

    let mut original = event();
    original.event_type_id = Some(uuid::Uuid::new_v4());
    original
        .set_schedule(
            original.time.clone(),
            Some(recurrence::Recurrence {
                rule: "FREQ=DAILY;COUNT=2".into(),
                rdates: Vec::new(),
                exdates: Vec::new(),
                timezone: None,
            }),
            Vec::new(),
        )
        .unwrap();
    let source = ExternalParticipantSource {
        user_id: "organizer".into(),
        mailbox_id: uuid::Uuid::new_v4(),
        recipient: "organizer@example.test".into(),
        invitation_id: uuid::Uuid::now_v7(),
        source_digest: "ab".repeat(32),
        sequence: 1,
        dtstamp: instant("2026-09-10T10:00:00Z"),
        last_action: Some(uuid::Uuid::new_v4()),
        last_action_occurrence: None,
        occurrence_orders: Vec::new(),
    };
    original.external = Some(ExternalScheduling::ExternalOrganizer {
        uid: "meeting@example.test".into(),
        organizer: "host@external.test".into(),
        sequence: 1,
        dtstamp: source.dtstamp,
        participants: vec![source],
    });
    original
        .set_attendees_with_participant_access(
            &attendees(&["organizer"]),
            &std::collections::BTreeSet::from(["organizer".to_string()]),
        )
        .unwrap();
    let attendee = original.attendees[0].clone();
    original
        .respond(
            "organizer",
            attendee.invitation_generation,
            attendee.response_revision,
            Response::Accepted,
        )
        .unwrap();
    original.validate().unwrap();

    let import = |state: &CalendarEvent| {
        let mut proposal = history_event("SET_METADATA", state);
        proposal.event_type = "CALENDAR_EVENT::IMPORT_INVITATION".into();
        proposal
    };
    let mut canonical = original.clone();
    let Some(ExternalScheduling::ExternalOrganizer { participants, .. }) = &mut canonical.external
    else {
        unreachable!()
    };
    participants[0].invitation_id = uuid::Uuid::now_v7();
    participants[0].source_digest = "cd".repeat(32);
    participants[0].last_action = Some(uuid::Uuid::new_v4());
    let attendee = canonical.attendees[0].clone();
    canonical
        .respond(
            "organizer",
            attendee.invitation_generation,
            attendee.response_revision,
            Response::Tentative,
        )
        .unwrap();
    assert_eq!(
        history::reduce_event(Some(&original), &import(&canonical), "organizer").unwrap(),
        canonical
    );

    let mut forged_type = canonical.clone();
    forged_type.event_type_id = None;
    assert!(history::reduce_event(Some(&original), &import(&forged_type), "organizer").is_err());

    let original_start = instant("2026-09-04T12:00:00Z");
    let mut scoped = original.clone();
    scoped
        .edit_occurrence(&occurrence_details::Edit {
            original_start,
            time: Some(EventTime::Timed {
                start: instant("2026-09-04T14:00:00Z"),
                end: instant("2026-09-04T15:00:00Z"),
                time_zone: "Europe/Madrid".into(),
            }),
            details: occurrence_details::Details {
                title: Some("Moved occurrence".into()),
                ..Default::default()
            },
            attendees: None,
        })
        .unwrap();
    let attendee = scoped
        .occurrence_attendees(Some(original_start))
        .iter()
        .find(|attendee| attendee.user_id == "organizer")
        .unwrap()
        .clone();
    scoped
        .respond_occurrence(
            "organizer",
            original_start,
            attendee.invitation_generation,
            attendee.response_revision,
            Response::Tentative,
        )
        .unwrap();
    let Some(ExternalScheduling::ExternalOrganizer { participants, .. }) = &mut scoped.external
    else {
        unreachable!()
    };
    participants[0].last_action = Some(uuid::Uuid::new_v4());
    participants[0].last_action_occurrence = Some(original_start);
    participants[0].occurrence_orders.push(
        crate::external_scheduling::state::ExternalOccurrenceOrder {
            original_start,
            invitation_id: uuid::Uuid::now_v7(),
            source_digest: "cd".repeat(32),
            sequence: 3,
            dtstamp: instant("2026-09-10T12:00:00Z"),
        },
    );
    assert_eq!(
        history::reduce_event(Some(&original), &import(&scoped), "organizer").unwrap(),
        scoped
    );
    let mut forged_scoped = scoped.clone();
    forged_scoped.title = "Forged series title".into();
    assert!(history::reduce_event(Some(&original), &import(&forged_scoped), "organizer").is_err());

    for verb in [
        "EDIT_OCCURRENCE",
        "RESCHEDULE_OCCURRENCE",
        "CANCEL_OCCURRENCE",
        "CANCEL_FOLLOWING",
        "SPLIT_FOLLOWING_SOURCE",
        "ORGANIZER_DEPARTURE",
    ] {
        let proposal = serde_json::from_value(serde_json::json!({
            "item_id": ItemId::new(),
            "event_type": format!("CALENDAR_EVENT::{verb}"),
            "base_seq": 1,
            "set_metadata": {}
        }))
        .unwrap();
        assert!(history::reduce_event(Some(&original), &proposal, "organizer").is_err());
    }
    let occurrence_description = serde_json::from_value(serde_json::json!({
        "item_id": ItemId::new(),
        "event_type": "CALENDAR_EVENT::SET_OCCURRENCE_DESCRIPTION",
        "base_seq": 1,
        "set_metadata": {"original_start":"2026-09-03T12:00:00Z"},
        "set_content": {"json":{"body":"forged"}}
    }))
    .unwrap();
    let with_occurrence_description =
        history::reduce_event(Some(&canonical), &occurrence_description, "organizer").unwrap();
    assert_eq!(with_occurrence_description.occurrence_descriptions.len(), 1);
    let description = serde_json::from_value(serde_json::json!({
        "item_id": ItemId::new(),
        "event_type": "CALENDAR_EVENT::SET_DESCRIPTION",
        "base_seq": 1,
        "set_content": {"json":{"body":"updated by the external organizer"}}
    }))
    .unwrap();
    assert_eq!(
        history::reduce_event(Some(&canonical), &description, "organizer").unwrap(),
        canonical
    );

    let target_id = ItemId::new();
    let split_target = serde_json::from_value(serde_json::json!({
        "item_id": target_id,
        "event_type": "CALENDAR_EVENT::SPLIT_FOLLOWING_TARGET",
        "create_item": {
            "item_type":"CALENDAR_EVENT",
            "name":history::internal_name("CALENDAR_EVENT", &target_id)
        },
        "set_metadata": original
    }))
    .unwrap();
    assert!(history::reduce_event(None, &split_target, "organizer").is_err());

    let mut forged = original.clone();
    forged.title = "forged".into();
    assert!(history::reduce_event(Some(&original), &import(&forged), "organizer").is_err());

    forged = original.clone();
    let Some(ExternalScheduling::ExternalOrganizer { participants, .. }) = &mut forged.external
    else {
        unreachable!()
    };
    participants[0].mailbox_id = uuid::Uuid::new_v4();
    assert!(history::reduce_event(Some(&original), &import(&forged), "organizer").is_err());

    forged = original.clone();
    forged.attendees[0].invitation_generation += 1;
    assert!(history::reduce_event(Some(&original), &import(&forged), "organizer").is_err());
}

#[test]
fn event_attachments_are_bounded_distinct_uuid_v7_file_references() {
    let mut value = event();
    let file = ItemId::new();
    value.attachment_item_ids = vec![file.clone()];
    value.validate().unwrap();

    value.attachment_item_ids.push(file);
    assert!(matches!(
        value.validate(),
        Err(CalendarError::Invalid("invalid attachment identity"))
    ));

    value.attachment_item_ids = (0..=MAX_ATTACHMENTS).map(|_| ItemId::new()).collect();
    assert!(matches!(
        value.validate(),
        Err(CalendarError::Invalid("event exceeds 20 attachments"))
    ));
}

#[test]
fn metadata_history_preserves_attachment_edits() {
    let original = event();
    let mut requested = original.clone();
    requested.attachment_item_ids.push(ItemId::new());
    let next = history::reduce_event(
        Some(&original),
        &history_event("SET_METADATA", &requested),
        &original.organizer_user_id,
    )
    .unwrap();
    assert_eq!(next.attachment_item_ids, requested.attachment_item_ids);
}

fn history_event(kind: &str, state: &CalendarEvent) -> synck_vcs::types::EventProposal {
    let item_id = ItemId::new();
    let mut value = serde_json::json!({
        "item_id": item_id,
        "event_type": format!("CALENDAR_EVENT::{kind}"),
        "set_metadata": state,
    });
    if kind == "CREATE" {
        value["create_item"] = serde_json::json!({
            "item_type": "CALENDAR_EVENT",
            "name": history::internal_name("CALENDAR_EVENT", &item_id),
        });
    } else {
        value["base_seq"] = serde_json::json!(1);
    }
    serde_json::from_value(value).unwrap()
}

#[test]
fn canonical_history_preserves_identity_and_rejects_metadata_rsvp_forgery() {
    let mut original = event();
    original.set_attendees(&attendees(&["b", "a"])).unwrap();
    original.set_reminders(&[60, 10]).unwrap();
    let create = history_event("CREATE", &original);
    let mut replayed = history::reduce_event(None, &create, "organizer").unwrap();
    assert_eq!(replayed, original);
    assert!(history::reduce_event(None, &create, "other").is_err());
    let response = synck_vcs::types::EventProposal {
        event_type: "CALENDAR_EVENT::ATTENDEE_RESPONSE".into(),
        set_metadata: Some(serde_json::json!({
            "invitation_generation": replayed.attendees[0].invitation_generation,
            "expected_response_revision": 0,
            "response": "ACCEPTED",
        })),
        ..history_event("SET_METADATA", &original)
    };
    replayed = history::reduce_event(Some(&replayed), &response, "a").unwrap();
    assert_eq!(replayed.attendees[0].response, Response::Accepted);
    assert!(
        history::reduce_event(
            Some(&replayed),
            &history_event("SET_METADATA", &original),
            "organizer"
        )
        .is_err()
    );
    let mut edited = replayed.clone();
    edited.title = "Changed title".into();
    edited.set_reminders(&[10, 30, 60]).unwrap();
    assert_eq!(
        history::reduce_event(
            Some(&replayed),
            &history_event("SET_METADATA", &edited),
            "organizer"
        )
        .unwrap(),
        edited
    );
    for mutate in [
        |value: &mut CalendarEvent| value.calendar_item_id = ItemId::new(),
        |value: &mut CalendarEvent| value.organizer_user_id = "other".into(),
        |value: &mut CalendarEvent| value.reminders.entries[0].reminder_id = 50,
        |value: &mut CalendarEvent| value.attendees[0].response = Response::Declined,
    ] {
        let mut forged = edited.clone();
        mutate(&mut forged);
        assert!(
            history::reduce_event(
                Some(&replayed),
                &history_event("SET_METADATA", &forged),
                "organizer"
            )
            .is_err()
        );
    }
}

#[test]
fn canonical_create_replays_initial_external_invitations() {
    let mut original = event();
    crate::external_scheduling::state::configure_outbound(
        &mut original,
        format!("{}@calendar.synckhub", ItemId::new().as_str()),
        "organizer@example.test".into(),
        vec!["guest@example.test".into()],
    )
    .unwrap();

    assert_eq!(
        history::reduce_event(None, &history_event("CREATE", &original), "organizer").unwrap(),
        original,
    );
}

#[test]
fn native_history_rejects_tree_and_lifecycle_bypasses() {
    let original = event();
    let create = history_event("CREATE", &original);
    let mut delegated_create = create.clone();
    delegated_create.event_type = "CALENDAR_EVENT::DELEGATE_CREATE".into();
    assert!(history::validate_envelope(&delegated_create).is_ok());
    let mut delegated_response = history_event("SET_METADATA", &original);
    delegated_response.event_type = "CALENDAR_EVENT::DELEGATE_RESPONSE".into();
    assert!(history::validate_envelope(&delegated_response).is_ok());
    let mut bad = create.clone();
    bad.create_item.as_mut().unwrap().parent_folder_id = Some(ItemId::new());
    assert!(history::validate_envelope(&bad).is_err());
    bad = create.clone();
    bad.create_item.as_mut().unwrap().name = original.title.clone();
    assert!(history::validate_envelope(&bad).is_err());
    for verb in [
        "TRASH",
        "RESTORE",
        "TOMBSTONE",
        "FEDERATE",
        "SET_PARENT",
        "SET_NAME",
    ] {
        assert!(history::validate_envelope(&history_event(verb, &original)).is_err());
    }
    let mut canceled = original.clone();
    canceled.cancel().unwrap();
    assert_eq!(
        history::reduce_event(
            Some(&original),
            &history_event("SET_METADATA", &canceled),
            "organizer"
        )
        .unwrap(),
        canceled
    );
    canceled.title = "Cancel and edit".into();
    assert!(
        history::reduce_event(
            Some(&original),
            &history_event("SET_METADATA", &canceled),
            "organizer"
        )
        .is_err()
    );
}

#[test]
fn native_identity_requires_uuidv7() {
    assert_eq!(
        uuid::Uuid::parse_str(ItemId::new().as_str())
            .unwrap()
            .get_version_num(),
        7
    );
    assert!(
        serde_json::from_value::<ItemId>(serde_json::json!(uuid::Uuid::new_v4().to_string()))
            .is_err()
    );
}

#[test]
fn wall_times_reject_gaps_and_require_fold_offsets() {
    let gap = "2026-03-29T02:30:00".parse().unwrap();
    assert!(resolve_local_time(gap, "Europe/Madrid", None).is_err());
    let fold = "2026-10-25T02:30:00".parse().unwrap();
    assert!(resolve_local_time(fold, "Europe/Madrid", None).is_err());
    assert_eq!(
        resolve_local_time(fold, "Europe/Madrid", Some(7200)).unwrap(),
        instant("2026-10-25T00:30:00Z")
    );
    assert_eq!(
        resolve_local_time(fold, "Europe/Madrid", Some(3600)).unwrap(),
        instant("2026-10-25T01:30:00Z")
    );
    assert!(resolve_local_time(fold, "Europe/Madrid", Some(0)).is_err());
    assert!(resolve_local_time(fold, "not/a-zone", None).is_err());
}

#[test]
fn timed_input_normalizes_and_rejects_inconsistent_offset() {
    let time: EventTimeInput = serde_json::from_value(serde_json::json!({
        "kind":"TIMED", "start":"2026-09-03T14:00:00+02:00", "end":"2026-09-03T15:00:00+02:00", "time_zone":"Europe/Madrid"
    })).unwrap();
    assert_eq!(time.normalize().unwrap(), event().time);
    let time: EventTimeInput = serde_json::from_value(serde_json::json!({
        "kind":"TIMED", "start":"2026-09-03T14:00:00+01:00", "end":"2026-09-03T15:00:00+01:00", "time_zone":"Europe/Madrid"
    })).unwrap();
    assert!(time.normalize().is_err());
}

#[test]
fn date_only_ranges_and_reminders_preserve_local_date() {
    let time = EventTime::AllDay {
        start_date: "2028-02-29".parse().unwrap(),
        end_date: "2028-03-01".parse().unwrap(),
        time_zone: "Europe/Madrid".into(),
    };
    time.validate().unwrap();
    assert_eq!(
        time.reminder_target(0).unwrap(),
        instant("2028-02-29T08:00:00Z")
    );
    let invalid = EventTime::AllDay {
        start_date: "2028-02-29".parse().unwrap(),
        end_date: "2028-02-29".parse().unwrap(),
        time_zone: "Europe/Madrid".into(),
    };
    assert!(invalid.validate().is_err());
    let skipped_date = EventTime::AllDay {
        start_date: "2011-12-30".parse().unwrap(),
        end_date: "2011-12-31".parse().unwrap(),
        time_zone: "Pacific/Apia".into(),
    };
    assert!(skipped_date.reminder_target(0).is_err());
}

#[test]
fn reminders_support_zero_one_many_without_reissuing_retained_ids() {
    let mut reminders = Reminders::default();
    assert!(!reminders.replace(&[]).unwrap());
    assert!(reminders.replace(&[60]).unwrap());
    let first = reminders.entries[0].clone();
    let offsets: Vec<u32> = (0..100).collect();
    assert!(reminders.replace(&offsets).unwrap());
    assert_eq!(reminders.entries.len(), 100);
    assert_eq!(
        reminders
            .entries
            .iter()
            .find(|r| r.minutes_before_start == 60),
        Some(&first)
    );
    let saved = reminders.clone();
    assert!(
        !reminders
            .replace(&offsets.iter().rev().copied().collect::<Vec<_>>())
            .unwrap()
    );
    assert_eq!(reminders, saved);
    assert!(reminders.replace(&[]).unwrap());
    reminders.replace(&[60]).unwrap();
    assert!(reminders.entries[0].reminder_id > first.reminder_id);
}

#[test]
fn reminder_validation_is_atomic_and_has_exact_bounds() {
    let mut reminders = Reminders::default();
    reminders.replace(&[0, MAX_REMINDER_MINUTES]).unwrap();
    let saved = reminders.clone();
    assert!(reminders.replace(&[1, 1]).is_err());
    assert_eq!(reminders, saved);
    assert!(reminders.replace(&[MAX_REMINDER_MINUTES + 1]).is_err());
    assert_eq!(reminders, saved);
    reminders.counter = MAX_COUNTER;
    let saved = reminders.clone();
    assert!(reminders.replace(&[2]).is_err());
    assert_eq!(reminders, saved);
    assert!(serde_json::from_str::<Vec<u32>>("[-1]").is_err());
    assert!(serde_json::from_str::<Vec<u32>>("[1.5]").is_err());
    assert!(serde_json::from_str::<Vec<u32>>("null").is_err());
}

#[test]
fn independent_responses_do_not_conflict_or_overwrite() {
    let mut event = event();
    event.set_attendees(&attendees(&["alice", "bob"])).unwrap();
    let alice = event.attendees[0].clone();
    let bob = event.attendees[1].clone();
    assert!(
        event
            .respond("alice", alice.invitation_generation, 0, Response::Accepted)
            .unwrap()
    );
    assert!(
        event
            .respond("bob", bob.invitation_generation, 0, Response::Tentative)
            .unwrap()
    );
    assert_eq!(event.attendees[0].response, Response::Accepted);
    assert!(
        event
            .respond("alice", alice.invitation_generation, 0, Response::Declined)
            .is_err()
    );
    assert!(
        !event
            .respond("alice", alice.invitation_generation, 1, Response::Accepted)
            .unwrap()
    );
    assert!(
        event
            .respond("other", alice.invitation_generation, 1, Response::Accepted)
            .is_err()
    );
    assert!(
        event
            .respond(
                "alice",
                alice.invitation_generation,
                1,
                Response::NeedsAction
            )
            .is_err()
    );
}

#[test]
fn participant_changes_preserve_responses_and_never_reuse_generations() {
    let mut event = event();
    event.set_attendees(&attendees(&["alice"])).unwrap();
    let generation = event.attendees[0].invitation_generation;
    event
        .respond("alice", generation, 0, Response::Accepted)
        .unwrap();
    event.set_attendees(&attendees(&["bob", "alice"])).unwrap();
    assert_eq!(event.attendees[0].response, Response::Accepted);
    event.set_attendees(&attendees(&["bob"])).unwrap();
    event.set_attendees(&attendees(&["alice", "bob"])).unwrap();
    assert!(event.attendees[0].invitation_generation > generation);
    assert_eq!(event.attendees[0].response, Response::NeedsAction);
    let saved = event.clone();
    assert!(
        event
            .set_attendees(&attendees(&["alice", "alice"]))
            .is_err()
    );
    assert_eq!(event, saved);
}

#[test]
fn schedule_changes_reset_responses_but_reminder_edits_do_not() {
    let mut event = event();
    event.set_attendees(&attendees(&["alice"])).unwrap();
    let generation = event.attendees[0].invitation_generation;
    event
        .respond("alice", generation, 0, Response::Accepted)
        .unwrap();
    event.set_reminders(&[60]).unwrap();
    let reminder_id = event.reminders.entries[0].reminder_id;
    event.set_reminders(&[10, 60]).unwrap();
    assert_eq!(event.reminder_schedule_generation, 0);
    assert_eq!(event.attendees[0].response, Response::Accepted);
    let mut time = event.time.clone();
    if let EventTime::Timed { start, .. } = &mut time {
        *start += Duration::minutes(15);
    }
    event.set_time(time).unwrap();
    assert_eq!(event.reminder_schedule_generation, 1);
    assert_eq!(event.attendees[0].response, Response::NeedsAction);
    assert!(
        event
            .respond("alice", generation, 1, Response::Declined)
            .is_err()
    );
    assert_eq!(
        event
            .reminders
            .entries
            .iter()
            .find(|r| r.minutes_before_start == 60)
            .unwrap()
            .reminder_id,
        reminder_id
    );
    event.validate().unwrap();
}

#[test]
fn display_zone_change_preserves_schedule_and_invitations() {
    let mut event = event();
    event.set_attendees(&attendees(&["alice"])).unwrap();
    let original = event.attendees.clone();
    let mut time = event.time.clone();
    if let EventTime::Timed { time_zone, .. } = &mut time {
        *time_zone = "America/New_York".into();
    }
    assert!(event.set_time(time).unwrap());
    assert_eq!(event.attendees, original);
    assert_eq!(event.reminder_schedule_generation, 0);
}

#[test]
fn cancel_is_terminal_and_idempotent() {
    let mut event = event();
    assert!(event.cancel().unwrap());
    assert!(!event.cancel().unwrap());
    assert_eq!(event.reminder_schedule_generation, 1);
    assert!(event.set_attendees(&[]).is_err());
    assert!(event.set_reminders(&[]).is_err());
    assert!(event.set_time(event.time.clone()).is_err());
}

#[test]
fn admission_limits_and_urls_are_explicit() {
    let mut event = event();
    event.title = "x".repeat(MAX_TITLE_BYTES);
    event.validate().unwrap();
    event.title.push('x');
    assert!(event.validate().is_err());
    event.title = "valid".into();
    for value in [
        "javascript:alert(1)",
        "/relative",
        "file:///tmp/a",
        "https://user:secret@example.com",
    ] {
        event.meeting_url = Some(value.into());
        assert!(event.validate().is_err());
    }
    event.meeting_url = Some("https://example.com/meeting".into());
    event.validate().unwrap();
    let maximum_attendees = (0..MAX_ATTENDEES)
        .map(|index| AttendeeInput {
            user_id: format!("person-{index}"),
            role: AttendeeRole::Required,
        })
        .collect::<Vec<_>>();
    event.set_attendees(&maximum_attendees).unwrap();
    let mut too_many_attendees = maximum_attendees;
    too_many_attendees.push(AttendeeInput {
        user_id: "one-too-many".into(),
        role: AttendeeRole::Required,
    });
    assert!(event.set_attendees(&too_many_attendees).is_err());

    // Organizer-as-attendee existed before command admission rejected new
    // duplicates. It remains valid persisted state for reads and removal.
    let mut historical = self::event();
    historical
        .set_attendees(&attendees(&["organizer"]))
        .unwrap();
    historical.validate().unwrap();

    let mut outbound = self::event();
    crate::external_scheduling::state::configure_outbound(
        &mut outbound,
        "event@example.test".into(),
        "organizer@example.test".into(),
        (0..crate::external_scheduling::state::MAX_EXTERNAL_ATTENDEES)
            .map(|index| format!("person-{index}@example.test"))
            .collect(),
    )
    .unwrap();
    assert!(
        crate::external_scheduling::state::configure_outbound(
            &mut outbound,
            "event@example.test".into(),
            "organizer@example.test".into(),
            (0..=crate::external_scheduling::state::MAX_EXTERNAL_ATTENDEES)
                .map(|index| format!("person-{index}@example.test"))
                .collect(),
        )
        .is_err()
    );
    let mut calendar = Calendar {
        title: "M&A".into(),
        description: None,
        color: "#aa44FF".into(),
        archived: false,
    };
    calendar.validate().unwrap();
    calendar.color = "#éabcd".into();
    assert!(calendar.validate().is_err());
}

#[test]
fn persisted_state_round_trips_and_rejects_unknown_fields() {
    let mut event = event();
    event.set_attendees(&attendees(&["alice"])).unwrap();
    event.set_reminders(&[10, 60]).unwrap();
    let value = serde_json::to_value(&event).unwrap();
    let restored: CalendarEvent = serde_json::from_value(value.clone()).unwrap();
    restored.validate().unwrap();
    assert_eq!(restored, event);
    let mut invalid = value;
    invalid["private"] = serde_json::json!(true);
    assert!(serde_json::from_value::<CalendarEvent>(invalid).is_err());
}

#[test]
fn reminder_window_has_inclusive_deadline() {
    let target = instant("2026-09-03T12:00:00Z");
    assert!(!reminder_is_due(target, target - Duration::milliseconds(1)));
    assert!(reminder_is_due(target, target));
    assert!(reminder_is_due(target, target + Duration::minutes(15)));
    assert!(!reminder_is_due(
        target,
        target + Duration::minutes(15) + Duration::milliseconds(1)
    ));
}

#[test]
fn invitation_admission_preserves_legacy_wire_and_rejects_client_assignment() {
    let legacy = serde_json::json!({"user_id":"a","role":"REQUIRED","response":"NEEDS_ACTION","invitation_generation":1,"response_revision":0});
    let attendee: Attendee = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(
        attendee.admission,
        InvitationAdmission::RepositoryMembership
    );
    assert_eq!(serde_json::to_value(&attendee).unwrap(), legacy);
    assert!(
        serde_json::from_value::<AttendeeInput>(
            serde_json::json!({"user_id":"a","role":"REQUIRED","admission":"EVENT_PARTICIPATION"})
        )
        .is_err()
    );
    let mut invalid = legacy;
    invalid["admission"] = serde_json::json!("UNKNOWN");
    assert!(serde_json::from_value::<Attendee>(invalid).is_err());
}

#[test]
fn participant_admission_survives_edits_and_requires_a_new_invitation_to_change() {
    let mut value = event();
    value
        .set_attendees_with_participant_access(
            &attendees(&["a", "b"]),
            &BTreeSet::from(["b".to_string()]),
        )
        .unwrap();
    assert_eq!(
        value.attendees[0].admission,
        InvitationAdmission::RepositoryMembership
    );
    assert_eq!(
        value.attendees[1].admission,
        InvitationAdmission::EventParticipation
    );
    let before = value.clone();
    assert!(
        value
            .set_attendees_with_participant_access(
                &attendees(&["a", "b"]),
                &BTreeSet::from(["a".to_string(), "b".to_string()])
            )
            .is_err()
    );
    assert_eq!(value, before);
    let generation = value.attendees[1].invitation_generation;
    value
        .respond("b", generation, 0, Response::Accepted)
        .unwrap();
    value.set_attendees(&attendees(&["a", "b"])).unwrap();
    assert_eq!(
        value.attendees[1].admission,
        InvitationAdmission::EventParticipation
    );
    assert_eq!(value.attendees[1].response, Response::Accepted);
    value.set_attendees(&attendees(&["a"])).unwrap();
    value.set_attendees(&attendees(&["a", "b"])).unwrap();
    assert_eq!(
        value.attendees[1].admission,
        InvitationAdmission::RepositoryMembership
    );
    assert!(value.attendees[1].invitation_generation > generation);
    assert!(
        value
            .respond("b", generation, 0, Response::Accepted)
            .is_err()
    );
    let before = value.clone();
    assert!(
        value
            .set_attendees_with_participant_access(
                &attendees(&["a", "b"]),
                &BTreeSet::from(["absent".to_string()])
            )
            .is_err()
    );
    assert_eq!(value, before);
}

#[test]
fn history_preserves_participant_admission_and_rejects_in_place_conversion() {
    let mut value = event();
    value
        .set_attendees_with_participant_access(
            &attendees(&["a"]),
            &BTreeSet::from(["a".to_string()]),
        )
        .unwrap();
    let id = ItemId::new();
    let create: synck_vcs::types::EventProposal = serde_json::from_value(serde_json::json!({
        "item_id":id,"event_type":"CALENDAR_EVENT::CREATE",
        "create_item":{"item_type":"CALENDAR_EVENT","name":history::internal_name("CALENDAR_EVENT",&id)},
        "set_metadata":value,
    })).unwrap();
    let replay = history::reduce_event(None, &create, &value.organizer_user_id).unwrap();
    assert_eq!(replay, value);
    let mut changed = value.clone();
    changed.attendees[0].admission = InvitationAdmission::RepositoryMembership;
    let patch: synck_vcs::types::EventProposal = serde_json::from_value(serde_json::json!({
        "item_id":id,"event_type":"CALENDAR_EVENT::SET_METADATA","base_seq":1,"set_metadata":changed,
    })).unwrap();
    assert!(history::reduce_event(Some(&value), &patch, &value.organizer_user_id).is_err());
}

#[test]
fn recurring_schedule_is_atomic_and_legacy_serialization_is_unchanged() {
    let mut value = event();
    let legacy = serde_json::to_value(&value).unwrap();
    assert!(legacy.get("recurrence").is_none());
    assert!(legacy.get("exceptions").is_none());
    value.set_attendees(&attendees(&["a"])).unwrap();
    value
        .respond(
            "a",
            value.attendees[0].invitation_generation,
            0,
            Response::Accepted,
        )
        .unwrap();
    let rule = recurrence::Recurrence {
        rule: "FREQ=DAILY;COUNT=3".into(),
        rdates: vec![],
        exdates: vec![],
        timezone: None,
    };
    let old_generation = value.reminder_schedule_generation;
    let old_invitation = value.attendees[0].invitation_generation;
    value
        .set_schedule(value.time.clone(), Some(rule.clone()), vec![])
        .unwrap();
    assert_eq!(value.reminder_schedule_generation, old_generation + 1);
    assert!(value.attendees[0].invitation_generation > old_invitation);
    assert_eq!(value.attendees[0].response, Response::NeedsAction);
    let before = value.clone();
    assert!(
        !value
            .set_schedule(value.time.clone(), Some(rule), vec![])
            .unwrap()
    );
    assert_eq!(value, before);
    let invalid = recurrence::OccurrenceException {
        revision: None,
        canceled: false,
        original_start: instant("2027-01-01T00:00:00Z"),
        time: None,
    };
    assert!(
        value
            .set_schedule(value.time.clone(), value.recurrence.clone(), vec![invalid])
            .is_err()
    );
    assert_eq!(value, before);
    assert_eq!(
        serde_json::from_value::<CalendarEvent>(serde_json::to_value(&value).unwrap()).unwrap(),
        value
    );
}

#[test]
fn occurrence_responses_replay_and_reset_only_for_new_schedule() {
    use synck_vcs::types::EventProposal;
    let mut event = event();
    event
        .set_schedule(
            event.time.clone(),
            Some(recurrence::Recurrence {
                rule: "FREQ=DAILY;COUNT=3".into(),
                rdates: vec![],
                exdates: vec![],
                timezone: None,
            }),
            vec![],
        )
        .unwrap();
    event.set_attendees(&attendees(&["alice"])).unwrap();
    let first = instant("2026-09-03T12:00:00Z");
    let generation = event.attendees[0].invitation_generation;
    let proposal: EventProposal=serde_json::from_value(serde_json::json!({"event_type":"CALENDAR_EVENT::ATTENDEE_RESPONSE","item_id":ItemId::new(),"base_seq":1,"set_metadata":{"occurrence_start":first,"invitation_generation":generation,"expected_response_revision":0,"response":"DECLINED"}})).unwrap();
    event = history::reduce_event(Some(&event), &proposal, "alice").unwrap();
    let mut requested = event.clone();
    requested.title = "New title".into();
    let edit: EventProposal = serde_json::from_value(serde_json::json!({
        "event_type":"CALENDAR_EVENT::SET_METADATA", "item_id":ItemId::new(),
        "base_seq":2, "set_metadata":requested,
    }))
    .unwrap();
    let preserved = history::reduce_event(Some(&event), &edit, "organizer").unwrap();
    assert_eq!(preserved.occurrence_responses, event.occurrence_responses);
    requested.occurrence_responses[0].response = Response::Accepted;
    let mut forged = edit.clone();
    forged.set_metadata = Some(serde_json::to_value(requested).unwrap());
    assert!(history::reduce_event(Some(&event), &forged, "organizer").is_err());
    let unchanged = event.clone();
    assert!(!event.set_time(event.time.clone()).unwrap());
    assert_eq!(event, unchanged);
    event
        .set_schedule(
            event.time.clone(),
            Some(recurrence::Recurrence {
                rule: "FREQ=DAILY;COUNT=2".into(),
                rdates: vec![],
                exdates: vec![],
                timezone: None,
            }),
            vec![],
        )
        .unwrap();
    assert!(event.occurrence_responses.is_empty());
    assert_eq!(event.attendees[0].response, Response::NeedsAction);
    let legacy = serde_json::to_value(super::tests::event()).unwrap();
    assert!(legacy.get("occurrence_responses").is_none());
}

#[test]
fn canceling_a_moved_occurrence_retains_its_last_displayed_time() {
    let mut event = event();
    let original = instant("2026-09-03T12:00:00Z");
    event
        .set_schedule(
            event.time.clone(),
            Some(recurrence::Recurrence {
                rule: "FREQ=DAILY;COUNT=3".into(),
                rdates: vec![],
                exdates: vec![],
                timezone: None,
            }),
            vec![],
        )
        .unwrap();
    let plain_bounds = event.range_bounds().unwrap();
    let moved = EventTime::Timed {
        start: instant("2026-10-01T12:00:00Z"),
        end: instant("2026-10-01T13:00:00Z"),
        time_zone: "UTC".into(),
    };
    event
        .set_schedule(
            event.time.clone(),
            event.recurrence.clone(),
            vec![recurrence::OccurrenceException {
                revision: None,
                canceled: false,
                original_start: original,
                time: Some(moved.clone()),
            }],
        )
        .unwrap();
    let before = event.clone();
    event.cancel_occurrence(original).unwrap();
    assert_eq!(
        event.occurrence_at(original).unwrap(),
        Some((moved, EventState::Canceled))
    );
    assert!(event.exceptions[0].is_canceled());
    assert_eq!(event.range_bounds().unwrap(), plain_bounds);
    assert!(
        event
            .recurrence
            .as_ref()
            .unwrap()
            .expand_with_exceptions(
                &event.time,
                &event.exceptions,
                "2026-10-01".parse().unwrap(),
                "2026-10-02".parse().unwrap(),
                "UTC",
                None,
            )
            .unwrap()
            .is_empty()
    );
    let serialized = serde_json::to_value(&before.exceptions[0]).unwrap();
    assert!(serialized.get("canceled").is_none());
    let decoded: recurrence::OccurrenceException = serde_json::from_value(serialized).unwrap();
    assert!(!decoded.is_canceled());
    let before_repeat = event.clone();
    assert!(!event.cancel_occurrence(original).unwrap());
    assert_eq!(event, before_repeat);
}

#[test]
fn canceled_tail_bounds_expansion_without_losing_earlier_moved_occurrences() {
    let mut source = event();
    source
        .set_schedule(
            source.time.clone(),
            Some(recurrence::Recurrence {
                rule: "FREQ=DAILY".into(),
                rdates: vec![],
                exdates: vec![],
                timezone: None,
            }),
            vec![],
        )
        .unwrap();
    let first = instant("2026-09-03T12:00:00Z");
    let moved = EventTime::Timed {
        start: instant("9000-01-01T12:00:00Z"),
        end: instant("9000-01-01T13:00:00Z"),
        time_zone: "UTC".into(),
    };
    source.reschedule_occurrence(first, moved.clone()).unwrap();
    source.cancel_following(first + Duration::days(1)).unwrap();
    let occurrences = source
        .recurrence
        .as_ref()
        .unwrap()
        .expand_with_exceptions(
            &source.time,
            &source.exceptions,
            NaiveDate::from_ymd_opt(9000, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(9000, 1, 2).unwrap(),
            "UTC",
            source.canceled_from,
        )
        .unwrap();
    assert_eq!(occurrences.len(), 1);
    assert_eq!(occurrences[0].original_start, first);
    assert_eq!(occurrences[0].time, moved);
    let EventTime::Timed { end, .. } = source.range_bounds().unwrap() else {
        unreachable!()
    };
    assert_eq!(end, instant("9000-01-01T13:00:00Z"));
}

#[test]
fn shortening_series_retires_only_removed_occurrence_overrides() {
    let mut source = event();
    source
        .set_schedule(
            source.time.clone(),
            Some(recurrence::Recurrence {
                rule: "FREQ=DAILY;COUNT=4".into(),
                rdates: vec![],
                exdates: vec![],
                timezone: None,
            }),
            vec![],
        )
        .unwrap();
    let first = instant("2026-09-03T12:00:00Z");
    for (index, start) in [first, first + Duration::days(3)].into_iter().enumerate() {
        source
            .edit_occurrence(&occurrence_details::Edit {
                original_start: start,
                time: None,
                details: occurrence_details::Details {
                    title: Some(format!("Override {index}")),
                    ..Default::default()
                },
                attendees: None,
            })
            .unwrap();
        source
            .set_occurrence_description(start, 10 + index as i64)
            .unwrap();
    }
    source.cancel_occurrence(first + Duration::days(2)).unwrap();
    let before = source.clone();
    let mut shorter = source.recurrence.clone().unwrap();
    shorter.rule = "FREQ=DAILY;COUNT=2".into();
    assert!(
        source
            .set_schedule(
                source.time.clone(),
                Some(shorter),
                source.exceptions.clone()
            )
            .unwrap()
    );
    assert_eq!(source.occurrence_details.len(), 1);
    assert_eq!(source.occurrence_descriptions.len(), 1);
    assert_eq!(source.occurrence_details(Some(first)).title, "Override 0");
    assert!(source.exceptions.is_empty());
    assert!(
        source
            .occurrence_at(first + Duration::days(3))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        history::reduce_event(
            Some(&before),
            &history_event("SET_METADATA", &source),
            "organizer"
        )
        .unwrap(),
        source
    );
    let stable = source.clone();
    let mut invalid = source.recurrence.clone().unwrap();
    invalid.rule = "FREQ=INVALID".into();
    assert!(
        source
            .set_schedule(
                source.time.clone(),
                Some(invalid),
                source.exceptions.clone()
            )
            .is_err()
    );
    assert_eq!(source, stable);
}

#[test]
fn occurrence_attendees_are_scoped_and_keep_independent_rsvp_revision() {
    let mut source = event();
    source
        .set_schedule(
            source.time.clone(),
            Some(recurrence::Recurrence {
                rule: "FREQ=DAILY;COUNT=3".into(),
                rdates: vec![],
                exdates: vec![],
                timezone: None,
            }),
            vec![],
        )
        .unwrap();
    source.set_attendees(&attendees(&["a"])).unwrap();
    let start = instant("2026-09-04T12:00:00Z");
    source
        .set_occurrence_attendees(start, &attendees(&["a", "b"]), &BTreeSet::new())
        .unwrap();
    assert_eq!(source.attendees.len(), 1);
    assert_eq!(source.occurrence_attendees(Some(start)).len(), 2);
    let invited = source
        .occurrence_attendees(Some(start))
        .iter()
        .find(|attendee| attendee.user_id == "b")
        .unwrap()
        .clone();
    source
        .respond_occurrence(
            "b",
            start,
            invited.invitation_generation,
            invited.response_revision,
            Response::Accepted,
        )
        .unwrap();
    assert_eq!(
        source.occurrence_response("b", Some(start)),
        Some(Response::Accepted)
    );
    assert_eq!(
        source.occurrence_response("b", Some(start + Duration::days(1))),
        None
    );
    source.set_attendees(&attendees(&["a", "c"])).unwrap();
    assert_eq!(
        source.occurrence_response("b", Some(start)),
        Some(Response::Accepted),
        "a whole-series attendee edit must retain a scoped attendee RSVP"
    );
    let mut shorter = source.recurrence.clone().unwrap();
    shorter.rule = "FREQ=DAILY;COUNT=1".into();
    source
        .set_schedule(
            source.time.clone(),
            Some(shorter),
            source.exceptions.clone(),
        )
        .unwrap();
    assert!(source.occurrence_attendees.is_empty());
    assert!(source.occurrence_responses.is_empty());
}

#[test]
fn split_following_preserves_original_identities_and_links_both_series() {
    let mut source = event();
    source
        .set_schedule(
            source.time.clone(),
            Some(recurrence::Recurrence {
                rule: "FREQ=DAILY;COUNT=4".into(),
                rdates: vec![],
                exdates: vec![],
                timezone: None,
            }),
            vec![],
        )
        .unwrap();
    let source_id = ItemId::new();
    let target_id = ItemId::new();
    let boundary = instant("2026-09-05T12:00:00Z");
    let (prefix, tail) = source
        .split_following(source_id.clone(), target_id.clone(), boundary)
        .unwrap();
    assert_eq!(prefix.canceled_from, Some(boundary));
    assert_eq!(prefix.split_to.unwrap().event_item_id, target_id);
    assert_eq!(tail.split_from.as_ref().unwrap().event_item_id, source_id);
    assert!(tail.occurrence_at(boundary).unwrap().is_some());
    assert!(
        tail.occurrence_at(instant("2026-09-04T12:00:00Z"))
            .unwrap()
            .is_none()
    );
    assert!(
        tail.occurrence_at(instant("2026-09-06T12:00:00Z"))
            .unwrap()
            .is_some()
    );
}
