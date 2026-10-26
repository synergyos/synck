//! Exact conversion of reviewed VEVENT data. Unknown scheduling semantics fail
//! explicitly; callers retain the original staging evidence for review.
use super::{CalendarMessage, Component, Error, Identity, Method, Property, Result, decode_text};
use crate::native_calendars::{
    CalendarEvent, EventTime,
    occurrence_details::{Details, Edit},
    recurrence::{Observance, OccurrenceException, Recurrence, TimezoneDefinition},
};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use std::collections::{BTreeMap, BTreeSet};
use synck_vcs::types::ItemId;

pub struct Imported {
    pub event: CalendarEvent,
    pub description: String,
    pub occurrence_descriptions: BTreeMap<DateTime<Utc>, String>,
}

pub struct ReviewedOccurrenceRequest {
    pub identity: Identity,
    pub original_start: DateTime<Utc>,
    pub time: EventTime,
    pub details: Details,
    pub description: Option<String>,
}
fn domain(_: crate::native_calendars::CalendarError) -> Error {
    Error::Invalid("unsupported event or recurrence semantics")
}
fn naive(value: &str) -> Result<NaiveDateTime> {
    NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
        .map_err(|_| Error::Invalid("invalid local calendar time"))
}
fn offset(value: &str) -> Result<i32> {
    if !matches!(value.len(), 5 | 7)
        || !value.is_ascii()
        || !matches!(&value[..1], "+" | "-")
        || !value[1..].bytes().all(|b| b.is_ascii_digit())
    {
        return Err(Error::Invalid("invalid timezone offset"));
    }
    let hour = value[1..3].parse::<i32>().unwrap();
    let minute = value[3..5].parse::<i32>().unwrap();
    let second = if value.len() == 7 {
        value[5..7].parse::<i32>().unwrap()
    } else {
        0
    };
    if hour >= 24 || minute >= 60 || second >= 60 {
        return Err(Error::Invalid("invalid timezone offset"));
    }
    Ok((hour * 3600 + minute * 60 + second) * if value.starts_with('-') { -1 } else { 1 })
}
pub(crate) fn zones(document: &Component) -> Result<BTreeMap<String, TimezoneDefinition>> {
    let mut zones = BTreeMap::new();
    for component in document.children.iter().filter(|c| c.name == "VTIMEZONE") {
        let name = component.required("TZID")?.value.clone();
        let mut observances = Vec::new();
        for obs in &component.children {
            if !matches!(obs.name.as_str(), "STANDARD" | "DAYLIGHT") {
                return Err(Error::Unsupported);
            }
            let mut rdates = Vec::new();
            for prop in obs.properties.iter().filter(|p| p.name == "RDATE") {
                for value in prop.value.split(',') {
                    rdates.push(naive(value)?);
                }
            }
            observances.push(Observance {
                start: naive(&obs.required("DTSTART")?.value)?,
                offset_from: offset(&obs.required("TZOFFSETFROM")?.value)?,
                offset_to: offset(&obs.required("TZOFFSETTO")?.value)?,
                rule: obs.get("RRULE")?.map(|p| p.value.clone()),
                rdates,
            });
        }
        if zones
            .insert(name.clone(), TimezoneDefinition { name, observances })
            .is_some()
        {
            return Err(Error::Invalid("duplicate timezone identity"));
        }
    }
    Ok(zones)
}
fn zone_of<'a>(property: &'a Property, default: &'a str) -> &'a str {
    if property.value.ends_with('Z') {
        "UTC"
    } else {
        property
            .params
            .get("TZID")
            .map(String::as_str)
            .unwrap_or(default)
    }
}

pub(crate) fn instant(
    property: &Property,
    zones: &BTreeMap<String, TimezoneDefinition>,
    default: &str,
) -> Result<DateTime<Utc>> {
    if property.params.get("VALUE").is_some_and(|v| v == "DATE") {
        if property.value.len() != 8 {
            return Err(Error::Invalid("invalid date"));
        }
        return NaiveDate::parse_from_str(&property.value, "%Y%m%d")
            .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc())
            .map_err(|_| Error::Invalid("invalid date"));
    }
    if property.value.ends_with('Z') {
        if property.params.contains_key("TZID") {
            return Err(Error::Invalid("UTC time cannot carry TZID"));
        }
        return super::utc_timestamp(&property.value);
    }
    let local = naive(&property.value)?;
    let name = zone_of(property, default);
    if let Some(zone) = zones.get(name) {
        let transitions = zone
            .transitions(
                local
                    .checked_add_signed(Duration::days(366))
                    .ok_or(Error::Invalid("timezone date overflow"))?,
            )
            .map_err(domain)?;
        let mut offsets = BTreeSet::new();
        for (_, from, to) in &transitions {
            offsets.insert(*from);
            offsets.insert(*to);
        }
        let mut candidates = Vec::new();
        for offset in offsets {
            let candidate = local
                .checked_sub_signed(Duration::seconds(offset as i64))
                .ok_or(Error::Invalid("timezone date overflow"))?
                .and_utc();
            let i = transitions.partition_point(|t| t.0 <= candidate);
            let effective = if i == 0 {
                transitions.first().map(|t| t.1)
            } else {
                Some(transitions[i - 1].2)
            };
            if effective == Some(offset) {
                candidates.push(candidate);
            }
        }
        return candidates
            .into_iter()
            .min()
            .ok_or(Error::Invalid("calendar time falls in timezone gap"));
    }
    let zone: chrono_tz::Tz = name
        .parse()
        .map_err(|_| Error::Invalid("unknown timezone without definition"))?;
    zone.from_local_datetime(&local)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .ok_or(Error::Invalid("calendar time falls in timezone gap"))
}
pub(crate) fn time(
    event: &Component,
    zones: &BTreeMap<String, TimezoneDefinition>,
    default: &str,
) -> Result<EventTime> {
    let start = event.required("DTSTART")?;
    let end = event.required("DTEND")?;
    if event.get("DURATION")?.is_some() {
        return Err(Error::Unsupported);
    }
    let date = start.params.get("VALUE").is_some_and(|v| v == "DATE");
    if date != end.params.get("VALUE").is_some_and(|v| v == "DATE") {
        return Err(Error::Invalid("event start and end types differ"));
    }
    let time = if date {
        EventTime::AllDay {
            start_date: instant(start, zones, default)?.date_naive(),
            end_date: instant(end, zones, default)?.date_naive(),
            time_zone: default.into(),
        }
    } else {
        let name = zone_of(start, default);
        EventTime::Timed {
            start: instant(start, zones, default)?,
            end: instant(end, zones, default)?,
            time_zone: if name.parse::<chrono_tz::Tz>().is_ok() {
                name.into()
            } else {
                "UTC".into()
            },
        }
    };
    time.validate().map_err(domain)?;
    Ok(time)
}

pub fn reviewed_action(
    message: &CalendarMessage,
    expected_uid: &str,
    expected_organizer: &str,
    expected_participant: &str,
    default_zone: &str,
) -> Result<super::state::ReviewedExternalAction> {
    if default_zone.parse::<chrono_tz::Tz>().is_err() || message.method == Method::Request {
        return Err(Error::Unsupported);
    }
    let zones = zones(&message.document)?;
    let mut events = message
        .document
        .children
        .iter()
        .filter(|c| c.name == "VEVENT");
    let event = events.next().ok_or(Error::Invalid("missing event"))?;
    if events.next().is_some() {
        return Err(Error::Invalid("reviewed action must contain one event"));
    }
    let identity = Identity::from_event(event)?;
    if identity.uid != expected_uid || identity.organizer != expected_organizer {
        return Err(Error::Invalid(
            "scheduling identity does not match established meeting",
        ));
    }
    let original_start = identity
        .recurrence_id
        .as_ref()
        .map(|p| instant(p, &zones, default_zone))
        .transpose()?;
    let attendees = event
        .properties
        .iter()
        .filter(|property| property.name == "ATTENDEE")
        .map(|property| super::mailto(&property.value))
        .collect::<Result<Vec<_>>>()?;
    let attendee = attendees.first().map(String::as_str);
    if message.method != Method::Cancel && attendees.len() > 1 {
        return Err(Error::Invalid("ambiguous scheduling participant"));
    }
    use super::state::ReviewedExternalAction as A;
    match message.method {
        Method::Cancel => {
            if !attendees.is_empty()
                && !attendees
                    .iter()
                    .any(|attendee| attendee == expected_participant)
            {
                return Err(Error::Invalid("cancellation targets another attendee"));
            }
            Ok(A::Cancel {
                original_start,
                sequence: None,
                dtstamp: None,
                command_id: None,
                occurrence_order: None,
            })
        }
        Method::Reply => {
            if attendee != Some(expected_participant) {
                return Err(Error::Invalid("replying attendee does not match"));
            }
            let response = match event
                .required("ATTENDEE")?
                .params
                .get("PARTSTAT")
                .map(String::as_str)
            {
                Some("ACCEPTED") => crate::native_calendars::Response::Accepted,
                Some("TENTATIVE") => crate::native_calendars::Response::Tentative,
                Some("DECLINED") => crate::native_calendars::Response::Declined,
                _ => return Err(Error::Unsupported),
            };
            Ok(A::Reply {
                address: expected_participant.into(),
                response,
                original_start,
            })
        }
        Method::Refresh => {
            if attendee != Some(expected_participant) || original_start.is_some() {
                return Err(Error::Unsupported);
            }
            Ok(A::Refresh {
                command_id: uuid::Uuid::nil(),
                address: expected_participant.into(),
            })
        }
        Method::Counter => {
            if attendee != Some(expected_participant) {
                return Err(Error::Invalid("countering attendee does not match"));
            }
            Ok(A::AcceptCounter {
                address: expected_participant.into(),
                time: time(event, &zones, default_zone)?,
                original_start,
            })
        }
        Method::Declinecounter => {
            if attendee != Some(expected_participant) {
                return Err(Error::Invalid("declining attendee does not match"));
            }
            Ok(A::DeclineCounter {
                command_id: uuid::Uuid::nil(),
                address: expected_participant.into(),
                original_start,
            })
        }
        Method::Request => unreachable!(),
    }
}
fn text(event: &Component, name: &str) -> Result<Option<String>> {
    event.get(name)?.map(|p| decode_text(&p.value)).transpose()
}
fn explicit_dates(
    event: &Component,
    key: &str,
    zones: &BTreeMap<String, TimezoneDefinition>,
    default: &str,
) -> Result<Vec<DateTime<Utc>>> {
    let mut out = Vec::new();
    for property in event.properties.iter().filter(|p| p.name == key) {
        for value in property.value.split(',') {
            let mut property = property.clone();
            property.value = value.into();
            out.push(instant(&property, zones, default)?);
            if out.len() > 512 {
                return Err(Error::Invalid("too many recurrence dates"));
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}
pub fn reviewed_request(
    message: &CalendarMessage,
    calendar: ItemId,
    owner: &str,
    recipient: &str,
    default_zone: &str,
) -> Result<Imported> {
    convert_request(message, calendar, owner, Some(recipient), default_zone)
}

pub fn reviewed_occurrence_request(
    message: &CalendarMessage,
    expected_recipient: &str,
    default_zone: &str,
) -> Result<ReviewedOccurrenceRequest> {
    if message.method != Method::Request || default_zone.parse::<chrono_tz::Tz>().is_err() {
        return Err(Error::Unsupported);
    }
    let zones = zones(&message.document)?;
    let mut events = message
        .document
        .children
        .iter()
        .filter(|component| component.name == "VEVENT");
    let event = events.next().ok_or(Error::Invalid("missing event"))?;
    if events.next().is_some() {
        return Err(Error::Invalid("detached request must contain one event"));
    }
    let identity = Identity::from_event(event)?;
    let recurrence_id = identity.recurrence_id.as_ref().ok_or(Error::Unsupported)?;
    if recurrence_id.params.contains_key("RANGE")
        || event.get("RRULE")?.is_some()
        || event.get("RDATE")?.is_some()
        || event.get("EXDATE")?.is_some()
        || event.get("EXRULE")?.is_some()
        || event
            .get("STATUS")?
            .is_some_and(|property| property.value == "CANCELLED")
    {
        return Err(Error::Unsupported);
    }
    if !event
        .properties
        .iter()
        .filter(|property| property.name == "ATTENDEE")
        .any(|property| super::mailto(&property.value).ok().as_deref() == Some(expected_recipient))
    {
        return Err(Error::Invalid("recipient is not an invited attendee"));
    }
    let zone_name = zone_of(event.required("DTSTART")?, default_zone);
    Ok(ReviewedOccurrenceRequest {
        original_start: instant(recurrence_id, &zones, zone_name)?,
        time: time(event, &zones, default_zone)?,
        details: Details {
            title: text(event, "SUMMARY")?,
            location: event
                .get("LOCATION")?
                .map(|property| decode_text(&property.value))
                .transpose()?
                .map(Some),
            meeting_url: event
                .get("URL")?
                .map(|property| Some(property.value.clone())),
        },
        description: text(event, "DESCRIPTION")?,
        identity,
    })
}

pub fn migration_request(
    message: &CalendarMessage,
    calendar: ItemId,
    owner: &str,
    default_zone: &str,
) -> Result<Imported> {
    convert_request(message, calendar, owner, None, default_zone)
}

fn convert_request(
    message: &CalendarMessage,
    calendar: ItemId,
    owner: &str,
    recipient: Option<&str>,
    default_zone: &str,
) -> Result<Imported> {
    if message.method != Method::Request || default_zone.parse::<chrono_tz::Tz>().is_err() {
        return Err(Error::Unsupported);
    }
    let zones = zones(&message.document)?;
    let mut masters = message
        .document
        .children
        .iter()
        .filter(|c| c.name == "VEVENT" && c.get("RECURRENCE-ID").ok().flatten().is_none());
    let master = masters.next().ok_or(Error::Unsupported)?;
    if masters.next().is_some() {
        return Err(Error::Invalid("multiple master events"));
    }
    if recipient.is_some_and(|recipient| {
        !master
            .properties
            .iter()
            .filter(|p| p.name == "ATTENDEE")
            .any(|p| super::mailto(&p.value).ok().as_deref() == Some(recipient))
    }) {
        return Err(Error::Invalid("recipient is not an invited attendee"));
    }
    if master
        .get("STATUS")?
        .is_some_and(|p| p.value == "CANCELLED")
    {
        return Err(Error::Invalid("request is canceled"));
    }
    if master.get("RANGE")?.is_some() || master.get("EXRULE")?.is_some() {
        return Err(Error::Unsupported);
    }
    let start = master.required("DTSTART")?;
    let zone_name = zone_of(start, default_zone);
    let mut event = CalendarEvent::new(
        calendar,
        text(master, "SUMMARY")?.unwrap_or_else(|| "Untitled event".into()),
        time(master, &zones, default_zone)?,
        owner.into(),
    )
    .map_err(domain)?;
    event.location = text(master, "LOCATION")?;
    event.meeting_url = master.get("URL")?.map(|p| p.value.clone());
    let recurrence = master
        .get("RRULE")?
        .map(|p| -> Result<_> {
            Ok(Recurrence {
                rule: p.value.clone(),
                rdates: explicit_dates(master, "RDATE", &zones, zone_name)?,
                exdates: explicit_dates(master, "EXDATE", &zones, zone_name)?,
                timezone: zones.get(zone_name).cloned(),
            })
        })
        .transpose()?;
    if recurrence.is_none()
        && master
            .properties
            .iter()
            .any(|p| matches!(p.name.as_str(), "RDATE" | "EXDATE"))
    {
        return Err(Error::Unsupported);
    }
    let mut exceptions = Vec::new();
    let mut details = Vec::new();
    let mut occurrence_descriptions = BTreeMap::new();
    let description = text(master, "DESCRIPTION")?.unwrap_or_default();
    for occurrence in message
        .document
        .children
        .iter()
        .filter(|c| c.name == "VEVENT")
    {
        let Some(identity) = occurrence.get("RECURRENCE-ID")? else {
            continue;
        };
        if identity.params.contains_key("RANGE") {
            return Err(Error::Unsupported);
        }
        let original_start = instant(identity, &zones, zone_name)?;
        let canceled = occurrence
            .get("STATUS")?
            .is_some_and(|p| p.value == "CANCELLED");
        exceptions.push(OccurrenceException {
            revision: None,
            canceled,
            original_start,
            time: if canceled {
                None
            } else {
                Some(time(occurrence, &zones, zone_name)?)
            },
        });
        if !canceled {
            details.push(Edit {
                original_start,
                time: None,
                details: Details {
                    title: text(occurrence, "SUMMARY")?,
                    location: Some(text(occurrence, "LOCATION")?),
                    meeting_url: Some(occurrence.get("URL")?.map(|p| p.value.clone())),
                },
                attendees: None,
            });
            let body = text(occurrence, "DESCRIPTION")?.unwrap_or_default();
            if body != description {
                occurrence_descriptions.insert(original_start, body);
            }
        }
    }
    exceptions.sort_by_key(|e| e.original_start);
    event
        .set_schedule(event.time.clone(), recurrence, exceptions)
        .map_err(domain)?;
    for detail in details {
        event.edit_occurrence(&detail).map_err(domain)?;
    }
    event.validate().map_err(domain)?;
    let _ = Identity::from_event(master)?;
    Ok(Imported {
        event,
        description,
        occurrence_descriptions,
    })
}
