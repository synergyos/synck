//! Bounded iCalendar syntax and iTIP identity. Parsed properties are untrusted
//! evidence, never authorization to mutate a calendar.
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub mod import;
pub mod migration;
pub mod state;
pub use state::{ExternalAttendee, ExternalScheduling};

pub const MAX_BYTES: usize = 1024 * 1024;
const MAX_PROPERTIES: usize = 8192;
const MAX_LINE: usize = 64 * 1024;
const MAX_COMPONENTS: usize = 1024;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid calendar: {0}")]
    Invalid(&'static str),
    #[error("unsupported calendar action")]
    Unsupported,
}
type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Request,
    Reply,
    Cancel,
    Refresh,
    Counter,
    Declinecounter,
}
impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Request => "REQUEST",
            Self::Reply => "REPLY",
            Self::Cancel => "CANCEL",
            Self::Refresh => "REFRESH",
            Self::Counter => "COUNTER",
            Self::Declinecounter => "DECLINECOUNTER",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        match s {
            "REQUEST" => Ok(Self::Request),
            "REPLY" => Ok(Self::Reply),
            "CANCEL" => Ok(Self::Cancel),
            "REFRESH" => Ok(Self::Refresh),
            "COUNTER" => Ok(Self::Counter),
            "DECLINECOUNTER" => Ok(Self::Declinecounter),
            _ => Err(Error::Unsupported),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Property {
    pub name: String,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    pub value: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub name: String,
    pub properties: Vec<Property>,
    #[serde(default)]
    pub children: Vec<Component>,
}
impl Component {
    /// Singular protocol fields must not silently select one conflicting value.
    pub fn get(&self, name: &str) -> Result<Option<&Property>> {
        let mut values = self.properties.iter().filter(|p| p.name == name);
        let first = values.next();
        if values.next().is_some() {
            return Err(Error::Invalid("duplicate singleton property"));
        }
        Ok(first)
    }
    pub fn required(&self, name: &str) -> Result<&Property> {
        self.get(name)?
            .ok_or(Error::Invalid("missing required property"))
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarMessage {
    pub method: Method,
    pub document: Component,
}

fn token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}
fn split_unquoted(s: &str, separator: char) -> Result<Vec<&str>> {
    let mut quoted = false;
    let mut start = 0;
    let mut out = Vec::new();
    for (i, c) in s.char_indices() {
        if c == '"' {
            quoted = !quoted;
        }
        if c == separator && !quoted {
            out.push(&s[start..i]);
            start = i + c.len_utf8();
        }
    }
    if quoted {
        return Err(Error::Invalid("unclosed parameter quote"));
    }
    out.push(&s[start..]);
    Ok(out)
}
fn property(line: &str) -> Result<Property> {
    let mut quoted = false;
    let colon = line
        .char_indices()
        .find_map(|(i, c)| {
            if c == '"' {
                quoted = !quoted;
            }
            (c == ':' && !quoted).then_some(i)
        })
        .ok_or(Error::Invalid("missing property separator"))?;
    let fields = split_unquoted(&line[..colon], ';')?;
    if !token(fields[0]) {
        return Err(Error::Invalid("invalid property name"));
    }
    let mut params = BTreeMap::new();
    for field in &fields[1..] {
        let (name, value) = field
            .split_once('=')
            .ok_or(Error::Invalid("invalid parameter"))?;
        if !token(name) || params.len() >= 32 {
            return Err(Error::Invalid("invalid parameter"));
        }
        let value = if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if value.contains('"')
            || params
                .insert(name.to_ascii_uppercase(), value.to_owned())
                .is_some()
        {
            return Err(Error::Invalid("ambiguous parameter"));
        }
    }
    Ok(Property {
        name: fields[0].to_ascii_uppercase(),
        params,
        value: line[colon + 1..].to_owned(),
    })
}

/// Parses one bounded VCALENDAR, preserving timezone/recurrence evidence. It
/// performs no URL fetching, alarm execution, identity resolution or scheduling.
pub fn parse_document(bytes: &[u8]) -> Result<Component> {
    if bytes.len() > MAX_BYTES {
        return Err(Error::Invalid("calendar exceeds byte limit"));
    }
    let input = std::str::from_utf8(bytes).map_err(|_| Error::Invalid("calendar is not UTF-8"))?;
    if input
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        return Err(Error::Invalid("calendar contains control characters"));
    }
    let mut lines: Vec<String> = Vec::new();
    for physical in input.split('\n') {
        let physical = physical.strip_suffix('\r').unwrap_or(physical);
        if physical.contains('\r') {
            return Err(Error::Invalid("bare carriage return"));
        }
        if physical.starts_with([' ', '\t']) {
            let prior = lines
                .last_mut()
                .ok_or(Error::Invalid("orphan folded line"))?;
            if prior.len() + physical.len() - 1 > MAX_LINE {
                return Err(Error::Invalid("line exceeds limit"));
            }
            prior.push_str(&physical[1..]);
        } else if !physical.is_empty() {
            if physical.len() > MAX_LINE || lines.len() >= MAX_PROPERTIES {
                return Err(Error::Invalid("calendar exceeds property limit"));
            }
            lines.push(physical.to_owned());
        }
    }
    let mut stack: Vec<Component> = Vec::new();
    let mut document = None;
    let mut components = 0;
    for line in lines {
        let prop = property(&line)?;
        match prop.name.as_str() {
            "BEGIN" => {
                if !prop.params.is_empty()
                    || !token(&prop.value)
                    || stack.len() >= 4
                    || document.is_some()
                {
                    return Err(Error::Invalid("invalid component start"));
                }
                components += 1;
                if components > MAX_COMPONENTS {
                    return Err(Error::Invalid("too many components"));
                }
                stack.push(Component {
                    name: prop.value.to_ascii_uppercase(),
                    properties: Vec::new(),
                    children: Vec::new(),
                });
            }
            "END" => {
                let current = stack
                    .pop()
                    .ok_or(Error::Invalid("unexpected component end"))?;
                if !prop.params.is_empty() || !current.name.eq_ignore_ascii_case(&prop.value) {
                    return Err(Error::Invalid("mismatched component end"));
                }
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(current);
                } else {
                    document = Some(current);
                }
            }
            _ => stack
                .last_mut()
                .ok_or(Error::Invalid("property outside component"))?
                .properties
                .push(prop),
        }
    }
    if !stack.is_empty() {
        return Err(Error::Invalid("unclosed component"));
    }
    let document = document.ok_or(Error::Invalid("missing calendar"))?;
    if document.name != "VCALENDAR" || document.required("VERSION")?.value != "2.0" {
        return Err(Error::Invalid("expected iCalendar version 2.0"));
    }
    Ok(document)
}

pub fn parse(bytes: &[u8]) -> Result<CalendarMessage> {
    let document = parse_document(bytes)?;
    let method = Method::parse(&document.required("METHOD")?.value.to_ascii_uppercase())?;
    let mut identities = BTreeSet::new();
    let mut series_identity = None;
    for event in &document.children {
        if event.name == "VTIMEZONE" {
            continue;
        }
        if event.name != "VEVENT" {
            return Err(Error::Unsupported);
        }
        let identity = Identity::from_event(event)?;
        let series = (identity.uid.clone(), identity.organizer.clone());
        if series_identity
            .as_ref()
            .is_some_and(|prior| prior != &series)
        {
            return Err(Error::Invalid(
                "one scheduling message must identify one meeting",
            ));
        }
        series_identity = Some(series);
        if !identities.insert(identity.recurrence_id.clone()) {
            return Err(Error::Invalid("duplicate occurrence"));
        }
        utc_timestamp(&event.required("DTSTAMP")?.value)?;
        sequence(event)?;
        let attendees: Vec<_> = event
            .properties
            .iter()
            .filter(|p| p.name == "ATTENDEE")
            .collect();
        if attendees.len() > 500 {
            return Err(Error::Invalid("too many attendees"));
        }
        let mut addresses = BTreeSet::new();
        for attendee in &attendees {
            if !addresses.insert(mailto(&attendee.value)?) {
                return Err(Error::Invalid("duplicate attendee"));
            }
        }
        if matches!(method, Method::Reply | Method::Refresh | Method::Counter)
            && attendees.len() != 1
        {
            return Err(Error::Invalid(
                "attendee action requires exactly one attendee",
            ));
        }
        if matches!(method, Method::Request | Method::Counter) {
            event.required("DTSTART")?;
        }
        if method == Method::Reply {
            let status = attendees[0]
                .params
                .get("PARTSTAT")
                .ok_or(Error::Invalid("reply has no participation status"))?;
            if !matches!(status.as_str(), "ACCEPTED" | "DECLINED" | "TENTATIVE") {
                return Err(Error::Unsupported);
            }
        }
    }
    if identities.is_empty() {
        return Err(Error::Invalid("missing event"));
    }
    Ok(CalendarMessage { method, document })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub uid: String,
    pub organizer: String,
    pub recurrence_id: Option<Property>,
}
impl Identity {
    pub fn from_event(event: &Component) -> Result<Self> {
        let uid = decode_text(&event.required("UID")?.value)?;
        if uid.is_empty() || uid.len() > 1024 {
            return Err(Error::Invalid("invalid UID"));
        }
        Ok(Self {
            uid,
            organizer: mailto(&event.required("ORGANIZER")?.value)?,
            recurrence_id: event.get("RECURRENCE-ID")?.cloned(),
        })
    }
}

/// Preserve the local part: SMTP does not grant lowercasing authority for it.
pub fn mailto(value: &str) -> Result<String> {
    if !value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("mailto:"))
    {
        return Err(Error::Invalid("calendar address must use mailto"));
    }
    let address = &value[7..];
    let (local, domain) = address
        .rsplit_once('@')
        .ok_or(Error::Invalid("invalid calendar address"))?;
    if address.len() > 254
        || local.is_empty()
        || local.contains('@')
        || domain.is_empty()
        || !address.is_ascii()
        || address.bytes().any(|b| {
            b.is_ascii_control() || b.is_ascii_whitespace() || b"<>\"(),:;\\%?".contains(&b)
        })
    {
        return Err(Error::Invalid("invalid calendar address"));
    }
    Ok(format!("{local}@{}", domain.to_ascii_lowercase()))
}
pub fn sequence(event: &Component) -> Result<u32> {
    match event.get("SEQUENCE")? {
        None => Ok(0),
        Some(p) if !p.value.is_empty() && p.value.bytes().all(|c| c.is_ascii_digit()) => p
            .value
            .parse::<u32>()
            .ok()
            .filter(|v| *v <= i32::MAX as u32)
            .ok_or(Error::Invalid("invalid sequence")),
        _ => Err(Error::Invalid("invalid sequence")),
    }
}
pub fn utc_timestamp(value: &str) -> Result<DateTime<Utc>> {
    // Some Exchange/ActiveSync producers put presentation whitespace around
    // machine timestamps. Keep text properties byte-exact, but tolerate bounded
    // ASCII space here so otherwise unambiguous scheduling evidence is usable.
    let value = value.trim_matches(' ');
    if value.len() != 16 || !value.ends_with('Z') {
        return Err(Error::Invalid("timestamp must be UTC"));
    }
    NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ")
        .map(|v| v.and_utc())
        .map_err(|_| Error::Invalid("invalid timestamp"))
}
pub fn decode_text(value: &str) -> Result<String> {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next() {
            Some('n' | 'N') => '\n',
            Some(c @ ('\\' | ',' | ';')) => c,
            _ => return Err(Error::Invalid("invalid text escape")),
        });
    }
    Ok(out)
}
pub fn encode_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace(';', "\\;")
        .replace(',', "\\,")
}

/// Serializer accepts only a tree that the bounded parser can read. Folding is
/// in octets and never cuts a UTF-8 code point.
pub fn serialize(document: &Component) -> Result<Vec<u8>> {
    let out = serialize_with_limits(document, MAX_BYTES, MAX_COMPONENTS, MAX_PROPERTIES)?;
    parse_document(&out)?;
    Ok(out)
}

/// Serialize a generated calendar under caller-owned aggregate limits. This is
/// used by whole-calendar export, whose valid output is larger than one iTIP
/// message but must remain explicitly bounded.
pub fn serialize_with_limits(
    document: &Component,
    max_bytes: usize,
    max_components: usize,
    max_properties: usize,
) -> Result<Vec<u8>> {
    struct Limits {
        bytes: usize,
        components: usize,
        properties: usize,
    }
    struct State {
        out: String,
        components: usize,
        properties: usize,
    }
    fn line(state: &mut State, value: &str, limits: &Limits) -> Result<()> {
        if value.contains(['\r', '\n']) || value.len() > MAX_LINE {
            return Err(Error::Invalid("invalid serialized line"));
        }
        state.properties = state.properties.saturating_add(1);
        if state.properties > limits.properties {
            return Err(Error::Invalid("calendar exceeds property limit"));
        }
        let mut remaining = value;
        let mut width = 75;
        while remaining.len() > width {
            let mut at = width;
            while !remaining.is_char_boundary(at) {
                at -= 1;
            }
            state.out.push_str(&remaining[..at]);
            state.out.push_str("\r\n ");
            remaining = &remaining[at..];
            width = 74;
        }
        state.out.push_str(remaining);
        state.out.push_str("\r\n");
        if state.out.len() > limits.bytes {
            return Err(Error::Invalid("calendar exceeds byte limit"));
        }
        Ok(())
    }
    fn component(state: &mut State, c: &Component, depth: usize, limits: &Limits) -> Result<()> {
        if depth > 4 || !token(&c.name) {
            return Err(Error::Invalid("invalid component"));
        }
        state.components = state.components.saturating_add(1);
        if state.components > limits.components {
            return Err(Error::Invalid("too many components"));
        }
        line(state, &format!("BEGIN:{}", c.name), limits)?;
        for p in &c.properties {
            if !token(&p.name) || matches!(p.name.as_str(), "BEGIN" | "END") {
                return Err(Error::Invalid("invalid property name"));
            }
            let mut s = p.name.clone();
            for (key, value) in &p.params {
                if !token(key) || value.contains('"') {
                    return Err(Error::Invalid("invalid parameter"));
                }
                s.push(';');
                s.push_str(key);
                s.push('=');
                if value.contains([';', ':', ',']) {
                    s.push('"');
                    s.push_str(value);
                    s.push('"');
                } else {
                    s.push_str(value);
                }
            }
            s.push(':');
            s.push_str(&p.value);
            line(state, &s, limits)?;
        }
        for child in &c.children {
            component(state, child, depth + 1, limits)?;
        }
        line(state, &format!("END:{}", c.name), limits)
    }
    let limits = Limits {
        bytes: max_bytes,
        components: max_components,
        properties: max_properties,
    };
    let mut state = State {
        out: String::new(),
        components: 0,
        properties: 0,
    };
    component(&mut state, document, 1, &limits)?;
    Ok(state.out.into_bytes())
}

#[cfg(test)]
mod tests;
