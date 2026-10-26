//! Explicit, bounded `.ics` archive conversion. This parser produces inert
//! canonical candidates; it owns neither repository authority nor delivery.
use super::state::ExternalOccurrenceResponse;
use super::{
    CalendarMessage, Component, Error, ExternalAttendee, ExternalScheduling, Identity, Method,
    Property, Result, mailto, parse_document,
};
use crate::native_calendars::{CalendarEvent, Response};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use synck_vcs::types::ItemId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub ordinal: u32,
    pub uid: Option<String>,
    pub organizer: Option<String>,
    pub event: Option<CalendarEvent>,
    pub description: Option<String>,
    pub occurrence_descriptions: BTreeMap<chrono::DateTime<chrono::Utc>, String>,
    pub rejection: Option<String>,
}

pub fn trial(
    bytes: &[u8],
    calendar: &ItemId,
    owner: &str,
    default_zone: &str,
) -> Result<Vec<Candidate>> {
    let document = parse_document(bytes)?;
    let zones: Vec<_> = document
        .children
        .iter()
        .filter(|c| c.name == "VTIMEZONE")
        .cloned()
        .collect();
    let mut groups: BTreeMap<(String, String), (u32, Vec<Component>)> = BTreeMap::new();
    let mut rejected = Vec::new();
    for (ordinal, event) in document
        .children
        .iter()
        .filter(|c| c.name == "VEVENT")
        .enumerate()
    {
        match Identity::from_event(event) {
            Ok(id) => groups
                .entry((id.uid, id.organizer))
                .or_insert_with(|| (ordinal as u32, Vec::new()))
                .1
                .push(event.clone()),
            Err(error) => rejected.push(Candidate {
                ordinal: ordinal as u32,
                uid: None,
                organizer: None,
                event: None,
                description: None,
                rejection: Some(error.to_string()),
                occurrence_descriptions: BTreeMap::new(),
            }),
        }
    }
    if groups.len() + rejected.len() > 1000 {
        return Err(Error::Invalid("too many imported events"));
    }
    let mut out = rejected;
    for ((uid, organizer), (ordinal, mut events)) in groups {
        let result = (|| {
            let masters: Vec<_> = events
                .iter()
                .enumerate()
                .filter(|(_, e)| e.get("RECURRENCE-ID").ok().flatten().is_none())
                .map(|(i, _)| i)
                .collect();
            if masters.len() != 1 {
                return Err(Error::Invalid("meeting requires one master event"));
            }
            let canceled = events[masters[0]]
                .get("STATUS")?
                .is_some_and(|p| p.value == "CANCELLED");
            events[masters[0]].properties.retain(|p| p.name != "STATUS");
            let mut children = zones.clone();
            children.extend(events.clone());
            let message = CalendarMessage {
                method: Method::Request,
                document: Component {
                    name: "VCALENDAR".into(),
                    properties: vec![
                        Property {
                            name: "VERSION".into(),
                            params: BTreeMap::new(),
                            value: "2.0".into(),
                        },
                        Property {
                            name: "METHOD".into(),
                            params: BTreeMap::new(),
                            value: "REQUEST".into(),
                        },
                    ],
                    children,
                },
            };
            let mut imported =
                super::import::migration_request(&message, calendar.clone(), owner, default_zone)?;
            let master = &events[masters[0]];
            let mut attendee_map = BTreeMap::new();
            for property in master.properties.iter().filter(|p| p.name == "ATTENDEE") {
                let address = mailto(&property.value)?;
                let response = match property.params.get("PARTSTAT").map(String::as_str) {
                    Some("ACCEPTED") => Response::Accepted,
                    Some("TENTATIVE") => Response::Tentative,
                    Some("DECLINED") => Response::Declined,
                    _ => Response::NeedsAction,
                };
                if attendee_map.insert(address, response).is_some() {
                    return Err(Error::Invalid("duplicate imported attendee"));
                }
            }
            let attendees: Vec<_> = attendee_map
                .into_iter()
                .map(|(address, response)| ExternalAttendee { address, response })
                .collect();
            let parsed_zones = super::import::zones(&message.document)?;
            let mut occurrence_responses = Vec::new();
            for occurrence in events
                .iter()
                .filter(|event| event.get("RECURRENCE-ID").ok().flatten().is_some())
            {
                let original_start = super::import::instant(
                    occurrence.required("RECURRENCE-ID")?,
                    &parsed_zones,
                    default_zone,
                )?;
                for property in occurrence
                    .properties
                    .iter()
                    .filter(|p| p.name == "ATTENDEE")
                {
                    let address = mailto(&property.value)?;
                    let response = match property.params.get("PARTSTAT").map(String::as_str) {
                        Some("ACCEPTED") => Response::Accepted,
                        Some("TENTATIVE") => Response::Tentative,
                        Some("DECLINED") => Response::Declined,
                        _ => Response::NeedsAction,
                    };
                    occurrence_responses.push(ExternalOccurrenceResponse {
                        original_start,
                        address,
                        response,
                    });
                }
            }
            occurrence_responses.sort_by(|a, b| {
                (&a.original_start, &a.address).cmp(&(&b.original_start, &b.address))
            });
            if occurrence_responses.windows(2).any(|pair| {
                pair[0].original_start == pair[1].original_start
                    && pair[0].address == pair[1].address
            }) {
                return Err(Error::Invalid("duplicate imported occurrence attendee"));
            }
            imported.event.external = Some(ExternalScheduling::Imported {
                uid: uid.clone(),
                organizer: organizer.clone(),
                attendees,
                occurrence_responses,
                canceled,
            });
            if canceled {
                imported
                    .event
                    .cancel()
                    .map_err(|_| Error::Invalid("invalid canceled meeting"))?;
            }
            imported
                .event
                .validate()
                .map_err(|_| Error::Invalid("invalid imported event"))?;
            Ok((
                imported.event,
                imported.description,
                imported.occurrence_descriptions,
            ))
        })();
        match result {
            Ok((event, description, occurrence_descriptions)) => out.push(Candidate {
                ordinal,
                uid: Some(uid),
                organizer: Some(organizer),
                event: Some(event),
                description: Some(description),
                occurrence_descriptions,
                rejection: None,
            }),
            Err(error) => out.push(Candidate {
                ordinal,
                uid: Some(uid),
                organizer: Some(organizer),
                event: None,
                description: None,
                occurrence_descriptions: BTreeMap::new(),
                rejection: Some(error.to_string()),
            }),
        }
    }
    out.sort_by_key(|c| c.ordinal);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_trial_preserves_identity_responses_exceptions_and_cancellation() {
        let source=b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:series@example.test\r\nDTSTAMP:20260910T100000Z\r\nORGANIZER:mailto:owner@example.test\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:a@example.test\r\nDTSTART:20260911T100000Z\r\nDTEND:20260911T110000Z\r\nRRULE:FREQ=DAILY;COUNT=3\r\nSUMMARY:Series\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:series@example.test\r\nDTSTAMP:20260910T100000Z\r\nORGANIZER:mailto:owner@example.test\r\nRECURRENCE-ID:20260912T100000Z\r\nATTENDEE;PARTSTAT=TENTATIVE:mailto:a@example.test\r\nSTATUS:CANCELLED\r\nDTSTART:20260912T100000Z\r\nDTEND:20260912T110000Z\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:canceled@example.test\r\nDTSTAMP:20260910T100000Z\r\nORGANIZER:mailto:remote@example.test\r\nATTENDEE;PARTSTAT=DECLINED:mailto:owner@example.test\r\nSTATUS:CANCELLED\r\nDTSTART:20260920T100000Z\r\nDTEND:20260920T110000Z\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:broken\r\nDTSTART:20260920T100000Z\r\nDTEND:20260920T110000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let items = trial(source, &ItemId::new(), "owner", "UTC").unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items.iter().filter(|i| i.rejection.is_some()).count(), 1);
        let series = items
            .iter()
            .find(|i| i.uid.as_deref() == Some("series@example.test"))
            .unwrap()
            .event
            .as_ref()
            .unwrap();
        assert!(series.exceptions.iter().any(|e| e.canceled));
        let Some(ExternalScheduling::Imported {
            organizer,
            attendees,
            occurrence_responses,
            canceled,
            ..
        }) = &series.external
        else {
            panic!("missing origin")
        };
        assert_eq!(organizer, "owner@example.test");
        assert_eq!(attendees[0].response, Response::Accepted);
        assert_eq!(occurrence_responses[0].response, Response::Tentative);
        assert!(!canceled);
        let mut activated = series.clone();
        super::super::state::configure_outbound(
            &mut activated,
            "series@example.test".into(),
            "owner@example.test".into(),
            vec!["a@example.test".into()],
        )
        .unwrap();
        let Some(ExternalScheduling::LocalOrganizer {
            attendees,
            occurrence_responses,
            ..
        }) = activated.external
        else {
            panic!("not activated")
        };
        assert_eq!(attendees[0].response, Response::Accepted);
        assert_eq!(occurrence_responses[0].response, Response::Tentative);
        let canceled = items
            .iter()
            .find(|i| i.uid.as_deref() == Some("canceled@example.test"))
            .unwrap()
            .event
            .as_ref()
            .unwrap();
        assert_eq!(
            canceled.state,
            crate::native_calendars::EventState::Canceled
        );
        let mut canceled = canceled.clone();
        assert!(
            super::super::state::configure_outbound(
                &mut canceled,
                "canceled@example.test".into(),
                "remote@example.test".into(),
                vec!["owner@example.test".into()],
            )
            .is_err()
        );
    }
}
