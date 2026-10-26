//! Native state distinguishes organizer authority from a private attendee copy.
use super::{Error, Property, Result, mailto};
use crate::native_calendars::{CalendarEvent, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MAX_EXTERNAL_ATTENDEES: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum ExternalScheduling {
    LocalOrganizer {
        uid: String,
        organizer: String,
        attendees: Vec<ExternalAttendee>,
        #[serde(default)]
        occurrence_responses: Vec<ExternalOccurrenceResponse>,
        #[serde(default)]
        delivery: Option<ExternalDelivery>,
    },
    AttendeeCopy {
        mailbox_id: uuid::Uuid,
        invitation_id: uuid::Uuid,
        source_digest: String,
        response: Response,
        uid: String,
        organizer: String,
        recipient: String,
        sequence: u32,
        dtstamp: DateTime<Utc>,
        recurrence_id: Option<Property>,
        #[serde(default)]
        last_action: Option<uuid::Uuid>,
    },
    /// One repository event for an externally organized meeting. Mailbox
    /// evidence remains recipient-private; this list retains only the bounded
    /// proof needed to produce that participant's replies and apply later iTIP.
    ExternalOrganizer {
        uid: String,
        organizer: String,
        sequence: u32,
        dtstamp: DateTime<Utc>,
        participants: Vec<ExternalParticipantSource>,
    },
    Imported {
        uid: String,
        organizer: String,
        attendees: Vec<ExternalAttendee>,
        #[serde(default)]
        occurrence_responses: Vec<ExternalOccurrenceResponse>,
        canceled: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalParticipantSource {
    pub user_id: String,
    pub mailbox_id: uuid::Uuid,
    pub recipient: String,
    pub invitation_id: uuid::Uuid,
    pub source_digest: String,
    pub sequence: u32,
    pub dtstamp: DateTime<Utc>,
    #[serde(default)]
    pub last_action: Option<uuid::Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action_occurrence: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub occurrence_orders: Vec<ExternalOccurrenceOrder>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalOccurrenceOrder {
    pub original_start: DateTime<Utc>,
    pub invitation_id: uuid::Uuid,
    pub source_digest: String,
    pub sequence: u32,
    pub dtstamp: DateTime<Utc>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalAttendee {
    pub address: String,
    pub response: Response,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalOccurrenceResponse {
    pub original_start: DateTime<Utc>,
    pub address: String,
    pub response: Response,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalDelivery {
    pub command_id: uuid::Uuid,
    pub recipient: String,
    pub method: super::Method,
    pub original_start: Option<DateTime<Utc>>,
}
impl ExternalScheduling {
    pub fn validate(&self) -> Result<()> {
        let (uid, organizer) = match self {
            Self::LocalOrganizer {
                uid,
                organizer,
                attendees,
                occurrence_responses,
                delivery,
            } => {
                if attendees.len() > MAX_EXTERNAL_ATTENDEES {
                    return Err(Error::Invalid("too many external attendees"));
                }
                for attendee in attendees {
                    canonical_address(&attendee.address)?;
                }
                if attendees.windows(2).any(|a| a[0].address >= a[1].address) {
                    return Err(Error::Invalid(
                        "external attendees must be distinct and sorted",
                    ));
                }
                if occurrence_responses.len() > 1024
                    || occurrence_responses.windows(2).any(|pair| {
                        (&pair[0].original_start, &pair[0].address)
                            >= (&pair[1].original_start, &pair[1].address)
                    })
                {
                    return Err(Error::Invalid("invalid external occurrence responses"));
                }
                for response in occurrence_responses {
                    canonical_address(&response.address)?;
                    if !attendees.iter().any(|a| a.address == response.address)
                        || response.response == Response::NeedsAction
                    {
                        return Err(Error::Invalid("invalid external occurrence response"));
                    }
                }
                if let Some(delivery) = delivery {
                    canonical_address(&delivery.recipient)?;
                    if delivery.command_id.is_nil()
                        || !attendees.iter().any(|a| a.address == delivery.recipient)
                        || !matches!(
                            delivery.method,
                            super::Method::Request | super::Method::Declinecounter
                        )
                    {
                        return Err(Error::Invalid("invalid external delivery request"));
                    }
                }
                (uid, organizer)
            }
            Self::AttendeeCopy {
                uid,
                organizer,
                recipient,
                sequence,
                dtstamp: _,
                recurrence_id,
                mailbox_id,
                invitation_id,
                source_digest,
                response: _,
                last_action,
            } => {
                if mailbox_id.is_nil()
                    || invitation_id.get_version_num() != 7
                    || source_digest.len() != 64
                    || !source_digest.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err(Error::Invalid("invalid invitation source proof"));
                }
                canonical_address(recipient)?;
                if *sequence > i32::MAX as u32 {
                    return Err(Error::Invalid("invalid sequence"));
                }
                if recurrence_id
                    .as_ref()
                    .is_some_and(|p| p.name != "RECURRENCE-ID")
                {
                    return Err(Error::Invalid("invalid recurrence identity"));
                }
                if last_action.is_some_and(|id| id.is_nil()) {
                    return Err(Error::Invalid("invalid reviewed action identity"));
                }
                (uid, organizer)
            }
            Self::ExternalOrganizer {
                uid,
                organizer,
                sequence,
                participants,
                ..
            } => {
                if *sequence > i32::MAX as u32
                    || participants.is_empty()
                    || participants.len() > 500
                {
                    return Err(Error::Invalid("invalid repository external meeting"));
                }
                if participants
                    .windows(2)
                    .any(|pair| pair[0].user_id >= pair[1].user_id)
                {
                    return Err(Error::Invalid(
                        "external participants must be distinct and sorted",
                    ));
                }
                for participant in participants {
                    crate::native_calendars::validate_user_id(&participant.user_id)
                        .map_err(|_| Error::Invalid("invalid external participant"))?;
                    canonical_address(&participant.recipient)?;
                    if participant.mailbox_id.is_nil()
                        || participant.invitation_id.get_version_num() != 7
                        || participant.sequence > i32::MAX as u32
                        || participant.source_digest.len() != 64
                        || !participant
                            .source_digest
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit())
                        || participant.last_action.is_some_and(|id| id.is_nil())
                    {
                        return Err(Error::Invalid("invalid external participant source proof"));
                    }
                    if participant.occurrence_orders.len() > 512
                        || participant
                            .occurrence_orders
                            .windows(2)
                            .any(|pair| pair[0].original_start >= pair[1].original_start)
                        || participant.occurrence_orders.iter().any(|order| {
                            order.invitation_id.get_version_num() != 7
                                || order.source_digest.len() != 64
                                || !order
                                    .source_digest
                                    .bytes()
                                    .all(|byte| byte.is_ascii_hexdigit())
                                || order.sequence > i32::MAX as u32
                        })
                    {
                        return Err(Error::Invalid("invalid external occurrence source proof"));
                    }
                }
                (uid, organizer)
            }
            Self::Imported {
                uid,
                organizer,
                attendees,
                occurrence_responses,
                ..
            } => {
                if attendees.len() > 500 {
                    return Err(Error::Invalid("too many imported attendees"));
                }
                for attendee in attendees {
                    canonical_address(&attendee.address)?;
                }
                if attendees.windows(2).any(|p| p[0].address >= p[1].address) {
                    return Err(Error::Invalid(
                        "imported attendees must be distinct and sorted",
                    ));
                }
                if occurrence_responses.len() > 4096
                    || occurrence_responses.windows(2).any(|pair| {
                        (&pair[0].original_start, &pair[0].address)
                            >= (&pair[1].original_start, &pair[1].address)
                    })
                {
                    return Err(Error::Invalid("invalid imported occurrence responses"));
                }
                for response in occurrence_responses {
                    canonical_address(&response.address)?;
                }
                (uid, organizer)
            }
        };
        canonical_address(organizer)?;
        if uid.is_empty() || uid.len() > 1024 || uid.chars().any(char::is_control) {
            return Err(Error::Invalid("invalid UID"));
        }
        Ok(())
    }
}
fn canonical_address(address: &str) -> Result<()> {
    if mailto(&format!("mailto:{address}"))? != address {
        return Err(Error::Invalid("calendar address domain must be canonical"));
    }
    Ok(())
}

/// Invitation identity is allocated by the accepted server command, not derived
/// from an incoming external UID. Reconfiguration preserves current responses.
pub fn configure_outbound(
    event: &mut CalendarEvent,
    uid: String,
    organizer: String,
    mut addresses: Vec<String>,
) -> Result<()> {
    addresses.sort();
    if addresses.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(Error::Invalid("duplicate external attendee"));
    }
    let (previous, occurrence_responses) = match &event.external {
        Some(ExternalScheduling::LocalOrganizer {
            uid: old_uid,
            organizer: old_organizer,
            attendees,
            occurrence_responses,
            ..
        }) => {
            if *old_uid != uid || *old_organizer != organizer {
                return Err(Error::Invalid("external organizer identity is immutable"));
            }
            (Some(attendees), occurrence_responses.clone())
        }
        Some(ExternalScheduling::AttendeeCopy { .. }) => {
            return Err(Error::Invalid(
                "attendee copies cannot organize invitations",
            ));
        }
        Some(ExternalScheduling::ExternalOrganizer { .. }) => {
            return Err(Error::Invalid(
                "externally organized meetings cannot organize invitations",
            ));
        }
        Some(ExternalScheduling::Imported {
            uid: old_uid,
            organizer: old_organizer,
            attendees,
            occurrence_responses,
            canceled,
        }) => {
            if *canceled {
                return Err(Error::Invalid("canceled import cannot send invitations"));
            }
            if *old_uid != uid || *old_organizer != organizer {
                return Err(Error::Invalid("imported organizer identity is immutable"));
            }
            (Some(attendees), occurrence_responses.clone())
        }
        None => (None, Vec::new()),
    };
    let attendees: Vec<ExternalAttendee> = addresses
        .into_iter()
        .map(|address| {
            let response = previous
                .and_then(|prior| prior.iter().find(|a| a.address == address))
                .map(|a| a.response)
                .unwrap_or(Response::NeedsAction);
            ExternalAttendee { address, response }
        })
        .collect();
    let occurrence_responses = occurrence_responses
        .into_iter()
        .filter(|response| {
            attendees
                .iter()
                .any(|attendee| attendee.address == response.address)
        })
        .collect();
    let state = ExternalScheduling::LocalOrganizer {
        uid,
        organizer,
        attendees,
        occurrence_responses,
        delivery: None,
    };
    state.validate()?;
    event.external = Some(state);
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureExternalEvent {
    pub uid: String,
    pub organizer: String,
    pub attendees: Vec<String>,
}

impl ExternalScheduling {
    pub fn is_local_organizer(&self) -> bool {
        matches!(self, Self::LocalOrganizer { .. } | Self::Imported { .. })
    }

    pub fn external_participant(&self, user_id: &str) -> Option<&ExternalParticipantSource> {
        match self {
            Self::ExternalOrganizer { participants, .. } => participants
                .binary_search_by(|participant| participant.user_id.as_str().cmp(user_id))
                .ok()
                .map(|index| &participants[index]),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExternalReviewDecision {
    Apply,
    Accept,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum ReviewedExternalAction {
    Cancel {
        original_start: Option<DateTime<Utc>>,
        #[serde(default)]
        sequence: Option<u32>,
        #[serde(default)]
        dtstamp: Option<DateTime<Utc>>,
        #[serde(default)]
        command_id: Option<uuid::Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        occurrence_order: Option<ExternalOccurrenceOrder>,
    },
    Reply {
        address: String,
        response: Response,
        original_start: Option<DateTime<Utc>>,
    },
    Refresh {
        command_id: uuid::Uuid,
        address: String,
    },
    AcceptCounter {
        address: String,
        time: crate::native_calendars::EventTime,
        original_start: Option<DateTime<Utc>>,
    },
    RejectCounter {
        command_id: uuid::Uuid,
        address: String,
        time: crate::native_calendars::EventTime,
        original_start: Option<DateTime<Utc>>,
    },
    DeclineCounter {
        command_id: uuid::Uuid,
        address: String,
        #[serde(default)]
        original_start: Option<DateTime<Utc>>,
    },
}

pub fn apply_reviewed(
    event: &mut CalendarEvent,
    actor: &str,
    action: &ReviewedExternalAction,
) -> Result<()> {
    match action {
        ReviewedExternalAction::Cancel {
            original_start,
            sequence,
            dtstamp,
            command_id,
            occurrence_order,
        } => {
            let authorized = match &event.external {
                Some(ExternalScheduling::AttendeeCopy { .. }) => actor == event.organizer_user_id,
                Some(ExternalScheduling::ExternalOrganizer { participants, .. }) => participants
                    .iter()
                    .any(|participant| participant.user_id == actor),
                _ => false,
            };
            if !authorized {
                return Err(Error::Invalid("invalid external cancellation"));
            }
            let already_canceled = if let Some(start) = original_start {
                event
                    .exceptions
                    .binary_search_by_key(start, |exception| exception.original_start)
                    .is_ok_and(|index| event.exceptions[index].is_canceled())
            } else {
                event.state == crate::native_calendars::EventState::Canceled
            };
            let repeated_repository_cancel = match &event.external {
                Some(ExternalScheduling::ExternalOrganizer {
                    sequence: current_sequence,
                    dtstamp: current_stamp,
                    ..
                }) => {
                    already_canceled
                        && (original_start.is_none() || occurrence_order.is_none())
                        && sequence.zip(*dtstamp) == Some((*current_sequence, *current_stamp))
                }
                _ => false,
            };
            if repeated_repository_cancel || (already_canceled && occurrence_order.is_some()) {
                return Ok(());
            }
            if let Some(start) = original_start {
                event
                    .cancel_occurrence(*start)
                    .map_err(|_| Error::Invalid("unknown canceled occurrence"))?;
            } else {
                event
                    .cancel()
                    .map_err(|_| Error::Invalid("invalid external cancellation"))?;
            }
            if let Some(ExternalScheduling::ExternalOrganizer {
                sequence: current_sequence,
                dtstamp: current_stamp,
                participants,
                ..
            }) = &mut event.external
            {
                let sequence = sequence.ok_or(Error::Invalid("missing cancellation sequence"))?;
                let dtstamp = dtstamp.ok_or(Error::Invalid("missing cancellation timestamp"))?;
                let command_id = command_id
                    .filter(|command_id| !command_id.is_nil())
                    .ok_or(Error::Invalid("missing cancellation identity"))?;
                let source = participants
                    .iter_mut()
                    .find(|participant| participant.user_id == actor)
                    .ok_or(Error::Invalid("external participant is not established"))?;
                source.last_action = Some(command_id);
                if let Some(start) = original_start {
                    if let Some(order) = occurrence_order.clone() {
                        if order.original_start != *start
                            || order.sequence != sequence
                            || order.dtstamp != dtstamp
                        {
                            return Err(Error::Invalid("invalid cancellation occurrence order"));
                        }
                        source.last_action_occurrence = Some(*start);
                        match source
                            .occurrence_orders
                            .binary_search_by_key(start, |order| order.original_start)
                        {
                            Ok(index) => source.occurrence_orders[index] = order,
                            Err(index) => source.occurrence_orders.insert(index, order),
                        }
                    } else {
                        // Historical REVIEW_EXTERNAL actions predate scoped source
                        // proof and must retain their exact replay semantics.
                        if (sequence, dtstamp) < (*current_sequence, *current_stamp) {
                            return Err(Error::Invalid("stale external cancellation"));
                        }
                        *current_sequence = sequence;
                        *current_stamp = dtstamp;
                        source.sequence = sequence;
                        source.dtstamp = dtstamp;
                    }
                } else {
                    if (sequence, dtstamp) < (*current_sequence, *current_stamp) {
                        return Err(Error::Invalid("stale external cancellation"));
                    }
                    *current_sequence = sequence;
                    *current_stamp = dtstamp;
                    source.sequence = sequence;
                    source.dtstamp = dtstamp;
                    source.last_action_occurrence = None;
                }
            }
        }
        ReviewedExternalAction::Reply {
            address,
            response,
            original_start,
        } => {
            if let Some(start) = original_start {
                event
                    .occurrence_at(*start)
                    .map_err(|_| Error::Invalid("unknown reply occurrence"))?
                    .ok_or(Error::Invalid("unknown reply occurrence"))?;
            }
            let Some(ExternalScheduling::LocalOrganizer {
                attendees,
                occurrence_responses,
                ..
            }) = &mut event.external
            else {
                return Err(Error::Invalid("external organizer required"));
            };
            if actor != event.organizer_user_id || *response == Response::NeedsAction {
                return Err(Error::Invalid("invalid external reply"));
            }
            let attendee = attendees
                .iter_mut()
                .find(|a| a.address == *address)
                .ok_or(Error::Invalid("replying attendee is not established"))?;
            if let Some(start) = original_start {
                occurrence_responses
                    .retain(|r| !(r.original_start == *start && r.address == *address));
                occurrence_responses.push(ExternalOccurrenceResponse {
                    original_start: *start,
                    address: address.clone(),
                    response: *response,
                });
                occurrence_responses.sort_by(|a, b| {
                    (&a.original_start, &a.address).cmp(&(&b.original_start, &b.address))
                });
            } else {
                attendee.response = *response;
            }
        }
        ReviewedExternalAction::Refresh {
            command_id,
            address,
        } => set_delivery(
            event,
            actor,
            *command_id,
            address,
            super::Method::Request,
            None,
        )?,
        ReviewedExternalAction::RejectCounter {
            command_id,
            address,
            time,
            original_start,
        } => {
            require_organizer_attendee(event, actor, address)?;
            time.validate()
                .map_err(|_| Error::Invalid("invalid counter time"))?;
            if let Some(start) = original_start {
                event
                    .occurrence_at(*start)
                    .map_err(|_| Error::Invalid("unknown counter occurrence"))?
                    .ok_or(Error::Invalid("unknown counter occurrence"))?;
            }
            set_delivery(
                event,
                actor,
                *command_id,
                address,
                super::Method::Declinecounter,
                *original_start,
            )?;
        }
        ReviewedExternalAction::AcceptCounter {
            address,
            time,
            original_start,
        } => {
            require_organizer_attendee(event, actor, address)?;
            if let Some(start) = original_start {
                event
                    .reschedule_occurrence(*start, time.clone())
                    .map_err(|_| Error::Invalid("invalid counter occurrence"))?;
            } else {
                event
                    .set_time(time.clone())
                    .map_err(|_| Error::Invalid("invalid counter time"))?;
            }
        }
        ReviewedExternalAction::DeclineCounter {
            command_id,
            address,
            original_start: _,
        } => {
            let Some(ExternalScheduling::AttendeeCopy {
                recipient,
                last_action,
                ..
            }) = &mut event.external
            else {
                return Err(Error::Invalid("external attendee copy required"));
            };
            if actor != event.organizer_user_id || recipient != address {
                return Err(Error::Invalid("decline-counter targets another attendee"));
            }
            *last_action = Some(*command_id);
        }
    }
    event
        .validate()
        .map_err(|_| Error::Invalid("invalid reviewed external action"))
}

fn require_organizer_attendee(event: &CalendarEvent, actor: &str, address: &str) -> Result<()> {
    let Some(ExternalScheduling::LocalOrganizer { attendees, .. }) = &event.external else {
        return Err(Error::Invalid("external organizer required"));
    };
    if actor != event.organizer_user_id || !attendees.iter().any(|a| a.address == address) {
        return Err(Error::Invalid("external attendee is not established"));
    }
    Ok(())
}
fn set_delivery(
    event: &mut CalendarEvent,
    actor: &str,
    command_id: uuid::Uuid,
    address: &str,
    method: super::Method,
    original_start: Option<DateTime<Utc>>,
) -> Result<()> {
    require_organizer_attendee(event, actor, address)?;
    let Some(ExternalScheduling::LocalOrganizer { delivery, .. }) = &mut event.external else {
        unreachable!()
    };
    *delivery = Some(ExternalDelivery {
        command_id,
        recipient: address.into(),
        method,
        original_start,
    });
    Ok(())
}

pub fn respond_copy(event: &mut CalendarEvent, actor: &str, response: Response) -> Result<()> {
    if response == Response::NeedsAction {
        return Err(Error::Invalid("invalid external attendee response"));
    }
    if actor != event.organizer_user_id {
        return Err(Error::Invalid("invalid external attendee response"));
    }
    let Some(ExternalScheduling::AttendeeCopy {
        response: current, ..
    }) = &mut event.external
    else {
        return Err(Error::Invalid("external attendee copy required"));
    };
    *current = response;
    Ok(())
}
