//! One-way conversion of an unencrypted format 7 history into format 8.
//!
//! Storage-free. A driver reads one repository's published format 7 commits in
//! `repo_seq` order, passes each with its events and their metadata bytes, and
//! writes what [`Converter::convert`] returns. Every sequence, position, time,
//! operation ID and row field stays; envelopes, commit IDs, checkpoints and the
//! item metadata a complete map changes are new. The rules are in
//! `docs/format-7-conversion.md`.
use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use synck_vcs::checkpoint;
use synck_vcs::content::{ContentRef, EventContent};
use synck_vcs::errors::CoreError;
use synck_vcs::metadata::{Metadata, MetadataError, MetadataRef};
use synck_vcs::object::ENCRYPTED_PREFIX;
use synck_vcs::types::{
    CommitId, EventProposal, EventsDigest, ItemId, ObjectId, RefEventProposal, RepoId, Timestamp,
    canonical_commit_bytes_for_hash, canonical_event_proposal_bytes, canonical_json_bytes,
    canonical_ref_event_proposal_bytes, compute_commit_id, parse_hashed_time,
    sha256_prefixed_hex_from_digest_bytes,
};

/// Label keys that repeat a fact the envelope or its commit holds (O11).
const LABEL_FACTS: [&str; 4] = [
    "file_item_id",
    "operation_id",
    "source_commit_id",
    "source_commit_event_idx",
];

/// `mailcal` keys the Hub wrote for its renderings of an email and the object
/// references to them. Format 8 keeps only the originals; a reader rebuilds the
/// rest from `source` with the current parser.
const EMAIL_RENDERINGS: [&str; 8] = [
    "body",
    "body_html_blob_id",
    "raw_rfc822_blob_id",
    "raw_rfc822_provenance",
    "provider_raw_json_blob_id",
    "snippet",
    "inline_images",
    "text_repair",
];

/// How a two-segment event's format 7 metadata becomes format 8 item state.
/// Format 7 left that to each family's reader, so the families' owner says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fold {
    /// Its keys overlay the item's map; a null removes one.
    Patch,
    /// It is the item's whole map.
    Replace,
    /// Its keys describe the event alone and move to its `revision`; the
    /// event changes no state.
    Arguments,
    /// The family's reducer computes the item's map from the event's keys as
    /// written and the map before it. Keys [`Policy::argument`] names also
    /// move to `revision`; the others were consumed and are not stored again.
    Reduce,
}

/// A family's reducer: the event, its keys as written, the commit author and
/// the item's map before the event (empty for a create). Returns the item's
/// map after it.
pub type Reduce = fn(
    &EventProposal,
    &Map<String, Value>,
    &str,
    &Map<String, Value>,
) -> Result<Map<String, Value>, String>;

/// The families' rules. A record's `SET_TITLE` is always the format's own: it
/// sets `title` and nothing else.
#[derive(Clone, Copy)]
pub struct Policy {
    /// The fold of a full two-segment event type.
    pub fold: fn(&str) -> Fold,
    /// Whether a key of a folding event describes that event alone, as an
    /// argument does, so it moves to the event's `revision`.
    pub argument: fn(&str, &str) -> bool,
    /// The reducer of every [`Fold::Reduce`] event type.
    pub reduce: Reduce,
}

impl Policy {
    /// Format 7's own rule: CREATE and SET_METADATA overlay, nothing else is state.
    pub const FORMAT_7: Policy = Policy {
        fold: format_7_fold,
        argument: no_argument,
        reduce: no_reduce,
    };
}

fn format_7_fold(event_type: &str) -> Fold {
    match event_type.split_once("::").map(|(_, verb)| verb) {
        Some("CREATE" | "SET_METADATA") => Fold::Patch,
        _ => Fold::Arguments,
    }
}

fn no_argument(_: &str, _: &str) -> bool {
    false
}

/// For a policy without reducing event types.
pub fn no_reduce(
    event: &EventProposal,
    _: &Map<String, Value>,
    _: &str,
    _: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    Err(format!("{} has no reducer", event.event_type))
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConvertError(pub String);

impl From<CoreError> for ConvertError {
    fn from(error: CoreError) -> Self {
        Self(error.to_string())
    }
}

impl From<MetadataError> for ConvertError {
    fn from(error: MetadataError) -> Self {
        Self(error.to_string())
    }
}

fn fail(detail: impl Into<String>) -> ConvertError {
    ConvertError(detail.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Item,
    Ref,
}

/// One stored format 7 event: its exact canonical JSON payload, the stored
/// bytes of the metadata object it names, or `None` when it names none or the
/// object is erased, and the stored size of every object its converted content
/// names: those of its `set_content`, and the originals an `EMAIL::CREATE` or a
/// `POST` names in metadata. Format 7 content held no sizes, so the driver
/// reads them from its object store; an erased object's size comes from the
/// event's format 7 metadata where it holds one.
pub struct SourceEvent {
    pub kind: Kind,
    pub payload: Vec<u8>,
    pub metadata: Option<Vec<u8>>,
    pub sizes: BTreeMap<ObjectId, u64>,
}

/// One published format 7 commit as stored. Events are in commit-event-index
/// order: item events, then attached-ref events.
pub struct SourceCommit {
    pub commit_id: CommitId,
    pub repo_seq: i64,
    pub author_id: String,
    pub committer_id: String,
    pub message: Option<String>,
    /// The stored RFC 3339 text, which the format 7 commit ID hashes.
    pub occurred_at: Option<String>,
    pub created_at: Timestamp,
    pub events: Vec<SourceEvent>,
}

/// The format 8 form of one commit. `payloads` replace the source payloads
/// index for index; `metadata` holds the objects the new envelopes name that
/// the source did not have.
pub struct Commit {
    pub commit_id: CommitId,
    pub checkpoint: [u8; 32],
    pub payloads: Vec<Vec<u8>>,
    pub metadata: Vec<Metadata>,
}

struct Item {
    item_type: String,
    /// Created in record form; format 7 checked the form against its type.
    record: bool,
    /// The item's metadata as format 7 folded it, without a FILE's `content`.
    metadata: Map<String, Value>,
    /// The item's format 8 metadata is the empty map, or it has none yet.
    written_empty: bool,
    /// Its latest folded payload is erased, so its state is unknown.
    erased: bool,
}

/// A Hub upload intent a FILE event named in format 7 metadata. It is a
/// transport ID, not history, so it leaves the map; the driver keeps it in its
/// own upload records, found by the event's position.
#[derive(Debug, PartialEq, Eq)]
pub struct UploadIntent {
    pub repo_seq: i64,
    pub commit_event_idx: usize,
    pub upload_intent_id: String,
}

pub struct Converter {
    policy: Policy,
    checkpoint: [u8; 32],
    next_repo_seq: i64,
    items: HashMap<ItemId, Item>,
    /// EMAIL events after CREATE whose payload held `mailcal`. Format 8 keeps
    /// only CREATE's, so these values are not carried over.
    pub dropped_mailcal: u64,
    /// Every upload intent dropped so far, in history order.
    pub upload_intents: Vec<UploadIntent>,
}

impl Converter {
    /// Start a repository. The history generation stays; the genesis changes
    /// with the format.
    pub fn new(repo_id: &RepoId, generation: &str, policy: Policy) -> Result<Self, ConvertError> {
        Ok(Self {
            policy,
            checkpoint: checkpoint::genesis(repo_id, generation, false)?,
            next_repo_seq: 1,
            items: HashMap::new(),
            dropped_mailcal: 0,
            upload_intents: Vec::new(),
        })
    }

    pub fn convert(&mut self, commit: SourceCommit) -> Result<Commit, ConvertError> {
        if commit.repo_seq != self.next_repo_seq {
            return Err(fail(format!(
                "commit {} arrived at position {}, expected {}",
                commit.commit_id, commit.repo_seq, self.next_repo_seq
            )));
        }
        if format7_commit_id(&commit)? != commit.commit_id {
            return Err(fail(format!(
                "commit {} does not hash to its ID",
                commit.commit_id
            )));
        }
        let mut digest = EventsDigest::default();
        let mut payloads = Vec::with_capacity(commit.events.len());
        let mut metadata = Vec::new();
        for (index, event) in commit.events.iter().enumerate() {
            let payload = match event.kind {
                Kind::Item => self.item_event(
                    event,
                    &commit.author_id,
                    (commit.repo_seq, index),
                    &mut metadata,
                )?,
                Kind::Ref => ref_event(event, &mut metadata)?,
            };
            digest.push(&payload);
            payloads.push(payload);
        }
        let occurred_at = commit
            .occurred_at
            .as_deref()
            .map(parse_hashed_time)
            .transpose()
            .map_err(fail)?;
        let header = canonical_commit_bytes_for_hash(
            &commit.author_id,
            &commit.committer_id,
            occurred_at,
            commit.message.as_deref(),
            payloads.len(),
            &digest.finish(Some(commit.repo_seq)),
        )?;
        let commit_id = compute_commit_id(&header);
        let repo_seq = u64::try_from(commit.repo_seq).map_err(|_| fail("negative repo_seq"))?;
        self.checkpoint =
            checkpoint::digest(&self.checkpoint, repo_seq, &commit_id, commit.created_at)?;
        self.next_repo_seq += 1;
        Ok(Commit {
            commit_id,
            checkpoint: self.checkpoint,
            payloads,
            metadata,
        })
    }

    fn item_event(
        &mut self,
        source: &SourceEvent,
        author: &str,
        (repo_seq, commit_event_idx): (i64, usize),
        written: &mut Vec<Metadata>,
    ) -> Result<Vec<u8>, ConvertError> {
        let mut event: EventProposal = serde_json::from_value(sized(
            serde_json::from_slice(&source.payload)
                .map_err(|error| fail(format!("format 7 event: {error}")))?,
            &source.sizes,
        )?)
        .map_err(|error| fail(format!("format 7 item event: {error}")))?;
        let mut source_metadata = None;
        let translated = translate(
            &self.policy,
            &mut self.items,
            &mut event,
            author,
            &source.sizes,
            |event| {
                let Some(reference) = &event.set_metadata else {
                    return Ok(Patch::None);
                };
                source_metadata = readable(&reference.id(), source.metadata.as_deref())?;
                Ok(match &source_metadata {
                    Some(metadata) => Patch::Value(object(metadata)?.clone()),
                    None => Patch::Erased,
                })
            },
        )?;
        if let Some(upload_intent_id) = translated.upload_intent {
            self.upload_intents.push(UploadIntent {
                repo_seq,
                commit_event_idx,
                upload_intent_id,
            });
        }
        self.dropped_mailcal += u64::from(translated.dropped_mailcal);
        match (translated.metadata, &source_metadata) {
            (Payload::Unchanged, _) => {}
            (Payload::Omit, _) => event.set_metadata = None,
            (Payload::Map(map), Some(source)) => {
                event.set_metadata = Some(rewrite(source, map, written)?);
            }
            (Payload::Map(_), None) => unreachable!("a map comes from a readable payload"),
        }
        Ok(canonical_event_proposal_bytes(&event)?)
    }
}

/// One event of a federation stream's wire v1 history, as a member holds it
/// after decrypting: the format 7 proposal as JSON, its content a map from role
/// to object ID and without a metadata ID; its metadata, inline on the wire;
/// and the size of every object its converted content names.
pub struct StreamEvent {
    pub kind: Kind,
    /// The proposal's author, which a reducer may read.
    pub author: String,
    pub proposal: Value,
    pub metadata: Option<Value>,
    pub sizes: BTreeMap<ObjectId, u64>,
}

pub enum StreamRead {
    Item(Box<EventProposal>),
    Ref(RefEventProposal),
}

/// Completes a live writer's events with the rules of [`Converter`]: a writer
/// that states each change as format 7 did gets format 8 events back, whose
/// metadata is the item's complete map. Plain repositories only. The caller
/// names every existing item an event changes, with its current map, and
/// passes a commit's events in order; holding them at those maps is its job.
pub struct Completer {
    policy: Policy,
    items: HashMap<ItemId, Item>,
}

impl Completer {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            items: HashMap::new(),
        }
    }

    /// An existing item and its current map: empty when none was written,
    /// `None` when its latest payload is erased, which a reducing event refuses.
    pub fn existing(
        &mut self,
        item_id: ItemId,
        item_type: &str,
        record: bool,
        current: Option<Map<String, Value>>,
    ) {
        let erased = current.is_none();
        let metadata = current.unwrap_or_default();
        self.items.insert(
            item_id,
            Item {
                item_type: item_type.to_string(),
                record,
                written_empty: metadata.is_empty(),
                erased,
                metadata,
            },
        );
    }

    /// Complete one event of a commit by `author` in place, sizes in its
    /// content already. Returns the upload intent a FILE event named, which
    /// leaves history.
    pub fn complete(
        &mut self,
        event: &mut EventProposal,
        author: &str,
    ) -> Result<Option<String>, ConvertError> {
        if event.create_item.is_none() && event.set_metadata.is_none() {
            return Ok(None);
        }
        let sizes: BTreeMap<ObjectId, u64> = event
            .set_content
            .iter()
            .flat_map(|content| content.roles().values())
            .map(|entry| (entry.id.clone(), entry.size))
            .collect();
        let translated = translate(
            &self.policy,
            &mut self.items,
            event,
            author,
            &sizes,
            |event| {
                Ok(match &event.set_metadata {
                    None => Patch::None,
                    Some(reference) => match reference.value() {
                        Some(Value::Object(map)) => Patch::Value(map.clone()),
                        Some(_) => return Err(fail("metadata is not a JSON object")),
                        None => return Err(fail("a written event's metadata is not resolved")),
                    },
                })
            },
        )?;
        match translated.metadata {
            Payload::Unchanged => {}
            Payload::Omit => event.set_metadata = None,
            Payload::Map(map) => {
                event.set_metadata = Some(
                    MetadataRef::new(Value::Object(map))
                        .map_err(|error| fail(error.to_string()))?,
                );
            }
        }
        Ok(translated.upload_intent)
    }
}

/// Reads a federation stream's v1 prefix into format 8 events, with the rules
/// of [`Converter`], from the stream's first event in stream order. A member
/// files what it returns in its own repository; there are no commit IDs or
/// checkpoints to compute. An upload intent belongs to the proposer's Hub and
/// is dropped.
pub struct StreamReader {
    policy: Policy,
    items: HashMap<ItemId, Item>,
    /// EMAIL events after CREATE whose payload held `mailcal`.
    pub dropped_mailcal: u64,
}

impl StreamReader {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            items: HashMap::new(),
            dropped_mailcal: 0,
        }
    }

    pub fn read(&mut self, source: StreamEvent) -> Result<StreamRead, ConvertError> {
        let proposal = sized(source.proposal, &source.sizes)?;
        if proposal.get("metadata_id").is_some() || proposal.get("set_metadata").is_some() {
            return Err(fail("a stream event carries its metadata apart"));
        }
        let metadata = source
            .metadata
            .map(|value| match value {
                Value::Object(map) => Ok(map),
                _ => Err(fail("metadata is not a JSON object")),
            })
            .transpose()?;
        if source.kind == Kind::Ref {
            let mut event: RefEventProposal = serde_json::from_value(proposal)
                .map_err(|error| fail(format!("format 7 ref event: {error}")))?;
            if let Some(mut map) = metadata {
                if event.event_type.starts_with("FILE_VERSION::") {
                    slim_label(&mut map);
                }
                event.set_metadata = Some(MetadataRef::new(Value::Object(map))?);
            }
            return Ok(StreamRead::Ref(event));
        }
        let mut event: EventProposal = serde_json::from_value(proposal)
            .map_err(|error| fail(format!("format 7 item event: {error}")))?;
        let inline = metadata.clone();
        let translated = translate(
            &self.policy,
            &mut self.items,
            &mut event,
            &source.author,
            &source.sizes,
            |_| Ok(metadata.map_or(Patch::None, Patch::Value)),
        )?;
        self.dropped_mailcal += u64::from(translated.dropped_mailcal);
        event.set_metadata = match translated.metadata {
            Payload::Unchanged => inline
                .map(|map| MetadataRef::new(Value::Object(map)))
                .transpose()?,
            Payload::Omit => None,
            Payload::Map(map) => Some(MetadataRef::new(Value::Object(map))?),
        };
        Ok(StreamRead::Item(Box::new(event)))
    }
}

/// An item event's format 7 metadata.
enum Patch {
    None,
    Erased,
    Value(Map<String, Value>),
}

/// What an item event's format 8 metadata becomes.
enum Payload {
    /// The source payload, or none, stays.
    Unchanged,
    Omit,
    Map(Map<String, Value>),
}

struct Translated {
    metadata: Payload,
    upload_intent: Option<String>,
    dropped_mailcal: bool,
}

/// The per-event rules of this crate, shared by stored history and
/// federation streams. `event` is the format 8 proposal, content already
/// sized, its metadata as the source names it; `patch` reads that metadata
/// only for an item-level event.
fn translate(
    policy: &Policy,
    items: &mut HashMap<ItemId, Item>,
    event: &mut EventProposal,
    author: &str,
    sizes: &BTreeMap<ObjectId, u64>,
    patch: impl FnOnce(&EventProposal) -> Result<Patch, ConvertError>,
) -> Result<Translated, ConvertError> {
    let unchanged = Translated {
        metadata: Payload::Unchanged,
        upload_intent: None,
        dropped_mailcal: false,
    };
    let item_id = event
        .item_id
        .clone()
        .ok_or_else(|| fail("format 7 item event has no item_id"))?;
    if let Some(create) = &event.create_item {
        let item = Item {
            item_type: create.item_type.clone(),
            record: create.name.is_none(),
            metadata: Map::new(),
            written_empty: true,
            erased: false,
        };
        if items.insert(item_id.clone(), item).is_some() {
            return Err(fail(format!("item {item_id} is created twice")));
        }
    }
    let item = items
        .get_mut(&item_id)
        .ok_or_else(|| fail(format!("item {item_id} has an event before its CREATE")))?;
    // A longer event type carries a subitem's metadata, a whole replacement
    // already; it stays as it is.
    let Some(verb) = event
        .event_type
        .split_once("::")
        .map(|(_, verb)| verb.to_string())
        .filter(|verb| !verb.contains("::"))
    else {
        return Ok(unchanged);
    };
    if item.item_type == "POST" {
        post_blob_body(event)?;
    }
    let mut patch = match patch(event)? {
        Patch::None => return Ok(unchanged),
        Patch::Erased => None,
        Patch::Value(patch) => Some(patch),
    };
    let mut revision = Map::new();
    let mut upload_intent = None;
    if item.item_type == "FILE"
        && let Some(patch) = &mut patch
    {
        file_size(&item_id, event, patch)?;
        upload_intent = file_revision(event, &verb, patch, &mut revision)?;
    }
    let title = verb == "SET_TITLE" && item.record;
    let fold = if title {
        Fold::Arguments
    } else {
        (policy.fold)(&event.event_type)
    };
    // A reducer reads the event's keys as written and the map before it; it
    // cannot compute a state from an erased payload or an unknown one.
    let reduced = match (fold, &patch) {
        (Fold::Reduce, Some(arguments)) if !item.erased => {
            let state =
                (policy.reduce)(event, arguments, author, &item.metadata).map_err(|error| {
                    fail(format!("{} on item {item_id}: {error}", event.event_type))
                })?;
            if state.contains_key("revision") {
                return Err(fail(format!(
                    "the reducer of {} returned a `revision`",
                    event.event_type
                )));
            }
            Some(state)
        }
        (Fold::Reduce, _) => {
            return Err(fail(format!(
                "{} on item {item_id} meets erased metadata",
                event.event_type
            )));
        }
        _ => None,
    };
    // A key that is not item state is a fact about its event and moves to its
    // `revision`, except the keys the rules below move.
    if let Some(patch) = &mut patch {
        let arguments: Vec<String> = patch
            .keys()
            .filter(|key| fold == Fold::Arguments || (policy.argument)(&event.event_type, key))
            .filter(|key| {
                !((title && key.as_str() == "title")
                    || (item.item_type == "EMAIL" && key.as_str() == "mailcal")
                    || (item.item_type == "POST" && key.as_str() == "body_markdown_blob_id"))
            })
            .cloned()
            .collect();
        for key in arguments {
            let value = patch.remove(&key).expect("listed key");
            if value.is_null() {
                continue;
            }
            if revision.insert(key.clone(), value).is_some() {
                return Err(fail(format!("{} names {key} twice", event.event_type)));
            }
        }
    }
    match reduced {
        Some(state) => item.metadata = state,
        None => item.fold(event, fold, title, patch.as_ref())?,
    }
    let Some(patch) = patch else {
        item.written_empty = false;
        return Ok(unchanged);
    };
    let mut map = item.metadata.clone();
    let mut dropped_mailcal = false;
    if item.item_type == "EMAIL" && verb == "CREATE" {
        email_originals(&item_id, event, &mut map, sizes)?;
    } else if item.item_type == "POST" {
        post_body(&item_id, event, &patch, &mut map, sizes)?;
    } else if item.item_type == "EMAIL" {
        map.remove("mailcal");
        dropped_mailcal = patch.contains_key("mailcal");
    }
    // A FILE whose only key was its size now has no metadata to say, and
    // omitting it leaves the current map, which is empty too.
    let empty = map.is_empty();
    if !revision.is_empty() {
        map.insert("revision".into(), Value::Object(revision));
    }
    let metadata = if item.item_type == "FILE" && map.is_empty() && item.written_empty {
        Payload::Omit
    } else {
        item.written_empty = empty;
        Payload::Map(map)
    };
    Ok(Translated {
        metadata,
        upload_intent,
        dropped_mailcal,
    })
}

impl Item {
    /// A folding event sets the map from its state keys, and an erased one
    /// clears every key; a record's SET_TITLE sets `title`, and an erased one
    /// removes it. Arguments and a FILE's size are gone before this fold.
    fn fold(
        &mut self,
        event: &EventProposal,
        fold: Fold,
        title: bool,
        patch: Option<&Map<String, Value>>,
    ) -> Result<(), ConvertError> {
        match (fold, patch) {
            (Fold::Arguments, _) => {}
            (Fold::Reduce, _) => unreachable!("a reduced event's state comes from its reducer"),
            (Fold::Patch, Some(patch)) => overlay(&mut self.metadata, patch),
            (Fold::Replace, Some(patch)) => {
                self.metadata.clear();
                overlay(&mut self.metadata, patch);
            }
            (_, None) => self.metadata.clear(),
        }
        if fold != Fold::Arguments {
            self.erased = patch.is_none();
        }
        if title && event.event_type == format!("{}::SET_TITLE", self.item_type) {
            match patch {
                Some(patch) => match patch.get("title") {
                    Some(Value::String(title)) if !title.is_empty() => {
                        self.metadata.insert("title".into(), title.as_str().into());
                    }
                    _ => return Err(fail("SET_TITLE has no title")),
                },
                None => {
                    self.metadata.remove("title");
                }
            }
        }
        Ok(())
    }
}

/// Format 7 content maps named objects alone; format 8 entries add each one's
/// stored size, which the driver supplies.
fn sized(mut event: Value, sizes: &BTreeMap<ObjectId, u64>) -> Result<Value, ConvertError> {
    if let Some(Value::Object(roles)) = event.get_mut("set_content") {
        for entry in roles.values_mut() {
            let id: ObjectId = serde_json::from_value(entry.clone())
                .map_err(|error| fail(format!("format 7 content: {error}")))?;
            *entry = serde_json::to_value(content_ref(id, sizes)?).expect("content entry is JSON");
        }
    }
    Ok(event)
}

fn content_ref(id: ObjectId, sizes: &BTreeMap<ObjectId, u64>) -> Result<ContentRef, ConvertError> {
    let size = *sizes
        .get(&id)
        .ok_or_else(|| fail(format!("object {id} has no stored size")))?;
    Ok(ContentRef { id, size })
}

/// A FILE's format 7 metadata held its size as `content.size`, which its
/// bytes entry now holds. Where a content event names one it must equal the
/// supplied size; the key leaves every map. Empty files and received revisions
/// named none, and a live writer may leave the size to the entry.
fn file_size(
    item_id: &ItemId,
    event: &EventProposal,
    patch: &mut Map<String, Value>,
) -> Result<(), ConvertError> {
    let Some(content) = patch.remove("content") else {
        return Ok(());
    };
    let Some(size) = event
        .set_content
        .as_ref()
        .filter(|content| content.only("bytes").is_some())
        .and_then(|content| content.size("bytes"))
    else {
        return Ok(());
    };
    if content != serde_json::json!({"size": size}) {
        return Err(fail(format!(
            "FILE {item_id} metadata does not hold its stored size {size}"
        )));
    }
    Ok(())
}

/// Format 7 kept a FILE revision's facts in its map, so a CREATE's became item
/// state and every later map repeated them. They move to the event's own
/// `revision`, and the Hub's upload intent leaves history; it is returned for
/// the driver. Only content events describe a revision.
fn file_revision(
    event: &EventProposal,
    verb: &str,
    patch: &mut Map<String, Value>,
    revision: &mut Map<String, Value>,
) -> Result<Option<String>, ConvertError> {
    for (from, to) in [("revision_origin", "origin"), ("provenance", "provenance")] {
        match patch.remove(from) {
            None | Some(Value::Null) => {}
            Some(value) => {
                revision.insert(to.into(), value);
            }
        }
    }
    let intent = match patch.remove("upload_intent_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(intent)) => Some(intent),
        Some(_) => return Err(fail("upload_intent_id is not a string")),
    };
    if (!revision.is_empty() || intent.is_some()) && !matches!(verb, "CREATE" | "SET_CONTENT") {
        return Err(fail(format!(
            "{} holds revision facts outside a content event",
            event.event_type
        )));
    }
    Ok(intent)
}

/// The Hub kept an email's originals only as references inside `mailcal`: the
/// imported file in `source.blob_id`, or else a provider message's raw MIME in
/// `raw_rfc822_blob_id`, and the provider's JSON in `provider_raw_json_blob_id`.
/// They move to the `source` and `provider` roles of `EMAIL::CREATE`, and the
/// renderings go, as do the Hub's job IDs; a provider message's `archived_at`
/// is its `imported_at`. An email with no original is refused: nothing could rebuild
/// its text.
fn email_originals(
    item_id: &ItemId,
    event: &mut EventProposal,
    map: &mut Map<String, Value>,
    sizes: &BTreeMap<ObjectId, u64>,
) -> Result<(), ConvertError> {
    let blob = |value: Option<&Value>| match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) => ObjectId::parse(id)
            .map(Some)
            .map_err(|error| fail(format!("EMAIL {item_id} mailcal: {error}"))),
        Some(_) => Err(fail(format!(
            "EMAIL {item_id} mailcal blob ID is not a string"
        ))),
    };
    let mut roles = BTreeMap::new();
    if let Some(Value::Object(mailcal)) = map.get_mut("mailcal") {
        let source = match mailcal.get_mut("source") {
            Some(Value::Object(source)) => blob(source.remove("blob_id").as_ref())?,
            _ => None,
        };
        let generated = mailcal.get("raw_rfc822_provenance") == Some(&"generated".into());
        let raw = blob(mailcal.get("raw_rfc822_blob_id"))?.filter(|_| !generated);
        if let Some(source) = source.or(raw) {
            roles.insert("source".to_string(), content_ref(source, sizes)?);
        }
        if let Some(provider) = blob(mailcal.get("provider_raw_json_blob_id"))? {
            roles.insert("provider".to_string(), content_ref(provider, sizes)?);
        }
        for key in EMAIL_RENDERINGS {
            mailcal.remove(key);
        }
        // Hub job and upload IDs say how an email arrived, not what it is.
        mailcal.remove("archive_command_id");
        if let Some(Value::Object(origin)) = mailcal.get_mut("origin") {
            origin.remove("upload_intent_id");
        }
        if let Some(at) = mailcal.remove("archived_at")
            && !mailcal.contains_key("imported_at")
        {
            mailcal.insert("imported_at".into(), at);
        }
    }
    match &event.set_content {
        Some(content) if roles.is_empty() || content.roles() == &roles => {}
        Some(_) => {
            return Err(fail(format!(
                "EMAIL {item_id} content and mailcal name different originals"
            )));
        }
        None if roles.contains_key("source") => {
            event.set_content = Some(EventContent::new(roles).map_err(fail)?);
        }
        None => return Err(fail(format!("EMAIL {item_id} names no original"))),
    }
    Ok(())
}

/// A Hub POST's body is an uploaded Markdown blob, which the Hub named with the
/// `body` role that elsewhere holds salted text. Format 8 gives a role one form,
/// so the blob becomes `bytes`.
fn post_blob_body(event: &mut EventProposal) -> Result<(), ConvertError> {
    let Some(content) = &event.set_content else {
        return Ok(());
    };
    let mut roles = content.roles().clone();
    if let Some(body) = roles.remove("body") {
        if roles.insert("bytes".to_string(), body).is_some() {
            return Err(fail("POST content has both body and bytes"));
        }
        event.set_content = Some(EventContent::new(roles).map_err(fail)?);
    }
    Ok(())
}

/// Early Hub POSTs named their body in metadata as `body_markdown_blob_id`;
/// the event's own key becomes its `bytes` role, and no map keeps it.
fn post_body(
    item_id: &ItemId,
    event: &mut EventProposal,
    patch: &Map<String, Value>,
    map: &mut Map<String, Value>,
    sizes: &BTreeMap<ObjectId, u64>,
) -> Result<(), ConvertError> {
    map.remove("body_markdown_blob_id");
    let body = match patch.get("body_markdown_blob_id") {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::String(id)) => ObjectId::parse(id)
            .map_err(|error| fail(format!("POST {item_id} body_markdown_blob_id: {error}")))?,
        Some(_) => {
            return Err(fail(format!(
                "POST {item_id} body_markdown_blob_id is not a string"
            )));
        }
    };
    match &event.set_content {
        None => {
            let body = content_ref(body, sizes)?;
            event.set_content = Some(EventContent::bytes(body.id, body.size));
        }
        Some(content) if content.get("bytes") == Some(&body) => {}
        Some(_) => {
            return Err(fail(format!(
                "POST {item_id} content and metadata name different bodies"
            )));
        }
    }
    Ok(())
}

fn ref_event(source: &SourceEvent, written: &mut Vec<Metadata>) -> Result<Vec<u8>, ConvertError> {
    let mut event: RefEventProposal = serde_json::from_value(sized(
        serde_json::from_slice(&source.payload)
            .map_err(|error| fail(format!("format 7 event: {error}")))?,
        &source.sizes,
    )?)
    .map_err(|error| fail(format!("format 7 ref event: {error}")))?;
    if event.event_type.starts_with("FILE_VERSION::")
        && let Some(reference) = &event.set_metadata
        && let Some(source_metadata) = readable(&reference.id(), source.metadata.as_deref())?
    {
        let mut map = object(&source_metadata)?.clone();
        slim_label(&mut map);
        event.set_metadata = Some(rewrite(&source_metadata, map, written)?);
    }
    Ok(canonical_ref_event_proposal_bytes(&event)?)
}

/// A label drops what its owner, commit and revision already hold (O11).
fn slim_label(map: &mut Map<String, Value>) {
    for key in LABEL_FACTS {
        map.remove(key);
    }
}

/// The verified source object, or `None` when it is erased.
fn readable(id: &ObjectId, stored: Option<&[u8]>) -> Result<Option<Metadata>, ConvertError> {
    let Some(stored) = stored else {
        return Ok(None);
    };
    if stored.starts_with(ENCRYPTED_PREFIX) {
        return Err(fail("encrypted repositories are not converted"));
    }
    Ok(Some(Metadata::verify(id, stored.to_vec())?))
}

fn object(metadata: &Metadata) -> Result<&Map<String, Value>, ConvertError> {
    metadata
        .value()
        .as_object()
        .ok_or_else(|| fail("metadata is not a JSON object"))
}

/// Apply a patch: a null removes its key, because null meant unset.
fn overlay(map: &mut Map<String, Value>, patch: &Map<String, Value>) {
    for (key, value) in patch {
        match value {
            Value::Null => map.remove(key),
            value => map.insert(key.clone(), value.clone()),
        };
    }
}

/// The source object when it already holds `map`, otherwise a rewrite with the
/// source's salt, so a rerun gives the same object ID.
fn rewrite(
    source: &Metadata,
    map: Map<String, Value>,
    written: &mut Vec<Metadata>,
) -> Result<MetadataRef, ConvertError> {
    let map = Value::Object(map);
    if canonical_json_bytes(&map)? == source.payload()[synck_vcs::object::HEADER_BYTES..] {
        return Ok(MetadataRef::Available(source.clone()));
    }
    let metadata = source.rewrite(map)?;
    written.push(metadata.clone());
    Ok(MetadataRef::Available(metadata))
}

/// Format 7's commit ID: RFC 8785 JSON over the header, whose events digest
/// frames each event digest with a family byte.
fn format7_commit_id(commit: &SourceCommit) -> Result<CommitId, ConvertError> {
    let events_digest: [u8; 32] = if commit.events.is_empty() {
        Sha256::digest(commit.repo_seq.to_be_bytes()).into()
    } else {
        let mut hash = Sha256::new();
        let mut refs = false;
        for event in &commit.events {
            refs |= event.kind == Kind::Ref;
            if refs && event.kind == Kind::Item {
                return Err(fail("an item event follows an attached-ref event"));
            }
            hash.update([u8::from(event.kind == Kind::Ref)]);
            hash.update(Sha256::digest(&event.payload));
        }
        hash.finalize().into()
    };
    let header = serde_json::json!({
        "object_type": "vcs_commit",
        "format_version": 6,
        "author_id": commit.author_id,
        "committer_id": commit.committer_id,
        "occurred_at": commit.occurred_at,
        "message": commit.message,
        "event_count": commit.events.len(),
        "events_digest": sha256_prefixed_hex_from_digest_bytes(events_digest),
    });
    Ok(compute_commit_id(&canonical_json_bytes(&header)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use synck_vcs::codec::decode_event_proposal;

    const REPO: &str = "019a2900-0000-7000-8000-000000000001";
    const GENERATION: &str = "019a2900-0000-7000-8000-000000000002";

    /// Format 7's rule, and a POST's content event carries its state, as the
    /// Hub's does.
    const POLICY: Policy = Policy {
        fold: |event_type| match event_type {
            "POST::SET_CONTENT" => Fold::Patch,
            _ => (Policy::FORMAT_7.fold)(event_type),
        },
        argument: |_, _| false,
        reduce: no_reduce,
    };

    fn converter() -> Converter {
        Converter::new(&RepoId::parse(REPO).unwrap(), GENERATION, POLICY).unwrap()
    }

    /// The stored size of each object these tests name: `[n; 32]` holds n bytes.
    fn sizes() -> BTreeMap<ObjectId, u64> {
        (1..=8)
            .map(|n| (ObjectId::from_digest([n; 32]), u64::from(n)))
            .collect()
    }

    /// A format 7 commit of item events, each with its metadata object, or
    /// `None` for an erased one.
    fn source(repo_seq: i64, events: Vec<(Value, Option<&Metadata>)>) -> SourceCommit {
        let mut commit = SourceCommit {
            commit_id: compute_commit_id(b""),
            repo_seq,
            author_id: "owner".into(),
            committer_id: "owner".into(),
            message: None,
            occurred_at: None,
            created_at: parse_hashed_time("2026-10-28T00:00:01.000000001Z").unwrap(),
            events: events
                .into_iter()
                .map(|(event, metadata)| SourceEvent {
                    kind: Kind::Item,
                    payload: canonical_json_bytes(&event).unwrap(),
                    metadata: metadata.map(|metadata| metadata.payload().to_vec()),
                    sizes: sizes(),
                })
                .collect(),
        };
        commit.commit_id = format7_commit_id(&commit).unwrap();
        commit
    }

    fn event(item: &str, event_type: &str, base_seq: Option<i64>, metadata: &Metadata) -> Value {
        let mut event = json!({
            "item_id": item, "event_type": event_type,
            "metadata_id": metadata.id(),
        });
        match base_seq {
            Some(base_seq) => event["base_seq"] = base_seq.into(),
            None => {
                let item_type = event_type.split("::").next().unwrap();
                event["create_item"] = match item_type {
                    "FILE" => json!({"item_type": item_type, "name": "a.txt"}),
                    _ => json!({"item_type": item_type}),
                };
                if item_type == "FILE" {
                    event["set_content"] = json!({"bytes": ObjectId::from_digest([1; 32])});
                }
            }
        }
        event
    }

    fn metadata(value: Value) -> Metadata {
        Metadata::new(value).unwrap()
    }

    /// The map each converted event names, in order.
    fn maps(commit: &Commit, sources: &[&Metadata]) -> Vec<Value> {
        commit
            .payloads
            .iter()
            .map(|payload| {
                let id = decode_event_proposal(payload)
                    .unwrap()
                    .set_metadata
                    .unwrap()
                    .id();
                commit
                    .metadata
                    .iter()
                    .chain(sources.iter().copied())
                    .find(|metadata| metadata.id() == id)
                    .map(|metadata| metadata.value().clone())
                    .unwrap_or(Value::Null)
            })
            .collect()
    }

    const FILE: &str = "019a2900-0000-7000-8000-000000000a01";

    #[test]
    fn patches_become_complete_maps_and_a_rerun_gives_the_same_ids() {
        let create = metadata(json!({"color": "red", "content": {"size": 1}}));
        let content = metadata(json!({
            "content": {"size": 2}, "revision_origin": "LOCAL", "upload_intent_id": "intent",
        }));
        let edit = metadata(json!({"color": null, "tag": "x"}));
        let mut events = vec![(event(FILE, "FILE::CREATE", None, &create), Some(&create))];
        let mut written = event(FILE, "FILE::SET_CONTENT", Some(1), &content);
        written["set_content"] = json!({"bytes": ObjectId::from_digest([2; 32])});
        events.push((written, Some(&content)));
        events.push((
            event(FILE, "FILE::SET_METADATA", Some(1), &edit),
            Some(&edit),
        ));
        let commit = || converter().convert(source(1, events.clone())).unwrap();

        let first = commit();
        assert_eq!(
            maps(&first, &[&create, &content, &edit]),
            [
                // The size moved to the bytes entry.
                json!({"color": "red"}),
                // Its revision facts describe that event alone, and the
                // upload intent leaves history.
                json!({"color": "red", "revision": {"origin": "LOCAL"}}),
                json!({"tag": "x"}),
            ]
        );
        let sizes = first
            .payloads
            .iter()
            .map(|payload| {
                decode_event_proposal(payload)
                    .unwrap()
                    .set_content
                    .and_then(|content| content.size("bytes"))
            })
            .collect::<Vec<_>>();
        assert_eq!(sizes, [Some(1), Some(2), None]);
        assert_eq!(first.metadata.len(), 3);
        assert_eq!(first.commit_id, commit().commit_id);
        let mut converter = converter();
        converter.convert(source(1, events.clone())).unwrap();
        assert_eq!(
            converter.upload_intents,
            [UploadIntent {
                repo_seq: 1,
                commit_event_idx: 1,
                upload_intent_id: "intent".into(),
            }]
        );
    }

    #[test]
    fn a_file_create_keeps_its_revision_facts_out_of_item_state() {
        let provenance = json!({"source": "DESKTOP", "committed_by": "a", "human_editor_ids": []});
        let create = metadata(json!({
            "color": "red", "content": {"size": 1}, "provenance": provenance,
        }));
        let edit = metadata(json!({"tag": "x"}));
        let commit = converter()
            .convert(source(
                1,
                vec![
                    (event(FILE, "FILE::CREATE", None, &create), Some(&create)),
                    (
                        event(FILE, "FILE::SET_METADATA", Some(1), &edit),
                        Some(&edit),
                    ),
                ],
            ))
            .unwrap();
        assert_eq!(
            maps(&commit, &[&create, &edit]),
            [
                json!({"color": "red", "revision": {"provenance": provenance}}),
                json!({"color": "red", "tag": "x"}),
            ]
        );
        let stray = metadata(json!({"revision_origin": "LOCAL"}));
        let error = converter()
            .convert(source(
                1,
                vec![
                    (event(FILE, "FILE::CREATE", None, &create), Some(&create)),
                    (
                        event(FILE, "FILE::SET_METADATA", Some(1), &stray),
                        Some(&stray),
                    ),
                ],
            ))
            .err()
            .unwrap();
        assert!(error.0.contains("outside a content event"), "{error}");
    }

    /// The same history read as a stream: metadata inline, no metadata ID.
    fn stream(events: &[(Value, Option<&Metadata>)]) -> Result<Vec<EventProposal>, ConvertError> {
        let mut reader = StreamReader::new(POLICY);
        events
            .iter()
            .map(|(event, metadata)| {
                let mut proposal = event.clone();
                proposal.as_object_mut().unwrap().remove("metadata_id");
                match reader.read(StreamEvent {
                    kind: Kind::Item,
                    author: "owner".into(),
                    proposal,
                    metadata: metadata.map(|metadata| metadata.value().clone()),
                    sizes: sizes(),
                })? {
                    StreamRead::Item(event) => Ok(*event),
                    StreamRead::Ref(_) => unreachable!(),
                }
            })
            .collect()
    }

    #[test]
    fn a_stream_reads_as_its_stored_history_converts() {
        let email = "019a2900-0000-7000-8000-000000000a02";
        let original = ObjectId::from_digest([3; 32]);
        let file_create = metadata(json!({"color": "red", "content": {"size": 1}}));
        let file_content = metadata(json!({
            "content": {"size": 2}, "revision_origin": "LOCAL", "upload_intent_id": "intent",
        }));
        let file_edit = metadata(json!({"color": null, "tag": "x"}));
        let email_create =
            metadata(json!({"mailcal": {"subject": "a", "source": {"blob_id": original}}}));
        let email_repair = metadata(json!({"mailcal": {"subject": "b"}}));
        let attachment = metadata(json!({"filename": "a.pdf"}));
        let mut content = event(FILE, "FILE::SET_CONTENT", Some(1), &file_content);
        content["set_content"] = json!({"bytes": ObjectId::from_digest([2; 32])});
        let events = vec![
            (
                event(FILE, "FILE::CREATE", None, &file_create),
                Some(&file_create),
            ),
            (content, Some(&file_content)),
            (
                event(FILE, "FILE::SET_METADATA", Some(1), &file_edit),
                Some(&file_edit),
            ),
            (
                event(email, "EMAIL::CREATE", None, &email_create),
                Some(&email_create),
            ),
            (
                event(email, "EMAIL::SET_METADATA", Some(1), &email_repair),
                Some(&email_repair),
            ),
            (
                event(
                    email,
                    "EMAIL::ATTACHMENT::SET_METADATA",
                    Some(1),
                    &attachment,
                ),
                Some(&attachment),
            ),
        ];
        let stored = converter().convert(source(1, events.clone())).unwrap();
        let sources = [
            &file_create,
            &file_content,
            &file_edit,
            &email_create,
            &email_repair,
            &attachment,
        ];
        let read = stream(&events).unwrap();
        assert_eq!(
            read.iter()
                .map(|event| event
                    .set_metadata
                    .as_ref()
                    .and_then(MetadataRef::value)
                    .cloned()
                    .unwrap_or(Value::Null))
                .collect::<Vec<_>>(),
            maps(&stored, &sources)
        );
        // Apart from their metadata objects, the events are the same.
        for (read, payload) in read.into_iter().zip(&stored.payloads) {
            let mut read = read;
            let mut stored = decode_event_proposal(payload).unwrap();
            read.set_metadata = None;
            stored.set_metadata = None;
            assert_eq!(
                canonical_event_proposal_bytes(&read).unwrap(),
                canonical_event_proposal_bytes(&stored).unwrap()
            );
        }
        // A stream event names no metadata object.
        let mut named = events[0].0.clone();
        named["metadata_id"] = json!(file_create.id());
        let error = StreamReader::new(POLICY)
            .read(StreamEvent {
                kind: Kind::Item,
                author: "owner".into(),
                proposal: named,
                metadata: None,
                sizes: sizes(),
            })
            .err()
            .unwrap();
        assert!(error.0.contains("metadata apart"), "{error}");
    }

    #[test]
    fn a_stream_label_drops_what_its_ref_holds() {
        let read = StreamReader::new(POLICY)
            .read(StreamEvent {
                kind: Kind::Ref,
                author: "owner".into(),
                proposal: json!({
                    "owner_item_id": FILE, "event_type": "FILE_VERSION::CREATE",
                    "base_ref_seq": 0,
                }),
                metadata: Some(json!({"version_id": "v", "file_item_id": FILE, "label": "L"})),
                sizes: sizes(),
            })
            .unwrap();
        let StreamRead::Ref(event) = read else {
            panic!("a ref event reads as a ref event");
        };
        assert_eq!(
            event.set_metadata.unwrap().value().unwrap(),
            &json!({"version_id": "v", "label": "L"})
        );
    }

    #[test]
    fn a_file_size_must_match_its_stored_object() {
        for (value, sizes, refusal) in [
            (json!({"content": {"size": 2}}), sizes(), "stored size 1"),
            (
                json!({"content": {"size": 1}}),
                BTreeMap::new(),
                "no stored size",
            ),
        ] {
            let create = metadata(value);
            let mut commit = source(
                1,
                vec![(event(FILE, "FILE::CREATE", None, &create), Some(&create))],
            );
            commit.events[0].sizes = sizes;
            let error = converter().convert(commit).err().unwrap();
            assert!(error.0.contains(refusal), "{error}");
        }
        // A FILE whose only key was its size carries no metadata.
        let create = metadata(json!({"content": {"size": 1}}));
        let commit = converter()
            .convert(source(
                1,
                vec![(event(FILE, "FILE::CREATE", None, &create), Some(&create))],
            ))
            .unwrap();
        let converted = decode_event_proposal(&commit.payloads[0]).unwrap();
        assert!(converted.set_metadata.is_none());
        assert!(commit.metadata.is_empty());
        // An empty or received FILE named no size; its entry holds it.
        let create = metadata(json!({"revision_origin": "FEDERATED"}));
        let commit = converter()
            .convert(source(
                1,
                vec![(event(FILE, "FILE::CREATE", None, &create), Some(&create))],
            ))
            .unwrap();
        let converted = decode_event_proposal(&commit.payloads[0]).unwrap();
        assert_eq!(converted.set_content.unwrap().size("bytes"), Some(1));
    }

    #[test]
    fn an_event_keeps_its_arguments_in_its_revision() {
        let chat = "019a2900-0000-7000-8000-000000000a03";
        let create = metadata(json!({"title": "Room", "color": "red"}));
        let add = metadata(json!({"assistant_id": "bot", "note": null}));
        let title = metadata(json!({"title": "Hall"}));
        let commit = converter()
            .convert(source(
                1,
                vec![
                    (event(chat, "CHAT::CREATE", None, &create), Some(&create)),
                    (
                        event(chat, "CHAT::ADD_ASSISTANT", Some(1), &add),
                        Some(&add),
                    ),
                    (
                        event(chat, "CHAT::SET_TITLE", Some(2), &title),
                        Some(&title),
                    ),
                ],
            ))
            .unwrap();
        assert_eq!(
            maps(&commit, &[&create, &add, &title]),
            [
                json!({"title": "Room", "color": "red"}),
                // An argument describes its event, never the item.
                json!({"title": "Room", "color": "red", "revision": {"assistant_id": "bot"}}),
                json!({"title": "Hall", "color": "red"}),
            ]
        );
    }

    #[test]
    fn a_policy_replaces_maps_and_moves_arguments() {
        let task = "019a2900-0000-7000-8000-000000000a04";
        let policy = Policy {
            fold: |event_type| match event_type {
                "TASK::SET_METADATA" => Fold::Replace,
                _ => (Policy::FORMAT_7.fold)(event_type),
            },
            argument: |event_type, key| event_type == "TASK::CREATE" && key == "template",
            reduce: no_reduce,
        };
        let create = metadata(json!({"title": "Plan", "due": "soon", "template": "weekly"}));
        let edit = metadata(json!({"title": "Ship"}));
        let commit = Converter::new(&RepoId::parse(REPO).unwrap(), GENERATION, policy)
            .unwrap()
            .convert(source(
                1,
                vec![
                    (event(task, "TASK::CREATE", None, &create), Some(&create)),
                    (
                        event(task, "TASK::SET_METADATA", Some(1), &edit),
                        Some(&edit),
                    ),
                ],
            ))
            .unwrap();
        assert_eq!(
            maps(&commit, &[&create, &edit]),
            [
                json!({"title": "Plan", "due": "soon", "revision": {"template": "weekly"}}),
                // A replacement holds no key it does not name.
                json!({"title": "Ship"}),
            ]
        );
    }

    /// A TASK's `ADD_TAG` adds its tag to `tags` and records its author; its
    /// `note` describes the event alone.
    const REDUCING: Policy = Policy {
        fold: |event_type| match event_type {
            "TASK::ADD_TAG" => Fold::Reduce,
            _ => (Policy::FORMAT_7.fold)(event_type),
        },
        argument: |event_type, key| event_type == "TASK::ADD_TAG" && key == "note",
        reduce: |_, arguments, author, previous| {
            let tag = arguments["tag"].as_str().ok_or("no tag")?;
            let mut map = previous.clone();
            let tags = map
                .entry("tags")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or("tags is not a list")?;
            tags.push(tag.into());
            map.insert("tagged_by".into(), author.into());
            Ok(map)
        },
    };

    const TASK: &str = "019a2900-0000-7000-8000-000000000a04";

    #[test]
    fn a_reducer_writes_the_state_and_only_named_arguments_stay() {
        let create = metadata(json!({"title": "Plan"}));
        let tag = metadata(json!({"tag": "urgent", "note": "from triage"}));
        let commit = Converter::new(&RepoId::parse(REPO).unwrap(), GENERATION, REDUCING)
            .unwrap()
            .convert(source(
                1,
                vec![
                    (event(TASK, "TASK::CREATE", None, &create), Some(&create)),
                    (event(TASK, "TASK::ADD_TAG", Some(1), &tag), Some(&tag)),
                ],
            ))
            .unwrap();
        assert_eq!(
            maps(&commit, &[&create, &tag]),
            [
                json!({"title": "Plan"}),
                // `tag` was consumed by the reducer and is not stored again.
                json!({
                    "title": "Plan", "tags": ["urgent"], "tagged_by": "owner",
                    "revision": {"note": "from triage"},
                }),
            ]
        );
    }

    #[test]
    fn a_reducer_refuses_erased_metadata() {
        let create = metadata(json!({"title": "Plan"}));
        let edit = metadata(json!({"title": "Ship"}));
        let tag = metadata(json!({"tag": "urgent"}));
        // The map before the event is erased.
        let error = Converter::new(&RepoId::parse(REPO).unwrap(), GENERATION, REDUCING)
            .unwrap()
            .convert(source(
                1,
                vec![
                    (event(TASK, "TASK::CREATE", None, &create), Some(&create)),
                    (event(TASK, "TASK::SET_METADATA", Some(1), &edit), None),
                    (event(TASK, "TASK::ADD_TAG", Some(2), &tag), Some(&tag)),
                ],
            ))
            .err()
            .unwrap();
        assert!(error.0.contains("meets erased metadata"), "{error}");
        // The event's own payload is erased.
        let error = Converter::new(&RepoId::parse(REPO).unwrap(), GENERATION, REDUCING)
            .unwrap()
            .convert(source(
                1,
                vec![
                    (event(TASK, "TASK::CREATE", None, &create), Some(&create)),
                    (event(TASK, "TASK::ADD_TAG", Some(1), &tag), None),
                ],
            ))
            .err()
            .unwrap();
        assert!(error.0.contains("meets erased metadata"), "{error}");
        // A live write whose item's latest payload is erased.
        let task = ItemId::parse(TASK).unwrap();
        let mut completer = Completer::new(REDUCING);
        completer.existing(task.clone(), "TASK", true, None);
        let mut write = EventProposal {
            item_id: Some(task),
            event_type: "TASK::ADD_TAG".into(),
            set_metadata: Some(MetadataRef::new(json!({"tag": "urgent"})).unwrap()),
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
            base_seq: Some(2),
        };
        let error = completer.complete(&mut write, "owner").err().unwrap();
        assert!(error.0.contains("meets erased metadata"), "{error}");
    }

    #[test]
    fn a_live_write_gets_its_complete_map() {
        let file = ItemId::parse(FILE).unwrap();
        let mut completer = Completer::new(Policy::FORMAT_7);
        completer.existing(
            file.clone(),
            "FILE",
            false,
            Some(
                json!({"color": "red", "tag": "a"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        );
        let change = |event_type: &str, metadata: Value| EventProposal {
            item_id: Some(file.clone()),
            event_type: event_type.into(),
            set_metadata: Some(MetadataRef::new(metadata).unwrap()),
            set_content: (event_type == "FILE::SET_CONTENT")
                .then(|| EventContent::bytes(ObjectId::from_digest([1; 32]), 1)),
            create_item: None,
            set_parent: None,
            set_name: None,
            set_link: None,
            merged_from: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: Some(3),
        };
        let mut content = change(
            "FILE::SET_CONTENT",
            json!({"revision_origin": "LOCAL", "upload_intent_id": "u-1"}),
        );
        assert_eq!(
            completer
                .complete(&mut content, "owner")
                .unwrap()
                .as_deref(),
            Some("u-1")
        );
        let mut edit = change("FILE::SET_METADATA", json!({"tag": null, "note": "b"}));
        assert_eq!(completer.complete(&mut edit, "owner").unwrap(), None);
        let map = |event: &EventProposal| event.set_metadata.as_ref().unwrap().value().cloned();
        assert_eq!(
            map(&content),
            Some(json!({"color": "red", "tag": "a", "revision": {"origin": "LOCAL"}}))
        );
        assert_eq!(map(&edit), Some(json!({"color": "red", "note": "b"})));
    }

    #[test]
    fn an_email_keeps_mailcal_on_create_only() {
        let email = "019a2900-0000-7000-8000-000000000a02";
        let original = ObjectId::from_digest([3; 32]);
        let create =
            metadata(json!({"mailcal": {"subject": "a", "source": {"blob_id": original}}}));
        let repair = metadata(json!({"mailcal": {"subject": "b", "body_html_blob_id": original}}));
        let title = metadata(json!({"title": "T"}));
        let mut converter = converter();
        let commit = converter
            .convert(source(
                1,
                vec![
                    (event(email, "EMAIL::CREATE", None, &create), Some(&create)),
                    (
                        event(email, "EMAIL::SET_METADATA", Some(1), &repair),
                        Some(&repair),
                    ),
                    (
                        event(email, "EMAIL::SET_TITLE", Some(1), &title),
                        Some(&title),
                    ),
                ],
            ))
            .unwrap();
        assert_eq!(
            maps(&commit, &[&create, &repair, &title]),
            [
                json!({"mailcal": {"subject": "a", "source": {}}}),
                json!({}),
                json!({"title": "T"})
            ]
        );
        assert_eq!(converter.dropped_mailcal, 1);
    }

    #[test]
    fn a_hub_email_moves_its_originals_to_roles_and_drops_renderings() {
        let email = "019a2900-0000-7000-8000-000000000a02";
        let raw = ObjectId::from_digest([4; 32]);
        let json = ObjectId::from_digest([5; 32]);
        let rendering = ObjectId::from_digest([6; 32]);
        let provider = metadata(json!({"mailcal": {
            "v": 1, "subject": "s", "snippet": "x",
            "body": {"kind": "MARKDOWN", "blob_id": rendering},
            "body_html_blob_id": rendering, "raw_rfc822_blob_id": raw,
            "provider_raw_json_blob_id": json,
            "inline_images": [{"blob_id": rendering}],
            "archived_at": "2026-03-29T12:00:00Z", "archive_command_id": "c",
        }}));
        let commit = converter()
            .convert(source(
                1,
                vec![(
                    event(email, "EMAIL::CREATE", None, &provider),
                    Some(&provider),
                )],
            ))
            .unwrap();
        let converted = decode_event_proposal(&commit.payloads[0]).unwrap();
        let content = converted.set_content.unwrap();
        assert_eq!(content.get("source"), Some(&raw));
        assert_eq!(content.get("provider"), Some(&json));
        assert_eq!(
            maps(&commit, &[])[0],
            json!({"mailcal": {"v": 1, "subject": "s", "imported_at": "2026-03-29T12:00:00Z"}})
        );

        // A generated RFC 822 is a rendering, never the original.
        let msg = metadata(json!({"mailcal": {
            "v": 2, "raw_rfc822_blob_id": raw, "raw_rfc822_provenance": "generated",
        }}));
        let error = converter()
            .convert(source(
                1,
                vec![(event(email, "EMAIL::CREATE", None, &msg), Some(&msg))],
            ))
            .err()
            .unwrap();
        assert!(error.0.contains("names no original"), "{error}");
    }

    #[test]
    fn a_post_body_becomes_its_bytes_role() {
        let post = "019a2900-0000-7000-8000-000000000a03";
        let body = ObjectId::from_digest([7; 32]);
        let next = ObjectId::from_digest([8; 32]);
        let create = metadata(json!({"title": "P", "body_markdown_blob_id": body}));
        let edit = metadata(json!({"summary": "s"}));
        let upload = metadata(json!({"summary": "t"}));
        let mut uploaded = event(post, "POST::SET_CONTENT", Some(2), &upload);
        uploaded["set_content"] = json!({"body": next});
        let commit = converter()
            .convert(source(
                1,
                vec![
                    (event(post, "POST::CREATE", None, &create), Some(&create)),
                    (
                        event(post, "POST::SET_METADATA", Some(1), &edit),
                        Some(&edit),
                    ),
                    (uploaded, Some(&upload)),
                ],
            ))
            .unwrap();
        let content = |index: usize| {
            decode_event_proposal(&commit.payloads[index])
                .unwrap()
                .set_content
        };
        assert_eq!(content(0).unwrap().get("bytes"), Some(&body));
        // The folded key is not repeated, and the later event changes no body.
        assert!(content(1).is_none());
        // A blob in the `body` role becomes `bytes`.
        assert_eq!(
            content(2).unwrap().roles().keys().collect::<Vec<_>>(),
            ["bytes"]
        );
        assert_eq!(
            maps(&commit, &[]),
            [
                json!({"title": "P"}),
                json!({"title": "P", "summary": "s"}),
                json!({"title": "P", "summary": "t"})
            ]
        );
    }

    #[test]
    fn an_erased_payload_keeps_its_id_and_clears_what_it_folded() {
        let erased = metadata(json!({"secret": 1}));
        let later = metadata(json!({"a": 1}));
        let commit = converter()
            .convert(source(
                1,
                vec![
                    (event(FILE, "FILE::CREATE", None, &erased), None),
                    (
                        event(FILE, "FILE::SET_METADATA", Some(1), &later),
                        Some(&later),
                    ),
                ],
            ))
            .unwrap();
        let kept = decode_event_proposal(&commit.payloads[0]).unwrap();
        assert_eq!(kept.set_metadata.unwrap().id(), erased.id());
        assert_eq!(maps(&commit, &[&later])[1], json!({"a": 1}));
    }

    #[test]
    fn a_source_that_does_not_hash_to_its_id_is_refused() {
        let create = metadata(json!({"content": {"size": 1}}));
        let mut commit = source(
            1,
            vec![(event(FILE, "FILE::CREATE", None, &create), Some(&create))],
        );
        commit.message = Some("changed".into());
        let error = converter().convert(commit).err().unwrap();
        assert!(error.0.contains("does not hash"), "{error}");
        let commit = source(2, vec![]);
        let error = converter().convert(commit).err().unwrap();
        assert!(error.0.contains("expected 1"), "{error}");
    }
}
