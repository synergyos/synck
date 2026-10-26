//! Closed, replayable native event vocabulary. This validates history shape and
//! domain transitions, not repository membership, case access, or ACL authority.

use super::*;
use synck_vcs::types::{EventContent, EventProposal};

pub fn is_native_kind(kind: &str) -> bool {
    matches!(kind, "CALENDAR" | "CALENDAR_EVENT")
}

pub fn is_native_event(kind: &str) -> bool {
    kind.starts_with("CALENDAR::") || kind.starts_with("CALENDAR_EVENT::")
}

pub fn internal_name(kind: &str, item_id: &ItemId) -> String {
    format!("{}-{}", kind.to_ascii_lowercase(), item_id.as_str())
}

/// No native event may mutate topology, generic lifecycle, or federation.
pub fn validate_envelope(event: &EventProposal) -> Result<(), CalendarError> {
    let (kind, verb) = event
        .event_type
        .split_once("::")
        .filter(|(kind, _)| is_native_kind(kind))
        .ok_or(CalendarError::Invalid("invalid native calendar event type"))?;
    let item_id = event.item_id.as_ref().ok_or(CalendarError::Invalid(
        "native event requires explicit item identity",
    ))?;
    if event.set_parent.is_some()
        || event.set_name.is_some()
        || event.set_in_trash.is_some()
        || event.set_tombstoned.is_some()
        || event.set_is_federated.is_some()
        || event.set_federation_stream_id.is_some()
    {
        return Err(CalendarError::Invalid(
            "native items forbid structural modifiers",
        ));
    }
    match verb {
        "CREATE"
        | "DELEGATE_CREATE"
        | "IMPORT_INVITATION"
        | "IMPORT_ICS_EVENT"
        | "SPLIT_FOLLOWING_TARGET"
            if event.base_seq.is_none() =>
        {
            let create = event
                .create_item
                .as_ref()
                .ok_or(CalendarError::Invalid("native create requires create_item"))?;
            if create.item_type != kind
                || create.parent_folder_id.is_some()
                || create.name != internal_name(kind, item_id)
                || event.base_seq.is_some()
            {
                return Err(CalendarError::Invalid(
                    "invalid native item identity or parent",
                ));
            }
            if event.set_metadata.is_none() || event.set_content.is_some() {
                return Err(CalendarError::Invalid(
                    "native create requires metadata only",
                ));
            }
        }
        "SET_METADATA"
        | "IMPORT_INVITATION"
        | "IMPORT_ICS_EVENT"
        | "CONFIGURE_EXTERNAL"
        | "EXTERNAL_RESPONSE"
        | "REVIEW_EXTERNAL"
        | "ATTENDEE_RESPONSE"
        | "DELEGATE_RESPONSE"
        | "CANCEL_OCCURRENCE"
        | "CANCEL_FOLLOWING"
        | "ORGANIZER_DEPARTURE"
        | "SET_EVENT_TYPE"
        | "SPLIT_FOLLOWING_SOURCE"
        | "RESCHEDULE_OCCURRENCE"
        | "EDIT_OCCURRENCE" => {
            if verb != "SET_METADATA" && kind != "CALENDAR_EVENT" {
                return Err(CalendarError::Invalid("invalid calendar mutation"));
            }
            if event.create_item.is_some()
                || event.set_metadata.is_none()
                || event.set_content.is_some()
            {
                return Err(CalendarError::Invalid(
                    "native mutation requires metadata only",
                ));
            }
        }
        "SET_OCCURRENCE_DESCRIPTION" if kind == "CALENDAR_EVENT" => {
            if event.create_item.is_some()
                || event.base_seq.is_none()
                || !matches!(
                    event.set_content,
                    Some(EventContent::Json { .. } | EventContent::Content { .. })
                )
            {
                return Err(CalendarError::Invalid(
                    "occurrence description requires scope and body content",
                ));
            }
            let _: SetOccurrenceDescription = metadata(event)?;
        }
        "SET_DESCRIPTION" if kind == "CALENDAR_EVENT" => {
            if event.create_item.is_some()
                || event.set_metadata.is_some()
                || !matches!(
                    event.set_content,
                    Some(EventContent::Json { .. } | EventContent::Content { .. })
                )
            {
                return Err(CalendarError::Invalid(
                    "description requires body content only",
                ));
            }
        }
        _ => {
            return Err(CalendarError::Invalid(
                "unsupported native calendar mutation",
            ));
        }
    }
    Ok(())
}

fn metadata<T: serde::de::DeserializeOwned>(event: &EventProposal) -> Result<T, CalendarError> {
    serde_json::from_value(
        event
            .set_metadata
            .clone()
            .ok_or(CalendarError::Invalid("native metadata is missing"))?,
    )
    .map_err(|_| CalendarError::Invalid("invalid native metadata shape"))
}

/// Stored represented-actor evidence. Live calendar grants are checked by the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub represented_user_id: String,
    pub grant_version: i64,
    pub data: CalendarEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Respond {
    pub represented_user_id: String,
    pub grant_version: i64,
    pub response: AttendeeResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OrganizerDeparture {
    pub departing_user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_user_id: Option<String>,
}

/// Validate the stored proof shape; this does not resolve or grant permissions.
pub fn validate_represented_actor(
    actual: &str,
    represented: &str,
    version: i64,
) -> Result<(), CalendarError> {
    if actual == represented || represented.is_empty() || represented.len() > 200 || version <= 0 {
        return Err(CalendarError::Invalid(
            "invalid represented calendar authority",
        ));
    }
    Ok(())
}

pub fn reduce_calendar(
    previous: Option<&Calendar>,
    event: &EventProposal,
) -> Result<Calendar, CalendarError> {
    validate_envelope(event)?;
    let next: Calendar = metadata(event)?;
    next.validate()?;
    match (previous, event.event_type.as_str()) {
        (None, "CALENDAR::CREATE") if !next.archived => Ok(next),
        (Some(_), "CALENDAR::SET_METADATA") => Ok(next),
        _ => Err(CalendarError::Invalid(
            "invalid calendar history transition",
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetOccurrenceDescription {
    pub original_start: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RescheduleOccurrence {
    pub original_start: DateTime<Utc>,
    pub time: EventTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelOccurrence {
    pub original_start: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitFollowingSource {
    pub original_start: DateTime<Utc>,
    pub target_item_id: ItemId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttendeeResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence_start: Option<DateTime<Utc>>,
    pub invitation_generation: u64,
    pub expected_response_revision: u64,
    pub response: Response,
}

/// Reconstruct editor changes from user-editable fields. The resulting comparison
/// protects organizer/owner identity, monotonic counters, and other users' RSVP.
fn edited_state(
    previous: &CalendarEvent,
    requested: &CalendarEvent,
) -> Result<CalendarEvent, CalendarError> {
    let mut next = previous.clone();
    if requested.state == EventState::Canceled {
        next.cancel()?;
        return Ok(next);
    }
    next.require_scheduled()?;
    next.title = requested.title.clone();
    next.event_type_id = requested.event_type_id;
    next.case_item_id = requested.case_item_id.clone();
    next.attachment_item_ids = requested.attachment_item_ids.clone();
    next.location = requested.location.clone();
    next.meeting_url = requested.meeting_url.clone();
    let offsets: Vec<_> = requested
        .reminders
        .entries
        .iter()
        .map(|r| r.minutes_before_start)
        .collect();
    next.reminders.replace(&offsets)?;
    // Validate the final pair, so clearing reminders and changing to a skipped
    // local date in the same command does not validate already-removed alarms.
    next.reminders.validate(&requested.time)?;
    next.set_schedule(
        requested.time.clone(),
        requested.recurrence.clone(),
        requested.exceptions.clone(),
    )?;
    let attendees: Vec<_> = requested
        .attendees
        .iter()
        .map(|a| AttendeeInput {
            user_id: a.user_id.clone(),
            role: a.role,
        })
        .collect();
    let participants = requested
        .attendees
        .iter()
        .filter(|a| a.admission == InvitationAdmission::EventParticipation)
        .map(|a| a.user_id.clone())
        .collect();
    next.set_attendees_with_participant_access(&attendees, &participants)?;
    for requested in &requested.attendees {
        if requested.admission == InvitationAdmission::RemoteParticipation {
            if previous.attendees.iter().any(|a| {
                a.user_id == requested.user_id
                    && a.admission != InvitationAdmission::RemoteParticipation
            }) {
                return Err(CalendarError::Conflict(
                    "invitation admission cannot change in place",
                ));
            }
            next.attendees
                .iter_mut()
                .find(|a| a.user_id == requested.user_id)
                .unwrap()
                .admission = InvitationAdmission::RemoteParticipation;
        }
    }
    next.validate()?;
    Ok(next)
}

fn require_local_organizer(event: &CalendarEvent) -> Result<(), CalendarError> {
    if event.is_externally_organized_repository_event() {
        return Err(CalendarError::Invalid(
            "external meeting requires its scheduling command",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventTypeChange {
    pub from_type_id: uuid::Uuid,
    pub event_type_id: Option<uuid::Uuid>,
}

pub fn reduce_event(
    previous: Option<&CalendarEvent>,
    event: &EventProposal,
    actor: &str,
) -> Result<CalendarEvent, CalendarError> {
    validate_envelope(event)?;
    if let Some(external) = previous.and_then(|old| old.external.as_ref()) {
        let allowed = match external {
            crate::external_scheduling::ExternalScheduling::ExternalOrganizer { .. } => matches!(
                event.event_type.as_str(),
                "CALENDAR_EVENT::IMPORT_INVITATION"
                    | "CALENDAR_EVENT::ATTENDEE_RESPONSE"
                    | "CALENDAR_EVENT::REVIEW_EXTERNAL"
                    | "CALENDAR_EVENT::SET_EVENT_TYPE"
                    | "CALENDAR_EVENT::SET_METADATA"
                    | "CALENDAR_EVENT::SET_DESCRIPTION"
                    | "CALENDAR_EVENT::SET_OCCURRENCE_DESCRIPTION"
            ),
            crate::external_scheduling::ExternalScheduling::AttendeeCopy { .. } => matches!(
                event.event_type.as_str(),
                "CALENDAR_EVENT::IMPORT_INVITATION"
                    | "CALENDAR_EVENT::ATTENDEE_RESPONSE"
                    | "CALENDAR_EVENT::EXTERNAL_RESPONSE"
                    | "CALENDAR_EVENT::REVIEW_EXTERNAL"
                    | "CALENDAR_EVENT::SET_EVENT_TYPE"
                    | "CALENDAR_EVENT::SET_METADATA"
                    | "CALENDAR_EVENT::SET_DESCRIPTION"
                    | "CALENDAR_EVENT::SET_OCCURRENCE_DESCRIPTION"
            ),
            _ => true,
        };
        if !allowed {
            return Err(CalendarError::Invalid(
                "external meeting requires its scheduling command",
            ));
        }
    }
    match (previous, event.event_type.as_str()) {
        (None, "CALENDAR_EVENT::DELEGATE_CREATE") => {
            let proof: Create = metadata(event)?;
            validate_represented_actor(actor, &proof.represented_user_id, proof.grant_version)?;
            if proof.data.organizer_user_id != proof.represented_user_id {
                return Err(CalendarError::Invalid("delegated organizer mismatch"));
            }
            let mut ordinary = event.clone();
            ordinary.event_type = "CALENDAR_EVENT::CREATE".into();
            ordinary.set_metadata = Some(
                serde_json::to_value(proof.data)
                    .map_err(|_| CalendarError::Invalid("invalid delegated metadata"))?,
            );
            reduce_event(None, &ordinary, &proof.represented_user_id)
        }
        (Some(previous), "CALENDAR_EVENT::DELEGATE_RESPONSE") => {
            let proof: Respond = metadata(event)?;
            validate_represented_actor(actor, &proof.represented_user_id, proof.grant_version)?;
            let mut ordinary = event.clone();
            ordinary.event_type = "CALENDAR_EVENT::ATTENDEE_RESPONSE".into();
            ordinary.set_metadata = Some(
                serde_json::to_value(proof.response)
                    .map_err(|_| CalendarError::Invalid("invalid delegated response"))?,
            );
            reduce_event(Some(previous), &ordinary, &proof.represented_user_id)
        }
        (previous, "CALENDAR_EVENT::IMPORT_INVITATION") => {
            let requested: CalendarEvent = metadata(event)?;
            requested.validate()?;
            match &requested.external {
                Some(crate::external_scheduling::ExternalScheduling::AttendeeCopy {
                    uid,
                    organizer,
                    recipient,
                    sequence,
                    dtstamp,
                    ..
                }) => {
                    if requested.organizer_user_id != actor || !requested.attendees.is_empty() {
                        return Err(CalendarError::Invalid(
                            "imported calendar custodian mismatch",
                        ));
                    }
                    if let Some(old) = previous {
                        let Some(crate::external_scheduling::ExternalScheduling::AttendeeCopy {
                            uid: old_uid,
                            organizer: old_organizer,
                            recipient: old_recipient,
                            sequence: old_sequence,
                            dtstamp: old_stamp,
                            ..
                        }) = &old.external
                        else {
                            return Err(CalendarError::Invalid(
                                "cannot replace a native organized meeting",
                            ));
                        };
                        if old_uid != uid
                            || old_organizer != organizer
                            || old_recipient != recipient
                            || old.calendar_item_id != requested.calendar_item_id
                            || (*sequence, *dtstamp) <= (*old_sequence, *old_stamp)
                        {
                            return Err(CalendarError::Conflict(
                                "stale or mismatched external meeting",
                            ));
                        }
                        if old.reminders != requested.reminders
                            || old.event_type_id != requested.event_type_id
                        {
                            return Err(CalendarError::Invalid(
                                "import cannot replace personal reminders",
                            ));
                        }
                    }
                    Ok(requested)
                }
                Some(crate::external_scheduling::ExternalScheduling::ExternalOrganizer {
                    uid,
                    organizer,
                    sequence,
                    dtstamp,
                    participants,
                }) => {
                    if !participants
                        .iter()
                        .any(|participant| participant.user_id == actor)
                        || !requested
                            .attendees
                            .iter()
                            .any(|attendee| attendee.user_id == actor)
                    {
                        return Err(CalendarError::Invalid(
                            "imported participant source mismatch",
                        ));
                    }
                    if let Some(old) = previous {
                        let Some(
                            crate::external_scheduling::ExternalScheduling::ExternalOrganizer {
                                uid: old_uid,
                                organizer: old_organizer,
                                sequence: old_sequence,
                                dtstamp: old_stamp,
                                participants: old_participants,
                            },
                        ) = &old.external
                        else {
                            return Err(CalendarError::Invalid(
                                "cannot replace established calendar history",
                            ));
                        };
                        if old_uid != uid
                            || old_organizer != organizer
                            || old.calendar_item_id != requested.calendar_item_id
                            || (*sequence, *dtstamp) < (*old_sequence, *old_stamp)
                        {
                            return Err(CalendarError::Conflict(
                                "stale or mismatched repository external meeting",
                            ));
                        }
                        let requested_source = participants
                            .iter()
                            .find(|participant| participant.user_id == actor)
                            .ok_or(CalendarError::Invalid(
                                "imported participant source mismatch",
                            ))?;
                        if let Some(original_start) = requested_source.last_action_occurrence {
                            if requested_source.last_action.is_none() {
                                return Err(CalendarError::Invalid(
                                    "detached import command identity is missing",
                                ));
                            }
                            let old_source = old_participants
                                .iter()
                                .find(|participant| participant.user_id == actor)
                                .ok_or(CalendarError::Invalid(
                                    "detached import participant is not established",
                                ))?;
                            let order = requested_source
                                .occurrence_orders
                                .iter()
                                .find(|order| order.original_start == original_start)
                                .ok_or(CalendarError::Invalid(
                                    "detached import source order is missing",
                                ))?;
                            if (order.sequence, order.dtstamp) < (*old_sequence, *old_stamp) {
                                return Err(CalendarError::Conflict(
                                    "series order supersedes detached import",
                                ));
                            }
                            if let Ok(index) = old_source
                                .occurrence_orders
                                .binary_search_by_key(&original_start, |order| order.original_start)
                            {
                                let old_order = &old_source.occurrence_orders[index];
                                if (order.sequence, order.dtstamp)
                                    < (old_order.sequence, old_order.dtstamp)
                                    || ((order.sequence, order.dtstamp)
                                        == (old_order.sequence, old_order.dtstamp)
                                        && order.source_digest != old_order.source_digest)
                                {
                                    return Err(CalendarError::Conflict(
                                        "detached import source order is stale",
                                    ));
                                }
                            }
                            let mut expected_source = old_source.clone();
                            expected_source.last_action = requested_source.last_action;
                            expected_source.last_action_occurrence = Some(original_start);
                            match expected_source
                                .occurrence_orders
                                .binary_search_by_key(&original_start, |order| order.original_start)
                            {
                                Ok(index) => {
                                    expected_source.occurrence_orders[index] = order.clone()
                                }
                                Err(index) => expected_source
                                    .occurrence_orders
                                    .insert(index, order.clone()),
                            }
                            if expected_source != *requested_source {
                                return Err(CalendarError::Invalid(
                                    "invalid detached import source transition",
                                ));
                            }

                            let Some((time, EventState::Scheduled)) =
                                requested.occurrence_at(original_start)?
                            else {
                                return Err(CalendarError::Invalid(
                                    "invalid detached import occurrence",
                                ));
                            };
                            let details = requested.occurrence_details(Some(original_start));
                            let mut expected = old.clone();
                            expected.edit_occurrence(&occurrence_details::Edit {
                                original_start,
                                time: Some(time),
                                details: occurrence_details::Details {
                                    title: Some(details.title),
                                    location: Some(details.location),
                                    meeting_url: Some(details.meeting_url),
                                },
                                attendees: None,
                            })?;
                            let response = requested
                                .occurrence_response(actor, Some(original_start))
                                .ok_or(CalendarError::Invalid(
                                    "detached import response is missing",
                                ))?;
                            let attendee = expected
                                .occurrence_attendees(Some(original_start))
                                .iter()
                                .find(|attendee| attendee.user_id == actor)
                                .ok_or(CalendarError::Invalid(
                                    "detached import participant is missing",
                                ))?
                                .clone();
                            expected.respond_occurrence(
                                actor,
                                original_start,
                                attendee.invitation_generation,
                                attendee.response_revision,
                                response,
                            )?;
                            let Some(
                                crate::external_scheduling::ExternalScheduling::ExternalOrganizer {
                                    participants,
                                    ..
                                },
                            ) = &mut expected.external
                            else {
                                unreachable!("external meeting checked above")
                            };
                            let source = participants
                                .iter_mut()
                                .find(|participant| participant.user_id == actor)
                                .unwrap();
                            *source = requested_source.clone();
                            if expected != requested {
                                return Err(CalendarError::Invalid(
                                    "invalid detached import transition",
                                ));
                            }
                            return Ok(requested);
                        }
                        let actor_was_participant = old_participants
                            .iter()
                            .any(|participant| participant.user_id == actor);
                        let actor_identity_changed = old_participants
                            .iter()
                            .find(|participant| participant.user_id == actor)
                            .zip(
                                participants
                                    .iter()
                                    .find(|participant| participant.user_id == actor),
                            )
                            .is_some_and(|(old, new)| {
                                old.mailbox_id != new.mailbox_id || old.recipient != new.recipient
                            });
                        if participants.len()
                            != old_participants.len() + usize::from(!actor_was_participant)
                            || actor_identity_changed
                            || old_participants
                                .iter()
                                .filter(|participant| participant.user_id != actor)
                                .ne(participants
                                    .iter()
                                    .filter(|participant| participant.user_id != actor))
                            || old
                                .attendees
                                .iter()
                                .filter(|attendee| attendee.user_id != actor)
                                .ne(requested
                                    .attendees
                                    .iter()
                                    .filter(|attendee| attendee.user_id != actor))
                            || requested.attendees.len()
                                != old.attendees.len() + usize::from(!actor_was_participant)
                            || old
                                .occurrence_responses
                                .iter()
                                .filter(|response| response.user_id != actor)
                                .ne(requested
                                    .occurrence_responses
                                    .iter()
                                    .filter(|response| response.user_id != actor))
                            || old.occurrence_attendees != requested.occurrence_attendees
                        {
                            return Err(CalendarError::Invalid(
                                "import may change only the acting participant",
                            ));
                        }
                        let requested_actor = requested
                            .attendees
                            .iter()
                            .find(|attendee| attendee.user_id == actor)
                            .ok_or(CalendarError::Invalid(
                                "imported participant source mismatch",
                            ))?;
                        let mut expected_participants = old.clone();
                        expected_participants.state = EventState::Scheduled;
                        if let Some(old_actor) = old
                            .attendees
                            .iter()
                            .find(|attendee| attendee.user_id == actor)
                        {
                            expected_participants.respond(
                                actor,
                                old_actor.invitation_generation,
                                old_actor.response_revision,
                                requested_actor.response,
                            )?;
                        } else {
                            let mut inputs: Vec<_> = old
                                .attendees
                                .iter()
                                .map(|attendee| AttendeeInput {
                                    user_id: attendee.user_id.clone(),
                                    role: attendee.role,
                                })
                                .collect();
                            inputs.push(AttendeeInput {
                                user_id: actor.to_owned(),
                                role: AttendeeRole::Required,
                            });
                            expected_participants.set_attendees_with_participant_access(
                                &inputs,
                                &std::collections::BTreeSet::from([actor.to_owned()]),
                            )?;
                            let actor_state = expected_participants
                                .attendees
                                .iter()
                                .find(|attendee| attendee.user_id == actor)
                                .unwrap()
                                .clone();
                            expected_participants.respond(
                                actor,
                                actor_state.invitation_generation,
                                actor_state.response_revision,
                                requested_actor.response,
                            )?;
                        }
                        if expected_participants.attendees != requested.attendees
                            || expected_participants.invitation_counter
                                != requested.invitation_counter
                            || expected_participants.occurrence_responses
                                != requested.occurrence_responses
                        {
                            return Err(CalendarError::Invalid(
                                "invalid imported participant response",
                            ));
                        }
                        if (*sequence, *dtstamp) == (*old_sequence, *old_stamp)
                            && (old.title != requested.title
                                || old.time != requested.time
                                || old.location != requested.location
                                || old.meeting_url != requested.meeting_url
                                || old.recurrence != requested.recurrence
                                || old.exceptions != requested.exceptions
                                || old.occurrence_details != requested.occurrence_details
                                || old.occurrence_descriptions != requested.occurrence_descriptions
                                || old.state != requested.state)
                        {
                            return Err(CalendarError::Conflict(
                                "equal-sequence meeting content differs",
                            ));
                        }
                        if old.reminders != requested.reminders
                            || old.event_type_id != requested.event_type_id
                            || old.organizer_user_id != requested.organizer_user_id
                        {
                            return Err(CalendarError::Invalid(
                                "import cannot replace custodian or reminders",
                            ));
                        }
                    } else if requested.organizer_user_id != actor || participants.len() != 1 {
                        return Err(CalendarError::Invalid(
                            "invalid repository external meeting creation",
                        ));
                    }
                    Ok(requested)
                }
                _ => Err(CalendarError::Invalid(
                    "import requires external scheduling provenance",
                )),
            }
        }

        (None, "CALENDAR_EVENT::IMPORT_ICS_EVENT") => {
            let requested: CalendarEvent = metadata(event)?;
            requested.validate()?;
            if requested.organizer_user_id != actor
                || !requested.attendees.is_empty()
                || !requested.reminders.entries.is_empty()
                || !matches!(
                    requested.external,
                    Some(crate::external_scheduling::ExternalScheduling::Imported { .. })
                )
            {
                return Err(CalendarError::Invalid(
                    "invalid inert calendar archive item",
                ));
            }
            Ok(requested)
        }

        (None, "CALENDAR_EVENT::CREATE") => {
            let requested: CalendarEvent = metadata(event)?;
            let mut initial = CalendarEvent::new(
                requested.calendar_item_id.clone(),
                requested.title.clone(),
                requested.time.clone(),
                actor.to_owned(),
            )?;
            match &requested.external {
                Some(crate::external_scheduling::ExternalScheduling::LocalOrganizer {
                    uid,
                    organizer,
                    attendees,
                    ..
                }) => crate::external_scheduling::state::configure_outbound(
                    &mut initial,
                    uid.clone(),
                    organizer.clone(),
                    attendees
                        .iter()
                        .map(|attendee| attendee.address.clone())
                        .collect(),
                )
                .map_err(|_| CalendarError::Invalid("invalid initial external invitations"))?,
                Some(_) => {
                    return Err(CalendarError::Invalid(
                        "invalid initial external invitations",
                    ));
                }
                None => {}
            }
            let next = edited_state(&initial, &requested)?;
            if next != requested || next.state != EventState::Scheduled {
                return Err(CalendarError::Invalid("invalid initial event state"));
            }
            Ok(next)
        }
        (None, "CALENDAR_EVENT::SPLIT_FOLLOWING_TARGET") => {
            let requested: CalendarEvent = metadata(event)?;
            if requested.is_externally_organized_repository_event() {
                return Err(CalendarError::Invalid(
                    "external meeting requires its scheduling command",
                ));
            }
            requested.validate()?;
            let link = requested
                .split_from
                .as_ref()
                .ok_or(CalendarError::Invalid("split target lacks source linkage"))?;
            if requested.organizer_user_id != actor
                || requested.state != EventState::Scheduled
                || requested.recurrence.is_none()
                || requested.occurrence_at(link.original_start)?.is_none()
            {
                return Err(CalendarError::Invalid("invalid split target"));
            }
            Ok(requested)
        }
        (Some(previous), "CALENDAR_EVENT::SET_EVENT_TYPE") => {
            let change: EventTypeChange =
                serde_json::from_value(event.set_metadata.clone().unwrap())
                    .map_err(|_| CalendarError::Invalid("invalid event type change"))?;
            if previous.event_type_id != Some(change.from_type_id) {
                return Err(CalendarError::Conflict("event type changed"));
            }
            let mut next = previous.clone();
            next.event_type_id = change.event_type_id;
            next.validate()?;
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::SET_METADATA") => {
            let requested: CalendarEvent = metadata(event)?;
            if matches!(
                previous.external,
                Some(
                    crate::external_scheduling::ExternalScheduling::AttendeeCopy { .. }
                        | crate::external_scheduling::ExternalScheduling::ExternalOrganizer { .. }
                )
            ) {
                let mut allowed = previous.clone();
                allowed.event_type_id = requested.event_type_id;
                allowed.set_reminders(
                    &requested
                        .reminders
                        .entries
                        .iter()
                        .map(|r| r.minutes_before_start)
                        .collect::<Vec<_>>(),
                )?;
                if allowed != requested {
                    return Err(CalendarError::Invalid(
                        "external meeting details belong to its organizer",
                    ));
                }
            }
            let next = edited_state(previous, &requested)?;
            if next != requested {
                return Err(CalendarError::Invalid("invalid event metadata transition"));
            }
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::CONFIGURE_EXTERNAL") => {
            if actor != previous.organizer_user_id {
                return Err(CalendarError::Invalid(
                    "only the organizer configures external invitations",
                ));
            }
            previous.require_scheduled()?;
            let input: crate::external_scheduling::state::ConfigureExternalEvent = metadata(event)?;
            let mut next = previous.clone();
            crate::external_scheduling::state::configure_outbound(
                &mut next,
                input.uid,
                input.organizer,
                input.attendees,
            )
            .map_err(|_| CalendarError::Invalid("invalid external invitation configuration"))?;
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::EXTERNAL_RESPONSE") => {
            if matches!(
                previous.external,
                Some(crate::external_scheduling::ExternalScheduling::ExternalOrganizer { .. })
            ) {
                return Err(CalendarError::Invalid(
                    "repository external meetings use attendee responses",
                ));
            }
            let response: crate::native_calendars::Response = metadata(event)?;
            let mut next = previous.clone();
            crate::external_scheduling::state::respond_copy(&mut next, actor, response)
                .map_err(|_| CalendarError::Invalid("invalid external attendee response"))?;
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::REVIEW_EXTERNAL") => {
            let action: crate::external_scheduling::state::ReviewedExternalAction =
                metadata(event)?;
            let mut next = previous.clone();
            crate::external_scheduling::state::apply_reviewed(&mut next, actor, &action)
                .map_err(|_| CalendarError::Invalid("invalid reviewed external action"))?;
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::EDIT_OCCURRENCE") => {
            require_local_organizer(previous)?;
            let input: occurrence_details::Edit = metadata(event)?;
            let mut next = previous.clone();
            next.edit_occurrence(&input)?;
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::RESCHEDULE_OCCURRENCE") => {
            require_local_organizer(previous)?;
            let input: RescheduleOccurrence = metadata(event)?;
            let mut next = previous.clone();
            next.reschedule_occurrence(input.original_start, input.time)?;
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::CANCEL_FOLLOWING") => {
            require_local_organizer(previous)?;
            let input: CancelOccurrence = metadata(event)?;
            let mut next = previous.clone();
            next.cancel_following(input.original_start)?;
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::SPLIT_FOLLOWING_SOURCE") => {
            require_local_organizer(previous)?;
            let input: SplitFollowingSource = metadata(event)?;
            let source_id = event
                .item_id
                .clone()
                .ok_or(CalendarError::Invalid("split source identity is missing"))?;
            let (source, _) =
                previous.split_following(source_id, input.target_item_id, input.original_start)?;
            Ok(source)
        }
        (Some(previous), "CALENDAR_EVENT::CANCEL_OCCURRENCE") => {
            require_local_organizer(previous)?;
            let input: CancelOccurrence = metadata(event)?;
            let mut next = previous.clone();
            next.cancel_occurrence(input.original_start)?;
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::ORGANIZER_DEPARTURE") => {
            require_local_organizer(previous)?;
            let input: OrganizerDeparture = metadata(event)?;
            let mut next = previous.clone();
            match input.successor_user_id {
                Some(successor) => {
                    next.transfer_organizer(&input.departing_user_id, &successor)?;
                }
                None => {
                    if next.organizer_user_id != input.departing_user_id {
                        return Err(CalendarError::Invalid("organizer departure mismatch"));
                    }
                    next.cancel()?;
                }
            }
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::ATTENDEE_RESPONSE") => {
            let response: AttendeeResponse = metadata(event)?;
            let mut next = previous.clone();
            match response.occurrence_start {
                Some(start) => next.respond_occurrence(
                    actor,
                    start,
                    response.invitation_generation,
                    response.expected_response_revision,
                    response.response,
                )?,
                None => next.respond(
                    actor,
                    response.invitation_generation,
                    response.expected_response_revision,
                    response.response,
                )?,
            };
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::SET_OCCURRENCE_DESCRIPTION") => {
            let input: SetOccurrenceDescription = metadata(event)?;
            let mut next = previous.clone();
            next.set_occurrence_description(
                input.original_start,
                event.base_seq.ok_or(CalendarError::Invalid(
                    "description base version is missing",
                ))?,
            )?;
            Ok(next)
        }
        (Some(previous), "CALENDAR_EVENT::SET_DESCRIPTION") => {
            previous.require_scheduled()?;
            Ok(previous.clone())
        }
        _ => Err(CalendarError::Invalid(
            "invalid calendar event history transition",
        )),
    }
}
