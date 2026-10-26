//! Civil recurrence selection and explicit timezone resolution. The rrule crate
//! is used only with UTC-encoded civil times: its timezone fallback shifts gaps.
//! We skip nonexistent starts and select the first instant of folds (RFC 5545).
use super::*;
use chrono::Timelike;
use rrule::{RRule, RRuleSet, Unvalidated};

const WORK_LIMIT: u16 = 60_000;
pub(super) const MAX_EXPLICIT_DATES: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Recurrence {
    pub rule: String,
    #[serde(default)]
    pub rdates: Vec<DateTime<Utc>>,
    #[serde(default)]
    pub exdates: Vec<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<TimezoneDefinition>,
}

/// Preserved VTIMEZONE observances. The scheduling codec owns ICS syntax;
/// this model owns the transition semantics used to resolve occurrences.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TimezoneDefinition {
    pub name: String,
    pub observances: Vec<Observance>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Observance {
    pub start: NaiveDateTime,
    pub offset_from: i32,
    pub offset_to: i32,
    pub rule: Option<String>,
    #[serde(default)]
    pub rdates: Vec<NaiveDateTime>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Occurrence {
    /// Original scheduled start, never the moved exception's displayed start.
    pub original_start: DateTime<Utc>,
    pub time: EventTime,
}

/// Cancellation retains the original identity and, for a move, its last time.
/// A move changes only the displayed time, never the recurrence identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OccurrenceException {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub canceled: bool,
    pub original_start: DateTime<Utc>,
    pub time: Option<EventTime>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl OccurrenceException {
    pub fn is_canceled(&self) -> bool {
        self.canceled || self.time.is_none()
    }
}

struct Rule {
    civil_rule: RRule<rrule::Validated>,
    count: Option<u32>,
    until: Option<String>,
}
fn parse_rule(raw: &str, start: NaiveDateTime) -> Result<Rule, CalendarError> {
    if raw.is_empty() || raw.len() > 2048 || raw.contains(['\r', '\n']) {
        return Err(CalendarError::Invalid("invalid recurrence rule"));
    }
    let mut fields = BTreeSet::new();
    let mut civil = Vec::new();
    let mut count = None;
    let mut until = None;
    for field in raw.split(';') {
        let (key, value) = field
            .split_once('=')
            .ok_or(CalendarError::Invalid("invalid recurrence rule"))?;
        if !fields.insert(key) {
            return Err(CalendarError::Invalid("duplicate recurrence field"));
        }
        match key {
            "FREQ" if !matches!(value, "DAILY" | "WEEKLY" | "MONTHLY" | "YEARLY") => {
                return Err(CalendarError::Invalid("unsupported recurrence frequency"));
            }
            "COUNT" => {
                count = Some(
                    value
                        .parse::<u32>()
                        .ok()
                        .filter(|v| *v > 0)
                        .ok_or(CalendarError::Invalid("invalid recurrence count"))?,
                );
                continue;
            }
            "UNTIL" => {
                until = Some(value.to_owned());
                continue;
            }
            "FREQ" | "INTERVAL" | "BYDAY" | "BYMONTH" | "BYMONTHDAY" | "BYSETPOS" | "WKST" => {}
            _ => return Err(CalendarError::Invalid("unsupported recurrence field")),
        }
        civil.push(field);
    }
    if count.is_some() && until.is_some() {
        return Err(CalendarError::Invalid("use either COUNT or UNTIL"));
    }
    if let Some(value) = &until {
        if NaiveDate::parse_from_str(value, "%Y%m%d").is_err()
            && DateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ").is_err()
        {
            // Chrono's %Z is a name, not a fixed offset. Parse UTC explicitly.
            NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
                .map_err(|_| CalendarError::Invalid("UNTIL must be a date or UTC instant"))?;
        }
    }
    let civil_rule = civil
        .join(";")
        .parse::<RRule<Unvalidated>>()
        .map_err(|_| CalendarError::Invalid("invalid recurrence rule"))?
        .validate(start.and_utc().with_timezone(&rrule::Tz::UTC))
        .map_err(|_| CalendarError::Invalid("invalid recurrence rule"))?;
    Ok(Rule {
        civil_rule,
        count,
        until,
    })
}
fn civil_dates(
    rule: &RRule<rrule::Validated>,
    start: NaiveDateTime,
    end: NaiveDateTime,
) -> Result<Vec<NaiveDateTime>, CalendarError> {
    let result = RRuleSet::new(start.and_utc().with_timezone(&rrule::Tz::UTC))
        .rrule(rule.clone())
        .before(end.and_utc().with_timezone(&rrule::Tz::UTC))
        .all(WORK_LIMIT);
    if result.limited {
        return Err(CalendarError::ExpansionLimit);
    }
    Ok(result.dates.into_iter().map(|d| d.naive_utc()).collect())
}

impl TimezoneDefinition {
    pub fn transitions(
        &self,
        end: NaiveDateTime,
    ) -> Result<Vec<(DateTime<Utc>, i32, i32)>, CalendarError> {
        if self.name.is_empty()
            || self.name.len() > 256
            || self.observances.is_empty()
            || self.observances.len() > 16
        {
            return Err(CalendarError::Invalid("invalid VTIMEZONE definition"));
        }
        let mut transitions = Vec::new();
        for obs in &self.observances {
            if obs.offset_from.unsigned_abs() >= 86400
                || obs.offset_to.unsigned_abs() >= 86400
                || obs.rdates.len() > MAX_EXPLICIT_DATES
                || !(1..=9999).contains(&obs.start.year())
                || obs.rdates.iter().any(|d| !(1..=9999).contains(&d.year()))
            {
                return Err(CalendarError::Invalid("invalid VTIMEZONE observance"));
            }
            let mut dates = vec![obs.start];
            if let Some(raw) = &obs.rule {
                if !raw.split(';').any(|p| p == "FREQ=YEARLY") {
                    return Err(CalendarError::Invalid(
                        "VTIMEZONE requires yearly observance rules",
                    ));
                }
                let rule = parse_rule(raw, obs.start)?;
                if rule.count.is_some() {
                    return Err(CalendarError::Invalid(
                        "unsupported counted VTIMEZONE observance",
                    ));
                }
                for date in civil_dates(&rule.civil_rule, obs.start, end)? {
                    if let Some(until) = &rule.until {
                        let utc = (date - Duration::seconds(obs.offset_from as i64)).and_utc();
                        if past_until(until, utc, date)? {
                            continue;
                        }
                    }
                    dates.push(date);
                }
            }
            dates.extend(obs.rdates.iter().copied());
            for date in dates {
                let utc = date
                    .checked_sub_signed(Duration::seconds(obs.offset_from as i64))
                    .ok_or(CalendarError::Invalid("VTIMEZONE transition overflow"))?
                    .and_utc();
                transitions.push((utc, obs.offset_from, obs.offset_to));
            }
        }
        transitions.sort_unstable();
        transitions.dedup();
        if transitions
            .windows(2)
            .any(|w| w[0].0 == w[1].0 || w[0].2 != w[1].1)
        {
            return Err(CalendarError::Invalid("inconsistent VTIMEZONE transitions"));
        }
        Ok(transitions)
    }
}
fn past_until(
    until: &str,
    instant: DateTime<Utc>,
    civil: NaiveDateTime,
) -> Result<bool, CalendarError> {
    if until.len() == 8 {
        return Ok(civil.date()
            > NaiveDate::parse_from_str(until, "%Y%m%d")
                .map_err(|_| CalendarError::Invalid("invalid UNTIL date"))?);
    }
    Ok(instant
        > NaiveDateTime::parse_from_str(until, "%Y%m%dT%H%M%SZ")
            .map_err(|_| CalendarError::Invalid("invalid UNTIL instant"))?
            .and_utc())
}
struct Resolver {
    iana: Tz,
    transitions: Option<Vec<(DateTime<Utc>, i32, i32)>>,
}
impl Resolver {
    fn offset(&self, instant: DateTime<Utc>) -> i32 {
        let transitions = self
            .transitions
            .as_ref()
            .expect("custom transition resolver");
        let index = transitions.partition_point(|t| t.0 <= instant);
        if index == 0 {
            transitions[0].1
        } else {
            transitions[index - 1].2
        }
    }
    fn civil(&self, instant: DateTime<Utc>) -> NaiveDateTime {
        if self.transitions.is_some() {
            instant.naive_utc() + Duration::seconds(self.offset(instant) as i64)
        } else {
            instant.with_timezone(&self.iana).naive_local()
        }
    }
    fn resolve(&self, civil: NaiveDateTime) -> Option<DateTime<Utc>> {
        if let Some(transitions) = &self.transitions {
            let offsets: BTreeSet<_> = transitions.iter().flat_map(|t| [t.1, t.2]).collect();
            offsets
                .into_iter()
                .filter_map(|offset| {
                    let instant = civil
                        .checked_sub_signed(Duration::seconds(offset as i64))?
                        .and_utc();
                    (self.offset(instant) == offset).then_some(instant)
                })
                .min()
        } else {
            self.iana
                .from_local_datetime(&civil)
                .earliest()
                .map(|d| d.with_timezone(&Utc))
        }
    }
}

impl Recurrence {
    pub fn validate(&self, time: &EventTime) -> Result<(), CalendarError> {
        time.validate()?;
        if self.rdates.len() > MAX_EXPLICIT_DATES || self.exdates.len() > MAX_EXPLICIT_DATES {
            return Err(CalendarError::Invalid("too many explicit recurrence dates"));
        }
        if self.rdates.windows(2).any(|w| w[0] >= w[1])
            || self.exdates.windows(2).any(|w| w[0] >= w[1])
        {
            return Err(CalendarError::Invalid(
                "recurrence dates must be sorted and distinct",
            ));
        }
        if self.rdates.iter().chain(&self.exdates).any(|d| {
            !(1..=9999).contains(&d.year())
                || (matches!(time, EventTime::AllDay { .. }) && d.time() != chrono::NaiveTime::MIN)
        }) {
            return Err(CalendarError::Invalid("invalid explicit recurrence date"));
        }
        let (start, zone) = match time {
            EventTime::Timed {
                start, time_zone, ..
            } => (*start, time_zone),
            EventTime::AllDay {
                start_date,
                time_zone,
                ..
            } => (
                start_date.and_hms_opt(0, 0, 0).unwrap().and_utc(),
                time_zone,
            ),
        };
        let transitions = self
            .timezone
            .as_ref()
            .map(|tz| tz.transitions(start.naive_utc() + Duration::days(366)))
            .transpose()?;
        let all_day = matches!(time, EventTime::AllDay { .. });
        let resolver = Resolver {
            iana: if all_day {
                chrono_tz::UTC
            } else {
                super::zone(zone)?
            },
            transitions: if all_day { None } else { transitions },
        };
        let civil = resolver.civil(start).with_nanosecond(0).unwrap();
        let rule = parse_rule(&self.rule, civil)?;
        // RFC 5545 leaves mismatched DTSTART/RRULE undefined. Reject that
        // ambiguity instead of silently losing the first meeting identity.
        let first = RRuleSet::new(civil.and_utc().with_timezone(&rrule::Tz::UTC))
            .rrule(rule.civil_rule)
            .all(1);
        if first.dates.first().map(|d| d.naive_utc()) != Some(civil) {
            return Err(CalendarError::Invalid(
                "series start must match its recurrence rule",
            ));
        }
        if let Some(until) = rule.until {
            if past_until(&until, start, civil)? {
                return Err(CalendarError::Invalid("recurrence ends before its start"));
            }
        }
        Ok(())
    }

    // One bounded selection serves both range reads and exception membership.
    // Do not resolve each exception through a separate expansion.
    fn starts_before(
        &self,
        time: &EventTime,
        end_date: NaiveDate,
    ) -> Result<BTreeSet<DateTime<Utc>>, CalendarError> {
        let (anchor, zone) = match time {
            EventTime::Timed {
                start, time_zone, ..
            } => (*start, time_zone),
            EventTime::AllDay {
                start_date,
                time_zone,
                ..
            } => (
                start_date.and_hms_opt(0, 0, 0).unwrap().and_utc(),
                time_zone,
            ),
        };
        let all_day = matches!(time, EventTime::AllDay { .. });
        let transitions = self
            .timezone
            .as_ref()
            .map(|tz| tz.transitions(end_date.and_hms_opt(0, 0, 0).unwrap() + Duration::days(366)))
            .transpose()?;
        let resolver = Resolver {
            iana: if all_day {
                chrono_tz::UTC
            } else {
                super::zone(zone)?
            },
            transitions: if all_day { None } else { transitions },
        };
        let civil_start = resolver.civil(anchor).with_nanosecond(0).unwrap();
        let rule = parse_rule(&self.rule, civil_start)?;
        let civil_end = end_date.and_hms_opt(0, 0, 0).unwrap() + Duration::days(2);
        let budget = rule.count.map_or(WORK_LIMIT, |n| {
            n.saturating_mul(2).max(366).min(WORK_LIMIT as u32) as u16
        });
        let selected = RRuleSet::new(civil_start.and_utc().with_timezone(&rrule::Tz::UTC))
            .rrule(rule.civil_rule.clone())
            .before(civil_end.and_utc().with_timezone(&rrule::Tz::UTC))
            .all(budget);
        let mut exhausted = selected.limited;
        let mut starts = BTreeSet::new();
        let mut count = 0u32;
        // DTSTART remains the first identity, including an explicitly chosen fold.
        for date in selected.dates {
            let civil = date.naive_utc();
            let Some(instant) = (if civil == civil_start {
                Some(anchor)
            } else {
                resolver.resolve(civil).and_then(|d| {
                    d.checked_add_signed(Duration::nanoseconds(anchor.nanosecond() as i64))
                })
            }) else {
                continue;
            };
            if let Some(until) = &rule.until {
                if past_until(until, instant, civil)? {
                    exhausted = false;
                    break;
                }
            }
            count += 1;
            starts.insert(instant);
            if rule.count.is_some_and(|max| count == max) {
                exhausted = false;
                break;
            }
        }
        if exhausted {
            return Err(CalendarError::ExpansionLimit);
        }
        starts.extend(self.rdates.iter().copied());
        for excluded in &self.exdates {
            starts.remove(excluded);
        }
        Ok(starts)
    }

    /// Resolve candidate original identities once when changing a series rule.
    pub(super) fn retained_starts(
        &self,
        time: &EventTime,
        candidates: &BTreeSet<DateTime<Utc>>,
    ) -> Result<BTreeSet<DateTime<Utc>>, CalendarError> {
        self.validate(time)?;
        let Some(last) = candidates.last() else {
            return Ok(BTreeSet::new());
        };
        let starts = self.starts_before(time, last.date_naive())?;
        Ok(candidates.intersection(&starts).copied().collect())
    }

    /// Enumerate the scheduled identities preceding a split boundary. The
    /// ordinary recurrence work limit is the admission bound for a split.
    pub(super) fn starts_before_split(
        &self,
        time: &EventTime,
        boundary: DateTime<Utc>,
    ) -> Result<BTreeSet<DateTime<Utc>>, CalendarError> {
        let starts = self.starts_before(time, boundary.date_naive())?;
        if !starts.contains(&boundary) {
            return Err(CalendarError::Conflict(
                "split boundary is not an occurrence",
            ));
        }
        Ok(starts
            .into_iter()
            .filter(|start| *start < boundary)
            .collect())
    }

    pub fn validate_exceptions(
        &self,
        time: &EventTime,
        exceptions: &[OccurrenceException],
    ) -> Result<(), CalendarError> {
        self.validate(time)?;
        if exceptions.len() > MAX_EXPLICIT_DATES
            || exceptions
                .windows(2)
                .any(|w| w[0].original_start >= w[1].original_start)
        {
            return Err(CalendarError::Invalid(
                "exceptions must be bounded, sorted and distinct",
            ));
        }
        let Some(last) = exceptions.last() else {
            return Ok(());
        };
        if exceptions
            .iter()
            .any(|e| !(1..=9999).contains(&e.original_start.year()))
        {
            return Err(CalendarError::Invalid("invalid occurrence identity"));
        }
        let starts = self.starts_before(time, last.original_start.date_naive())?;
        for exception in exceptions {
            if !starts.contains(&exception.original_start) {
                return Err(CalendarError::Invalid(
                    "exception is not an occurrence of this series",
                ));
            }
            if let Some(replacement) = &exception.time {
                replacement.validate()?;
                if matches!(replacement, EventTime::AllDay { .. })
                    != matches!(time, EventTime::AllDay { .. })
                {
                    return Err(CalendarError::Invalid(
                        "an exception must preserve the series time type",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Conservative indexed envelope. Finite rules resolve their real final
    /// start under the same work budget; expired finite series must not remain
    /// candidates forever. Empty series retain a harmless one-event envelope.
    pub fn bounds(
        &self,
        time: &EventTime,
        exceptions: &[OccurrenceException],
        before: Option<DateTime<Utc>>,
    ) -> Result<EventTime, CalendarError> {
        self.validate_exceptions(time, exceptions)?;
        let finite = before.is_some()
            || self
                .rule
                .split(';')
                .any(|field| field.starts_with("COUNT=") || field.starts_with("UNTIL="));
        let ceiling = NaiveDate::from_ymd_opt(9999, 12, 31).unwrap();
        let mut starts = if finite {
            self.starts_before(time, before.map_or(ceiling, |start| start.date_naive()))?
        } else {
            self.rdates.iter().copied().collect()
        };
        starts.retain(|start| before.is_none_or(|boundary| *start < boundary));
        for exception in exceptions {
            starts.remove(&exception.original_start);
        }
        let mut result = time.clone();
        match &mut result {
            EventTime::Timed { start, end, .. } => {
                let duration = *end - *start;
                if let Some(first) = starts.first() {
                    *start = (*start).min(*first);
                }
                if finite {
                    if let Some(last) = starts.last() {
                        *end = (*end).max(*last + duration);
                    }
                } else {
                    *end = ceiling
                        .and_hms_nano_opt(23, 59, 59, 999_999_000)
                        .unwrap()
                        .and_utc();
                }
                for exception in exceptions.iter().filter(|e| {
                    !e.is_canceled() && before.is_none_or(|start| e.original_start < start)
                }) {
                    if let Some(EventTime::Timed {
                        start: first,
                        end: last,
                        ..
                    }) = exception.time
                    {
                        *start = (*start).min(first);
                        *end = (*end).max(last);
                    }
                }
            }
            EventTime::AllDay {
                start_date,
                end_date,
                ..
            } => {
                let duration = *end_date - *start_date;
                if let Some(first) = starts.first() {
                    *start_date = (*start_date).min(first.date_naive());
                }
                if finite {
                    if let Some(last) = starts.last() {
                        *end_date = (*end_date).max(last.date_naive() + duration);
                    }
                } else {
                    *end_date = ceiling;
                }
                for exception in exceptions.iter().filter(|e| {
                    !e.is_canceled() && before.is_none_or(|start| e.original_start < start)
                }) {
                    if let Some(EventTime::AllDay {
                        start_date: first,
                        end_date: last,
                        ..
                    }) = exception.time
                    {
                        *start_date = (*start_date).min(first);
                        *end_date = (*end_date).max(last);
                    }
                }
            }
        }
        result.validate()?;
        Ok(result)
    }

    /// Include moved-in exceptions even when the original start lies outside
    /// this range. Remove moved-out and canceled originals before publication.
    pub fn expand_with_exceptions(
        &self,
        time: &EventTime,
        exceptions: &[OccurrenceException],
        start_date: NaiveDate,
        end_date: NaiveDate,
        view_zone: &str,
        before: Option<DateTime<Utc>>,
    ) -> Result<Vec<Occurrence>, CalendarError> {
        self.validate_exceptions(time, exceptions)?;
        let mut occurrences = self.expand_before(time, start_date, end_date, view_zone, before)?;
        occurrences.retain(|o| {
            exceptions
                .binary_search_by_key(&o.original_start, |e| e.original_start)
                .is_err()
        });
        let view = super::zone(view_zone)?;
        let begin = view
            .from_local_datetime(&start_date.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .ok_or(CalendarError::Invalid("invalid range boundary"))?
            .with_timezone(&Utc);
        let end = view
            .from_local_datetime(&end_date.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .ok_or(CalendarError::Invalid("invalid range boundary"))?
            .with_timezone(&Utc);
        for exception in exceptions {
            if exception.is_canceled()
                || before.is_some_and(|start| exception.original_start >= start)
            {
                continue;
            }
            let Some(time) = &exception.time else {
                continue;
            };
            let overlaps = match time {
                EventTime::Timed {
                    start, end: finish, ..
                } => *start < end && *finish > begin,
                EventTime::AllDay {
                    start_date: first,
                    end_date: last,
                    ..
                } => *first < end_date && *last > start_date,
            };
            if overlaps {
                occurrences.push(Occurrence {
                    original_start: exception.original_start,
                    time: time.clone(),
                });
            }
        }
        occurrences.sort_by_key(|o| o.original_start);
        Ok(occurrences)
    }

    pub fn expand(
        &self,
        time: &EventTime,
        start_date: NaiveDate,
        end_date: NaiveDate,
        view_zone: &str,
    ) -> Result<Vec<Occurrence>, CalendarError> {
        self.expand_before(time, start_date, end_date, view_zone, None)
    }

    fn expand_before(
        &self,
        time: &EventTime,
        start_date: NaiveDate,
        end_date: NaiveDate,
        view_zone: &str,
        before: Option<DateTime<Utc>>,
    ) -> Result<Vec<Occurrence>, CalendarError> {
        self.validate(time)?;
        if !(1..=62).contains(&(end_date - start_date).num_days()) {
            return Err(CalendarError::Invalid(
                "recurrence range must be 1 to 62 days",
            ));
        }
        let view = super::zone(view_zone)?;
        let begin = view
            .from_local_datetime(&start_date.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .ok_or(CalendarError::Invalid("invalid range boundary"))?
            .with_timezone(&Utc);
        let end = view
            .from_local_datetime(&end_date.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .ok_or(CalendarError::Invalid("invalid range boundary"))?
            .with_timezone(&Utc);
        let starts = self.starts_before(
            time,
            before.map_or(end_date, |start| end_date.min(start.date_naive())),
        )?;
        let mut output = Vec::new();
        for original_start in starts {
            if before.is_some_and(|start| original_start >= start) {
                continue;
            }
            let occurrence = match time {
                EventTime::Timed {
                    start,
                    end: original_end,
                    time_zone,
                } => {
                    let occurrence_end = original_start
                        .checked_add_signed(*original_end - *start)
                        .ok_or(CalendarError::Invalid("occurrence end overflow"))?;
                    if original_start >= end || occurrence_end <= begin {
                        continue;
                    }
                    EventTime::Timed {
                        start: original_start,
                        end: occurrence_end,
                        time_zone: if self.timezone.is_some() {
                            "UTC".into()
                        } else {
                            time_zone.clone()
                        },
                    }
                }
                EventTime::AllDay {
                    start_date: base,
                    end_date: base_end,
                    time_zone,
                } => {
                    let first = original_start.date_naive();
                    let last = first
                        .checked_add_signed(*base_end - *base)
                        .ok_or(CalendarError::Invalid("occurrence end overflow"))?;
                    if first >= end_date || last <= start_date {
                        continue;
                    }
                    EventTime::AllDay {
                        start_date: first,
                        end_date: last,
                        time_zone: time_zone.clone(),
                    }
                }
            };
            occurrence.validate()?;
            output.push(Occurrence {
                original_start,
                time: occurrence,
            });
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn instant(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }
    fn timed(start: &str, zone: &str) -> EventTime {
        let start = instant(start);
        EventTime::Timed {
            start,
            end: start + Duration::hours(1),
            time_zone: zone.into(),
        }
    }
    fn rule(s: &str) -> Recurrence {
        Recurrence {
            rule: s.into(),
            rdates: vec![],
            exdates: vec![],
            timezone: None,
        }
    }
    fn expand(rule: &Recurrence, time: &EventTime, first: &str, last: &str) -> Vec<Occurrence> {
        rule.expand(time, first.parse().unwrap(), last.parse().unwrap(), "UTC")
            .unwrap()
    }
    #[test]
    fn reject_mismatched_start_and_preserve_date_only_weekday() {
        let monday = timed("2026-09-07T09:00:00Z", "UTC");
        assert!(rule("FREQ=WEEKLY;BYDAY=TU").validate(&monday).is_err());
        assert!(
            rule("FREQ=DAILY;UNTIL=20260906T090000Z")
                .validate(&monday)
                .is_err()
        );
        let day = EventTime::AllDay {
            start_date: "2026-09-07".parse().unwrap(),
            end_date: "2026-09-08".parse().unwrap(),
            time_zone: "America/Los_Angeles".into(),
        };
        assert!(rule("FREQ=WEEKLY;BYDAY=MO").validate(&day).is_ok());
        assert!(rule("FREQ=WEEKLY;BYDAY=SU").validate(&day).is_err());
    }

    #[test]
    fn moved_and_canceled_occurrences_keep_original_identity() {
        let r = rule("FREQ=DAILY;COUNT=5");
        let time = timed("2026-09-01T09:00:00Z", "UTC");
        let exceptions = vec![
            OccurrenceException {
                revision: None,
                canceled: false,
                original_start: instant("2026-09-01T09:00:00Z"),
                time: Some(timed("2026-10-01T09:00:00Z", "UTC")),
            },
            OccurrenceException {
                revision: None,
                canceled: false,
                original_start: instant("2026-09-02T09:00:00Z"),
                time: None,
            },
            OccurrenceException {
                revision: None,
                canceled: false,
                original_start: instant("2026-09-05T09:00:00Z"),
                time: Some(timed("2026-09-03T09:00:00Z", "UTC")),
            },
        ];
        let expand = |first: &str, last: &str| {
            r.expand_with_exceptions(
                &time,
                &exceptions,
                first.parse().unwrap(),
                last.parse().unwrap(),
                "UTC",
                None,
            )
            .unwrap()
        };
        let september = expand("2026-09-01", "2026-09-05");
        assert_eq!(
            september
                .iter()
                .map(|o| o.original_start.day())
                .collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        // Two meetings may have equal displayed time without becoming one item.
        assert_eq!(september[0].time, september[2].time);
        let october = expand("2026-10-01", "2026-10-02");
        assert_eq!(october.len(), 1);
        assert_eq!(october[0].original_start, exceptions[0].original_start);
        assert_eq!(october[0].time, exceptions[0].time.clone().unwrap());
        assert!(expand("2026-09-05", "2026-09-06").is_empty());
    }

    #[test]
    fn exceptions_reject_invented_excluded_duplicate_and_shortened_identities() {
        let time = timed("2026-09-01T09:00:00Z", "UTC");
        let mut r = rule("FREQ=DAILY;COUNT=2");
        let exception = OccurrenceException {
            revision: None,
            canceled: false,
            original_start: instant("2026-09-02T09:00:00Z"),
            time: None,
        };
        assert!(
            r.validate_exceptions(&time, std::slice::from_ref(&exception))
                .is_ok()
        );
        assert!(
            r.validate_exceptions(&time, &[exception.clone(), exception.clone()])
                .is_err()
        );
        r.rule = "FREQ=DAILY;COUNT=1".into();
        assert!(
            r.validate_exceptions(&time, std::slice::from_ref(&exception))
                .is_err()
        );
        r.rule = "FREQ=DAILY;COUNT=2".into();
        r.exdates.push(exception.original_start);
        assert!(r.validate_exceptions(&time, &[exception]).is_err());
        let invented = OccurrenceException {
            revision: None,
            canceled: false,
            original_start: instant("2026-09-01T09:01:00Z"),
            time: None,
        };
        assert!(r.validate_exceptions(&time, &[invented]).is_err());
    }

    #[test]
    fn all_day_exception_respects_exclusive_end_and_time_type() {
        let r = rule("FREQ=DAILY;COUNT=1");
        let time = EventTime::AllDay {
            start_date: "2026-09-01".parse().unwrap(),
            end_date: "2026-09-02".parse().unwrap(),
            time_zone: "Pacific/Auckland".into(),
        };
        let mut exception = OccurrenceException {
            revision: None,
            canceled: false,
            original_start: instant("2026-09-01T00:00:00Z"),
            time: Some(EventTime::AllDay {
                start_date: "2026-10-01".parse().unwrap(),
                end_date: "2026-10-03".parse().unwrap(),
                time_zone: "Pacific/Auckland".into(),
            }),
        };
        let output = r
            .expand_with_exceptions(
                &time,
                std::slice::from_ref(&exception),
                "2026-10-02".parse().unwrap(),
                "2026-10-03".parse().unwrap(),
                "America/Los_Angeles",
                None,
            )
            .unwrap();
        assert_eq!(output.len(), 1);
        assert!(
            r.expand_with_exceptions(
                &time,
                std::slice::from_ref(&exception),
                "2026-10-03".parse().unwrap(),
                "2026-10-04".parse().unwrap(),
                "UTC",
                None,
            )
            .unwrap()
            .is_empty()
        );
        exception.time = Some(timed("2026-10-01T00:00:00Z", "UTC"));
        assert!(r.validate_exceptions(&time, &[exception]).is_err());
    }

    #[test]
    fn daily_skips_gap_without_consuming_count_and_preserves_fold_choice() {
        let r = rule("FREQ=DAILY;COUNT=3");
        let output = expand(
            &r,
            &timed("2026-03-28T01:30:00Z", "Europe/Madrid"),
            "2026-03-28",
            "2026-04-02",
        );
        assert_eq!(
            output.iter().map(|o| o.original_start).collect::<Vec<_>>(),
            vec![
                instant("2026-03-28T01:30:00Z"),
                instant("2026-03-30T00:30:00Z"),
                instant("2026-03-31T00:30:00Z")
            ]
        );
        let output = expand(
            &r,
            &timed("2026-10-25T01:30:00Z", "Europe/Madrid"),
            "2026-10-25",
            "2026-10-29",
        );
        assert_eq!(output[0].original_start, instant("2026-10-25T01:30:00Z"));
    }
    #[test]
    fn recurring_starts_preserve_canonical_fractional_seconds() {
        let time = timed("2026-09-01T09:00:00.123456Z", "UTC");
        let out = expand(
            &rule("FREQ=DAILY;COUNT=2"),
            &time,
            "2026-09-01",
            "2026-09-04",
        );
        assert_eq!(
            out[0].original_start,
            instant("2026-09-01T09:00:00.123456Z")
        );
        assert_eq!(
            out[1].original_start,
            instant("2026-09-02T09:00:00.123456Z")
        );
    }
    #[test]
    fn month_end_leap_day_ordinal_and_weekly_intervals() {
        let time = timed("2026-01-31T09:00:00Z", "UTC");
        let out = expand(
            &rule("FREQ=MONTHLY;COUNT=3"),
            &time,
            "2026-02-01",
            "2026-04-01",
        );
        assert_eq!(
            out.iter()
                .map(|o| o.original_start.date_naive().to_string())
                .collect::<Vec<_>>(),
            vec!["2026-03-31"]
        );
        let out = expand(
            &rule("FREQ=MONTHLY;BYDAY=-1MO"),
            &timed("2026-01-26T09:00:00Z", "UTC"),
            "2026-02-01",
            "2026-04-01",
        );
        assert_eq!(
            out.iter()
                .map(|o| o.original_start.date_naive().to_string())
                .collect::<Vec<_>>(),
            vec!["2026-02-23", "2026-03-30"]
        );
        let out = expand(
            &rule("FREQ=YEARLY;COUNT=2"),
            &timed("2024-02-29T09:00:00Z", "UTC"),
            "2028-02-01",
            "2028-03-01",
        );
        assert_eq!(out[0].original_start, instant("2028-02-29T09:00:00Z"));
        let out = expand(
            &rule("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=4"),
            &timed("2026-09-07T09:00:00Z", "UTC"),
            "2026-09-01",
            "2026-10-01",
        );
        assert_eq!(
            out.iter()
                .map(|o| o.original_start.day())
                .collect::<Vec<_>>(),
            vec![7, 9, 21, 23]
        );
    }
    #[test]
    fn date_only_exclusive_end_and_explicit_dates() {
        let time = EventTime::AllDay {
            start_date: "2026-09-01".parse().unwrap(),
            end_date: "2026-09-03".parse().unwrap(),
            time_zone: "Europe/Madrid".into(),
        };
        let mut r = rule("FREQ=WEEKLY;COUNT=2");
        r.exdates = vec![instant("2026-09-08T00:00:00Z")];
        r.rdates = vec![instant("2026-09-10T00:00:00Z")];
        let out = expand(&r, &time, "2026-09-02", "2026-09-12");
        assert_eq!(out.len(), 2);
        assert!(
            matches!(&out[0].time,EventTime::AllDay{end_date,..} if end_date.to_string()=="2026-09-03")
        );
        assert!(expand(&r, &time, "2026-09-03", "2026-09-04").is_empty());
    }
    #[test]
    fn bounds_are_errors_not_silent_truncation() {
        let time = timed("1800-01-01T09:00:00Z", "UTC");
        assert_eq!(
            rule("FREQ=DAILY").expand(
                &time,
                "2026-09-01".parse().unwrap(),
                "2026-10-01".parse().unwrap(),
                "UTC"
            ),
            Err(CalendarError::ExpansionLimit)
        );
        assert!(
            expand(
                &rule("FREQ=DAILY;COUNT=1"),
                &time,
                "2026-09-01",
                "2026-10-01"
            )
            .is_empty()
        );
        assert!(rule("FREQ=HOURLY").validate(&time).is_err());
        assert!(
            rule("FREQ=DAILY;COUNT=3;UNTIL=20260903")
                .validate(&time)
                .is_err()
        );
    }
    #[test]
    fn preserved_vtimezone_matches_iana_dst_and_until() {
        let mut r = rule("FREQ=DAILY;UNTIL=20260331T003000Z");
        r.timezone = Some(TimezoneDefinition {
            name: "Imported Madrid".into(),
            observances: vec![
                Observance {
                    start: "2000-03-26T02:00:00".parse().unwrap(),
                    offset_from: 3600,
                    offset_to: 7200,
                    rule: Some("FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU".into()),
                    rdates: vec![],
                },
                Observance {
                    start: "2000-10-29T03:00:00".parse().unwrap(),
                    offset_from: 7200,
                    offset_to: 3600,
                    rule: Some("FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU".into()),
                    rdates: vec![],
                },
            ],
        });
        let time = timed("2026-03-28T01:30:00Z", "UTC");
        let actual = expand(&r, &time, "2026-03-28", "2026-04-02");
        r.timezone = None;
        let expected = expand(
            &r,
            &timed("2026-03-28T01:30:00Z", "Europe/Madrid"),
            "2026-03-28",
            "2026-04-02",
        );
        assert_eq!(
            actual.iter().map(|o| o.original_start).collect::<Vec<_>>(),
            expected
                .iter()
                .map(|o| o.original_start)
                .collect::<Vec<_>>()
        );
        assert_eq!(actual.len(), 3);
    }
}
