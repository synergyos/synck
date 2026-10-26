//! Per-occurrence presentation overrides. These do not change attendance or alarms.
use super::*;

/// Deserialize a present field without collapsing explicit null into absence.
pub fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Details {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub location: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub meeting_url: Option<Option<String>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResolvedDetails {
    pub title: String,
    pub location: Option<String>,
    pub meeting_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Override {
    pub original_start: DateTime<Utc>,
    pub revision: u64,
    pub details: Details,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub original_start: DateTime<Utc>,
    pub time: Option<EventTime>,
    pub details: Details,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attendees: Option<Vec<Attendee>>,
}

impl Details {
    fn validate(&self) -> Result<(), CalendarError> {
        if let Some(value) = &self.title {
            title(value)?;
        }
        if self
            .location
            .as_ref()
            .and_then(|v| v.as_ref())
            .is_some_and(|v| v.len() > MAX_LOCATION_BYTES)
        {
            return Err(CalendarError::Invalid("location exceeds 2048 bytes"));
        }
        if let Some(Some(value)) = &self.meeting_url {
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
        Ok(())
    }
}

impl CalendarEvent {
    pub fn occurrence_details(&self, start: Option<DateTime<Utc>>) -> ResolvedDetails {
        let saved = start.and_then(|start| {
            self.occurrence_details
                .binary_search_by_key(&start, |d| d.original_start)
                .ok()
                .map(|i| &self.occurrence_details[i].details)
        });
        ResolvedDetails {
            title: saved
                .and_then(|d| d.title.clone())
                .unwrap_or_else(|| self.title.clone()),
            location: saved
                .and_then(|d| d.location.clone())
                .unwrap_or_else(|| self.location.clone()),
            meeting_url: saved
                .and_then(|d| d.meeting_url.clone())
                .unwrap_or_else(|| self.meeting_url.clone()),
        }
    }

    pub(super) fn validate_occurrence_details(&self) -> Result<(), CalendarError> {
        if self.occurrence_details.len() > 512
            || self
                .occurrence_details
                .windows(2)
                .any(|w| w[0].original_start >= w[1].original_start)
        {
            return Err(CalendarError::Invalid(
                "invalid occurrence detail identities",
            ));
        }
        if !self.occurrence_details.is_empty() {
            let rule = self.recurrence.as_ref().ok_or(CalendarError::Invalid(
                "occurrence details require a series",
            ))?;
            let identities: Vec<_> = self
                .occurrence_details
                .iter()
                .map(|d| recurrence::OccurrenceException {
                    original_start: d.original_start,
                    revision: None,
                    canceled: false,
                    time: None,
                })
                .collect();
            rule.validate_exceptions(&self.time, &identities)?;
        }
        for entry in &self.occurrence_details {
            if entry.revision == 0 || entry.revision > self.invitation_counter {
                return Err(CalendarError::Invalid(
                    "invalid occurrence detail revision or identity",
                ));
            }
            entry.details.validate()?;
        }
        Ok(())
    }

    pub fn edit_occurrence(&mut self, input: &Edit) -> Result<bool, CalendarError> {
        self.require_scheduled()?;
        if !matches!(
            self.occurrence_at(input.original_start)?,
            Some((_, EventState::Scheduled))
        ) {
            return Err(CalendarError::Conflict(
                "occurrence no longer exists or is canceled",
            ));
        }
        input.details.validate()?;
        let mut next = self.clone();
        if let Some(time) = &input.time {
            next.reschedule_occurrence(input.original_start, time.clone())?;
        }
        let current = self.occurrence_details(Some(input.original_start));
        // Assign only changed fields. Repeating the current effective value must
        // not silently pin an inherited field or allocate a new revision.
        let mut details = self
            .occurrence_details
            .iter()
            .find(|d| d.original_start == input.original_start)
            .map(|d| d.details.clone())
            .unwrap_or_default();
        let before = details.clone();
        if input
            .details
            .title
            .as_ref()
            .is_some_and(|v| *v != current.title)
        {
            details.title = input.details.title.clone();
        }
        if input
            .details
            .location
            .as_ref()
            .is_some_and(|v| *v != current.location)
        {
            details.location = input.details.location.clone();
        }
        if input
            .details
            .meeting_url
            .as_ref()
            .is_some_and(|v| *v != current.meeting_url)
        {
            details.meeting_url = input.details.meeting_url.clone();
        }
        let details_changed = details != before;
        if details_changed {
            advance(&mut next.invitation_counter)?;
            let entry = Override {
                original_start: input.original_start,
                revision: next.invitation_counter,
                details: details.clone(),
            };
            match next
                .occurrence_details
                .binary_search_by_key(&input.original_start, |d| d.original_start)
            {
                Ok(i) => next.occurrence_details[i] = entry,
                Err(i) => next.occurrence_details.insert(i, entry),
            }
        }
        let mut attendees_changed = false;
        if let Some(requested) = &input.attendees {
            let before_attendees = next
                .occurrence_attendees(Some(input.original_start))
                .to_vec();
            let inputs: Vec<_> = requested
                .iter()
                .map(|attendee| AttendeeInput {
                    user_id: attendee.user_id.clone(),
                    role: attendee.role,
                })
                .collect();
            let participants = requested
                .iter()
                .filter(|attendee| attendee.admission == InvitationAdmission::EventParticipation)
                .map(|attendee| attendee.user_id.clone())
                .collect();
            next.set_occurrence_attendees(input.original_start, &inputs, &participants)?;
            if next.occurrence_attendees(Some(input.original_start)) != requested {
                return Err(CalendarError::Invalid(
                    "invalid occurrence attendee transition",
                ));
            }
            attendees_changed = before_attendees != *requested;
        }
        if attendees_changed && !details_changed {
            advance(&mut next.invitation_counter)?;
            let entry = Override {
                original_start: input.original_start,
                revision: next.invitation_counter,
                details,
            };
            match next
                .occurrence_details
                .binary_search_by_key(&input.original_start, |saved| saved.original_start)
            {
                Ok(index) => next.occurrence_details[index] = entry,
                Err(index) => next.occurrence_details.insert(index, entry),
            }
        }
        next.validate()?;
        let changed = next != *self;
        *self = next;
        Ok(changed)
    }
}

/// Body source is an accepted event after this guarded version, not inline text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DescriptionSource {
    pub original_start: DateTime<Utc>,
    pub base_version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_item_id: Option<ItemId>,
}

impl CalendarEvent {
    pub(super) fn validate_occurrence_descriptions(&self) -> Result<(), CalendarError> {
        if self.occurrence_descriptions.is_empty() {
            return Ok(());
        }
        if self.occurrence_descriptions.len() > 512
            || self
                .occurrence_descriptions
                .windows(2)
                .any(|w| w[0].original_start >= w[1].original_start)
            || self
                .occurrence_descriptions
                .iter()
                .any(|d| d.base_version <= 0 || d.base_version > i64::MAX - 2)
        {
            return Err(CalendarError::Invalid(
                "invalid occurrence description source",
            ));
        }
        let rule = self.recurrence.as_ref().ok_or(CalendarError::Invalid(
            "occurrence descriptions require a series",
        ))?;
        let identities: Vec<_> = self
            .occurrence_descriptions
            .iter()
            .map(|d| recurrence::OccurrenceException {
                original_start: d.original_start,
                revision: None,
                canceled: false,
                time: None,
            })
            .collect();
        rule.validate_exceptions(&self.time, &identities)
    }

    pub fn set_occurrence_description(
        &mut self,
        start: DateTime<Utc>,
        base_version: i64,
    ) -> Result<(), CalendarError> {
        self.require_scheduled()?;
        if !matches!(self.occurrence_at(start)?, Some((_, EventState::Scheduled))) {
            return Err(CalendarError::Conflict(
                "occurrence no longer exists or is canceled",
            ));
        }
        let mut next = self.clone();
        let source = DescriptionSource {
            original_start: start,
            base_version,
            source_item_id: None,
        };
        match next
            .occurrence_descriptions
            .binary_search_by_key(&start, |d| d.original_start)
        {
            Ok(index) => {
                if next.occurrence_descriptions[index].base_version >= base_version {
                    return Err(CalendarError::Invalid(
                        "occurrence description source must advance",
                    ));
                }
                next.occurrence_descriptions[index] = source;
            }
            Err(index) => next.occurrence_descriptions.insert(index, source),
        }
        next.validate()?;
        *self = next;
        Ok(())
    }
}
