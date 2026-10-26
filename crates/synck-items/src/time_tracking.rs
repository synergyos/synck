//! Timekeeping state and replay rules. Authorization and SQL live in the service.
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use synck_vcs::types::{EventProposal, ItemId};

pub const MAX_SECONDS: i64 = 86_400;
pub const MAX_DESCRIPTION: usize = 4_000;
pub const MAX_RATE: i64 = 100_000_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TimeError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
}
pub fn is_kind(kind: &str) -> bool {
    matches!(kind, "TIME_SETTINGS" | "TIME_BOOK" | "TIME_ENTRY")
}
pub fn is_event(event: &str) -> bool {
    event
        .split_once("::")
        .is_some_and(|(kind, _)| is_kind(kind))
}
pub fn event_policy(kind: &str, event: &str) -> Option<crate::MutationPolicy> {
    let (event_kind, verb) = event.split_once("::")?;
    (is_kind(kind) && event_kind == kind && matches!(verb, "CREATE" | "SET_METADATA"))
        .then_some(crate::MutationPolicy::Optimistic)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Label {
    pub id: uuid::Uuid,
    pub name: String,
    pub retired: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub currency: String,
    pub offices: Vec<Label>,
    pub practices: Vec<Label>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            currency: "EUR".into(),
            offices: vec![],
            practices: vec![],
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<(), TimeError> {
        if !matches!(self.currency.as_str(), "EUR" | "USD" | "GBP") {
            return Err(TimeError::Invalid(
                "supported currencies are EUR, USD and GBP",
            ));
        }
        for labels in [&self.offices, &self.practices] {
            if labels.len() > 200 {
                return Err(TimeError::Invalid("too many labels"));
            }
            let mut ids = std::collections::BTreeSet::new();
            for label in labels {
                if label.name.trim().is_empty() || label.name.len() > 128 || !ids.insert(&label.id)
                {
                    return Err(TimeError::Invalid(
                        "labels need unique IDs and 1..128 byte names",
                    ));
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Book {
    pub user_id: String,
    pub display_name: String,
    pub hourly_rate_minor: Option<i64>,
    pub office_id: Option<uuid::Uuid>,
    pub practice_id: Option<uuid::Uuid>,
    pub active_entry_id: Option<ItemId>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum Assignment {
    Internal,
    Case { id: ItemId },
    Company { id: ItemId },
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Classification {
    Billable,
    Nonbillable,
    NoCharge,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Running,
    Paused,
    Draft,
    Submitted,
    Approved,
    Void,
}
impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::Paused => "PAUSED",
            Self::Draft => "DRAFT",
            Self::Submitted => "SUBMITTED",
            Self::Approved => "APPROVED",
            Self::Void => "VOID",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub work_date: NaiveDate,
    pub time_zone: String,
    pub seconds: i64,
    pub description: String,
    pub assignment: Assignment,
    pub classification: Classification,
    pub office_id: Option<uuid::Uuid>,
    pub practice_id: Option<uuid::Uuid>,
    pub hourly_rate_minor: Option<i64>,
    pub rate_override: bool,
}
impl Draft {
    pub fn validate(&self) -> Result<(), TimeError> {
        if self.time_zone.parse::<chrono_tz::Tz>().is_err() {
            return Err(TimeError::Invalid("invalid IANA time zone"));
        }
        if self.seconds < 0
            || self.seconds > MAX_SECONDS
            || self.description.len() > MAX_DESCRIPTION
        {
            return Err(TimeError::Invalid(
                "duration or description exceeds entry limit",
            ));
        }
        if matches!(self.assignment, Assignment::Internal)
            && self.classification != Classification::Nonbillable
        {
            return Err(TimeError::Invalid("internal time must be nonbillable"));
        }
        validate_rate(self.hourly_rate_minor)
    }
}
pub fn validate_rate(rate: Option<i64>) -> Result<(), TimeError> {
    if rate.is_some_and(|rate| !(0..=MAX_RATE).contains(&rate)) {
        return Err(TimeError::Invalid("hourly rate is out of range"));
    }
    Ok(())
}
pub fn value(
    seconds: i64,
    rate: Option<i64>,
    classification: Classification,
) -> Result<Option<i64>, TimeError> {
    if classification != Classification::Billable {
        return Ok(Some(0));
    }
    rate.map(|rate| {
        seconds
            .checked_mul(rate)
            .and_then(|n| n.checked_add(1800))
            .map(|n| n / 3600)
            .ok_or(TimeError::Invalid("time value overflow"))
    })
    .transpose()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Interval {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub book_id: ItemId,
    pub user_id: String,
    pub display_name: String,
    pub draft: Draft,
    pub status: Status,
    pub currency: String,
    pub company_id: Option<ItemId>,
    pub company_name: Option<String>,
    pub case_name: Option<String>,
    pub office_name: Option<String>,
    pub practice_name: Option<String>,
    pub value_minor: Option<i64>,
    pub started_at: Option<DateTime<Utc>>,
    pub intervals: Vec<Interval>,
    pub submitted_at: Option<DateTime<Utc>>,
    pub approved_at: Option<DateTime<Utc>>,
    pub reviewed_by: Option<String>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub reason: Option<String>,
    pub correction_of: Option<ItemId>,
}

impl Entry {
    pub fn validate(&self) -> Result<(), TimeError> {
        // Long-running timers remain stoppable, but cannot be submitted unchanged.
        let mut draft = self.draft.clone();
        if matches!(
            self.status,
            Status::Draft | Status::Running | Status::Paused | Status::Void
        ) {
            draft.seconds = draft.seconds.min(MAX_SECONDS);
        }
        draft.validate()?;
        if self.user_id.is_empty()
            || self.user_id.len() > 256
            || self.draft.seconds < 0
            || self.intervals.len() > 1000
            || self.intervals.iter().any(|i| i.end < i.start)
            || (self.status == Status::Running) != self.started_at.is_some()
            || self.value_minor
                != value(
                    self.draft.seconds,
                    self.draft.hourly_rate_minor,
                    self.draft.classification,
                )?
        {
            return Err(TimeError::Invalid("invalid canonical time entry"));
        }
        if matches!(self.status, Status::Submitted | Status::Approved)
            && (self.draft.seconds == 0
                || self.draft.description.trim().is_empty()
                || self.submitted_at.is_none())
        {
            return Err(TimeError::Invalid(
                "submitted time needs positive duration and description",
            ));
        }
        if self.status == Status::Approved
            && (self.approved_at.is_none()
                || self.reviewed_at.is_none()
                || self.reviewed_by.as_deref().is_none_or(str::is_empty))
        {
            return Err(TimeError::Invalid("approved time requires review evidence"));
        }
        if !matches!(self.currency.as_str(), "EUR" | "USD" | "GBP")
            || self
                .intervals
                .windows(2)
                .any(|pair| pair[0].end > pair[1].start)
        {
            return Err(TimeError::Invalid(
                "invalid currency or overlapping timer intervals",
            ));
        }
        Ok(())
    }
    pub fn stop_interval(&mut self, now: DateTime<Utc>) -> Result<(), TimeError> {
        if let Some(start) = self.started_at.take() {
            let seconds = (now - start).num_seconds();
            if seconds < 0 || self.intervals.len() >= 1000 {
                return Err(TimeError::Invalid("invalid timer interval"));
            }
            self.draft.seconds = self
                .draft
                .seconds
                .checked_add(seconds)
                .ok_or(TimeError::Invalid("timer overflow"))?;
            self.intervals.push(Interval { start, end: now });
        }
        self.value_minor = value(
            self.draft.seconds,
            self.draft.hourly_rate_minor,
            self.draft.classification,
        )?;
        Ok(())
    }
}

/// Reject structural modifiers before accepting or replaying any domain record.
pub fn metadata<T: serde::de::DeserializeOwned>(event: &EventProposal) -> Result<T, TimeError> {
    let (kind, verb) = event
        .event_type
        .split_once("::")
        .ok_or(TimeError::Invalid("invalid time event"))?;
    let id = event
        .item_id
        .as_ref()
        .ok_or(TimeError::Invalid("missing time item id"))?;
    if !is_kind(kind)
        || !matches!(verb, "CREATE" | "SET_METADATA")
        || event.set_parent.is_some()
        || event.set_name.is_some()
        || event.set_content.is_some()
        || event.set_in_trash.is_some()
        || event.set_tombstoned.is_some()
        || event.set_is_federated.is_some()
        || event.set_federation_stream_id.is_some()
    {
        return Err(TimeError::Invalid("time records forbid generic modifiers"));
    }
    if verb == "CREATE" {
        let create = event
            .create_item
            .as_ref()
            .ok_or(TimeError::Invalid("missing create_item"))?;
        if event.base_seq.is_some()
            || create.item_type != kind
            || create.parent_folder_id.is_some()
            || create.name != format!("{}-{id}", kind.to_ascii_lowercase())
        {
            return Err(TimeError::Invalid("invalid time creation"));
        }
    } else if event.create_item.is_some() || event.base_seq.is_none_or(|v| v < 1) {
        return Err(TimeError::Invalid(
            "time edit requires exact positive version",
        ));
    }
    serde_json::from_value(
        event
            .set_metadata
            .clone()
            .ok_or(TimeError::Invalid("missing time metadata"))?,
    )
    .map_err(|_| TimeError::Invalid("invalid time metadata"))
}

pub fn reduce_entry(before: Option<&Entry>, event: &EventProposal) -> Result<Entry, TimeError> {
    let next: Entry = metadata(event)?;
    next.validate()?;
    let Some(before) = before else {
        if event.event_type != "TIME_ENTRY::CREATE"
            || !matches!(next.status, Status::Draft | Status::Running)
        {
            return Err(TimeError::Invalid("invalid initial time state"));
        }
        return Ok(next);
    };
    if event.event_type != "TIME_ENTRY::SET_METADATA"
        || before.book_id != next.book_id
        || before.user_id != next.user_id
        || before.currency != next.currency
        || before.correction_of != next.correction_of
    {
        return Err(TimeError::Invalid("immutable time identity changed"));
    }
    let allowed = match before.status {
        Status::Running => matches!(next.status, Status::Paused | Status::Draft),
        Status::Paused => matches!(next.status, Status::Running | Status::Draft),
        Status::Draft => matches!(
            next.status,
            Status::Draft | Status::Submitted | Status::Void
        ),
        Status::Submitted => matches!(
            next.status,
            Status::Draft | Status::Approved | Status::Submitted
        ),
        Status::Approved => next.status == Status::Void,
        Status::Void => false,
    };
    if !allowed {
        return Err(TimeError::Conflict("invalid time transition"));
    }
    if !matches!(before.status, Status::Draft) {
        let mut retained = next.clone();
        retained.status = before.status;
        retained.started_at = before.started_at;
        retained.intervals = before.intervals.clone();
        retained.draft.seconds = before.draft.seconds;
        retained.value_minor = before.value_minor;
        retained.reviewed_by = before.reviewed_by.clone();
        retained.reviewed_at = before.reviewed_at;
        retained.approved_at = before.approved_at;
        retained.reason = before.reason.clone();
        if before.status == Status::Submitted && next.status == Status::Submitted {
            retained.draft.hourly_rate_minor = before.draft.hourly_rate_minor;
            retained.draft.rate_override = before.draft.rate_override;
            if next.reason.as_deref().is_none_or(|r| r.trim().is_empty()) {
                return Err(TimeError::Invalid("rate adjustment requires a reason"));
            }
        }
        if &retained != before {
            return Err(TimeError::Invalid("time transition changed retained work"));
        }
    }
    if before.status == Status::Approved {
        let mut unchanged = next.clone();
        unchanged.status = before.status;
        unchanged.reason = before.reason.clone();
        unchanged.reviewed_by = before.reviewed_by.clone();
        unchanged.reviewed_at = before.reviewed_at;
        if &unchanged != before || next.reason.as_deref().is_none_or(|s| s.trim().is_empty()) {
            return Err(TimeError::Invalid(
                "approved time can only be voided with a reason",
            ));
        }
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Entry {
        serde_json::from_value(serde_json::json!({"book_id":ItemId::new(),"user_id":"lawyer","display_name":"Lawyer","draft":{"work_date":"2026-10-14","time_zone":"Europe/Madrid","seconds":600,"description":"Review documents","assignment":{"kind":"INTERNAL"},"classification":"NONBILLABLE","office_id":null,"practice_id":null,"hourly_rate_minor":null,"rate_override":false},"status":"DRAFT","currency":"EUR","value_minor":0,"intervals":[]})).unwrap()
    }
    fn edit(entry: &Entry) -> EventProposal {
        serde_json::from_value(serde_json::json!({"item_id":ItemId::new(),"event_type":"TIME_ENTRY::SET_METADATA","base_seq":1,"set_metadata":entry})).unwrap()
    }
    #[test]
    fn time_tracking_approved_work_is_immutable_and_requires_evidence() {
        let mut original = fixture();
        original.status = Status::Approved;
        assert!(original.validate().is_err());
        let now = Utc::now();
        original.submitted_at = Some(now);
        original.approved_at = Some(now);
        original.reviewed_at = Some(now);
        original.reviewed_by = Some("partner".into());
        original.validate().unwrap();
        let mut void = original.clone();
        void.status = Status::Void;
        assert!(reduce_entry(Some(&original), &edit(&void)).is_err());
        void.reason = Some("Correct date".into());
        assert_eq!(reduce_entry(Some(&original), &edit(&void)).unwrap(), void);
        void.draft.seconds += 60;
        assert!(reduce_entry(Some(&original), &edit(&void)).is_err());
        assert!(
            reduce_entry(Some(&fixture()), &edit(&original)).is_err(),
            "drafts cannot skip submission"
        );
    }
    #[test]
    fn time_tracking_stops_long_timers_but_does_not_submit_them() {
        let mut entry = fixture();
        let start = Utc::now();
        entry.status = Status::Running;
        entry.started_at = Some(start);
        entry
            .stop_interval(start + chrono::Duration::hours(25))
            .unwrap();
        entry.status = Status::Draft;
        assert_eq!(entry.draft.seconds, 90600);
        entry.validate().unwrap();
        entry.status = Status::Submitted;
        entry.submitted_at = Some(start);
        assert!(entry.validate().is_err());
        entry.status = Status::Draft;
        entry.intervals.push(Interval { start, end: start });
        assert!(
            entry.validate().is_err(),
            "overlapping intervals must fail replay"
        );
    }
    #[test]
    fn time_tracking_money_is_exact_and_missing_is_not_zero() {
        assert_eq!(
            value(360, Some(15_000), Classification::Billable).unwrap(),
            Some(1500)
        );
        assert_eq!(
            value(1, Some(1800), Classification::Billable).unwrap(),
            Some(1)
        );
        assert_eq!(value(600, None, Classification::Billable).unwrap(), None);
        assert_eq!(value(600, None, Classification::NoCharge).unwrap(), Some(0));
        assert!(value(i64::MAX, Some(2), Classification::Billable).is_err());
    }
    #[test]
    fn time_tracking_rejects_unregistered_events_and_negative_rates() {
        assert!(event_policy("TIME_ENTRY", "TIME_ENTRY::TRASH").is_none());
        assert!(event_policy("TIME_ENTRY", "TIME_BOOK::CREATE").is_none());
        assert!(validate_rate(Some(-1)).is_err());
    }
}
