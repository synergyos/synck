//! Incremental, storage-free reading of one item's accepted history.
//!
//! Callers supply contiguous item positions and hydrate referenced bodies in a
//! separate event view. Canonical history is never rewritten. Raw text, including
//! NUL bytes, remains intact; PostgreSQL sanitation is not a repository format.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use synck_vcs::types::{ContentId, EventContent, EventProposal, ItemId, RefEventProposal, RepoId};
use uuid::Uuid;

use crate::{calls, native_calendars, replica_projection, subitems, tasks, time_tracking};
use subitems::{SubitemEventVerb as Verb, SubitemKind as Kind};

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("invalid readable history: {0}")]
    Invalid(String),
    #[error("unsupported readable state for {item_type}: {event_type}")]
    Unsupported {
        item_type: String,
        event_type: String,
    },
    #[error("body {0} must be hydrated before reading")]
    NeedsContent(ContentId),
    #[error("readable state requires history for {0}")]
    NeedsHistory(ItemId),
}

impl From<subitems::SubitemError> for ReadError {
    fn from(error: subitems::SubitemError) -> Self {
        Self::Invalid(error.to_string())
    }
}

pub struct ReadContext<'a> {
    pub item_seq: i64,
    pub created_at: DateTime<Utc>,
    pub occurred_at: Option<DateTime<Utc>>,
    pub author: &'a str,
    pub committer: &'a str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", content = "state", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NativeState {
    Generic,
    Task {
        data: tasks::history::TaskData,
        lifecycle: tasks::history::Lifecycle,
    },
    Calendar(native_calendars::Calendar),
    CalendarEvent(native_calendars::CalendarEvent),
    ChatOrganization(crate::chat_organization::ChatOrganization),
    Call(calls::Call),
    TimeSettings(time_tracking::Settings),
    TimeBook(time_tracking::Book),
    TimeEntry(time_tracking::Entry),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubitemLifecycle {
    Live,
    Trash,
    Tombstoned,
    Draft,
    Archived,
    Merged,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubitemIdentity {
    Sequence(i64),
    Uuid(Uuid),
}

pub struct RefReadContext<'a> {
    pub ref_seq: i64,
    pub created_at: DateTime<Utc>,
    pub author: &'a str,
    pub committer: &'a str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HistoryLedger {
    Item,
    Ref,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReadablePosition {
    pub item_id: ItemId,
    pub ledger: HistoryLedger,
    pub seq: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LegacySidecar {
    pub item_id: ItemId,
    pub head_seq: i64,
    pub event_digest: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReadableSubitem {
    pub family: String,
    pub identity: SubitemIdentity,
    pub created_position: ReadablePosition,
    pub latest_position: ReadablePosition,
    pub lifecycle: SubitemLifecycle,
    pub parent_item_seq: Option<i64>,
    pub stable_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
    pub sender_id: Option<String>,
    pub sender_label: Option<String>,
    pub body: Option<String>,
    pub reply_to_item_seq: Option<i64>,
    pub emoji: Option<String>,
    pub name: Option<String>,
    pub attachment_kind: Option<String>,
    pub blob_id: Option<String>,
    pub file_item_id: Option<Uuid>,
    pub parent_folder_id: Option<Uuid>,
    pub size: Option<i64>,
    pub data: Option<Value>,
    pub display_value: Option<String>,
    pub record_version: Option<i64>,
    pub record_lifecycle: Option<String>,
    pub version_label: Option<String>,
    pub metadata: Value,
    pub content: Value,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ChatDetails {
    pub title: Option<String>,
    pub profile: Option<subitems::ChatProfileEvent>,
    pub assistants: BTreeSet<String>,
    pub participants: BTreeMap<String, subitems::ChatUserParticipantEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<crate::chat_organization::ChatOrganization>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PostDetails {
    pub channel_id: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub body_blob_id: Option<String>,
    pub audience_class: Option<String>,
    pub comments_allowed: Option<bool>,
    pub lifecycle: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LegacySidecarRoot {
    pub item_id: ItemId,
    pub head_seq: i64,
    pub event_digest: String,
    pub root_event_digest: String,
}

/// Preserved supplemental application data; it is not executed by public replay.
#[derive(Clone, Debug, Serialize)]
pub struct AncillaryEvent {
    pub item_seq: i64,
    pub event_type: String,
    pub metadata: Option<Value>,
    pub content: Option<EventContent>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReadableItem {
    pub projection: replica_projection::ReplicaItemProjection,
    pub native: NativeState,
    pub chat: Option<ChatDetails>,
    pub post: Option<PostDetails>,
    pub subitems: Vec<ReadableSubitem>,
    pub latest_ref_seq: i64,
    pub legacy_sidecar_roots: Vec<LegacySidecarRoot>,
    pub resource_fork: Option<crate::file_resources::Activation>,
    pub data_links: Vec<subitems::ItemDataLink>,
    pub ancillary_events: Vec<AncillaryEvent>,
    pub presentation_warnings: Vec<String>,
}

pub struct ItemReader {
    item_id: ItemId,
    repo_id: Option<RepoId>,
    ref_seq: i64,
    required_sidecars: BTreeMap<ItemId, LegacySidecar>,
    loaded_sidecars: BTreeMap<ItemId, i64>,
    file_content_positions: BTreeSet<i64>,
    resource_fork: Option<crate::file_resources::Activation>,
    legacy_sidecar_roots: Vec<LegacySidecarRoot>,
    data_links: BTreeMap<String, subitems::ItemDataLink>,
    ancillary_events: Vec<AncillaryEvent>,
    poisoned: bool,
    projection: Option<replica_projection::ReplicaItemProjection>,
    native: NativeState,
    chat: Option<ChatDetails>,
    post: Option<PostDetails>,
    subitems: BTreeMap<(String, SubitemIdentity), ReadableSubitem>,
}

fn invalid(error: impl std::fmt::Display) -> ReadError {
    ReadError::Invalid(error.to_string())
}
fn unsupported(kind: &str, event: &str) -> ReadError {
    ReadError::Unsupported {
        item_type: kind.into(),
        event_type: event.into(),
    }
}
fn uuid(value: &Value, field: &str) -> Result<Uuid, ReadError> {
    let raw = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("missing {field}")))?;
    Uuid::parse_str(raw).map_err(invalid)
}

impl ItemReader {
    pub fn new(item_id: ItemId) -> Self {
        Self {
            item_id,
            repo_id: None,
            ref_seq: 0,
            required_sidecars: BTreeMap::new(),
            loaded_sidecars: BTreeMap::new(),
            file_content_positions: BTreeSet::new(),
            resource_fork: None,
            legacy_sidecar_roots: Vec::new(),
            data_links: BTreeMap::new(),
            ancillary_events: Vec::new(),
            poisoned: false,
            projection: None,
            native: NativeState::Generic,
            chat: None,
            post: None,
            subitems: BTreeMap::new(),
        }
    }

    /// Supply repository identity for deterministic FILE versions and CHAT organization owners.
    pub fn for_repository(repo_id: RepoId, item_id: ItemId) -> Self {
        let mut reader = Self::new(item_id);
        reader.repo_id = Some(repo_id);
        reader
    }

    pub fn latest_item_seq(&self) -> i64 {
        self.projection
            .as_ref()
            .map_or(0, |item| item.latest_item_seq)
    }

    pub fn snapshot(&self) -> Result<ReadableItem, ReadError> {
        if self.poisoned {
            return Err(invalid("reader must be discarded after an error"));
        }
        for required in self.required_sidecars.values() {
            if self.loaded_sidecars.get(&required.item_id) != Some(&required.head_seq) {
                return Err(ReadError::NeedsHistory(required.item_id.clone()));
            }
        }
        Ok(ReadableItem {
            projection: self
                .projection
                .clone()
                .ok_or_else(|| invalid("item has no creation"))?,
            native: self.native.clone(),
            chat: self.chat.clone(),
            post: self.post.clone(),
            subitems: self.subitems.values().cloned().collect(),
            latest_ref_seq: self.ref_seq,
            legacy_sidecar_roots: self.legacy_sidecar_roots.clone(),
            resource_fork: self.resource_fork.clone(),
            data_links: self.data_links.values().cloned().collect(),
            ancillary_events: self.ancillary_events.clone(),
            presentation_warnings: if self.ancillary_events.is_empty() {
                Vec::new()
            } else {
                vec!["Supplemental workflow/revision annotations are retained without application presentation or execution.".into()]
            },
        })
    }

    pub fn required_sidecars(&self) -> impl Iterator<Item = &LegacySidecar> {
        self.required_sidecars.values()
    }

    /// Seed the exact frozen sidecar snapshot. The caller verifies its canonical
    /// head digest against required_sidecars before supplying this typed view.
    pub fn seed_legacy_versions(&mut self, sidecar: &ReadableItem) -> Result<(), ReadError> {
        if self.poisoned {
            return Err(invalid("reader must be discarded after an error"));
        }
        let result = self.seed_legacy_versions_inner(sidecar);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn seed_legacy_versions_inner(&mut self, sidecar: &ReadableItem) -> Result<(), ReadError> {
        if sidecar.projection.item_type != "FILE_VERSION_SET"
            || synck_file_folder::file_version_set::file_item_id_from_name(&sidecar.projection.name)
                .map_err(invalid)?
                != self.item_id
        {
            return Err(invalid("sidecar does not belong to FILE"));
        }
        if let Some(expected) = self.required_sidecars.get(&sidecar.projection.item_id) {
            if sidecar.projection.latest_item_seq != expected.head_seq {
                return Err(invalid("sidecar head does not match baseline"));
            }
        }
        for version in &sidecar.subitems {
            let Some(id) = version.stable_id else {
                return Err(invalid("legacy sidecar version_id missing"));
            };
            let mut row = version.clone();
            row.family = "FILE_VERSION".into();
            let key = (row.family.clone(), SubitemIdentity::Uuid(id));
            if !self.subitems.contains_key(&key) {
                self.subitems.insert(key, row);
            }
        }
        self.loaded_sidecars.insert(
            sidecar.projection.item_id.clone(),
            sidecar.projection.latest_item_seq,
        );
        self.relabel_versions("FILE_VERSION")
    }

    pub fn apply_ref(
        &mut self,
        event: &RefEventProposal,
        ctx: RefReadContext<'_>,
    ) -> Result<(), ReadError> {
        if self.poisoned {
            return Err(invalid("reader must be discarded after an error"));
        }
        let result = self.apply_ref_inner(event, &ctx);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn apply_ref_inner(
        &mut self,
        event: &RefEventProposal,
        ctx: &RefReadContext<'_>,
    ) -> Result<(), ReadError> {
        if event.owner_item_id != self.item_id {
            return Err(invalid("ref owner mismatch"));
        }
        let kind = self
            .projection
            .as_ref()
            .map(|item| item.item_type.as_str())
            .ok_or_else(|| invalid("ref before owner creation"))?;
        if kind != "FILE" {
            return Err(unsupported(kind, &event.event_type));
        }
        let first = self.ref_seq == 0;
        let expected = if first {
            event.initial_ref_seq.unwrap_or(1)
        } else {
            self.ref_seq + 1
        };
        if ctx.ref_seq != expected {
            return Err(invalid("noncontiguous ref sequence"));
        }
        if event.event_type == "FILE_VERSION::LEGACY_BASELINE" {
            let meta = event
                .set_metadata
                .as_ref()
                .ok_or_else(|| invalid("baseline metadata missing"))?;
            let id = ItemId::parse(
                meta.get("legacy_sidecar_item_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("baseline sidecar missing"))?,
            )
            .map_err(invalid)?;
            let head = meta
                .get("legacy_sidecar_head_seq")
                .and_then(Value::as_i64)
                .filter(|value| *value >= 1)
                .ok_or_else(|| invalid("baseline head missing"))?;
            let digest = meta
                .get("legacy_sidecar_head_event_digest")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("baseline digest missing"))?;
            synck_vcs::types::Sha256Digest::parse(digest).map_err(invalid)?;
            if !first
                || event.base_ref_seq != 0
                || event.set_content.is_some()
                || ctx.ref_seq != head
            {
                return Err(invalid("invalid legacy baseline envelope"));
            }
            self.required_sidecars.insert(
                id.clone(),
                LegacySidecar {
                    item_id: id,
                    head_seq: head,
                    event_digest: digest.into(),
                },
            );
        } else if crate::file_resources::is_resource_event(&event.event_type) {
            let activation = crate::file_resources::decode_activation(event)
                .map_err(invalid)?
                .ok_or_else(|| invalid("resource activation missing"))?;
            if !self
                .file_content_positions
                .contains(&activation.primary_item_seq)
            {
                return Err(invalid(
                    "resource activation names missing FILE content position",
                ));
            }
            self.resource_fork = Some(activation);
        } else if event.event_type == "FILE_VERSION::LEGACY_SIDECAR_ROOT" {
            let meta = event
                .set_metadata
                .as_ref()
                .and_then(Value::as_object)
                .ok_or_else(|| invalid("sidecar root metadata missing"))?;
            if meta.len() != 5 || event.set_content.is_some() {
                return Err(invalid("invalid sidecar root shape"));
            }
            let meta = Value::Object(meta.clone());
            if uuid(&meta, "file_item_id")?
                != Uuid::parse_str(self.item_id.as_str()).expect("typed item ID")
            {
                return Err(invalid("sidecar root owner mismatch"));
            }
            let id = ItemId::parse(
                meta.get("legacy_sidecar_item_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("sidecar root identity missing"))?,
            )
            .map_err(invalid)?;
            let head_seq = meta
                .get("legacy_sidecar_head_seq")
                .and_then(Value::as_i64)
                .filter(|seq| *seq >= 1)
                .ok_or_else(|| invalid("sidecar root head missing"))?;
            let event_digest = meta
                .get("legacy_sidecar_head_event_digest")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("sidecar root head digest missing"))?;
            let root_event_digest = meta
                .get("legacy_sidecar_root_event_digest")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("sidecar root digest missing"))?;
            synck_vcs::types::Sha256Digest::parse(event_digest).map_err(invalid)?;
            synck_vcs::types::Sha256Digest::parse(root_event_digest).map_err(invalid)?;
            self.legacy_sidecar_roots.push(LegacySidecarRoot {
                item_id: id,
                head_seq,
                event_digest: event_digest.into(),
                root_event_digest: root_event_digest.into(),
            });
        } else if let Some(verb) = event.event_type.strip_prefix("FILE_VERSION::") {
            let sidecar_event = match verb {
                "CREATE" => "FILE_VERSION_SET::VERSION_CREATE",
                "SET_DESCRIPTION" => "FILE_VERSION_SET::VERSION_SET_DESCRIPTION",
                "SET_KIND" => "FILE_VERSION_SET::VERSION_SET_KIND",
                "SET_LABEL_SOURCE" => "FILE_VERSION_SET::VERSION_SET_LABEL_SOURCE",
                "SET_ORDER" => "FILE_VERSION_SET::VERSION_SET_ORDER",
                "REPLACE_CONTENT" => "FILE_VERSION_SET::VERSION_REPLACE_CONTENT",
                "TOMBSTONE" => "FILE_VERSION_SET::VERSION_TOMBSTONE",
                _ => return Err(unsupported("FILE", &event.event_type)),
            };
            let meta = event
                .set_metadata
                .as_ref()
                .ok_or_else(|| invalid("version ref metadata missing"))?;
            if uuid(meta, "file_item_id")?
                != Uuid::parse_str(self.item_id.as_str()).expect("typed item ID")
            {
                return Err(invalid("version payload owner mismatch"));
            }
            let id = uuid(meta, "version_id")?;
            match verb {
                "CREATE" => {
                    subitems::file_version_ref_required_string(
                        meta,
                        "operation_id",
                        &event.event_type,
                    )?;
                    subitems::file_version_ref_required_kind(meta, &event.event_type)?;
                    subitems::file_version_ref_required_label_source(meta, &event.event_type)?;
                    subitems::validate_file_version_ref_description(
                        meta,
                        &event.event_type,
                        false,
                    )?;
                }
                "SET_DESCRIPTION" => {
                    subitems::validate_file_version_ref_description(meta, &event.event_type, true)?
                }
                "SET_KIND" => {
                    subitems::file_version_ref_required_kind(meta, &event.event_type)?;
                }
                "SET_LABEL_SOURCE" => {
                    subitems::file_version_ref_required_label_source(meta, &event.event_type)?;
                }
                _ => {}
            }
            if matches!(verb, "CREATE" | "REPLACE_CONTENT") {
                let content = subitems::reconstruct_subitem_content(
                    event.set_metadata.as_ref(),
                    event.set_content.as_ref(),
                );
                subitems::validate_file_version_ref_content(&content, &event.event_type)?;
                subitems::file_version_ref_optional_positive_i64(
                    meta,
                    "source_item_seq",
                    &event.event_type,
                )?;
                subitems::file_version_ref_optional_nonnegative_i32(
                    meta,
                    "source_commit_event_idx",
                    &event.event_type,
                )?;
                subitems::file_version_ref_optional_commit_id(
                    meta,
                    "source_commit_id",
                    &event.event_type,
                )?;
            }
            let key = ("FILE_VERSION".into(), SubitemIdentity::Uuid(id));
            if verb != "CREATE" && !self.subitems.contains_key(&key) {
                if let Some((legacy_key, legacy)) = self
                    .subitems
                    .iter()
                    .find(|(_, row)| row.family == "FILE::VERSION" && row.stable_id == Some(id))
                    .map(|(key, row)| (key.clone(), row.clone()))
                {
                    self.subitems.remove(&legacy_key);
                    let mut row = legacy;
                    row.family = "FILE_VERSION".into();
                    row.identity = SubitemIdentity::Uuid(id);
                    row.file_item_id = Some(Uuid::parse_str(self.item_id.as_str()).unwrap());
                    self.subitems.insert(key.clone(), row);
                }
            }
            let proposal = EventProposal {
                item_id: Some(self.item_id.clone()),
                event_type: sidecar_event.into(),
                set_metadata: event.set_metadata.clone(),
                set_content: event.set_content.clone(),
                create_item: None,
                set_parent: None,
                set_name: None,
                set_tombstoned: None,
                set_in_trash: None,
                set_is_federated: None,
                set_federation_stream_id: None,
                base_seq: None,
            };
            self.apply_version_descriptor(
                "FILE_VERSION",
                &proposal,
                &ReadContext {
                    item_seq: ctx.ref_seq,
                    created_at: ctx.created_at,
                    occurred_at: None,
                    author: ctx.author,
                    committer: ctx.committer,
                },
            )?;
            if let Some(row) = self.subitems.get_mut(&key) {
                row.latest_position = ReadablePosition {
                    item_id: self.item_id.clone(),
                    ledger: HistoryLedger::Ref,
                    seq: ctx.ref_seq,
                };
                if verb == "CREATE" {
                    row.created_position = row.latest_position.clone();
                }
            }
        } else {
            return Err(unsupported("FILE", &event.event_type));
        }
        self.ref_seq = ctx.ref_seq;
        Ok(())
    }

    /// Any error poisons this reader; discard it and replay a fresh reader.
    /// This prevents observing a partial mutation without copying all accumulated state.
    pub fn apply(
        &mut self,
        event: &EventProposal,
        context: ReadContext<'_>,
    ) -> Result<(), ReadError> {
        if self.poisoned {
            return Err(invalid("reader must be discarded after an error"));
        }
        let result = self.apply_inner(event, context);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn apply_inner(
        &mut self,
        event: &EventProposal,
        context: ReadContext<'_>,
    ) -> Result<(), ReadError> {
        if event.item_id.as_ref() != Some(&self.item_id)
            || context.item_seq != self.latest_item_seq() + 1
        {
            return Err(invalid("item identity or contiguous position mismatch"));
        }
        let kind = match &self.projection {
            Some(item) => item.item_type.as_str(),
            None => event
                .create_item
                .as_ref()
                .map(|item| item.item_type.as_str())
                .ok_or_else(|| invalid("first event must create the item"))?,
        }
        .to_string();
        if let Some(EventContent::Content { content_id }) = event.set_content {
            return Err(ReadError::NeedsContent(content_id));
        }
        let next_native = self.reduce_native(&kind, event, &context)?;
        let mut projection = replica_projection::apply_replica_item_event(
            self.projection.clone(),
            self.item_id.clone(),
            context.item_seq,
            event,
            context.occurred_at.unwrap_or(context.created_at),
            context.author,
        )
        .map_err(invalid)?;
        match kind.as_str() {
            "TASK" | "CALL" | "CALENDAR" | "CALENDAR_EVENT" | "CHAT_ORGANIZATION"
            | "TIME_SETTINGS" | "TIME_BOOK" | "TIME_ENTRY" => {
                if event.event_type == "TASK::COMMENT::CREATE" {
                    self.apply_uuid_comment(
                        "TASK::COMMENT",
                        "comment_id",
                        "body",
                        event,
                        &context,
                    )?;
                }
            }
            "FILE_VERSION_SET" => self.apply_version_set(event, &context)?,
            "FILE" | "FOLDER" | "CHAT" | "EMAIL" | "POST" | "TASK_LIST" | "TABLE" | "DATABASE"
            | "FEDERATED_MOUNT" => {
                if let Some((subkind, verb)) = subitems::classify_subitem_event(&event.event_type) {
                    if subkind.owning_item_type() != kind {
                        return Err(invalid("subitem family does not match owner"));
                    }
                    self.apply_subitem(subkind, verb, event, &context)?;
                } else if event.event_type.starts_with("POST::COMMENT::") {
                    self.apply_post_comment(event, &context)?;
                } else if event.event_type.starts_with("POST::ASSET::") {
                    self.apply_post_asset(event, &context)?;
                } else {
                    self.apply_owner(&kind, event, &context, &mut projection)?;
                }
            }
            _ => return Err(unsupported(&kind, &event.event_type)),
        }
        if kind == "FILE"
            && matches!(
                event.event_type.as_str(),
                "FILE::CREATE" | "FILE::SET_CONTENT"
            )
            && matches!(event.set_content, Some(EventContent::Blob { .. }))
        {
            self.file_content_positions.insert(context.item_seq);
        }
        self.native = next_native;
        self.projection = Some(projection);
        Ok(())
    }

    fn reduce_native(
        &self,
        kind: &str,
        event: &EventProposal,
        ctx: &ReadContext<'_>,
    ) -> Result<NativeState, ReadError> {
        if (kind == "TASK" && tasks::history::event_policy(&event.event_type).is_none())
            || (kind == "CALL" && crate::call::event_policy(&event.event_type).is_none())
            || (time_tracking::is_kind(kind)
                && time_tracking::event_policy(kind, &event.event_type).is_none())
        {
            return Err(unsupported(kind, &event.event_type));
        }
        Ok(match kind {
            "CHAT_ORGANIZATION" => {
                let organization = crate::chat_organization::reduce(
                    match &self.native {
                        NativeState::ChatOrganization(value) => Some(value),
                        _ => None,
                    },
                    event,
                )
                .map_err(invalid)?;
                if let Some(repo_id) = &self.repo_id {
                    crate::chat_organization::validate_identity(
                        repo_id,
                        &self.item_id,
                        &organization,
                    )
                    .map_err(invalid)?;
                }
                NativeState::ChatOrganization(organization)
            }
            "TASK" => {
                let before = match &self.native {
                    NativeState::Task { data, lifecycle } => Some(tasks::history::TaskState {
                        item_id: self.item_id.clone(),
                        data: data.clone(),
                        lifecycle: *lifecycle,
                    }),
                    _ => None,
                };
                let next =
                    tasks::history::reduce(before.as_ref(), event, ctx.author).map_err(invalid)?;
                NativeState::Task {
                    data: next.data,
                    lifecycle: next.lifecycle,
                }
            }
            "CALL" => NativeState::Call(
                calls::history::reduce(
                    match &self.native {
                        NativeState::Call(value) => Some(value),
                        _ => None,
                    },
                    event,
                )
                .map_err(invalid)?,
            ),
            "CALENDAR" => NativeState::Calendar(
                native_calendars::history::reduce_calendar(
                    match &self.native {
                        NativeState::Calendar(value) => Some(value),
                        _ => None,
                    },
                    event,
                )
                .map_err(invalid)?,
            ),
            "CALENDAR_EVENT" => NativeState::CalendarEvent(
                native_calendars::history::reduce_event(
                    match &self.native {
                        NativeState::CalendarEvent(value) => Some(value),
                        _ => None,
                    },
                    event,
                    ctx.author,
                )
                .map_err(invalid)?,
            ),
            "TIME_ENTRY" => NativeState::TimeEntry(
                time_tracking::reduce_entry(
                    match &self.native {
                        NativeState::TimeEntry(value) => Some(value),
                        _ => None,
                    },
                    event,
                )
                .map_err(invalid)?,
            ),
            "TIME_SETTINGS" => {
                let value: time_tracking::Settings =
                    time_tracking::metadata(event).map_err(invalid)?;
                value.validate().map_err(invalid)?;
                NativeState::TimeSettings(value)
            }
            "TIME_BOOK" => {
                let value: time_tracking::Book = time_tracking::metadata(event).map_err(invalid)?;
                time_tracking::validate_rate(value.hourly_rate_minor).map_err(invalid)?;
                if value.user_id.is_empty() || value.user_id.len() > 256 {
                    return Err(invalid("invalid timekeeper"));
                }
                if let NativeState::TimeBook(previous) = &self.native {
                    if previous.user_id != value.user_id {
                        return Err(invalid("immutable timekeeper changed"));
                    }
                }
                NativeState::TimeBook(value)
            }
            _ => NativeState::Generic,
        })
    }

    fn apply_owner(
        &mut self,
        kind: &str,
        event: &EventProposal,
        _ctx: &ReadContext<'_>,
        projection: &mut replica_projection::ReplicaItemProjection,
    ) -> Result<(), ReadError> {
        let meta = event.set_metadata.as_ref().unwrap_or(&Value::Null);
        if matches!(
            event.event_type.as_str(),
            "ITEM::DATA_LINK::PUT" | "ITEM::DATA_LINK::DELETE"
        ) {
            let link =
                subitems::parse_structured_data_item_link_projection(meta, &event.event_type)?;
            if event.event_type == "ITEM::DATA_LINK::PUT" {
                self.data_links.insert(link.link_key.clone(), link);
            } else {
                self.data_links.remove(&link.link_key);
            }
            return Ok(());
        }
        if matches!(
            event.event_type.as_str(),
            "FOLDER::WORKFLOW_RUNTIME" | "FILE::REVISION_SUPPRESS" | "FILE::REVISION_UNSUPPRESS"
        ) {
            self.ancillary_events.push(AncillaryEvent {
                item_seq: _ctx.item_seq,
                event_type: event.event_type.clone(),
                metadata: event.set_metadata.clone(),
                content: event.set_content.clone(),
            });
            return Ok(());
        }
        if kind == "CHAT" {
            let mut chat = self.chat.clone().unwrap_or_default();
            match event.event_type.as_str() {
                "CHAT::SET_PROFILE" => {
                    chat.profile = Some(subitems::parse_chat_profile_event(meta).map_err(invalid)?)
                }
                "CHAT::ADD_ASSISTANT" => {
                    chat.assistants.insert(
                        subitems::parse_chat_assistant_participant_event(meta).map_err(invalid)?,
                    );
                }
                "CHAT::REMOVE_ASSISTANT" => {
                    chat.assistants.remove(
                        &subitems::parse_chat_assistant_participant_event(meta).map_err(invalid)?,
                    );
                }
                "CHAT::UPSERT_USER_PARTICIPANT" | "CHAT::REMOVE_USER_PARTICIPANT" => {
                    let participant =
                        subitems::parse_chat_user_participant_event(meta).map_err(invalid)?;
                    let key = format!("{}:{}", participant.tenant_id, participant.user_id);
                    if event.event_type == "CHAT::REMOVE_USER_PARTICIPANT" {
                        chat.participants.remove(&key);
                    } else {
                        chat.participants.insert(key, participant);
                    }
                }
                "CHAT::MAKE_NON_TREE" => {
                    crate::chat::validate_non_tree_transition(event).map_err(invalid)?;
                    projection.name = meta
                        .get("chat_title")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("missing chat_title"))?
                        .into();
                    projection.metadata_json = meta
                        .get("chat_metadata")
                        .cloned()
                        .ok_or_else(|| invalid("missing chat_metadata"))?;
                    projection.lifecycle_state = parse_core_lifecycle(
                        meta.get("chat_lifecycle")
                            .and_then(Value::as_str)
                            .unwrap_or("LIVE"),
                    )?;
                    chat.title = Some(projection.name.clone());
                }
                "CHAT::SET_TITLE" => {
                    let title = meta
                        .get("title")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("missing CHAT title"))?;
                    chat.title = Some(title.into());
                    projection.name = title.into();
                }
                _ => {
                    require_generic_owner_event(kind, event)?;
                    chat.title = Some(
                        meta.get("chat_title")
                            .and_then(Value::as_str)
                            .unwrap_or(&projection.name)
                            .into(),
                    );
                }
            }
            self.chat = Some(chat);
            return Ok(());
        }
        if kind == "POST" {
            let mut post = self.post.clone().unwrap_or_default();
            match event.event_type.as_str() {
                "POST::CREATE" | "POST::SET_METADATA" | "POST::SET_CONTENT" => {
                    for (key, slot) in [
                        ("channel_id", &mut post.channel_id),
                        ("title", &mut post.title),
                        ("audience_class", &mut post.audience_class),
                        ("lifecycle_state", &mut post.lifecycle),
                    ] {
                        if let Some(value) = meta.get(key).and_then(Value::as_str) {
                            *slot = Some(value.into());
                        }
                    }
                    if let Some(value) = meta.get("summary").and_then(Value::as_str) {
                        post.summary = Some(value.into());
                    }
                    if event.event_type == "POST::CREATE" {
                        post.audience_class.get_or_insert("INTERNAL_ONLY".into());
                        post.comments_allowed.get_or_insert(true);
                        post.lifecycle.get_or_insert("DRAFT".into());
                    }
                    if let Some(value) = meta.get("comments_allowed").and_then(Value::as_bool) {
                        post.comments_allowed = Some(value);
                    }
                    post.body_blob_id = match event.set_content.as_ref() {
                        Some(EventContent::Blob { blob_id }) => Some(blob_id.clone()),
                        None => meta
                            .get("body_markdown_blob_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .or(post.body_blob_id),
                        _ => return Err(invalid("POST body requires blob content")),
                    };
                }
                "POST::TRASH" => post.lifecycle = Some("TRASH".into()),
                "POST::RESTORE" | "POST::PUBLISH" => post.lifecycle = Some("PUBLISHED".into()),
                "POST::ARCHIVE" => post.lifecycle = Some("ARCHIVED".into()),
                "POST::TOMBSTONE" => post.lifecycle = Some("TOMBSTONED".into()),
                _ => return Err(unsupported(kind, &event.event_type)),
            }
            self.post = Some(post);
            return Ok(());
        }
        require_generic_owner_event(kind, event)
    }

    fn blank(
        &self,
        family: &str,
        identity: SubitemIdentity,
        meta: Value,
        content: Value,
        ctx: &ReadContext<'_>,
    ) -> ReadableSubitem {
        ReadableSubitem {
            family: family.into(),
            identity,
            created_position: ReadablePosition {
                item_id: self.item_id.clone(),
                ledger: HistoryLedger::Item,
                seq: ctx.item_seq,
            },
            latest_position: ReadablePosition {
                item_id: self.item_id.clone(),
                ledger: HistoryLedger::Item,
                seq: ctx.item_seq,
            },
            lifecycle: SubitemLifecycle::Live,
            parent_item_seq: None,
            stable_id: None,
            created_at: ctx.created_at,
            created_by: ctx.author.into(),
            sender_id: None,
            sender_label: None,
            body: None,
            reply_to_item_seq: None,
            emoji: None,
            name: None,
            attachment_kind: None,
            blob_id: None,
            file_item_id: None,
            parent_folder_id: None,
            size: None,
            data: None,
            display_value: None,
            record_version: None,
            record_lifecycle: None,
            version_label: None,
            metadata: meta,
            content,
        }
    }

    fn apply_subitem(
        &mut self,
        kind: Kind,
        verb: Verb,
        event: &EventProposal,
        ctx: &ReadContext<'_>,
    ) -> Result<(), ReadError> {
        let family = kind.event_type_create().strip_suffix("::CREATE").unwrap();
        let creating = verb == Verb::Create;
        let target = if creating {
            ctx.item_seq
        } else {
            subitems::meta_get_i64(event.set_metadata.as_ref(), "item_seq")
                .ok_or_else(|| invalid("subitem mutation missing stable item_seq"))?
        };
        let key = (family.into(), SubitemIdentity::Sequence(target));
        let mut meta = subitems::reconstruct_subitem_meta(event.set_metadata.as_ref());
        let content = subitems::reconstruct_subitem_content(
            event.set_metadata.as_ref(),
            event.set_content.as_ref(),
        );
        let parent = subitems::meta_get_i64(event.set_metadata.as_ref(), "parent_item_seq");
        let previous = self.subitems.get(&key);
        let previous_parent = previous.and_then(|row| row.parent_item_seq);
        let previous_index = previous.map(task_index);
        if creating && kind == Kind::TaskListTask && meta.get("index").is_none() {
            let index = self
                .subitems
                .values()
                .filter(|row| {
                    row.family == family
                        && row.lifecycle == SubitemLifecycle::Live
                        && row.parent_item_seq == parent
                })
                .map(task_index)
                .max()
                .unwrap_or(-1)
                + 1;
            meta.as_object_mut()
                .ok_or_else(|| invalid("task metadata must be object"))?
                .insert("index".into(), Value::from(index));
        }
        let mut row = if creating {
            if previous.is_some() {
                return Err(invalid("duplicate subitem creation"));
            }
            let mut row = self.blank(family, key.1.clone(), meta, content, ctx);
            row.parent_item_seq = parent;
            initialize_subitem(kind, &mut row, ctx)?;
            if kind == Kind::FileVersion {
                row.stable_id = meta_version_id(&row.metadata).or_else(|| {
                    self.repo_id.as_ref().map(|repo| {
                        subitems::legacy_file_version_id(
                            &Uuid::parse_str(repo.as_str()).expect("typed repo ID"),
                            &Uuid::parse_str(self.item_id.as_str()).expect("typed item ID"),
                            ctx.item_seq,
                        )
                    })
                });
            }
            row
        } else {
            let mut row = previous
                .cloned()
                .ok_or_else(|| invalid("subitem mutation before creation"))?;
            update_subitem(kind, verb, &mut row, event, &meta, &content, ctx)?;
            row
        };
        if kind == Kind::StructuredDataRecord
            && self.projection.as_ref().is_some_and(|item| {
                item.metadata_json
                    .get("system_registry")
                    .and_then(Value::as_bool)
                    == Some(true)
                    && item
                        .metadata_json
                        .get("system_registry_key")
                        .and_then(Value::as_str)
                        == Some("CONTACTS")
            })
        {
            if creating || matches!(verb, Verb::SetMetadata | Verb::SetContent) {
                read_contact(&mut row)?;
            } else {
                row.record_lifecycle = Some(
                    match verb {
                        Verb::Trash => "ARCHIVED",
                        Verb::Tombstone => "TOMBSTONED",
                        Verb::Restore => "LIVE",
                        _ => unreachable!(),
                    }
                    .into(),
                );
                row.lifecycle = parse_lifecycle(row.record_lifecycle.as_deref().unwrap_or("LIVE"))?;
            }
        }
        row.latest_position.seq = ctx.item_seq;
        let before =
            subitems::meta_get_optional_i64(event.set_metadata.as_ref(), "before_item_seq")?;
        let reordered = kind == Kind::TaskListTask
            && !creating
            && row.lifecycle == SubitemLifecycle::Live
            && (previous_index != Some(task_index(&row))
                || previous_parent != row.parent_item_seq
                || before.is_some());
        let mut plans = Vec::new();
        if reordered {
            let mut siblings = self
                .subitems
                .values()
                .filter(|other| {
                    other.family == family
                        && other.created_position.seq != target
                        && other.lifecycle == SubitemLifecycle::Live
                        && other.parent_item_seq == row.parent_item_seq
                })
                .map(task_order_entry)
                .collect::<Vec<_>>();
            siblings.push(task_order_entry(&row));
            siblings.sort_by_key(|entry| (entry.sort_index, entry.item_seq));
            plans.push(subitems::reorder_task_siblings(
                siblings,
                Some(target),
                Some(task_index(&row)),
                before,
            )?);
            if previous_parent != row.parent_item_seq {
                let mut old = self
                    .subitems
                    .values()
                    .filter(|other| {
                        other.family == family
                            && other.created_position.seq != target
                            && other.lifecycle == SubitemLifecycle::Live
                            && other.parent_item_seq == previous_parent
                    })
                    .map(task_order_entry)
                    .collect::<Vec<_>>();
                old.sort_by_key(|entry| (entry.sort_index, entry.item_seq));
                plans.push(old);
            }
        }
        let removed_folder = (kind == Kind::ChatFileFolder
            && matches!(verb, Verb::Trash | Verb::Tombstone))
        .then_some(row.stable_id)
        .flatten();
        self.subitems.insert(key, row);
        for plan in plans {
            for (index, entry) in plan.into_iter().enumerate() {
                if let Some(sibling) = self
                    .subitems
                    .get_mut(&(family.into(), SubitemIdentity::Sequence(entry.item_seq)))
                {
                    sibling.metadata["index"] = Value::from(index as i64);
                    sibling.data = Some(sibling.metadata.clone());
                }
            }
        }
        if kind == Kind::TaskListTask && verb == Verb::Tombstone {
            self.subitems.retain(|_, comment| {
                !(comment.family == "TASK_LIST::TASK.COMMENT"
                    && comment.parent_item_seq == Some(target))
            });
        }
        if let Some(folder) = removed_folder {
            self.detach_chat_folder(folder);
        }
        if kind == Kind::FileVersion {
            self.relabel_versions(family)?;
        }
        Ok(())
    }

    fn detach_chat_folder(&mut self, folder: Uuid) {
        for row in self
            .subitems
            .values_mut()
            .filter(|row| row.parent_folder_id == Some(folder))
        {
            if row.family == "CHAT::MESSAGE.ATTACHMENT" {
                row.parent_folder_id = None;
                if let Some(meta) = row.metadata.as_object_mut() {
                    meta.remove("chat_file_folder_id");
                }
            } else if matches!(row.family.as_str(), "CHAT::FILE" | "CHAT::FILE_FOLDER")
                && row.lifecycle == SubitemLifecycle::Live
            {
                row.parent_folder_id = None;
                if let Some(name) = row.name.as_mut() {
                    let suffix = format!(" (moved-{})", row.created_position.seq);
                    if let Some((stem, extension)) = name
                        .rsplit_once('.')
                        .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
                    {
                        *name = format!("{stem}{suffix}.{extension}");
                    } else {
                        name.push_str(&suffix);
                    }
                }
                if let Some(meta) = row.metadata.as_object_mut() {
                    meta.insert(
                        "v2_projection_moved_from_folder_id".into(),
                        Value::String(folder.to_string()),
                    );
                }
            }
        }
    }

    fn apply_uuid_comment(
        &mut self,
        family: &str,
        id_field: &str,
        body_field: &str,
        event: &EventProposal,
        ctx: &ReadContext<'_>,
    ) -> Result<(), ReadError> {
        let meta = event
            .set_metadata
            .as_ref()
            .ok_or_else(|| invalid("comment metadata missing"))?;
        let id = uuid(meta, id_field)?;
        let content = match event.set_content.as_ref() {
            Some(EventContent::Json { json }) => json.clone(),
            _ => return Err(invalid("comment must have hydrated JSON body")),
        };
        let body = content
            .get(body_field)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("comment body missing"))?
            .to_owned();
        let key = (family.into(), SubitemIdentity::Uuid(id));
        if self.subitems.contains_key(&key) {
            return Ok(());
        }
        let mut row = self.blank(family, key.1.clone(), meta.clone(), content, ctx);
        row.body = Some(body);
        row.stable_id = Some(id);
        self.subitems.insert(key, row);
        Ok(())
    }

    fn apply_version_set(
        &mut self,
        event: &EventProposal,
        ctx: &ReadContext<'_>,
    ) -> Result<(), ReadError> {
        self.apply_version_descriptor("FILE_VERSION_SET::VERSION", event, ctx)
    }

    fn apply_version_descriptor(
        &mut self,
        family: &str,
        event: &EventProposal,
        ctx: &ReadContext<'_>,
    ) -> Result<(), ReadError> {
        use synck_file_folder::file_version_set as versions;
        if event.event_type == versions::CREATE_EVENT {
            let create = event
                .create_item
                .as_ref()
                .ok_or_else(|| invalid("sidecar creation missing identity"))?;
            versions::file_item_id_from_name(&create.name).map_err(invalid)?;
            return Ok(());
        }
        let meta = event
            .set_metadata
            .as_ref()
            .ok_or_else(|| invalid("sidecar metadata missing"))?;
        let id = uuid(meta, "version_id")?;
        let owner = uuid(meta, "file_item_id")?;
        let key = (family.into(), SubitemIdentity::Uuid(id));
        let content = subitems::reconstruct_subitem_content(
            event.set_metadata.as_ref(),
            event.set_content.as_ref(),
        );
        let mut row = if event.event_type == versions::VERSION_CREATE_EVENT {
            if self.subitems.contains_key(&key) {
                return Ok(());
            }
            let mut row = self.blank(family, key.1.clone(), meta.clone(), content.clone(), ctx);
            row.stable_id = Some(id);
            row.file_item_id = Some(owner);
            row.blob_id = Some(
                subitems::file_version_blob_id_from_content(&content)
                    .ok_or_else(|| invalid("sidecar blob missing"))?,
            );
            row.size = subitems::file_version_size_from_content(&content);
            row.body = subitems::file_version_comment_from_meta(meta);
            if subitems::file_version_order_key_from_meta(meta).is_none() {
                return Err(invalid("sidecar order_key missing"));
            }
            row
        } else {
            let mut row = self
                .subitems
                .get(&key)
                .cloned()
                .ok_or_else(|| invalid("sidecar version before creation"))?;
            if row.file_item_id != Some(owner) {
                return Err(invalid("sidecar file identity changed"));
            }
            if row.lifecycle != SubitemLifecycle::Live {
                return Err(invalid("sidecar version must be live"));
            }
            match event.event_type.as_str() {
                versions::VERSION_SET_DESCRIPTION_EVENT => {
                    row.body = subitems::file_version_comment_from_meta(meta)
                }
                versions::VERSION_SET_KIND_EVENT => {
                    row.metadata["kind"] =
                        Value::String(subitems::file_version_kind_from_meta(meta))
                }
                versions::VERSION_SET_LABEL_SOURCE_EVENT => {
                    row.metadata["label_source"] = Value::String(
                        versions::normalize_label_source(
                            meta.get("label_source").and_then(Value::as_str),
                        )
                        .map_err(invalid)?
                        .into(),
                    )
                }
                versions::VERSION_SET_ORDER_EVENT => {
                    row.metadata["order_key"] = Value::from(
                        subitems::file_version_order_key_from_meta(meta)
                            .ok_or_else(|| invalid("sidecar order_key missing"))?,
                    )
                }
                versions::VERSION_REPLACE_CONTENT_EVENT => {
                    row.blob_id = Some(
                        subitems::file_version_blob_id_from_content(&content)
                            .ok_or_else(|| invalid("sidecar blob missing"))?,
                    );
                    row.size = subitems::file_version_size_from_content(&content);
                    row.content = content;
                    if meta.get("description").is_some() || meta.get("comment").is_some() {
                        row.body = subitems::file_version_comment_from_meta(meta);
                    }
                    for field in [
                        "source_item_seq",
                        "source_commit_id",
                        "source_commit_event_idx",
                    ] {
                        if let Some(value) = meta.get(field).filter(|value| !value.is_null()) {
                            row.metadata[field] = value.clone();
                        }
                    }
                }
                versions::VERSION_TOMBSTONE_EVENT => row.lifecycle = SubitemLifecycle::Tombstoned,
                _ => return Err(unsupported("FILE_VERSION_SET", &event.event_type)),
            }
            row
        };
        row.latest_position.seq = ctx.item_seq;
        self.subitems.insert(key, row);
        self.relabel_versions(family)
    }

    fn relabel_versions(&mut self, family: &str) -> Result<(), ReadError> {
        let owner_versions = matches!(family, "FILE::VERSION" | "FILE_VERSION");
        let mut keys = self
            .subitems
            .iter()
            .filter(|(_, row)| {
                (row.family == family
                    || (owner_versions
                        && matches!(row.family.as_str(), "FILE::VERSION" | "FILE_VERSION")))
                    && row.lifecycle == SubitemLifecycle::Live
            })
            .map(|(key, row)| {
                (
                    key.clone(),
                    subitems::file_version_order_key_from_meta(&row.metadata).unwrap_or(0),
                    if row.family == "FILE::VERSION" {
                        row.created_position.seq
                    } else {
                        row.latest_position.seq
                    },
                    row.stable_id,
                )
            })
            .collect::<Vec<_>>();
        keys.sort_by(|a, b| (a.1, a.2, a.3).cmp(&(b.1, b.2, b.3)));
        let (mut major, mut minor) = (0, 0);
        for (key, _, _, _) in keys {
            let row = self.subitems.get_mut(&key).expect("collected key");
            synck_file_folder::file_version_set::advance_label(
                &subitems::file_version_kind_from_meta(&row.metadata),
                &mut major,
                &mut minor,
            )
            .map_err(invalid)?;
            row.version_label = Some(format!("{major}.{minor}"));
        }
        Ok(())
    }

    fn apply_post_comment(
        &mut self,
        event: &EventProposal,
        ctx: &ReadContext<'_>,
    ) -> Result<(), ReadError> {
        match event.event_type.as_str() {
            "POST::COMMENT::CREATE" => {
                self.apply_uuid_comment("POST::COMMENT", "comment_id", "body_markdown", event, ctx)
            }
            "POST::COMMENT::TRASH" | "POST::COMMENT::RESTORE" | "POST::COMMENT::TOMBSTONE" => {
                let seq = subitems::meta_get_i64(event.set_metadata.as_ref(), "item_seq")
                    .ok_or_else(|| invalid("POST comment mutation missing item_seq"))?;
                let row = self
                    .subitems
                    .values_mut()
                    .find(|row| row.family == "POST::COMMENT" && row.created_position.seq == seq)
                    .ok_or_else(|| invalid("POST comment target missing"))?;
                row.lifecycle = lifecycle_for_event(&event.event_type)?;
                row.latest_position.seq = ctx.item_seq;
                Ok(())
            }
            _ => Err(unsupported("POST", &event.event_type)),
        }
    }

    fn apply_post_asset(
        &mut self,
        event: &EventProposal,
        ctx: &ReadContext<'_>,
    ) -> Result<(), ReadError> {
        let meta = event
            .set_metadata
            .as_ref()
            .ok_or_else(|| invalid("POST asset metadata missing"))?;
        let id = uuid(meta, "asset_id")?;
        let key = ("POST::ASSET".into(), SubitemIdentity::Uuid(id));
        match event.event_type.as_str() {
            "POST::ASSET::CREATE" => {
                let blob = match event.set_content.as_ref() {
                    Some(EventContent::Blob { blob_id }) => blob_id.clone(),
                    None => meta
                        .get("blob_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("POST asset blob missing"))?
                        .into(),
                    _ => return Err(invalid("POST asset requires blob content")),
                };
                let mut row = self.subitems.get(&key).cloned().unwrap_or_else(|| {
                    self.blank("POST::ASSET", key.1.clone(), meta.clone(), Value::Null, ctx)
                });
                row.blob_id = Some(blob);
                row.stable_id = Some(id);
                row.name = meta
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                row.size = meta.get("size").and_then(Value::as_i64);
                row.lifecycle = SubitemLifecycle::Live;
                row.latest_position.seq = ctx.item_seq;
                row.metadata = meta.clone();
                self.subitems.insert(key, row);
                Ok(())
            }
            "POST::ASSET::TOMBSTONE" => {
                let row = self
                    .subitems
                    .get_mut(&key)
                    .ok_or_else(|| invalid("POST asset target missing"))?;
                row.lifecycle = SubitemLifecycle::Tombstoned;
                row.latest_position.seq = ctx.item_seq;
                Ok(())
            }
            _ => Err(unsupported("POST", &event.event_type)),
        }
    }
}

fn require_generic_owner_event(kind: &str, event: &EventProposal) -> Result<(), ReadError> {
    if event.event_type == crate::metadata::ITEM_SOURCE_MODIFIED_AT_REPAIR_EVENT_TYPE {
        return Ok(());
    }
    let Some((prefix, verb)) = event.event_type.split_once("::") else {
        return Err(unsupported(kind, &event.event_type));
    };
    if prefix == kind
        && matches!(
            verb,
            "CREATE"
                | "SET_NAME"
                | "SET_PARENT"
                | "SET_METADATA"
                | "TRASH"
                | "RESTORE"
                | "TOMBSTONE"
                | "FEDERATE"
        )
    {
        return Ok(());
    }
    if kind == "FILE" && verb == "SET_CONTENT" {
        return Ok(());
    }
    Err(unsupported(kind, &event.event_type))
}
fn parse_lifecycle(value: &str) -> Result<SubitemLifecycle, ReadError> {
    match value {
        "LIVE" => Ok(SubitemLifecycle::Live),
        "TRASH" => Ok(SubitemLifecycle::Trash),
        "TOMBSTONED" => Ok(SubitemLifecycle::Tombstoned),
        "DRAFT" => Ok(SubitemLifecycle::Draft),
        "ARCHIVED" => Ok(SubitemLifecycle::Archived),
        "MERGED" => Ok(SubitemLifecycle::Merged),
        _ => Err(invalid(format!("invalid lifecycle {value}"))),
    }
}
fn lifecycle_for_event(event: &str) -> Result<SubitemLifecycle, ReadError> {
    parse_lifecycle(match event.rsplit("::").next() {
        Some("TRASH") => "TRASH",
        Some("RESTORE") => "LIVE",
        Some("TOMBSTONE") => "TOMBSTONED",
        _ => return Err(invalid("not a lifecycle event")),
    })
}

fn initialize_subitem(
    kind: Kind,
    row: &mut ReadableSubitem,
    ctx: &ReadContext<'_>,
) -> Result<(), ReadError> {
    match kind {
        Kind::ChatMessage => {
            let (sender, label, body, reply) = subitems::extract_chat_message_fields(
                ctx.author,
                ctx.committer,
                &row.metadata,
                &row.content,
            )?;
            row.sender_id = Some(sender);
            row.sender_label = label;
            row.body = Some(body);
            row.reply_to_item_seq = reply;
            row.metadata = subitems::extract_chat_message_metadata(&row.metadata);
        }
        Kind::ChatMessageReaction => {
            row.sender_id = Some(ctx.author.into());
            row.emoji = Some(subitems::extract_chat_message_reaction_emoji(&row.content)?);
        }
        Kind::ChatAttachment => {
            let fields = subitems::extract_chat_attachment_fields(&row.metadata, &row.content)?;
            row.attachment_kind = Some(fields.kind);
            row.blob_id = fields.blob_id;
            row.file_item_id = fields.file_item_id;
            row.parent_folder_id = fields.chat_file_folder_id;
            row.size = fields.size;
        }
        Kind::EmailAttachment => {
            let (blob, meta) =
                subitems::extract_email_attachment_fields(&row.metadata, &row.content)?;
            row.blob_id = Some(blob);
            row.metadata = meta;
        }
        Kind::ChatFile => {
            let fields = subitems::extract_chat_file_fields(&row.metadata)?;
            row.stable_id = Some(fields.file_id);
            row.name = Some(fields.name);
            row.parent_folder_id = fields.parent_folder_id;
            row.file_item_id = fields.linked_file_item_id;
            row.attachment_kind = Some(fields.kind);
            row.metadata = fields.meta;
            if !row.content.is_null() {
                let content =
                    subitems::extract_chat_file_content_fields(&row.metadata, &row.content)?;
                row.blob_id = Some(content.blob_id);
                row.size = Some(content.size);
            }
        }
        Kind::ChatFileFolder => {
            let fields = subitems::extract_chat_file_folder_fields(&row.metadata)?;
            row.stable_id = Some(fields.folder_id);
            row.name = Some(fields.name);
            row.parent_folder_id = fields.parent_folder_id;
            row.metadata = fields.meta;
        }
        Kind::ChatMessageFileRef => {
            let fields =
                subitems::extract_chat_message_file_ref_fields(&row.metadata, row.parent_item_seq)?;
            row.parent_item_seq = Some(fields.parent_message_item_seq);
            row.stable_id = Some(fields.file_id);
            row.name = Some(fields.snapshot_name);
            row.metadata = fields.meta;
        }
        Kind::TaskListTask => {
            let fields = tasks::normalize_fields(&row.metadata).map_err(invalid)?;
            row.metadata = subitems::task_row_metadata_with_creator(
                fields.metadata_json,
                ctx.occurred_at.unwrap_or(ctx.created_at),
                ctx.author,
            )?;
            row.name = Some(fields.subject);
            row.data = Some(row.metadata.clone());
        }
        Kind::TaskListTaskComment => {
            row.stable_id = Some(uuid(&row.metadata, "comment_id")?);
            row.body = Some(comment_body(&row.content)?.trim().into());
        }
        Kind::StructuredDataRecord => {
            row.stable_id = Some(uuid(&row.metadata, "record_id")?);
            let data = subitems::structured_data_record_data_from_content(&row.content)?;
            row.display_value = Some(subitems::structured_data_record_display_value_from_content(
                &row.content,
                &data,
            ));
            row.data = Some(data);
            row.record_version = Some(subitems::structured_data_record_version_from_content(
                &row.content,
                1,
            )?);
            row.record_lifecycle = Some(subitems::structured_data_record_lifecycle_from_content(
                &row.content,
                "LIVE",
            )?);
            row.lifecycle = parse_lifecycle(row.record_lifecycle.as_deref().unwrap_or("LIVE"))?;
        }
        Kind::StructuredDataRecordComment => {
            row.stable_id = Some(uuid(&row.metadata, "comment_id")?);
            row.body = Some(comment_body(&row.content)?.trim().into());
        }
        Kind::FileVersion => {
            row.blob_id = Some(
                subitems::file_version_blob_id_from_content(&row.content)
                    .ok_or_else(|| invalid("file version blob missing"))?,
            );
            row.size = subitems::file_version_size_from_content(&row.content);
            row.body = subitems::file_version_comment_from_meta(&row.metadata);
            if subitems::file_version_order_key_from_meta(&row.metadata).is_none() {
                return Err(invalid("file version order_key missing"));
            }
        }
    }
    Ok(())
}
fn comment_body(content: &Value) -> Result<&str, ReadError> {
    content
        .get("body")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("comment body missing"))
}

fn update_subitem(
    kind: Kind,
    verb: Verb,
    row: &mut ReadableSubitem,
    event: &EventProposal,
    meta: &Value,
    content: &Value,
    ctx: &ReadContext<'_>,
) -> Result<(), ReadError> {
    if kind == Kind::TaskListTaskComment {
        return Err(unsupported("TASK_LIST", &event.event_type));
    }
    if matches!(verb, Verb::Trash | Verb::Restore | Verb::Tombstone) {
        if kind == Kind::StructuredDataRecord {
            row.record_lifecycle = Some(
                if verb == Verb::Restore {
                    "LIVE"
                } else {
                    "ARCHIVED"
                }
                .into(),
            );
        }
        row.lifecycle = if kind == Kind::StructuredDataRecord {
            if verb == Verb::Restore {
                SubitemLifecycle::Live
            } else {
                SubitemLifecycle::Archived
            }
        } else if (kind == Kind::StructuredDataRecordComment || kind == Kind::FileVersion)
            && verb == Verb::Trash
        {
            SubitemLifecycle::Tombstoned
        } else {
            lifecycle_for_event(&event.event_type)?
        };
        return Ok(());
    }
    let replacement = subitems::meta_get_json(event.set_metadata.as_ref(), "meta");
    match kind {
        Kind::ChatMessage => {
            if let Some(body) = content.get("body").and_then(Value::as_str) {
                row.body = Some(body.into());
            }
            if let Some(meta) = replacement {
                row.metadata = subitems::extract_chat_message_metadata(&meta);
            }
        }
        Kind::ChatMessageReaction => {
            if let Some(emoji) = content
                .get("emoji")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                row.emoji = Some(emoji.into());
            }
        }
        Kind::ChatAttachment => {
            if let Some(meta) = replacement {
                if meta
                    .get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| Some(kind) != row.attachment_kind.as_deref())
                {
                    return Err(invalid("chat attachment kind is immutable"));
                }
                row.parent_folder_id = subitems::chat_file_folder_id_from_meta(&meta)?;
                row.metadata = meta;
            }
            if !content.is_null() {
                if row.attachment_kind.as_deref() != Some("EMBEDDED")
                    || content.get("file_item_id").is_some()
                {
                    return Err(invalid(
                        "chat attachment content edit requires EMBEDDED blob",
                    ));
                }
                let blob = content
                    .get("blob_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| invalid("chat attachment blob missing"))?;
                row.blob_id = Some(blob.into());
                if let Some(size) = content.get("size").and_then(Value::as_i64) {
                    row.size = Some(size);
                }
            }
        }
        Kind::EmailAttachment => {
            if let Some(meta) = replacement {
                row.metadata = meta;
            }
        }
        Kind::ChatMessageFileRef => {
            let fields = subitems::extract_chat_message_file_ref_fields(meta, None)?;
            row.parent_item_seq = Some(fields.parent_message_item_seq);
            row.stable_id = Some(fields.file_id);
            row.name = Some(fields.snapshot_name);
            row.metadata = fields.meta;
            row.lifecycle = SubitemLifecycle::Live;
        }
        Kind::ChatFile => {
            if verb == Verb::SetContent {
                let fields = subitems::extract_chat_file_content_fields(meta, content)?;
                if row.stable_id != Some(fields.file_id) {
                    return Err(invalid("chat file identity changed"));
                }
                row.blob_id = Some(fields.blob_id);
                row.size = Some(fields.size);
            } else {
                let fields = subitems::extract_chat_file_fields(meta)?;
                if row.stable_id != Some(fields.file_id) {
                    return Err(invalid("chat file identity changed"));
                }
                row.name = Some(fields.name);
                row.parent_folder_id = fields.parent_folder_id;
                row.metadata = fields.meta;
            }
        }
        Kind::ChatFileFolder => {
            let fields = subitems::extract_chat_file_folder_fields(meta)?;
            if row.stable_id != Some(fields.folder_id) {
                return Err(invalid("chat folder identity changed"));
            }
            row.name = Some(fields.name);
            row.parent_folder_id = fields.parent_folder_id;
            row.metadata = fields.meta;
        }
        Kind::TaskListTask => {
            if let Some(parent) =
                subitems::meta_get_optional_i64(event.set_metadata.as_ref(), "parent_item_seq")?
            {
                row.parent_item_seq = parent;
            }
            if let Some(meta) = replacement {
                row.metadata = subitems::merge_task_row_meta(&row.metadata, &meta)?;
            }
            let fields = tasks::normalize_fields(&row.metadata).map_err(invalid)?;
            row.name = Some(fields.subject);
            row.metadata = fields.metadata_json;
            row.data = Some(row.metadata.clone());
        }
        Kind::TaskListTaskComment => return Err(unsupported("TASK_LIST", &event.event_type)),
        Kind::StructuredDataRecord => {
            if replacement.as_ref().is_some_and(|next| {
                next.get("record_id")
                    .is_some_and(|id| Some(id) != row.metadata.get("record_id"))
            }) {
                return Err(invalid("record identity changed"));
            }
            if !content.is_null() {
                if content.get("data").is_some() {
                    row.data = Some(subitems::structured_data_record_data_from_content(content)?);
                }
                let data = row
                    .data
                    .as_ref()
                    .ok_or_else(|| invalid("record data missing"))?;
                row.display_value = Some(
                    subitems::structured_data_record_display_value_from_content(content, data),
                );
                row.record_version = Some(subitems::structured_data_record_version_from_content(
                    content,
                    row.record_version.unwrap_or(1),
                )?);
                row.record_lifecycle =
                    Some(subitems::structured_data_record_lifecycle_from_content(
                        content,
                        row.record_lifecycle.as_deref().unwrap_or("LIVE"),
                    )?);
            }
            row.lifecycle = parse_lifecycle(row.record_lifecycle.as_deref().unwrap_or("LIVE"))?;
        }
        Kind::StructuredDataRecordComment => {
            if !content.is_null() {
                row.body = Some(comment_body(content)?.trim().into());
            }
        }
        Kind::FileVersion => {
            if let Some(meta) = replacement {
                if let Some(next) = meta
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    row.metadata["kind"] = Value::String(next.into());
                }
                if meta.get("label_source").is_some() {
                    row.metadata["label_source"] = Value::String(
                        synck_file_folder::file_version_set::normalize_label_source(
                            meta.get("label_source").and_then(Value::as_str),
                        )
                        .map_err(invalid)?
                        .into(),
                    );
                }
                if let Some(order) = subitems::file_version_order_key_from_meta(&meta) {
                    row.metadata["order_key"] = Value::from(order);
                }
                if meta.get("comment").is_some() {
                    row.body = subitems::file_version_comment_from_meta(&meta);
                }
                for field in [
                    "source_item_seq",
                    "source_commit_id",
                    "source_commit_event_idx",
                ] {
                    if let Some(value) = meta.get(field).filter(|value| !value.is_null()) {
                        row.metadata[field] = value.clone();
                    }
                }
            }
            if let Some(blob) = subitems::file_version_blob_id_from_content(content) {
                row.blob_id = Some(blob);
            }
            if subitems::file_version_content_has_size(content) {
                row.size = subitems::file_version_size_from_content(content);
            }
        }
    }
    if !content.is_null() && !matches!(kind, Kind::EmailAttachment) {
        if matches!(
            kind,
            Kind::ChatMessage
                | Kind::ChatMessageReaction
                | Kind::ChatAttachment
                | Kind::FileVersion
        ) {
            if let Some(patch) = content.as_object() {
                if !row.content.is_object() {
                    row.content = Value::Object(Default::default());
                }
                for (key, value) in patch {
                    row.content[key] = value.clone();
                }
            }
        } else {
            row.content = content.clone();
        }
    }
    row.latest_position.seq = ctx.item_seq;
    Ok(())
}

fn task_index(row: &ReadableSubitem) -> i64 {
    row.metadata
        .get("index")
        .and_then(Value::as_i64)
        .filter(|index| *index >= 0)
        .unwrap_or(row.created_position.seq)
}
fn task_order_entry(row: &ReadableSubitem) -> subitems::TaskOrderEntry {
    subitems::TaskOrderEntry {
        item_seq: row.created_position.seq,
        sort_index: task_index(row),
        metadata_json: row.metadata.clone(),
    }
}
fn read_contact(row: &mut ReadableSubitem) -> Result<(), ReadError> {
    let data = row
        .data
        .as_ref()
        .ok_or_else(|| invalid("contact data missing"))?;
    if data.get("schema_version").and_then(Value::as_i64) != Some(1) {
        return Err(unsupported("TABLE_CONTACTS", "contact schema_version"));
    }
    let id = uuid(data, "contact_id")?;
    if row.stable_id != Some(id) {
        return Err(invalid("contact record_id differs from contact_id"));
    }
    row.name = Some(
        data.get("display_name")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("contact display_name missing"))?
            .into(),
    );
    row.display_value = row.name.clone();
    row.record_lifecycle = Some(
        data.get("lifecycle_state")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("contact lifecycle missing"))?
            .into(),
    );
    row.body = data.get("notes").and_then(Value::as_str).map(str::to_owned);
    row.lifecycle = parse_lifecycle(row.record_lifecycle.as_deref().unwrap_or("LIVE"))?;
    Ok(())
}

fn meta_version_id(meta: &Value) -> Option<Uuid> {
    meta.get("version_id")
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
}

#[cfg(test)]
mod tests;

fn parse_core_lifecycle(value: &str) -> Result<synck_vcs::types::LifecycleState, ReadError> {
    use synck_vcs::types::LifecycleState;
    match value {
        "LIVE" => Ok(LifecycleState::Live),
        "TRASH" => Ok(LifecycleState::Trash),
        "TOMBSTONED" => Ok(LifecycleState::Tombstoned),
        _ => Err(invalid("invalid core lifecycle")),
    }
}
