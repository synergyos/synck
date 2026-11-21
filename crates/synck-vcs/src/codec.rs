//! Format 8 binary encoding (format.md, "Binary encoding"): its primitives, the
//! item and attached-ref envelopes and the commit header. Each value has one
//! encoding, so decoding is verification: the decoder refuses trailing bytes,
//! unknown kinds and flag bits, and every value a field's rule forbids. It
//! never normalizes.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::content::{ContentRef, EventContent, MAX_CONTENT_ROLES, MAX_CONTENT_SIZE};
use crate::errors::CoreError;
use crate::limits;
use crate::metadata::MetadataRef;
use crate::types::{
    AuthorId, CommitterId, CreateItem, EventProposal, ItemId, ItemRevision, ObjectId,
    RefEventProposal, Sha256Digest, Timestamp, sha256_digest_from_digest_bytes,
    sha256_digest_to_digest_bytes, timestamp_has_format_year,
};

pub const KIND_ITEM: u8 = 1;
pub const KIND_REF: u8 = 2;
pub const KIND_COMMIT: u8 = 3;

#[derive(Debug, thiserror::Error)]
#[error("invalid encoding: {0}")]
pub struct DecodeError(pub String);

impl From<DecodeError> for CoreError {
    fn from(error: DecodeError) -> Self {
        CoreError::InvalidProposal(error.0)
    }
}

impl From<CoreError> for DecodeError {
    fn from(error: CoreError) -> Self {
        DecodeError(error.to_string())
    }
}

fn bad(detail: impl Into<String>) -> DecodeError {
    DecodeError(detail.into())
}

fn invalid(detail: impl Into<String>) -> CoreError {
    CoreError::InvalidProposal(detail.into())
}

// --- Primitives ---

pub fn put_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}
pub fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}
pub fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}
pub fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}
pub fn put_bool(out: &mut Vec<u8>, value: bool) {
    out.push(value as u8);
}
pub fn put_digest(out: &mut Vec<u8>, digest: &[u8; 32]) {
    out.extend_from_slice(digest);
}

/// A UUID string already validated as UUIDv7 by its ID type.
pub fn put_uuid(out: &mut Vec<u8>, id: &str) {
    let uuid = Uuid::parse_str(id).expect("validated UUID");
    out.extend_from_slice(uuid.as_bytes());
}

pub fn put_str(out: &mut Vec<u8>, value: &str) -> Result<(), CoreError> {
    let length = u16::try_from(value.len()).map_err(|_| invalid("string exceeds 65535 bytes"))?;
    if value.contains('\0') {
        return Err(invalid("string contains U+0000"));
    }
    put_u16(out, length);
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

pub fn put_seq(out: &mut Vec<u8>, seq: i64) -> Result<(), CoreError> {
    if seq < 1 {
        return Err(invalid(format!("sequence {seq} is not positive")));
    }
    put_u64(out, seq as u64);
    Ok(())
}

pub fn put_time(out: &mut Vec<u8>, time: Timestamp) -> Result<(), CoreError> {
    if time.timestamp_subsec_nanos() >= 1_000_000_000 {
        return Err(invalid("leap seconds are not supported"));
    }
    if !timestamp_has_format_year(time) {
        return Err(invalid("a time's UTC year must be 0000 through 9999"));
    }
    out.extend_from_slice(&time.timestamp().to_be_bytes());
    put_u32(out, time.timestamp_subsec_nanos());
    Ok(())
}

pub fn put_target(out: &mut Vec<u8>, target: Option<&ItemId>) {
    match target {
        None => put_u8(out, 0),
        Some(id) => {
            put_u8(out, 1);
            put_uuid(out, id.as_str());
        }
    }
}

pub fn put_source(out: &mut Vec<u8>, source: &ItemRevision) -> Result<(), CoreError> {
    put_uuid(out, source.item_id.as_str());
    put_seq(out, source.item_seq)
}

/// Roles are a `BTreeMap`, so they are already in strictly increasing order.
pub fn put_content(
    out: &mut Vec<u8>,
    roles: &BTreeMap<String, ContentRef>,
) -> Result<(), CoreError> {
    put_u8(out, roles.len() as u8);
    for (role, entry) in roles {
        put_str(out, role)?;
        put_digest(out, &entry.id.as_bytes());
        if entry.size > MAX_CONTENT_SIZE {
            return Err(invalid("content size exceeds 2^63 - 1"));
        }
        put_u64(out, entry.size);
    }
    Ok(())
}

pub struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.position != self.bytes.len() {
            return Err(bad("trailing bytes"));
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }

    pub fn take(&mut self, length: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| bad("truncated"))?;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        Ok(self.take(N)?.try_into().expect("exact length"))
    }

    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    /// A `u64` that fits a signed 64-bit integer: counts, bases and sequences
    /// that may be zero.
    pub fn count(&mut self) -> Result<i64, DecodeError> {
        let value = self.u64()?;
        i64::try_from(value).map_err(|_| bad("value exceeds 2^63 - 1"))
    }

    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(bad("invalid bool")),
        }
    }

    pub fn digest(&mut self) -> Result<[u8; 32], DecodeError> {
        self.array()
    }

    pub fn uuid(&mut self) -> Result<Uuid, DecodeError> {
        let uuid = Uuid::from_bytes(self.array()?);
        if uuid.get_version_num() != 7 || uuid.get_variant() != uuid::Variant::RFC4122 {
            return Err(bad("UUID is not a UUIDv7"));
        }
        Ok(uuid)
    }

    pub fn item_id(&mut self) -> Result<ItemId, DecodeError> {
        let uuid = self.uuid()?;
        ItemId::parse(uuid.to_string()).map_err(|error| bad(error.to_string()))
    }

    pub fn str(&mut self) -> Result<&'a str, DecodeError> {
        let length = self.u16()? as usize;
        let text = std::str::from_utf8(self.take(length)?).map_err(|_| bad("invalid UTF-8"))?;
        if text.contains('\0') {
            return Err(bad("string contains U+0000"));
        }
        Ok(text)
    }

    pub fn seq(&mut self) -> Result<i64, DecodeError> {
        let value = self.count()?;
        if value == 0 {
            return Err(bad("sequence is zero"));
        }
        Ok(value)
    }

    pub fn time(&mut self) -> Result<Timestamp, DecodeError> {
        let seconds = i64::from_be_bytes(self.array()?);
        let nanos = self.u32()?;
        if nanos >= 1_000_000_000 {
            return Err(bad("nanoseconds out of range"));
        }
        chrono::DateTime::from_timestamp(seconds, nanos)
            .filter(|time| timestamp_has_format_year(*time))
            .ok_or_else(|| bad("time outside years 0000 through 9999"))
    }

    pub fn target(&mut self) -> Result<Option<ItemId>, DecodeError> {
        match self.u8()? {
            0 => Ok(None),
            1 => self.item_id().map(Some),
            _ => Err(bad("invalid target tag")),
        }
    }

    pub fn source(&mut self) -> Result<ItemRevision, DecodeError> {
        Ok(ItemRevision {
            item_id: self.item_id()?,
            item_seq: self.seq()?,
        })
    }

    pub fn object_id(&mut self) -> Result<ObjectId, DecodeError> {
        Ok(ObjectId::from_digest(self.digest()?))
    }

    pub fn content(&mut self) -> Result<EventContent, DecodeError> {
        let count = self.u8()? as usize;
        if count > MAX_CONTENT_ROLES {
            return Err(bad("more than 32 content roles"));
        }
        let mut roles = BTreeMap::new();
        let mut previous: Option<&str> = None;
        for _ in 0..count {
            let role = self.str()?;
            if previous.is_some_and(|previous| previous.as_bytes() >= role.as_bytes()) {
                return Err(bad("content roles are not strictly increasing"));
            }
            previous = Some(role);
            let id = self.object_id()?;
            let size = self.count()? as u64;
            roles.insert(role.to_string(), ContentRef { id, size });
        }
        EventContent::new(roles).map_err(bad)
    }
}

// --- Envelopes ---

const ITEM_CREATE: u16 = 1 << 0;
const ITEM_BASE_SEQ: u16 = 1 << 1;
const ITEM_METADATA: u16 = 1 << 2;
const ITEM_CONTENT: u16 = 1 << 3;
const ITEM_PARENT: u16 = 1 << 4;
const ITEM_NAME: u16 = 1 << 5;
const ITEM_LINK: u16 = 1 << 6;
const ITEM_MERGED_FROM: u16 = 1 << 7;
const ITEM_IN_TRASH: u16 = 1 << 8;
const ITEM_TOMBSTONED: u16 = 1 << 9;
const ITEM_IS_FEDERATED: u16 = 1 << 10;
const ITEM_STREAM: u16 = 1 << 11;
const ITEM_FLAGS: u16 = (1 << 12) - 1;

const CREATE_RECORD: u8 = 1 << 0;
const CREATE_PLACE: u8 = 1 << 1;
const CREATE_KEY: u8 = 1 << 2;
const CREATE_SOURCE: u8 = 1 << 3;

const REF_METADATA: u8 = 1 << 0;
const REF_CONTENT: u8 = 1 << 1;

/// The rules an item envelope's fields obey beyond their encoding.
fn check_item_event(event: &EventProposal) -> Result<(), CoreError> {
    limits::validate_item_event_type(&event.event_type)?;
    if let Some(create) = &event.create_item {
        create.validate()?;
        if event.base_seq.is_some() {
            return Err(invalid("a created item has no base_seq"));
        }
        if event.event_type.split("::").next() != Some(create.item_type.as_str()) {
            return Err(invalid(format!(
                "`{}` cannot create a {}",
                event.event_type, create.item_type
            )));
        }
        if create.item_type == "FILE"
            && event
                .set_content
                .as_ref()
                .and_then(|content| content.get("bytes"))
                .is_none()
        {
            return Err(invalid("a FILE is created with its first bytes"));
        }
    }
    for name in event
        .create_item
        .as_ref()
        .and_then(|item| item.name.as_deref())
        .into_iter()
        .chain(event.set_name.as_deref())
    {
        limits::check_bytes("name", name.len(), limits::MAX_NAME_BYTES)?;
    }
    if event.set_tombstoned == Some(false) {
        return Err(invalid("set_tombstoned is only ever true"));
    }
    if event
        .set_federation_stream_id
        .as_ref()
        .is_some_and(|stream| stream.is_empty())
    {
        return Err(invalid("set_federation_stream_id must not be empty"));
    }
    Ok(())
}

/// The item envelope of an event whose item ID is assigned.
pub fn canonical_event_proposal_bytes(event: &EventProposal) -> Result<Vec<u8>, CoreError> {
    check_item_event(event)?;
    let item_id = event
        .item_id
        .as_ref()
        .ok_or_else(|| invalid("an envelope requires its item_id"))?;
    let mut flags = 0;
    let mut set = |bit: u16, present: bool| {
        if present {
            flags |= bit;
        }
    };
    set(ITEM_CREATE, event.create_item.is_some());
    set(ITEM_BASE_SEQ, event.base_seq.is_some());
    set(ITEM_METADATA, event.set_metadata.is_some());
    set(ITEM_CONTENT, event.set_content.is_some());
    set(ITEM_PARENT, event.set_parent.is_some());
    set(ITEM_NAME, event.set_name.is_some());
    set(ITEM_LINK, event.set_link.is_some());
    set(ITEM_MERGED_FROM, event.merged_from.is_some());
    set(ITEM_IN_TRASH, event.set_in_trash.is_some());
    set(ITEM_TOMBSTONED, event.set_tombstoned.is_some());
    set(ITEM_IS_FEDERATED, event.set_is_federated.is_some());
    set(ITEM_STREAM, event.set_federation_stream_id.is_some());

    let mut out = Vec::new();
    put_u8(&mut out, KIND_ITEM);
    put_str(&mut out, &event.event_type)?;
    put_uuid(&mut out, item_id.as_str());
    put_u16(&mut out, flags);
    if let Some(create) = &event.create_item {
        put_create(&mut out, create)?;
    }
    if let Some(base) = event.base_seq {
        put_seq(&mut out, base)?;
    }
    if let Some(metadata) = &event.set_metadata {
        put_digest(&mut out, &metadata.id().as_bytes());
    }
    if let Some(content) = &event.set_content {
        put_content(&mut out, content.roles())?;
    }
    if let Some(parent) = &event.set_parent {
        put_target(&mut out, parent.as_ref());
    }
    if let Some(name) = &event.set_name {
        put_str(&mut out, name)?;
    }
    if let Some(link) = &event.set_link {
        put_target(&mut out, link.as_ref());
    }
    if let Some(source) = &event.merged_from {
        put_source(&mut out, source)?;
    }
    if let Some(in_trash) = event.set_in_trash {
        put_bool(&mut out, in_trash);
    }
    if let Some(federated) = event.set_is_federated {
        put_bool(&mut out, federated);
    }
    if let Some(stream) = &event.set_federation_stream_id {
        put_str(&mut out, stream)?;
    }
    limits::check_bytes("event payload", out.len(), limits::MAX_EVENT_PAYLOAD_BYTES)?;
    Ok(out)
}

fn put_create(out: &mut Vec<u8>, create: &CreateItem) -> Result<(), CoreError> {
    let record = create.name.is_none();
    let mut flags = 0;
    if record {
        flags |= CREATE_RECORD;
        if create.link.is_some() {
            flags |= CREATE_PLACE;
        }
        if create.key.is_some() {
            flags |= CREATE_KEY;
        }
    } else if create.parent_folder_id.is_some() {
        flags |= CREATE_PLACE;
    }
    if create.derived_from.is_some() {
        flags |= CREATE_SOURCE;
    }
    put_u8(out, flags);
    put_str(out, &create.item_type)?;
    if let Some(name) = &create.name {
        put_str(out, name)?;
        if let Some(parent) = &create.parent_folder_id {
            put_uuid(out, parent.as_str());
        }
    } else {
        if let Some(link) = &create.link {
            put_uuid(out, link.as_str());
        }
        if let Some(key) = &create.key {
            put_str(out, key)?;
        }
    }
    if let Some(source) = &create.derived_from {
        put_source(out, source)?;
    }
    Ok(())
}

fn decode_create(decoder: &mut Decoder) -> Result<CreateItem, DecodeError> {
    let flags = decoder.u8()?;
    if flags & !(CREATE_RECORD | CREATE_PLACE | CREATE_KEY | CREATE_SOURCE) != 0 {
        return Err(bad("unknown create_item flag"));
    }
    let item_type = decoder.str()?.to_string();
    let mut create = if flags & CREATE_RECORD == 0 {
        if flags & CREATE_KEY != 0 {
            return Err(bad("a tree item has no key"));
        }
        let name = decoder.str()?.to_string();
        let parent = (flags & CREATE_PLACE != 0)
            .then(|| decoder.item_id())
            .transpose()?;
        CreateItem::tree(item_type, parent, name)
    } else {
        let link = (flags & CREATE_PLACE != 0)
            .then(|| decoder.item_id())
            .transpose()?;
        let key = (flags & CREATE_KEY != 0)
            .then(|| decoder.str().map(str::to_string))
            .transpose()?;
        CreateItem::record(item_type, key, link)
    };
    if flags & CREATE_SOURCE != 0 {
        create.derived_from = Some(decoder.source()?);
    }
    Ok(create)
}

/// Decode one item envelope. Its metadata stays an absent reference.
pub fn decode_event_proposal(bytes: &[u8]) -> Result<EventProposal, DecodeError> {
    limits::check_bytes(
        "event payload",
        bytes.len(),
        limits::MAX_EVENT_PAYLOAD_BYTES,
    )?;
    let mut decoder = Decoder::new(bytes);
    if decoder.u8()? != KIND_ITEM {
        return Err(bad("not an item envelope"));
    }
    let event_type = decoder.str()?.to_string();
    let item_id = decoder.item_id()?;
    let flags = decoder.u16()?;
    if flags & !ITEM_FLAGS != 0 {
        return Err(bad("unknown item envelope flag"));
    }
    let has = |bit: u16| flags & bit != 0;
    let mut event = EventProposal {
        item_id: Some(item_id),
        event_type,
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_link: None,
        merged_from: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: None,
    };
    if has(ITEM_CREATE) {
        event.create_item = Some(decode_create(&mut decoder)?);
    }
    if has(ITEM_BASE_SEQ) {
        event.base_seq = Some(decoder.seq()?);
    }
    if has(ITEM_METADATA) {
        event.set_metadata = Some(MetadataRef::Absent(decoder.object_id()?));
    }
    if has(ITEM_CONTENT) {
        event.set_content = Some(decoder.content()?);
    }
    if has(ITEM_PARENT) {
        event.set_parent = Some(decoder.target()?);
    }
    if has(ITEM_NAME) {
        event.set_name = Some(decoder.str()?.to_string());
    }
    if has(ITEM_LINK) {
        event.set_link = Some(decoder.target()?);
    }
    if has(ITEM_MERGED_FROM) {
        event.merged_from = Some(decoder.source()?);
    }
    if has(ITEM_IN_TRASH) {
        event.set_in_trash = Some(decoder.bool()?);
    }
    if has(ITEM_TOMBSTONED) {
        event.set_tombstoned = Some(true);
    }
    if has(ITEM_IS_FEDERATED) {
        event.set_is_federated = Some(decoder.bool()?);
    }
    if has(ITEM_STREAM) {
        event.set_federation_stream_id = Some(decoder.str()?.to_string());
    }
    decoder.finish()?;
    check_item_event(&event)?;
    Ok(event)
}

pub fn canonical_ref_event_proposal_bytes(event: &RefEventProposal) -> Result<Vec<u8>, CoreError> {
    limits::validate_event_type(&event.event_type)?;
    if event.base_ref_seq < 0 {
        return Err(invalid("base_ref_seq is negative"));
    }
    let mut flags = 0;
    if event.set_metadata.is_some() {
        flags |= REF_METADATA;
    }
    if event.set_content.is_some() {
        flags |= REF_CONTENT;
    }
    let mut out = Vec::new();
    put_u8(&mut out, KIND_REF);
    put_str(&mut out, &event.event_type)?;
    put_uuid(&mut out, event.owner_item_id.as_str());
    put_u64(&mut out, event.base_ref_seq as u64);
    put_u8(&mut out, flags);
    if let Some(metadata) = &event.set_metadata {
        put_digest(&mut out, &metadata.id().as_bytes());
    }
    if let Some(content) = &event.set_content {
        put_content(&mut out, content.roles())?;
    }
    limits::check_bytes("event payload", out.len(), limits::MAX_EVENT_PAYLOAD_BYTES)?;
    Ok(out)
}

/// Decode one attached-ref envelope. Its metadata stays an absent reference.
pub fn decode_ref_event_proposal(bytes: &[u8]) -> Result<RefEventProposal, DecodeError> {
    limits::check_bytes(
        "event payload",
        bytes.len(),
        limits::MAX_EVENT_PAYLOAD_BYTES,
    )?;
    let mut decoder = Decoder::new(bytes);
    if decoder.u8()? != KIND_REF {
        return Err(bad("not an attached-ref envelope"));
    }
    let event_type = decoder.str()?.to_string();
    limits::validate_event_type(&event_type)?;
    let owner_item_id = decoder.item_id()?;
    let base_ref_seq = decoder.count()?;
    let flags = decoder.u8()?;
    if flags & !(REF_METADATA | REF_CONTENT) != 0 {
        return Err(bad("unknown attached-ref envelope flag"));
    }
    let set_metadata = (flags & REF_METADATA != 0)
        .then(|| decoder.object_id().map(MetadataRef::Absent))
        .transpose()?;
    let set_content = (flags & REF_CONTENT != 0)
        .then(|| decoder.content())
        .transpose()?;
    decoder.finish()?;
    Ok(RefEventProposal {
        owner_item_id,
        event_type,
        set_metadata,
        set_content,
        base_ref_seq,
    })
}

// --- Commit header ---

const COMMIT_OCCURRED_AT: u8 = 1 << 0;
const COMMIT_MESSAGE: u8 = 1 << 1;

/// A decoded commit header. Its commit ID is SHA-256 of the exact bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitHeader {
    pub author_id: AuthorId,
    pub committer_id: CommitterId,
    pub occurred_at: Option<Timestamp>,
    pub message: Option<String>,
    pub event_count: u32,
    pub events_digest: Sha256Digest,
}

pub fn canonical_commit_bytes_for_hash(
    author_id: &AuthorId,
    committer_id: &CommitterId,
    occurred_at: Option<Timestamp>,
    message: Option<&str>,
    event_count: usize,
    events_digest: &Sha256Digest,
) -> Result<Vec<u8>, CoreError> {
    limits::validate_commit_metadata(author_id, committer_id, message, None)?;
    let event_count = u32::try_from(event_count)
        .ok()
        .filter(|count| *count <= i32::MAX as u32)
        .ok_or_else(|| invalid("commit event count exceeds 2^31 - 1"))?;
    let mut flags = 0;
    if occurred_at.is_some() {
        flags |= COMMIT_OCCURRED_AT;
    }
    if message.is_some() {
        flags |= COMMIT_MESSAGE;
    }
    let mut out = Vec::new();
    put_u8(&mut out, KIND_COMMIT);
    put_str(&mut out, author_id)?;
    put_str(&mut out, committer_id)?;
    put_u8(&mut out, flags);
    if let Some(time) = occurred_at {
        put_time(&mut out, time)?;
    }
    if let Some(message) = message {
        put_str(&mut out, message)?;
    }
    put_u32(&mut out, event_count);
    put_digest(&mut out, &sha256_digest_to_digest_bytes(events_digest));
    Ok(out)
}

pub fn decode_commit_header(bytes: &[u8]) -> Result<CommitHeader, DecodeError> {
    let mut decoder = Decoder::new(bytes);
    if decoder.u8()? != KIND_COMMIT {
        return Err(bad("not a commit header"));
    }
    let author_id = decoder.str()?.to_string();
    let committer_id = decoder.str()?.to_string();
    let flags = decoder.u8()?;
    if flags & !(COMMIT_OCCURRED_AT | COMMIT_MESSAGE) != 0 {
        return Err(bad("unknown commit header flag"));
    }
    let occurred_at = (flags & COMMIT_OCCURRED_AT != 0)
        .then(|| decoder.time())
        .transpose()?;
    let message = (flags & COMMIT_MESSAGE != 0)
        .then(|| decoder.str().map(str::to_string))
        .transpose()?;
    let event_count = decoder.u32()?;
    if event_count > i32::MAX as u32 {
        return Err(bad("commit event count exceeds 2^31 - 1"));
    }
    let events_digest = sha256_digest_from_digest_bytes(&decoder.digest()?)
        .map_err(|error| bad(error.to_string()))?;
    decoder.finish()?;
    limits::validate_commit_metadata(&author_id, &committer_id, message.as_deref(), None)?;
    Ok(CommitHeader {
        author_id,
        committer_id,
        occurred_at,
        message,
        event_count,
        events_digest,
    })
}

/// The metadata ID an item or attached-ref envelope names, if any.
pub fn envelope_metadata_id(bytes: &[u8]) -> Result<Option<ObjectId>, DecodeError> {
    match bytes.first() {
        Some(&KIND_ITEM) => Ok(decode_event_proposal(bytes)?
            .set_metadata
            .map(|metadata| metadata.id())),
        Some(&KIND_REF) => Ok(decode_ref_event_proposal(bytes)?
            .set_metadata
            .map(|metadata| metadata.id())),
        _ => Err(bad("not an envelope")),
    }
}
