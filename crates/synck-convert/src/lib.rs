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
use synck_vcs::content::EventContent;
use synck_vcs::errors::CoreError;
use synck_vcs::metadata::{Metadata, MetadataError, MetadataRef};
use synck_vcs::object::ENCRYPTED_PREFIX;
use synck_vcs::types::{
    CommitId, EventProposal, EventsDigest, ItemId, ObjectId, RefEventProposal, RepoId, Timestamp,
    canonical_commit_bytes_for_hash, canonical_event_proposal_bytes, canonical_json_bytes,
    canonical_ref_event_proposal_bytes, compute_commit_id, is_record_type, parse_hashed_time,
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

/// One stored format 7 event: its exact canonical JSON payload and the stored
/// bytes of the metadata object it names, or `None` when it names none or the
/// object is erased.
pub struct SourceEvent {
    pub kind: Kind,
    pub payload: Vec<u8>,
    pub metadata: Option<Vec<u8>>,
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
    /// The item's metadata as format 7 folded it.
    metadata: Map<String, Value>,
}

pub struct Converter {
    checkpoint: [u8; 32],
    next_repo_seq: i64,
    items: HashMap<ItemId, Item>,
    /// EMAIL events after CREATE whose payload held `mailcal`. Format 8 keeps
    /// only CREATE's, so these values are not carried over.
    pub dropped_mailcal: u64,
}

impl Converter {
    /// Start a repository. The history generation stays; the genesis changes
    /// with the format.
    pub fn new(repo_id: &RepoId, generation: &str) -> Result<Self, ConvertError> {
        Ok(Self {
            checkpoint: checkpoint::genesis(repo_id, generation, false)?,
            next_repo_seq: 1,
            items: HashMap::new(),
            dropped_mailcal: 0,
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
        for event in commit.events.iter() {
            let payload = match event.kind {
                Kind::Item => self.item_event(event, &mut metadata)?,
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
        written: &mut Vec<Metadata>,
    ) -> Result<Vec<u8>, ConvertError> {
        let mut event: EventProposal = serde_json::from_slice(&source.payload)
            .map_err(|error| fail(format!("format 7 item event: {error}")))?;
        let item_id = event
            .item_id
            .clone()
            .ok_or_else(|| fail("format 7 item event has no item_id"))?;
        if let Some(create) = &event.create_item {
            let item = Item {
                item_type: create.item_type.clone(),
                metadata: Map::new(),
            };
            if self.items.insert(item_id.clone(), item).is_some() {
                return Err(fail(format!("item {item_id} is created twice")));
            }
        }
        let item = self
            .items
            .get_mut(&item_id)
            .ok_or_else(|| fail(format!("item {item_id} has an event before its CREATE")))?;
        // A longer event type carries a subitem's metadata, a whole replacement
        // already; it stays as it is.
        let Some((_, verb)) = event
            .event_type
            .split_once("::")
            .filter(|(_, verb)| !verb.contains("::"))
        else {
            return Ok(canonical_event_proposal_bytes(&event)?);
        };
        let Some(reference) = &event.set_metadata else {
            return Ok(canonical_event_proposal_bytes(&event)?);
        };
        let source_metadata = readable(&reference.id(), source.metadata.as_deref())?;
        let patch = source_metadata.as_ref().map(object).transpose()?;
        item.fold(&event, verb, patch)?;
        if let (Some(source_metadata), Some(patch)) = (&source_metadata, patch) {
            let mut map = item.metadata.clone();
            overlay(&mut map, patch);
            if item.item_type == "EMAIL" && verb == "CREATE" {
                email_originals(&item_id, &mut event, &mut map)?;
            } else if item.item_type == "EMAIL" {
                map.remove("mailcal");
                if patch.contains_key("mailcal") {
                    self.dropped_mailcal += 1;
                }
            }
            event.set_metadata = Some(rewrite(source_metadata, map, written)?);
        }
        Ok(canonical_event_proposal_bytes(&event)?)
    }
}

impl Item {
    /// Format 7's patch rule: CREATE and SET_METADATA fold, and an erased one
    /// clears every key; a record's SET_TITLE sets `title`; a FILE's new bytes
    /// set `content`. An erased payload of the last two removes only that key.
    fn fold(
        &mut self,
        event: &EventProposal,
        verb: &str,
        patch: Option<&Map<String, Value>>,
    ) -> Result<(), ConvertError> {
        if matches!(verb, "CREATE" | "SET_METADATA") {
            match patch {
                Some(patch) => overlay(&mut self.metadata, patch),
                None => self.metadata.clear(),
            }
        }
        if verb == "SET_TITLE"
            && is_record_type(&self.item_type)
            && event.event_type == format!("{}::SET_TITLE", self.item_type)
        {
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
        if self.item_type == "FILE"
            && event
                .set_content
                .as_ref()
                .is_some_and(|content| content.only("bytes").is_some())
        {
            match patch {
                Some(patch) => {
                    if let Some(content) = patch.get("content").filter(|value| !value.is_null()) {
                        self.metadata.insert("content".into(), content.clone());
                    }
                }
                None => {
                    self.metadata.remove("content");
                }
            }
        }
        Ok(())
    }
}

/// The Hub kept an email's originals only as references inside `mailcal`: the
/// imported file in `source.blob_id`, or else a provider message's raw MIME in
/// `raw_rfc822_blob_id`, and the provider's JSON in `provider_raw_json_blob_id`.
/// They move to the `source` and `provider` roles of `EMAIL::CREATE`, and the
/// renderings go. An email with no original is refused: nothing could rebuild
/// its text.
fn email_originals(
    item_id: &ItemId,
    event: &mut EventProposal,
    map: &mut Map<String, Value>,
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
            roles.insert("source".to_string(), source);
        }
        if let Some(provider) = blob(mailcal.get("provider_raw_json_blob_id"))? {
            roles.insert("provider".to_string(), provider);
        }
        for key in EMAIL_RENDERINGS {
            mailcal.remove(key);
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

fn ref_event(source: &SourceEvent, written: &mut Vec<Metadata>) -> Result<Vec<u8>, ConvertError> {
    let mut event: RefEventProposal = serde_json::from_slice(&source.payload)
        .map_err(|error| fail(format!("format 7 ref event: {error}")))?;
    if event.event_type.starts_with("FILE_VERSION::")
        && let Some(reference) = &event.set_metadata
        && let Some(source_metadata) = readable(&reference.id(), source.metadata.as_deref())?
    {
        let mut map = object(&source_metadata)?.clone();
        for key in LABEL_FACTS {
            map.remove(key);
        }
        event.set_metadata = Some(rewrite(&source_metadata, map, written)?);
    }
    Ok(canonical_ref_event_proposal_bytes(&event)?)
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

    fn converter() -> Converter {
        Converter::new(&RepoId::parse(REPO).unwrap(), GENERATION).unwrap()
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
        let content = metadata(json!({"content": {"size": 2}, "revision_origin": "upload"}));
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
                json!({"color": "red", "content": {"size": 1}}),
                // The event's own keys stay on its map; only `content` folds.
                json!({"color": "red", "content": {"size": 2}, "revision_origin": "upload"}),
                json!({"content": {"size": 2}, "tag": "x"}),
            ]
        );
        // CREATE already held its complete map, so its object is kept.
        assert_eq!(first.metadata.len(), 2);
        assert_eq!(first.commit_id, commit().commit_id);
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
            json!({"mailcal": {"v": 1, "subject": "s"}})
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
        let create = metadata(json!({}));
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
