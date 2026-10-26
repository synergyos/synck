//! Native calendar domain state. SQL, membership, and ACL authority live above
//! this module; callers must hold the acceptance fence before applying changes.

use chrono::{
    DateTime, Datelike, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, Offset,
    TimeZone, Utc,
};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use synck_vcs::types::ItemId;
use thiserror::Error;

pub mod history;
pub mod occurrence_details;
pub mod recurrence;

pub const MAX_TITLE_BYTES: usize = 512;
pub const MAX_LOCATION_BYTES: usize = 2048;
pub const MAX_MEETING_URL_BYTES: usize = 2048;
pub const MAX_ATTENDEES: usize = 500;
pub const MAX_ATTACHMENTS: usize = 20;
pub const MAX_REMINDER_MINUTES: u32 = 40320;
pub const REMINDER_GRACE_MINUTES: i64 = 15;
// Canonical JSON counters must remain exactly representable across clients.
const MAX_COUNTER: u64 = (1 << 53) - 1;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CalendarError {
    #[error("native calendar item not found")]
    NotFound,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
    #[error("CALENDAR_EXPANSION_LIMIT: recurrence exceeds the bounded expansion budget")]
    ExpansionLimit,
    #[error("event is canceled")]
    Canceled,
}

fn advance(counter: &mut u64) -> Result<u64, CalendarError> {
    let next = counter
        .checked_add(1)
        .filter(|next| *next <= MAX_COUNTER)
        .ok_or(CalendarError::Invalid("calendar counter exhausted"))?;
    *counter = next;
    Ok(next)
}

fn title(value: &str) -> Result<(), CalendarError> {
    if value.trim().is_empty() || value.len() > MAX_TITLE_BYTES {
        return Err(CalendarError::Invalid(
            "title must contain 1..512 UTF-8 bytes",
        ));
    }
    Ok(())
}

fn user_id(value: &str) -> Result<(), CalendarError> {
    if value.is_empty() || value.trim() != value || value.len() > 256 {
        return Err(CalendarError::Invalid("invalid user id"));
    }
    Ok(())
}

pub(crate) fn validate_user_id(value: &str) -> Result<(), CalendarError> {
    user_id(value)
}

fn zone(value: &str) -> Result<Tz, CalendarError> {
    value
        .parse()
        .map_err(|_| CalendarError::Invalid("invalid IANA time zone"))
}

/// Resolve wall time without guessing at DST gaps or ambiguous offsets.
pub fn resolve_local_time(
    local: NaiveDateTime,
    time_zone: &str,
    offset_seconds: Option<i32>,
) -> Result<DateTime<Utc>, CalendarError> {
    let matches_offset = |time: DateTime<Tz>| {
        offset_seconds.is_none_or(|offset| time.offset().fix().local_minus_utc() == offset)
    };
    let time = match zone(time_zone)?.from_local_datetime(&local) {
        LocalResult::Single(time) if matches_offset(time) => time,
        LocalResult::Ambiguous(first, second) if offset_seconds.is_some() => {
            if matches_offset(first) {
                first
            } else if matches_offset(second) {
                second
            } else {
                return Err(CalendarError::Invalid("offset does not match time zone"));
            }
        }
        LocalResult::None => return Err(CalendarError::Invalid("local time does not exist")),
        LocalResult::Ambiguous(_, _) => {
            return Err(CalendarError::Invalid(
                "ambiguous local time requires an offset",
            ));
        }
        _ => return Err(CalendarError::Invalid("offset does not match time zone")),
    };
    Ok(time.with_timezone(&Utc))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum EventTime {
    Timed {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        time_zone: String,
    },
    AllDay {
        start_date: NaiveDate,
        end_date: NaiveDate,
        time_zone: String,
    },
}

/// The write shape retains offsets until their consistency with the zone is
/// checked. Canonical state and responses use UTC instants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum EventTimeInput {
    Timed {
        start: DateTime<FixedOffset>,
        end: DateTime<FixedOffset>,
        time_zone: String,
    },
    AllDay {
        start_date: NaiveDate,
        end_date: NaiveDate,
        time_zone: String,
    },
}

impl EventTimeInput {
    pub fn normalize(self) -> Result<EventTime, CalendarError> {
        let value = match self {
            Self::Timed {
                start,
                end,
                time_zone,
            } => {
                let start = resolve_local_time(
                    start.naive_local(),
                    &time_zone,
                    Some(start.offset().local_minus_utc()),
                )?;
                let end = resolve_local_time(
                    end.naive_local(),
                    &time_zone,
                    Some(end.offset().local_minus_utc()),
                )?;
                EventTime::Timed {
                    start,
                    end,
                    time_zone,
                }
            }
            Self::AllDay {
                start_date,
                end_date,
                time_zone,
            } => EventTime::AllDay {
                start_date,
                end_date,
                time_zone,
            },
        };
        value.validate()?;
        Ok(value)
    }
}

impl EventTime {
    pub fn validate(&self) -> Result<(), CalendarError> {
        match self {
            Self::Timed {
                start,
                end,
                time_zone,
            } => {
                zone(time_zone)?;
                if start.timestamp_subsec_nanos() >= 1_000_000_000
                    || end.timestamp_subsec_nanos() >= 1_000_000_000
                {
                    return Err(CalendarError::Invalid(
                        "leap-second timestamps are not supported",
                    ));
                }
                if start.timestamp_subsec_nanos() % 1000 != 0
                    || end.timestamp_subsec_nanos() % 1000 != 0
                {
                    return Err(CalendarError::Invalid(
                        "event times must use microsecond precision",
                    ));
                }
                if !(1..=9999).contains(&start.year()) || !(1..=9999).contains(&end.year()) {
                    return Err(CalendarError::Invalid(
                        "event dates must be in years 1 to 9999",
                    ));
                }
                if end <= start {
                    return Err(CalendarError::Invalid("end must be after start"));
                }
            }
            Self::AllDay {
                start_date,
                end_date,
                time_zone,
            } => {
                zone(time_zone)?;
                if !(1..=9999).contains(&start_date.year())
                    || !(1..=9999).contains(&end_date.year())
                {
                    return Err(CalendarError::Invalid(
                        "event dates must be in years 1 to 9999",
                    ));
                }
                if end_date <= start_date {
                    return Err(CalendarError::Invalid("end date must be after start date"));
                }
            }
        }
        Ok(())
    }

    pub fn same_schedule(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Timed { start, end, .. },
                Self::Timed {
                    start: new_start,
                    end: new_end,
                    ..
                },
            ) => start == new_start && end == new_end,
            _ => self == other,
        }
    }

    pub fn reminder_target(&self, offset_minutes: u32) -> Result<DateTime<Utc>, CalendarError> {
        if offset_minutes > MAX_REMINDER_MINUTES {
            return Err(CalendarError::Invalid(
                "reminder offset exceeds 40320 minutes",
            ));
        }
        let start = match self {
            Self::Timed { start, .. } => *start,
            Self::AllDay {
                start_date,
                time_zone,
                ..
            } => resolve_local_time(
                start_date
                    .and_hms_opt(9, 0, 0)
                    .expect("09:00 is a valid time"),
                time_zone,
                None,
            )?,
        };
        start
            .checked_sub_signed(Duration::minutes(i64::from(offset_minutes)))
            .ok_or(CalendarError::Invalid("reminder target is out of range"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Calendar {
    pub title: String,
    pub description: Option<String>,
    pub color: String,
    pub archived: bool,
}

impl Calendar {
    pub fn validate(&self) -> Result<(), CalendarError> {
        title(&self.title)?;
        if self
            .description
            .as_ref()
            .is_some_and(|value| value.len() > 10_000)
        {
            return Err(CalendarError::Invalid(
                "calendar description exceeds 10000 bytes",
            ));
        }
        if self.color.len() != 7
            || !self.color.starts_with('#')
            || !self.color[1..].bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(CalendarError::Invalid("color must be #RRGGBB"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AttendeeRole {
    Required,
    Optional,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Response {
    NeedsAction,
    Accepted,
    Tentative,
    Declined,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttendeeInput {
    pub user_id: String,
    pub role: AttendeeRole,
}

/// The authority lifetime established when an invitation is created. Repository
/// removal must not silently convert a membership-bound invitation into a share.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InvitationAdmission {
    #[default]
    RepositoryMembership,
    EventParticipation,
    RemoteParticipation,
}

impl InvitationAdmission {
    fn is_repository_membership(&self) -> bool {
        *self == Self::RepositoryMembership
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Attendee {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_response_epoch: Option<u64>,
    pub user_id: String,
    #[serde(
        default,
        skip_serializing_if = "InvitationAdmission::is_repository_membership"
    )]
    pub admission: InvitationAdmission,
    pub role: AttendeeRole,
    pub response: Response,
    pub invitation_generation: u64,
    pub response_revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reminder {
    pub reminder_id: u64,
    pub minutes_before_start: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct Reminders {
    pub counter: u64,
    pub entries: Vec<Reminder>,
}

impl Reminders {
    pub fn replace(&mut self, offsets: &[u32]) -> Result<bool, CalendarError> {
        let mut offsets = offsets.to_vec();
        offsets.sort_unstable();
        if offsets.iter().any(|offset| *offset > MAX_REMINDER_MINUTES) {
            return Err(CalendarError::Invalid(
                "reminder offset exceeds 40320 minutes",
            ));
        }
        if offsets.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(CalendarError::Invalid("duplicate reminder offsets"));
        }
        let previous: BTreeMap<_, _> = self
            .entries
            .iter()
            .map(|entry| (entry.minutes_before_start, entry.reminder_id))
            .collect();
        if previous.keys().copied().eq(offsets.iter().copied()) {
            return Ok(false);
        }
        let mut counter = self.counter;
        let mut entries = Vec::with_capacity(offsets.len());
        for offset in offsets {
            let id = match previous.get(&offset) {
                Some(id) => *id,
                None => advance(&mut counter)?,
            };
            entries.push(Reminder {
                reminder_id: id,
                minutes_before_start: offset,
            });
        }
        self.counter = counter;
        self.entries = entries;
        Ok(true)
    }

    pub fn validate(&self, time: &EventTime) -> Result<(), CalendarError> {
        if self.counter > MAX_COUNTER {
            return Err(CalendarError::Invalid("invalid reminder counter"));
        }
        let mut ids = BTreeSet::new();
        let mut offsets = BTreeSet::new();
        for entry in &self.entries {
            if entry.reminder_id == 0
                || entry.reminder_id > self.counter
                || !ids.insert(entry.reminder_id)
                || !offsets.insert(entry.minutes_before_start)
            {
                return Err(CalendarError::Invalid("invalid reminder identity"));
            }
            time.reminder_target(entry.minutes_before_start)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventState {
    Scheduled,
    Canceled,
}

/// An RSVP override belongs to the current invitation and original occurrence.
/// Its revision is the attendee's existing response revision, shared by scopes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OccurrenceResponse {
    pub original_start: DateTime<Utc>,
    pub user_id: String,
    pub response: Response,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OccurrenceAttendees {
    pub original_start: DateTime<Utc>,
    pub attendees: Vec<Attendee>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SeriesLink {
    pub event_item_id: ItemId,
    pub original_start: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CalendarEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_type_id: Option<uuid::Uuid>,
    pub calendar_item_id: ItemId,
    pub case_item_id: Option<ItemId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_item_ids: Vec<ItemId>,
    pub title: String,
    pub time: EventTime,
    pub location: Option<String>,
    pub meeting_url: Option<String>,
    pub organizer_user_id: String,
    pub state: EventState,
    pub attendees: Vec<Attendee>,
    pub invitation_counter: u64,
    pub reminders: Reminders,
    pub reminder_schedule_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<recurrence::Recurrence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<recurrence::OccurrenceException>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub occurrence_responses: Vec<OccurrenceResponse>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub occurrence_attendees: Vec<OccurrenceAttendees>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub occurrence_details: Vec<occurrence_details::Override>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub occurrence_descriptions: Vec<occurrence_details::DescriptionSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canceled_from: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_from: Option<SeriesLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_to: Option<SeriesLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<crate::external_scheduling::state::ExternalScheduling>,
}

impl CalendarEvent {
    pub fn is_externally_organized_repository_event(&self) -> bool {
        matches!(
            self.external,
            Some(crate::external_scheduling::ExternalScheduling::ExternalOrganizer { .. })
        )
    }

    pub fn new(
        calendar_item_id: ItemId,
        title: String,
        time: EventTime,
        organizer_user_id: String,
    ) -> Result<Self, CalendarError> {
        let event = Self {
            event_type_id: None,
            calendar_item_id,
            case_item_id: None,
            attachment_item_ids: Vec::new(),
            title,
            time,
            location: None,
            meeting_url: None,
            organizer_user_id,
            state: EventState::Scheduled,
            attendees: Vec::new(),
            invitation_counter: 0,
            reminders: Reminders::default(),
            reminder_schedule_generation: 0,
            recurrence: None,
            exceptions: Vec::new(),
            occurrence_responses: Vec::new(),
            occurrence_attendees: Vec::new(),
            occurrence_details: Vec::new(),
            occurrence_descriptions: Vec::new(),
            canceled_from: None,
            split_from: None,
            split_to: None,
            external: None,
        };
        event.validate()?;
        Ok(event)
    }

    pub fn validate(&self) -> Result<(), CalendarError> {
        title(&self.title)?;
        user_id(&self.organizer_user_id)?;
        if let Some(external) = &self.external {
            external
                .validate()
                .map_err(|_| CalendarError::Invalid("invalid external scheduling state"))?;
        }
        self.validate_occurrence_details()?;
        self.validate_occurrence_descriptions()?;
        if let Some(start) = self.canceled_from {
            if self.occurrence_at(start)?.is_none() {
                return Err(CalendarError::Invalid(
                    "cancellation boundary must identify an occurrence",
                ));
            }
        }
        if self
            .split_from
            .as_ref()
            .zip(self.split_to.as_ref())
            .is_some_and(|(previous, next)| previous.event_item_id == next.event_item_id)
        {
            return Err(CalendarError::Invalid("invalid series split linkage"));
        }
        self.time.validate()?;
        self.reminders.validate(&self.time)?;
        match &self.recurrence {
            Some(rule) => {
                rule.bounds(&self.time, &self.exceptions, self.canceled_from)?;
            }
            None if !self.exceptions.is_empty() => {
                return Err(CalendarError::Invalid(
                    "exceptions require a recurring event",
                ));
            }
            None => {}
        }
        for exception in &self.exceptions {
            if exception
                .revision
                .is_some_and(|r| r == 0 || r > self.invitation_counter)
            {
                return Err(CalendarError::Invalid("invalid occurrence revision"));
            }
            if let Some(time) = &exception.time {
                self.reminders.validate(time)?;
            }
        }
        if self
            .location
            .as_ref()
            .is_some_and(|value| value.len() > MAX_LOCATION_BYTES)
        {
            return Err(CalendarError::Invalid("location exceeds 2048 bytes"));
        }
        if let Some(value) = &self.meeting_url {
            let url = url::Url::parse(value)
                .map_err(|_| CalendarError::Invalid("invalid meeting URL"))?;
            if value.len() > MAX_MEETING_URL_BYTES
                || !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(CalendarError::Invalid(
                    "meeting URL must be absolute HTTP(S) without credentials",
                ));
            }
        }
        if self.attendees.len() > MAX_ATTENDEES {
            return Err(CalendarError::Invalid("event exceeds 500 attendees"));
        }
        if self.attachment_item_ids.len() > MAX_ATTACHMENTS {
            return Err(CalendarError::Invalid("event exceeds 20 attachments"));
        }
        let mut attachments = BTreeSet::new();
        for item_id in &self.attachment_item_ids {
            let uuid = uuid::Uuid::parse_str(item_id.as_str())
                .map_err(|_| CalendarError::Invalid("invalid attachment identity"))?;
            if uuid.get_version_num() != 7 || !attachments.insert(item_id) {
                return Err(CalendarError::Invalid("invalid attachment identity"));
            }
        }
        if self.invitation_counter > MAX_COUNTER || self.reminder_schedule_generation > MAX_COUNTER
        {
            return Err(CalendarError::Invalid("invalid event counter"));
        }
        let mut users = BTreeSet::new();
        for attendee in &self.attendees {
            user_id(&attendee.user_id)?;
            if !users.insert(&attendee.user_id)
                || attendee.invitation_generation == 0
                || attendee.invitation_generation > self.invitation_counter
                || attendee.response_revision > MAX_COUNTER
                || attendee
                    .series_response_epoch
                    .is_some_and(|e| e > self.invitation_counter)
            {
                return Err(CalendarError::Invalid("invalid attendee identity"));
            }
        }
        if self.occurrence_attendees.len() > 512
            || self
                .occurrence_attendees
                .windows(2)
                .any(|pair| pair[0].original_start >= pair[1].original_start)
        {
            return Err(CalendarError::Invalid(
                "invalid occurrence attendee identities",
            ));
        }
        if !self.occurrence_attendees.is_empty() {
            let rule = self.recurrence.as_ref().ok_or(CalendarError::Invalid(
                "occurrence attendees require recurrence",
            ))?;
            let recurrence_identities: Vec<_> = self
                .occurrence_attendees
                .iter()
                .map(|entry| recurrence::OccurrenceException {
                    original_start: entry.original_start,
                    revision: None,
                    canceled: false,
                    time: None,
                })
                .collect();
            rule.validate_exceptions(&self.time, &recurrence_identities)?;
            for entry in &self.occurrence_attendees {
                if entry.attendees.len() > MAX_ATTENDEES {
                    return Err(CalendarError::Invalid("event exceeds 500 attendees"));
                }
                let mut scoped = BTreeSet::new();
                for attendee in &entry.attendees {
                    user_id(&attendee.user_id)?;
                    if !scoped.insert(&attendee.user_id)
                        || attendee.invitation_generation == 0
                        || attendee.invitation_generation > self.invitation_counter
                        || attendee.response_revision > MAX_COUNTER
                        || attendee
                            .series_response_epoch
                            .is_some_and(|epoch| epoch > self.invitation_counter)
                    {
                        return Err(CalendarError::Invalid(
                            "invalid occurrence attendee identity",
                        ));
                    }
                }
            }
        }
        if self.occurrence_responses.len() > 512 {
            return Err(CalendarError::Invalid(
                "event exceeds 512 occurrence responses",
            ));
        }
        let mut identities = BTreeSet::new();
        let mut starts = BTreeSet::new();
        for response in &self.occurrence_responses {
            if !self
                .occurrence_attendees(Some(response.original_start))
                .iter()
                .any(|attendee| attendee.user_id == response.user_id)
                || response.response == Response::NeedsAction
                || !identities.insert((response.original_start, &response.user_id))
            {
                return Err(CalendarError::Invalid("invalid occurrence response"));
            }
            starts.insert(response.original_start);
        }
        if !starts.is_empty() {
            let rule = self.recurrence.as_ref().ok_or(CalendarError::Invalid(
                "occurrence responses require recurrence",
            ))?;
            let identities: Vec<_> = starts
                .into_iter()
                .map(|original_start| recurrence::OccurrenceException {
                    revision: None,
                    canceled: false,
                    original_start,
                    time: None,
                })
                .collect();
            rule.validate_exceptions(&self.time, &identities)?;
        }
        Ok(())
    }

    /// Caller resolves/authorizes the occurrence separately. This never grants access.
    pub fn occurrence_response(
        &self,
        user: &str,
        start: Option<DateTime<Utc>>,
    ) -> Option<Response> {
        let attendee = self
            .occurrence_attendees(start)
            .iter()
            .find(|a| a.user_id == user)?;
        Some(
            start
                .and_then(|start| {
                    self.occurrence_responses
                        .iter()
                        .find(|r| r.original_start == start && r.user_id == user)
                        .map(|r| r.response)
                })
                .unwrap_or_else(|| self.inherited_occurrence_response(attendee, start)),
        )
    }

    fn inherited_occurrence_response(
        &self,
        attendee: &Attendee,
        start: Option<DateTime<Utc>>,
    ) -> Response {
        let revision = start.and_then(|start| {
            self.exceptions
                .binary_search_by_key(&start, |e| e.original_start)
                .ok()
                .and_then(|i| self.exceptions[i].revision)
        });
        if revision.is_some_and(|r| {
            r > attendee
                .series_response_epoch
                .unwrap_or(attendee.invitation_generation)
        }) {
            Response::NeedsAction
        } else {
            attendee.response
        }
    }

    /// Reading an update is not an RSVP. An accepted series can still contain
    /// a moved occurrence awaiting a new response.
    pub fn pending_occurrence(&self, user: &str) -> Option<&recurrence::OccurrenceException> {
        let attendee = self.attendees.iter().find(|a| a.user_id == user)?;
        if self.state != EventState::Scheduled || attendee.response == Response::NeedsAction {
            return None;
        }
        let epoch = attendee
            .series_response_epoch
            .unwrap_or(attendee.invitation_generation);
        self.exceptions.iter().find(|exception| {
            self.canceled_from
                .is_none_or(|start| exception.original_start < start)
                && !exception.is_canceled()
                && exception.revision.is_some_and(|r| r > epoch)
                && !self
                    .occurrence_responses
                    .iter()
                    .any(|r| r.user_id == user && r.original_start == exception.original_start)
        })
    }

    pub fn pending_occurrence_start(&self, user: &str) -> Option<DateTime<Utc>> {
        if self.state != EventState::Scheduled
            || self.attendees.iter().any(|attendee| {
                attendee.user_id == user && attendee.response == Response::NeedsAction
            })
        {
            return None;
        }
        self.pending_occurrence(user)
            .map(|entry| entry.original_start)
            .into_iter()
            .chain(self.occurrence_attendees.iter().filter_map(|entry| {
                (self.occurrence_response(user, Some(entry.original_start))
                    == Some(Response::NeedsAction)
                    && self
                        .occurrence_at(entry.original_start)
                        .ok()
                        .flatten()
                        .is_some_and(|(_, state)| state == EventState::Scheduled))
                .then_some(entry.original_start)
            }))
            .min()
    }

    pub fn has_pending_response(&self, user: &str) -> bool {
        self.state == EventState::Scheduled
            && (self
                .attendees
                .iter()
                .any(|a| a.user_id == user && a.response == Response::NeedsAction)
                || self.occurrence_attendees.iter().any(|entry| {
                    self.canceled_from
                        .is_none_or(|boundary| entry.original_start < boundary)
                        && entry.attendees.iter().any(|attendee| {
                            attendee.user_id == user
                                && self.occurrence_response(user, Some(entry.original_start))
                                    == Some(Response::NeedsAction)
                        })
                })
                || self.pending_occurrence(user).is_some())
    }

    pub fn occurrence_schedule_generation(&self, start: Option<DateTime<Utc>>) -> u64 {
        start
            .and_then(|start| {
                self.exceptions
                    .binary_search_by_key(&start, |e| e.original_start)
                    .ok()
                    .and_then(|i| self.exceptions[i].revision)
            })
            .unwrap_or(0)
            .max(self.reminder_schedule_generation)
    }

    pub fn reschedule_occurrence(
        &mut self,
        start: DateTime<Utc>,
        time: EventTime,
    ) -> Result<bool, CalendarError> {
        self.require_scheduled()?;
        let Some((previous, EventState::Scheduled)) = self.occurrence_at(start)? else {
            return Err(CalendarError::Conflict(
                "occurrence no longer exists or is canceled",
            ));
        };
        if previous == time {
            return Ok(false);
        }
        let mut next = self.clone();
        // Share the existing monotonic allocator with invitations. Retained
        // attendees keep their invitation identities and personal preferences.
        let mut revision = self
            .invitation_counter
            .max(self.reminder_schedule_generation);
        advance(&mut revision)?;
        next.invitation_counter = revision;
        let exception = recurrence::OccurrenceException {
            revision: Some(revision),
            canceled: false,
            original_start: start,
            time: Some(time),
        };
        match next
            .exceptions
            .binary_search_by_key(&start, |e| e.original_start)
        {
            Ok(index) => next.exceptions[index] = exception,
            Err(index) => next.exceptions.insert(index, exception),
        }
        next.occurrence_responses
            .retain(|r| r.original_start != start);
        for attendee in &mut next.attendees {
            advance(&mut attendee.response_revision)?;
        }
        if let Ok(index) = next
            .occurrence_attendees
            .binary_search_by_key(&start, |entry| entry.original_start)
        {
            for attendee in &mut next.occurrence_attendees[index].attendees {
                advance(&mut attendee.response_revision)?;
            }
        }
        next.validate()?;
        *self = next;
        Ok(true)
    }
    pub fn respond_occurrence(
        &mut self,
        actor: &str,
        start: DateTime<Utc>,
        generation: u64,
        revision: u64,
        response: Response,
    ) -> Result<bool, CalendarError> {
        self.require_scheduled()?;
        if !matches!(self.occurrence_at(start)?, Some((_, EventState::Scheduled))) {
            return Err(CalendarError::Conflict(
                "occurrence no longer exists or is canceled",
            ));
        }
        if response == Response::NeedsAction {
            return Err(CalendarError::Invalid(
                "choose accepted, tentative, or declined",
            ));
        }
        let attendee = self
            .occurrence_attendees(Some(start))
            .iter()
            .find(|a| a.user_id == actor)
            .ok_or(CalendarError::Conflict("invitation no longer exists"))?;
        if attendee.invitation_generation != generation || attendee.response_revision != revision {
            return Err(CalendarError::Conflict("invitation or response changed"));
        }
        if self.occurrence_response(actor, Some(start)) == Some(response) {
            return Ok(false);
        }
        let inherited = self.inherited_occurrence_response(attendee, Some(start));
        let mut next = self.clone();
        let attendee = match next
            .occurrence_attendees
            .binary_search_by_key(&start, |entry| entry.original_start)
        {
            Ok(index) => next.occurrence_attendees[index]
                .attendees
                .iter_mut()
                .find(|attendee| attendee.user_id == actor)
                .unwrap(),
            Err(_) => next
                .attendees
                .iter_mut()
                .find(|attendee| attendee.user_id == actor)
                .unwrap(),
        };
        advance(&mut attendee.response_revision)?;
        next.occurrence_responses
            .retain(|r| r.original_start != start || r.user_id != actor);
        if inherited != response {
            next.occurrence_responses.push(OccurrenceResponse {
                original_start: start,
                user_id: actor.to_owned(),
                response,
            });
            next.occurrence_responses.sort_by(|a, b| {
                (a.original_start, &a.user_id).cmp(&(b.original_start, &b.user_id))
            });
        }
        next.validate()?;
        *self = next;
        Ok(true)
    }

    pub fn occurrence_at(
        &self,
        original_start: DateTime<Utc>,
    ) -> Result<Option<(EventTime, EventState)>, CalendarError> {
        let Some(rule) = &self.recurrence else {
            return Ok(None);
        };
        let day = original_start.date_naive();
        let end = day
            .succ_opt()
            .ok_or(CalendarError::Invalid("invalid occurrence date"))?;
        let Some(original) = rule
            .expand(&self.time, day, end, "UTC")?
            .into_iter()
            .find(|o| o.original_start == original_start)
        else {
            return Ok(None);
        };
        match self
            .exceptions
            .binary_search_by_key(&original_start, |e| e.original_start)
        {
            Ok(index) => {
                let exception = &self.exceptions[index];
                let state = if exception.is_canceled()
                    || self
                        .canceled_from
                        .is_some_and(|start| original_start >= start)
                {
                    EventState::Canceled
                } else {
                    self.state
                };
                Ok(Some((
                    exception.time.clone().unwrap_or(original.time),
                    state,
                )))
            }
            Err(_) => Ok(Some((
                original.time,
                if self
                    .canceled_from
                    .is_some_and(|start| original_start >= start)
                {
                    EventState::Canceled
                } else {
                    self.state
                },
            ))),
        }
    }

    /// Split a recurring event without changing any original occurrence
    /// identity. The tail retains DTSTART/RRULE and excludes the finite prefix.
    pub fn split_following(
        &self,
        source_item_id: ItemId,
        target_item_id: ItemId,
        boundary: DateTime<Utc>,
    ) -> Result<(Self, Self), CalendarError> {
        self.require_scheduled()?;
        let rule = self.recurrence.as_ref().ok_or(CalendarError::Conflict(
            "only a recurring event can be split",
        ))?;
        if !matches!(
            self.occurrence_at(boundary)?,
            Some((_, EventState::Scheduled))
        ) {
            return Err(CalendarError::Conflict(
                "split boundary no longer exists or is canceled",
            ));
        }
        let prefix = rule.starts_before_split(&self.time, boundary)?;
        if prefix.len() + rule.exdates.len() > recurrence::MAX_EXPLICIT_DATES {
            return Err(CalendarError::ExpansionLimit);
        }

        let mut source = self.clone();
        source.cancel_following(boundary)?;
        source.split_to = Some(SeriesLink {
            event_item_id: target_item_id,
            original_start: boundary,
        });

        let mut tail = self.clone();
        let tail_rule = tail.recurrence.as_mut().expect("checked recurrence");
        let mut excluded: BTreeSet<_> = tail_rule.exdates.iter().copied().collect();
        excluded.extend(prefix);
        tail_rule.exdates = excluded.into_iter().collect();
        tail_rule.rdates.retain(|start| *start >= boundary);
        tail.exceptions
            .retain(|entry| entry.original_start >= boundary);
        tail.occurrence_responses
            .retain(|entry| entry.original_start >= boundary);
        tail.occurrence_attendees
            .retain(|entry| entry.original_start >= boundary);
        tail.occurrence_details
            .retain(|entry| entry.original_start >= boundary);
        tail.occurrence_descriptions
            .retain(|entry| entry.original_start >= boundary);
        for source in &mut tail.occurrence_descriptions {
            if source.source_item_id.is_none() {
                source.source_item_id = Some(source_item_id.clone());
            }
        }
        tail.canceled_from = None;
        tail.split_from = Some(SeriesLink {
            event_item_id: source_item_id,
            original_start: boundary,
        });
        tail.split_to = None;
        source.validate()?;
        tail.validate()?;
        Ok((source, tail))
    }

    /// Shared index envelope for repository and personal agenda projections.
    pub fn range_bounds(&self) -> Result<EventTime, CalendarError> {
        match &self.recurrence {
            Some(rule) => rule.bounds(&self.time, &self.exceptions, self.canceled_from),
            None => Ok(self.time.clone()),
        }
    }

    pub fn require_scheduled(&self) -> Result<(), CalendarError> {
        if self.state == EventState::Canceled {
            Err(CalendarError::Canceled)
        } else {
            Ok(())
        }
    }

    pub fn set_attendees(&mut self, inputs: &[AttendeeInput]) -> Result<bool, CalendarError> {
        self.set_attendees_with_participant_access(inputs, &BTreeSet::new())
    }

    pub fn occurrence_attendees(&self, start: Option<DateTime<Utc>>) -> &[Attendee] {
        start
            .and_then(|start| {
                self.occurrence_attendees
                    .binary_search_by_key(&start, |entry| entry.original_start)
                    .ok()
                    .map(|index| self.occurrence_attendees[index].attendees.as_slice())
            })
            .unwrap_or(&self.attendees)
    }

    pub fn set_occurrence_attendees(
        &mut self,
        start: DateTime<Utc>,
        inputs: &[AttendeeInput],
        participants: &BTreeSet<String>,
    ) -> Result<bool, CalendarError> {
        self.require_scheduled()?;
        if !matches!(self.occurrence_at(start)?, Some((_, EventState::Scheduled))) {
            return Err(CalendarError::Conflict(
                "occurrence no longer exists or is canceled",
            ));
        }
        let current = self.occurrence_attendees(Some(start)).to_vec();
        let mut candidate = self.clone();
        candidate.attendees = current.clone();
        candidate.occurrence_responses.clear();
        candidate.set_attendees_with_participant_access(inputs, participants)?;
        let attendees = candidate.attendees;
        if attendees == current {
            return Ok(false);
        }
        self.invitation_counter = candidate.invitation_counter;
        match self
            .occurrence_attendees
            .binary_search_by_key(&start, |entry| entry.original_start)
        {
            Ok(index) => self.occurrence_attendees[index].attendees = attendees,
            Err(index) => self.occurrence_attendees.insert(
                index,
                OccurrenceAttendees {
                    original_start: start,
                    attendees,
                },
            ),
        }
        let users: BTreeSet<_> = self
            .occurrence_attendees(Some(start))
            .iter()
            .map(|attendee| attendee.user_id.to_owned())
            .collect();
        self.occurrence_responses.retain(|response| {
            response.original_start != start || users.contains(&response.user_id)
        });
        self.validate()?;
        Ok(true)
    }

    /// The caller establishes recipient authority. This method records that
    /// decision only for new invitations; edits cannot upgrade a retained one.
    pub fn set_attendees_with_participant_access(
        &mut self,
        inputs: &[AttendeeInput],
        participants: &BTreeSet<String>,
    ) -> Result<bool, CalendarError> {
        self.require_scheduled()?;
        if inputs.len() > MAX_ATTENDEES || participants.len() > MAX_ATTENDEES {
            return Err(CalendarError::Invalid("event exceeds 500 attendees"));
        }
        let previous: BTreeMap<_, _> = self
            .attendees
            .iter()
            .map(|a| (a.user_id.as_str(), a))
            .collect();
        let mut users = BTreeSet::new();
        let mut counter = self.invitation_counter;
        let mut attendees = Vec::with_capacity(inputs.len());
        let mut inputs: Vec<_> = inputs.iter().collect();
        inputs.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        for input in inputs {
            user_id(&input.user_id)?;
            if !users.insert(input.user_id.as_str()) {
                return Err(CalendarError::Invalid("duplicate attendees"));
            }
            let mut attendee = match previous.get(input.user_id.as_str()) {
                Some(previous) => {
                    if participants.contains(&input.user_id)
                        && previous.admission != InvitationAdmission::EventParticipation
                    {
                        return Err(CalendarError::Conflict(
                            "invitation admission cannot change in place",
                        ));
                    }
                    (*previous).clone()
                }
                None => Attendee {
                    series_response_epoch: None,
                    user_id: input.user_id.clone(),
                    admission: if participants.contains(&input.user_id) {
                        InvitationAdmission::EventParticipation
                    } else {
                        InvitationAdmission::RepositoryMembership
                    },
                    role: input.role,
                    response: Response::NeedsAction,
                    invitation_generation: advance(&mut counter)?,
                    response_revision: 0,
                },
            };
            attendee.role = input.role;
            attendees.push(attendee);
        }
        if participants.iter().any(|id| !users.contains(id.as_str())) {
            return Err(CalendarError::Invalid(
                "participant admission requires an attendee",
            ));
        }
        attendees.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        if attendees == self.attendees {
            return Ok(false);
        }
        self.occurrence_responses.retain(|response| {
            users.contains(response.user_id.as_str())
                || self
                    .occurrence_attendees
                    .binary_search_by_key(&response.original_start, |entry| entry.original_start)
                    .ok()
                    .is_some_and(|index| {
                        self.occurrence_attendees[index]
                            .attendees
                            .iter()
                            .any(|attendee| attendee.user_id == response.user_id)
                    })
        });
        self.attendees = attendees;
        self.invitation_counter = counter;
        Ok(true)
    }

    pub fn respond(
        &mut self,
        actor: &str,
        generation: u64,
        revision: u64,
        response: Response,
    ) -> Result<bool, CalendarError> {
        self.require_scheduled()?;
        if response == Response::NeedsAction {
            return Err(CalendarError::Invalid(
                "choose accepted, tentative, or declined",
            ));
        }
        let epoch = self.exceptions.iter().filter_map(|e| e.revision).max();
        let attendee = self
            .attendees
            .iter_mut()
            .find(|a| a.user_id == actor)
            .ok_or(CalendarError::Conflict("invitation no longer exists"))?;
        if attendee.invitation_generation != generation || attendee.response_revision != revision {
            return Err(CalendarError::Conflict("invitation or response changed"));
        }
        if attendee.response == response
            && !self.occurrence_responses.iter().any(|r| r.user_id == actor)
            && epoch.is_none_or(|e| {
                attendee
                    .series_response_epoch
                    .unwrap_or(attendee.invitation_generation)
                    >= e
            })
        {
            return Ok(false);
        }
        advance(&mut attendee.response_revision)?;
        attendee.response = response;
        attendee.series_response_epoch = epoch.map(|_| self.invitation_counter);
        self.occurrence_responses.retain(|r| r.user_id != actor);
        Ok(true)
    }

    pub fn set_time(&mut self, time: EventTime) -> Result<bool, CalendarError> {
        self.set_schedule(time, self.recurrence.clone(), self.exceptions.clone())
    }

    pub fn set_schedule(
        &mut self,
        time: EventTime,
        recurrence: Option<recurrence::Recurrence>,
        exceptions: Vec<recurrence::OccurrenceException>,
    ) -> Result<bool, CalendarError> {
        self.require_scheduled()?;
        for exception in &exceptions {
            if exception.revision.is_some()
                && !self.exceptions.iter().any(|old| {
                    old.original_start == exception.original_start
                        && old.revision == exception.revision
                })
            {
                return Err(CalendarError::Invalid(
                    "occurrence revision requires scoped rescheduling",
                ));
            }
        }
        if time == self.time && recurrence == self.recurrence && exceptions == self.exceptions {
            return Ok(false);
        }
        let mut candidate = self.clone();
        candidate.time = time.clone();
        candidate.recurrence = recurrence.clone();
        candidate.exceptions = exceptions.clone();
        if time != self.time || recurrence != self.recurrence {
            let identities: BTreeSet<_> = self
                .exceptions
                .iter()
                .map(|entry| entry.original_start)
                .chain(
                    self.occurrence_attendees
                        .iter()
                        .map(|entry| entry.original_start),
                )
                .chain(
                    self.occurrence_details
                        .iter()
                        .map(|entry| entry.original_start),
                )
                .chain(
                    self.occurrence_descriptions
                        .iter()
                        .map(|entry| entry.original_start),
                )
                .collect();
            let retained = match &recurrence {
                Some(rule) => rule.retained_starts(&time, &identities)?,
                None => BTreeSet::new(),
            };
            // Only retire already accepted overrides. Invalid newly supplied
            // exceptions must still fail validation, including initial history.
            candidate.exceptions.retain(|entry| {
                retained.contains(&entry.original_start)
                    || !self
                        .exceptions
                        .iter()
                        .any(|old| old.original_start == entry.original_start)
            });
            candidate
                .occurrence_attendees
                .retain(|entry| retained.contains(&entry.original_start));
            candidate
                .occurrence_details
                .retain(|entry| retained.contains(&entry.original_start));
            candidate
                .occurrence_descriptions
                .retain(|entry| retained.contains(&entry.original_start));
        }
        if !self.time.same_schedule(&time)
            || (recurrence.is_some() && time != self.time)
            || recurrence != self.recurrence
            || exceptions != self.exceptions
        {
            let mut generation = self
                .exceptions
                .iter()
                .filter_map(|e| e.revision)
                .max()
                .unwrap_or(0)
                .max(self.reminder_schedule_generation);
            advance(&mut generation)?;
            candidate.reminder_schedule_generation = generation;
            for attendee in &mut candidate.attendees {
                attendee.invitation_generation = advance(&mut candidate.invitation_counter)?;
                attendee.response_revision = 0;
                attendee.series_response_epoch = None;
                attendee.response = Response::NeedsAction;
            }
            candidate.occurrence_responses.clear();
        }
        candidate.validate()?;
        *self = candidate;
        Ok(true)
    }

    pub fn set_reminders(&mut self, offsets: &[u32]) -> Result<bool, CalendarError> {
        self.require_scheduled()?;
        let mut reminders = self.reminders.clone();
        let changed = reminders.replace(offsets)?;
        reminders.validate(&self.time)?;
        self.reminders = reminders;
        Ok(changed)
    }

    /// Retain the original identity and all RSVP state. Cancellation does not
    /// reschedule the other occurrences or allocate new reminder identities.
    pub fn cancel_occurrence(&mut self, start: DateTime<Utc>) -> Result<bool, CalendarError> {
        self.require_scheduled()?;
        match self.occurrence_at(start)? {
            None => return Err(CalendarError::Conflict("occurrence no longer exists")),
            Some((_, EventState::Canceled)) => return Ok(false),
            Some((_, EventState::Scheduled)) => {}
        }
        let mut next = self.clone();
        match next
            .exceptions
            .binary_search_by_key(&start, |e| e.original_start)
        {
            Ok(index) => next.exceptions[index].canceled = true,
            Err(index) => next.exceptions.insert(
                index,
                recurrence::OccurrenceException {
                    revision: None,
                    canceled: false,
                    original_start: start,
                    time: None,
                },
            ),
        }
        next.validate()?;
        *self = next;
        Ok(true)
    }

    /// Keep the old tail in history; cancellation authority only moves earlier.
    pub fn cancel_following(&mut self, start: DateTime<Utc>) -> Result<bool, CalendarError> {
        self.require_scheduled()?;
        if self.occurrence_at(start)?.is_none() {
            return Err(CalendarError::Conflict("occurrence no longer exists"));
        }
        if self.canceled_from.is_some_and(|old| old <= start) {
            return Ok(false);
        }
        self.canceled_from = Some(start);
        Ok(true)
    }

    pub fn cancel(&mut self) -> Result<bool, CalendarError> {
        if self.state == EventState::Canceled {
            return Ok(false);
        }
        advance(&mut self.reminder_schedule_generation)?;
        self.state = EventState::Canceled;
        Ok(true)
    }

    /// Transfer future meeting accountability without pretending the departing
    /// organizer authored the change. The route admitting this transition is
    /// repository-admin-only; history retains both identities.
    pub fn transfer_organizer(
        &mut self,
        departing: &str,
        successor: &str,
    ) -> Result<bool, CalendarError> {
        self.require_scheduled()?;
        user_id(successor)?;
        if departing == successor || self.organizer_user_id != departing {
            return Err(CalendarError::Invalid("invalid organizer succession"));
        }
        if self.external.is_some() {
            return Err(CalendarError::Invalid(
                "external organizer identity cannot be transferred",
            ));
        }
        self.organizer_user_id = successor.to_owned();
        self.attendees.retain(|entry| entry.user_id != successor);
        for entry in &mut self.occurrence_attendees {
            entry
                .attendees
                .retain(|attendee| attendee.user_id != successor);
        }
        self.occurrence_responses
            .retain(|entry| entry.user_id != successor);
        advance(&mut self.reminder_schedule_generation)?;
        for attendee in &mut self.attendees {
            attendee.invitation_generation = advance(&mut self.invitation_counter)?;
            attendee.response_revision = 0;
            attendee.series_response_epoch = None;
            attendee.response = Response::NeedsAction;
        }
        self.occurrence_responses.clear();
        self.validate()?;
        Ok(true)
    }
}

pub fn reminder_is_due(target: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now >= target && now.signed_duration_since(target) <= Duration::minutes(REMINDER_GRACE_MINUTES)
}

#[cfg(test)]
mod tests;
