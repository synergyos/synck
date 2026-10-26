//! Canonical call history. Media connections and credentials are not history.
pub mod history;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use synck_vcs::types::ItemId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum Origin {
    Standalone {
        recipient_id: String,
    },
    Direct {
        chat_item_id: ItemId,
        recipient_id: String,
    },
    Room {
        chat_item_id: ItemId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Media {
    Audio,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Transport {
    DirectP2p,
    GroupLivekit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Answer {
    Accepted,
    Declined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EndReason {
    Canceled,
    Ended,
    UnansweredExpiry,
    AcceptedExpiry,
    Restore,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Creation {
    pub creator_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub origin: Origin,
    pub media: Media,
    pub transport: Transport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub answer: Answer,
    pub first_accepted_at: Option<DateTime<Utc>>,
    pub first_declined_at: Option<DateTime<Utc>>,
    pub accepted_native_device_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct End {
    pub at: DateTime<Utc>,
    pub reason: EndReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub creation: Creation,
    pub participants: BTreeMap<String, Participant>,
    pub end: Option<End>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum Event {
    Create {
        creation: Creation,
    },
    Answer {
        principal_id: String,
        at: DateTime<Utc>,
        answer: Answer,
        native_device_id: Option<String>,
    },
    Join {
        principal_id: String,
        at: DateTime<Utc>,
        native_device_id: Option<String>,
    },
    End {
        end: End,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct Invalid(pub &'static str);

impl Origin {
    pub fn chat_item_id(&self) -> Option<&ItemId> {
        match self {
            Self::Standalone { .. } => None,
            Self::Direct { chat_item_id, .. } | Self::Room { chat_item_id } => Some(chat_item_id),
        }
    }
    pub fn recipient_id(&self) -> Option<&str> {
        match self {
            Self::Standalone { recipient_id } | Self::Direct { recipient_id, .. } => {
                Some(recipient_id)
            }
            Self::Room { .. } => None,
        }
    }
}

impl Call {
    pub fn answered(&self) -> bool {
        self.participants
            .iter()
            .any(|(id, p)| id != &self.creation.creator_id && p.first_accepted_at.is_some())
    }
    pub fn expiry(&self) -> End {
        End {
            at: self.creation.expires_at,
            reason: if self.answered() {
                EndReason::AcceptedExpiry
            } else {
                EndReason::UnansweredExpiry
            },
        }
    }
    fn admit_at(&self, at: DateTime<Utc>) -> Result<(), Invalid> {
        if self.end.is_some() {
            return Err(Invalid("call has ended"));
        }
        if at < self.creation.created_at || at >= self.creation.expires_at {
            return Err(Invalid("action outside call lifetime"));
        }
        Ok(())
    }
}

/// Validates event history, not current repository/chat authorization. The
/// command owner must authorize and serialize acceptance before appending.
pub fn apply(previous: Option<&Call>, event: &Event) -> Result<Call, Invalid> {
    if let Event::Create { creation } = event {
        if previous.is_some() {
            return Err(Invalid("call already exists"));
        }
        if creation.creator_id.trim().is_empty() || creation.expires_at <= creation.created_at {
            return Err(Invalid("invalid call creation"));
        }
        if creation
            .origin
            .recipient_id()
            .is_some_and(|id| id.trim().is_empty() || id == creation.creator_id)
        {
            return Err(Invalid("invalid call recipient"));
        }
        if creation.transport == Transport::DirectP2p
            && matches!(creation.origin, Origin::Room { .. })
        {
            return Err(Invalid("direct call requires exactly one recipient"));
        }
        return Ok(Call {
            creation: creation.clone(),
            participants: BTreeMap::from([(
                creation.creator_id.clone(),
                Participant {
                    answer: Answer::Accepted,
                    first_accepted_at: Some(creation.created_at),
                    first_declined_at: None,
                    accepted_native_device_id: None,
                },
            )]),
            end: None,
        });
    }
    let mut call = previous.cloned().ok_or(Invalid("call does not exist"))?;
    match event {
        Event::Create { .. } => unreachable!(),
        Event::Answer {
            principal_id,
            at,
            native_device_id,
            ..
        }
        | Event::Join {
            principal_id,
            at,
            native_device_id,
        } => {
            call.admit_at(*at)?;
            if principal_id.trim().is_empty()
                || native_device_id
                    .as_ref()
                    .is_some_and(|id| id.trim().is_empty())
            {
                return Err(Invalid("invalid participant identity"));
            }
            let answer = match event {
                Event::Answer { answer, .. } => *answer,
                _ => Answer::Accepted,
            };
            match (&call.creation.origin, event) {
                (Origin::Room { .. }, Event::Join { .. }) => {}
                (Origin::Room { .. }, Event::Answer { .. }) => {
                    return Err(Invalid("room participation uses join"));
                }
                (_, Event::Join { .. }) => return Err(Invalid("direct participation uses answer")),
                _ if call.creation.origin.recipient_id() != Some(principal_id.as_str()) => {
                    return Err(Invalid("participant is not invited"));
                }
                _ => {}
            }
            if principal_id == &call.creation.creator_id {
                return Err(Invalid("creator is already admitted"));
            }
            let participant =
                call.participants
                    .entry(principal_id.clone())
                    .or_insert(Participant {
                        answer,
                        first_accepted_at: None,
                        first_declined_at: None,
                        accepted_native_device_id: None,
                    });
            if answer == Answer::Declined
                && participant.answer == Answer::Accepted
                && participant.first_accepted_at.is_some()
            {
                return Err(Invalid("accepted participant cannot decline"));
            }
            if answer == Answer::Declined && native_device_id.is_some() {
                return Err(Invalid("decline cannot assign answer device"));
            }
            participant.answer = answer;
            match answer {
                Answer::Accepted if participant.first_accepted_at.is_none() => {
                    participant.first_accepted_at = Some(*at);
                    participant.accepted_native_device_id = native_device_id.clone();
                }
                Answer::Declined => {
                    participant.first_declined_at.get_or_insert(*at);
                }
                _ => {}
            }
        }
        Event::End { end } => {
            if call.end.is_some() {
                return Err(Invalid("call already ended"));
            }
            if end.at < call.creation.created_at {
                return Err(Invalid("end precedes creation"));
            }
            match end.reason {
                EndReason::UnansweredExpiry | EndReason::AcceptedExpiry
                    if *end != call.expiry() =>
                {
                    return Err(Invalid("incorrect expiry outcome"));
                }
                EndReason::Canceled if call.answered() => {
                    return Err(Invalid("answered call cannot be canceled"));
                }
                EndReason::Ended if !call.answered() => {
                    return Err(Invalid("unanswered call must be canceled"));
                }
                EndReason::Canceled | EndReason::Ended if end.at >= call.creation.expires_at => {
                    return Err(Invalid("expired call requires expiry outcome"));
                }
                _ => {}
            }
            call.end = Some(end.clone());
        }
    }
    Ok(call)
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn start(room: bool) -> Call {
        apply(
            None,
            &Event::Create {
                creation: Creation {
                    creator_id: "caller".into(),
                    created_at: DateTime::from_timestamp(100, 0).unwrap(),
                    expires_at: DateTime::from_timestamp(200, 0).unwrap(),
                    origin: if room {
                        Origin::Room {
                            chat_item_id: ItemId::new(),
                        }
                    } else {
                        Origin::Standalone {
                            recipient_id: "recipient".into(),
                        }
                    },
                    media: Media::Audio,
                    transport: Transport::GroupLivekit,
                },
            },
        )
        .unwrap()
    }
    fn answer(at: i64, answer: Answer, device: Option<&str>) -> Event {
        Event::Answer {
            principal_id: "recipient".into(),
            at: DateTime::from_timestamp(at, 0).unwrap(),
            answer,
            native_device_id: device.map(str::to_owned),
        }
    }
    #[test]
    fn creator_does_not_make_unanswered_call_answered() {
        let call = start(false);
        assert!(!call.answered());
        assert_eq!(call.expiry().reason, EndReason::UnansweredExpiry);
        assert!(call.creation.origin.chat_item_id().is_none());
    }
    #[test]
    fn decline_can_be_followed_by_acceptance_but_not_reverse() {
        let call = apply(Some(&start(false)), &answer(110, Answer::Declined, None)).unwrap();
        let call = apply(
            Some(&call),
            &answer(120, Answer::Accepted, Some("device-a")),
        )
        .unwrap();
        assert!(call.answered());
        assert!(apply(Some(&call), &answer(130, Answer::Declined, None)).is_err());
        let rejoined = apply(
            Some(&call),
            &answer(140, Answer::Accepted, Some("device-b")),
        )
        .unwrap();
        assert_eq!(
            rejoined.participants["recipient"]
                .accepted_native_device_id
                .as_deref(),
            Some("device-a")
        );
        assert_eq!(
            rejoined.participants["recipient"].first_accepted_at,
            DateTime::from_timestamp(120, 0)
        );
        assert_eq!(rejoined.expiry().reason, EndReason::AcceptedExpiry);
    }
    #[test]
    fn expiry_and_terminal_state_fence_admission() {
        let call = start(false);
        assert!(apply(Some(&call), &answer(200, Answer::Accepted, None)).is_err());
        let ended = apply(Some(&call), &Event::End { end: call.expiry() }).unwrap();
        assert!(apply(Some(&ended), &answer(150, Answer::Accepted, None)).is_err());
        assert!(apply(Some(&ended), &Event::End { end: call.expiry() }).is_err());
    }
    #[test]
    fn event_shapes_cannot_manufacture_invites_or_outcomes() {
        let call = start(false);
        let outsider = Event::Answer {
            principal_id: "outsider".into(),
            at: DateTime::from_timestamp(120, 0).unwrap(),
            answer: Answer::Accepted,
            native_device_id: None,
        };
        assert!(apply(Some(&call), &outsider).is_err());
        assert!(apply(Some(&start(true)), &answer(120, Answer::Accepted, None)).is_err());
        assert!(
            apply(
                Some(&call),
                &Event::End {
                    end: End {
                        at: call.creation.expires_at,
                        reason: EndReason::AcceptedExpiry
                    }
                }
            )
            .is_err()
        );
    }
    #[test]
    fn room_join_is_explicit_and_does_not_change_origin() {
        let call = start(true);
        let joined = apply(
            Some(&call),
            &Event::Join {
                principal_id: "reader".into(),
                at: DateTime::from_timestamp(120, 0).unwrap(),
                native_device_id: None,
            },
        )
        .unwrap();
        assert_eq!(call.creation, joined.creation);
        assert!(!call.participants.contains_key("reader"));
        assert!(joined.participants.contains_key("reader"));
    }

    #[test]
    fn direct_transport_rejects_room_origin() {
        let mut creation = start(true).creation;
        creation.transport = Transport::DirectP2p;
        assert_eq!(
            apply(None, &Event::Create { creation }),
            Err(Invalid("direct call requires exactly one recipient"))
        );
    }
}
