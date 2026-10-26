use super::*;

fn request() -> String {
    concat!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//SynckHub//Calendar//EN\r\nMETHOD:REQUEST\r\n",
        "BEGIN:VEVENT\r\nUID:opaque-not-a-uuid@example.test\r\nDTSTAMP:20260910T100000Z\r\n",
        "ORGANIZER:mailto:Owner@EXAMPLE.test\r\nATTENDEE;CN=\"User: Example\":mailto:guest@example.test\r\n",
        "DTSTART:20260911T100000Z\r\nDTEND:20260911T110000Z\r\nSUMMARY:Hello\\, world\r\n",
        "END:VEVENT\r\nEND:VCALENDAR\r\n").to_owned()
}

#[test]
fn preserves_identity_and_roundtrips_folded_unicode() {
    let raw = request().replace(
        "SUMMARY:Hello\\, world",
        &format!("SUMMARY:{}", encode_text(&"日本語; 💬\n".repeat(80))),
    );
    let parsed = parse(raw.as_bytes()).unwrap();
    let event = &parsed.document.children[0];
    let identity = Identity::from_event(event).unwrap();
    assert_eq!(identity.organizer, "Owner@example.test");
    assert_eq!(identity.uid, "opaque-not-a-uuid@example.test");
    assert_eq!(
        event
            .properties
            .iter()
            .find(|p| p.name == "ATTENDEE")
            .unwrap()
            .params["CN"],
        "User: Example"
    );
    let output = serialize(&parsed.document).unwrap();
    for line in output.split(|b| *b == b'\n') {
        assert!(line.len() <= 76);
    }
    assert_eq!(parse(&output).unwrap(), parsed);
}

#[test]
fn rejects_duplicate_authority_and_component_smuggling() {
    for (from, to) in [
        ("UID:opaque-not-a-uuid@example.test", "UID:a\r\nUID:b"),
        (
            "ORGANIZER:mailto:Owner@EXAMPLE.test",
            "ORGANIZER:mailto:a@x.test\r\nORGANIZER:mailto:b@x.test",
        ),
        ("END:VEVENT", "END:VTODO"),
        ("METHOD:REQUEST", "METHOD:REQUEST\r\nMETHOD:CANCEL"),
        ("ATTENDEE;CN=\"User: Example\"", "ATTENDEE;CN=a;CN=b"),
        (
            "END:VCALENDAR",
            "END:VCALENDAR\r\nBEGIN:VCALENDAR\r\nEND:VCALENDAR",
        ),
    ] {
        assert!(
            parse(request().replace(from, to).as_bytes()).is_err(),
            "{to}"
        );
    }
}

#[test]
fn all_attendee_methods_require_exact_actor() {
    for method in ["REPLY", "REFRESH", "COUNTER"] {
        let source = request()
            .replace("METHOD:REQUEST", &format!("METHOD:{method}"))
            .replace("ATTENDEE;CN=", "ATTENDEE;PARTSTAT=ACCEPTED;CN=");
        assert!(parse(source.as_bytes()).is_ok());
        assert!(
            parse(
                source
                    .replace("DTSTART:", "ATTENDEE:mailto:other@example.test\r\nDTSTART:")
                    .as_bytes()
            )
            .is_err()
        );
    }
}

#[test]
fn parsing_is_bounded_and_never_slices_untrusted_utf8() {
    assert!(parse_document(&vec![b'x'; MAX_BYTES + 1]).is_err());
    assert!(parse_document(b" orphan\r\n").is_err());
    assert!(parse_document(b"BEGIN:VCALENDAR\rVERSION:2.0\n").is_err());
    assert!(mailto("日本語@example.test").is_err());
    assert!(
        parse(
            request()
                .replace("CN=\"User: Example\"", "CN=\"")
                .as_bytes()
        )
        .is_err()
    );
    assert!(
        parse(
            request()
                .replace("DTSTAMP:20260910T100000Z", "DTSTAMP:20260910T100000")
                .as_bytes()
        )
        .is_err()
    );
}

#[test]
fn unsupported_actions_are_explicit() {
    assert_eq!(
        parse(
            request()
                .replace("METHOD:REQUEST", "METHOD:PUBLISH")
                .as_bytes()
        ),
        Err(Error::Unsupported)
    );
    assert!(parse_document(request().replace("METHOD:REQUEST\r\n", "").as_bytes()).is_ok());
}

#[test]
fn reviewed_import_preserves_utc_recurrence_even_in_another_user_timezone() {
    let source = request().replace(
        "DTEND:20260911T110000Z",
        "DTEND:20260911T110000Z\r\nRRULE:FREQ=DAILY;COUNT=3",
    );
    let parsed = parse(source.as_bytes()).unwrap();
    let imported = import::reviewed_request(
        &parsed,
        synck_vcs::types::ItemId::new(),
        "owner",
        "guest@example.test",
        "Europe/Madrid",
    )
    .unwrap();
    assert_eq!(imported.event.title, "Hello, world");
    assert!(
        matches!(imported.event.time,crate::native_calendars::EventTime::Timed {ref time_zone,..} if time_zone=="UTC")
    );
    assert_eq!(
        imported.event.recurrence.unwrap().rule,
        "FREQ=DAILY;COUNT=3"
    );
    assert!(
        import::reviewed_request(
            &parsed,
            synck_vcs::types::ItemId::new(),
            "owner",
            "not-invited@example.test",
            "UTC"
        )
        .is_err()
    );
}

#[test]
fn reviewed_import_preserves_detached_time_and_description() {
    let source=request().replace("DTEND:20260911T110000Z","DTEND:20260911T110000Z\r\nRRULE:FREQ=DAILY;COUNT=3").replace("END:VCALENDAR",concat!("BEGIN:VEVENT\r\nUID:opaque-not-a-uuid@example.test\r\nDTSTAMP:20260910T100000Z\r\nORGANIZER:mailto:Owner@EXAMPLE.test\r\nRECURRENCE-ID:20260912T100000Z\r\nDTSTART:20260912T120000Z\r\nDTEND:20260912T130000Z\r\nSUMMARY:Moved occurrence\r\nDESCRIPTION:Private notes\r\nEND:VEVENT\r\nEND:VCALENDAR"));
    let parsed = parse(source.as_bytes()).unwrap();
    let imported = import::reviewed_request(
        &parsed,
        synck_vcs::types::ItemId::new(),
        "owner",
        "guest@example.test",
        "UTC",
    )
    .unwrap();
    assert_eq!(imported.event.exceptions.len(), 1);
    assert_eq!(imported.occurrence_descriptions.len(), 1);
    let scope = utc_timestamp("20260912T100000Z").unwrap();
    assert_eq!(imported.occurrence_descriptions[&scope], "Private notes");
    assert_eq!(
        imported.event.occurrence_details(Some(scope)).title,
        "Moved occurrence"
    );
}

#[test]
fn reviewed_occurrence_request_preserves_timed_and_date_identity() {
    let timed = parse(
        request()
            .replace(
                "DTSTART:20260911T100000Z",
                "RECURRENCE-ID:20260911T090000Z\r\nDTSTART:20260911T100000Z",
            )
            .as_bytes(),
    )
    .unwrap();
    let timed = import::reviewed_occurrence_request(&timed, "guest@example.test", "UTC").unwrap();
    assert_eq!(
        timed.original_start,
        utc_timestamp("20260911T090000Z").unwrap()
    );
    assert_eq!(timed.details.title.as_deref(), Some("Hello, world"));
    assert!(timed.details.location.is_none());
    assert!(timed.description.is_none());

    let all_day_source = request()
        .replace(
            "DTSTART:20260911T100000Z\r\nDTEND:20260911T110000Z",
            "RECURRENCE-ID;VALUE=DATE:20260911\r\nDTSTART;VALUE=DATE:20260912\r\nDTEND;VALUE=DATE:20260913",
        )
        .replace("SUMMARY:Hello\\, world", "DESCRIPTION:Day notes");
    let all_day = parse(all_day_source.as_bytes()).unwrap();
    let all_day =
        import::reviewed_occurrence_request(&all_day, "guest@example.test", "Europe/Madrid")
            .unwrap();
    assert_eq!(
        all_day.original_start,
        utc_timestamp("20260911T000000Z").unwrap()
    );
    assert!(matches!(
        all_day.time,
        crate::native_calendars::EventTime::AllDay {
            start_date,
            end_date,
            ref time_zone,
        } if start_date.to_string() == "2026-09-12"
            && end_date.to_string() == "2026-09-13"
            && time_zone == "Europe/Madrid"
    ));
    assert_eq!(all_day.description.as_deref(), Some("Day notes"));
}

#[test]
fn reviewed_actions_bind_identity_participant_and_method() {
    let legacy_decline_counter: state::ReviewedExternalAction =
        serde_json::from_value(serde_json::json!({
            "kind": "DECLINE_COUNTER",
            "command_id": uuid::Uuid::new_v4(),
            "address": "guest@example.test"
        }))
        .unwrap();
    assert!(matches!(
        legacy_decline_counter,
        state::ReviewedExternalAction::DeclineCounter {
            original_start: None,
            ..
        }
    ));
    let reply = parse(
        request()
            .replace("METHOD:REQUEST", "METHOD:REPLY")
            .replace(
                "ATTENDEE;CN=\"User: Example\":mailto:guest@example.test",
                "ATTENDEE;PARTSTAT=DECLINED:mailto:guest@example.test",
            )
            .as_bytes(),
    )
    .unwrap();
    assert!(matches!(
        import::reviewed_action(
            &reply,
            "opaque-not-a-uuid@example.test",
            "Owner@example.test",
            "guest@example.test",
            "UTC"
        )
        .unwrap(),
        state::ReviewedExternalAction::Reply {
            response: crate::native_calendars::Response::Declined,
            ..
        }
    ));
    assert!(
        import::reviewed_action(
            &reply,
            "wrong",
            "Owner@example.test",
            "guest@example.test",
            "UTC"
        )
        .is_err()
    );
    assert!(
        import::reviewed_action(
            &reply,
            "opaque-not-a-uuid@example.test",
            "Owner@example.test",
            "other@example.test",
            "UTC"
        )
        .is_err()
    );
    for (method, kind) in [
        ("CANCEL", 0),
        ("REFRESH", 1),
        ("COUNTER", 2),
        ("DECLINECOUNTER", 3),
    ] {
        let mut source = request().replace("METHOD:REQUEST", &format!("METHOD:{method}"));
        if method == "DECLINECOUNTER" {
            source = source.replace(
                "DTSTART:20260911T100000Z",
                "RECURRENCE-ID:20260911T090000Z\r\nDTSTART:20260911T100000Z",
            );
        }
        let message = parse(source.as_bytes()).unwrap();
        let action = import::reviewed_action(
            &message,
            "opaque-not-a-uuid@example.test",
            "Owner@example.test",
            "guest@example.test",
            "UTC",
        )
        .unwrap();
        let (actual_kind, original_start) = match action {
            state::ReviewedExternalAction::Cancel { original_start, .. } => (0, original_start),
            state::ReviewedExternalAction::Refresh { .. } => (1, None),
            state::ReviewedExternalAction::AcceptCounter { original_start, .. } => {
                (2, original_start)
            }
            state::ReviewedExternalAction::DeclineCounter { original_start, .. } => {
                (3, original_start)
            }
            _ => (9, None),
        };
        assert_eq!(actual_kind, kind);
        if method == "DECLINECOUNTER" {
            assert_eq!(
                original_start,
                Some(utc_timestamp("20260911T090000Z").unwrap())
            );
        }
    }
}

#[test]
fn repository_external_participants_are_distinct_sorted_and_address_bound() {
    use state::{ExternalParticipantSource, ExternalScheduling};
    let source = |user_id: &str, recipient: &str| ExternalParticipantSource {
        user_id: user_id.into(),
        mailbox_id: uuid::Uuid::new_v4(),
        recipient: recipient.into(),
        invitation_id: uuid::Uuid::now_v7(),
        source_digest: "ab".repeat(32),
        sequence: 4,
        dtstamp: utc_timestamp("20260910T100000Z").unwrap(),
        last_action: None,
        last_action_occurrence: None,
        occurrence_orders: Vec::new(),
    };
    let state = ExternalScheduling::ExternalOrganizer {
        uid: "meeting@example.test".into(),
        organizer: "host@example.test".into(),
        sequence: 4,
        dtstamp: utc_timestamp("20260910T100000Z").unwrap(),
        participants: vec![
            source("ana", "ana@example.test"),
            source("jaime", "jaime@example.test"),
        ],
    };
    state.validate().unwrap();
    assert_eq!(
        state.external_participant("jaime").unwrap().recipient,
        "jaime@example.test"
    );

    let mut duplicate = state.clone();
    let ExternalScheduling::ExternalOrganizer { participants, .. } = &mut duplicate else {
        unreachable!()
    };
    participants[1].user_id = "ana".into();
    assert!(duplicate.validate().is_err());

    let mut invalid = state;
    let ExternalScheduling::ExternalOrganizer { participants, .. } = &mut invalid else {
        unreachable!()
    };
    participants[1].recipient = "jaime@EXAMPLE.test".into();
    assert!(invalid.validate().is_err());
}

#[test]
fn generated_calendar_serializer_enforces_caller_limits() {
    let event = Component {
        name: "VEVENT".into(),
        properties: vec![Property {
            name: "UID".into(),
            params: Default::default(),
            value: "one@example.test".into(),
        }],
        children: Vec::new(),
    };
    let document = Component {
        name: "VCALENDAR".into(),
        properties: vec![Property {
            name: "VERSION".into(),
            params: Default::default(),
            value: "2.0".into(),
        }],
        children: vec![event],
    };
    assert!(serialize_with_limits(&document, 4096, 2, 8).is_ok());
    assert_eq!(
        serialize_with_limits(&document, 4096, 1, 8),
        Err(Error::Invalid("too many components"))
    );
    assert_eq!(
        serialize_with_limits(&document, 4096, 2, 5),
        Err(Error::Invalid("calendar exceeds property limit"))
    );
    assert_eq!(
        serialize_with_limits(&document, 8, 2, 8),
        Err(Error::Invalid("calendar exceeds byte limit"))
    );
}
