use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use synck_file_folder::file_version_set as version_set;
use synck_vcs::types::{
    BlobId, CommitId, CommitProposal, EventContent, EventProposal, EventRow, ItemCursor, ItemId,
    LifecycleState, RefEventProposal, RefEventRow, canonical_event_proposal_bytes,
    canonical_ref_event_proposal_bytes, compute_sha256_digest_bytes,
    sha256_prefixed_hex_from_digest_bytes,
};
use uuid::Uuid;

use crate::commands::normalize_input_path;
use crate::error::{LocalError, Result};
use crate::event_profile::parse_version_content;
use crate::integrity::Findings;
use crate::repository::Repository;
use crate::worktree::NodeKind;

const PAGE_SIZE: usize = 1_024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedVersionEntry {
    pub label: Option<String>,
    pub version_id: String,
    pub lifecycle_state: String,
    pub kind: String,
    pub label_source: String,
    pub description: Option<String>,
    pub source_item_seq: i64,
    pub blob_id: String,
    pub size: i64,
    pub version_set_seq: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone)]
struct StoredVersion {
    version_id: String,
    operation_id: String,
    live: bool,
    kind: String,
    label_source: String,
    order_key: i64,
    description: Option<String>,
    source_item_seq: i64,
    source_commit_id: Option<CommitId>,
    source_commit_event_idx: Option<i32>,
    blob_id: BlobId,
    size: i64,
    created_seq: i64,
    current_seq: i64,
    created_at: DateTime<Utc>,
}

struct VersionSetState {
    item_id: ItemId,
    head_seq: i64,
    legacy_head_seq: i64,
    legacy_head_digest: Option<String>,
    replay_commit_id: Option<CommitId>,
    replay_commit_base_seq: i64,
    versions: HashMap<String, StoredVersion>,
    live_sources: HashMap<i64, String>,
}

struct FileRevision {
    item_seq: i64,
    repo_seq: i64,
    commit_id: CommitId,
    commit_event_idx: i32,
    blob_id: BlobId,
    size: i64,
}

fn named_version_ref_event(
    file_item_id: &ItemId,
    event_type: &'static str,
    version_id: &str,
    mut metadata: Map<String, Value>,
    content: Option<EventContent>,
    base_ref_seq: i64,
) -> RefEventProposal {
    metadata.insert(
        "file_item_id".to_string(),
        Value::String(file_item_id.to_string()),
    );
    metadata.insert(
        "version_id".to_string(),
        Value::String(version_id.to_string()),
    );
    RefEventProposal {
        owner_item_id: file_item_id.clone(),
        event_type: event_type.to_string(),
        set_metadata: Some(Value::Object(metadata)),
        set_content: content,
        base_ref_seq,
        initial_ref_seq: None,
    }
}

impl Repository {
    pub async fn named_versions(
        &self,
        caller: &Path,
        input: &Path,
        include_tombstoned: bool,
    ) -> Result<Vec<NamedVersionEntry>> {
        self.recover().await?;
        let (_, file_item_id) = self.resolve_live_file(caller, input).await?;
        let state = self.load_version_set(&file_item_id).await?;
        version_entries(&state, include_tombstoned)
    }

    pub async fn create_named_version(
        &self,
        caller: &Path,
        input: &Path,
        requested_seq: Option<i64>,
        kind: &str,
        description: Option<&str>,
        message: &str,
    ) -> Result<String> {
        require_message(message)?;
        self.recover().await?;
        let (_, file_item_id) = self.resolve_live_file(caller, input).await?;
        let revisions = self.file_revisions(&file_item_id).await?;
        let revision = select_revision(&revisions, requested_seq)?;
        let state = self.load_version_set(&file_item_id).await?;
        if state
            .versions
            .values()
            .any(|version| version.live && version.source_item_seq == revision.item_seq)
        {
            return Err(LocalError::Invalid(format!(
                "file revision {} already has a live named version",
                revision.item_seq
            )));
        }
        let kind = version_set::normalize_kind(Some(kind)).map_err(LocalError::Invalid)?;
        let order_key = state
            .versions
            .values()
            .filter(|version| version.live)
            .map(|version| version.order_key)
            .max()
            .map_or(Ok(version_set::ORDER_KEY_GAP), |value| {
                value
                    .checked_add(version_set::ORDER_KEY_GAP)
                    .ok_or_else(|| {
                        LocalError::Invalid("file version ordering is exhausted".to_string())
                    })
            })?;
        let version_id = Uuid::now_v7().to_string();
        let operation_id = Uuid::new_v4().to_string();
        let mut metadata = Map::new();
        metadata.insert("operation_id".to_string(), operation_id.clone().into());
        metadata.insert("source_item_seq".to_string(), revision.item_seq.into());
        metadata.insert(
            "source_commit_id".to_string(),
            revision.commit_id.to_string().into(),
        );
        metadata.insert(
            "source_commit_event_idx".to_string(),
            i64::from(revision.commit_event_idx).into(),
        );
        metadata.insert("kind".to_string(), kind.into());
        metadata.insert(
            "label_source".to_string(),
            version_set::LABEL_SOURCE_INTERNAL.into(),
        );
        metadata.insert("order_key".to_string(), order_key.into());
        metadata.insert(
            "content".to_string(),
            serde_json::json!({ "size": revision.size }),
        );
        if let Some(description) = normalize_description(description) {
            metadata.insert("description".to_string(), description.into());
        }
        let event = named_version_ref_event(
            &file_item_id,
            "FILE_VERSION::CREATE",
            &version_id,
            metadata,
            Some(EventContent::Blob {
                blob_id: revision.blob_id.to_string(),
            }),
            state.head_seq,
        );
        self.append_version_commit(&file_item_id, message, operation_id, event)
            .await
    }

    pub async fn set_named_version_description(
        &self,
        caller: &Path,
        input: &Path,
        version_id: &str,
        description: &str,
        message: &str,
    ) -> Result<String> {
        let mut metadata = Map::new();
        metadata.insert(
            "description".to_string(),
            normalize_description(Some(description))
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        self.mutate_named_version(
            caller,
            input,
            version_id,
            "FILE_VERSION::SET_DESCRIPTION",
            metadata,
            message,
        )
        .await
    }

    pub async fn set_named_version_kind(
        &self,
        caller: &Path,
        input: &Path,
        version_id: &str,
        kind: &str,
        message: &str,
    ) -> Result<String> {
        let kind = version_set::normalize_kind(Some(kind)).map_err(LocalError::Invalid)?;
        let mut metadata = Map::new();
        metadata.insert("kind".to_string(), kind.into());
        self.mutate_named_version(
            caller,
            input,
            version_id,
            "FILE_VERSION::SET_KIND",
            metadata,
            message,
        )
        .await
    }

    pub async fn tombstone_named_version(
        &self,
        caller: &Path,
        input: &Path,
        version_id: &str,
        message: &str,
    ) -> Result<String> {
        self.mutate_named_version(
            caller,
            input,
            version_id,
            "FILE_VERSION::TOMBSTONE",
            Map::new(),
            message,
        )
        .await
    }

    pub async fn restore_named_version(
        &self,
        caller: &Path,
        input: &Path,
        version_id: &str,
    ) -> Result<()> {
        self.recover().await?;
        let (display, file_item_id) = self.resolve_live_file(caller, input).await?;
        let state = self.load_version_set(&file_item_id).await?;
        let version = require_live_version(&state, version_id)?;
        self.write_working_file(&display, &version.blob_id, version.size, true)?;
        self.refresh_worktree_identities().await
    }

    async fn mutate_named_version(
        &self,
        caller: &Path,
        input: &Path,
        version_id: &str,
        event_type: &'static str,
        metadata: Map<String, Value>,
        message: &str,
    ) -> Result<String> {
        require_message(message)?;
        self.recover().await?;
        let (_, file_item_id) = self.resolve_live_file(caller, input).await?;
        let state = self.load_version_set(&file_item_id).await?;
        let existing = require_live_version(&state, version_id)?;
        let unchanged = match event_type {
            "FILE_VERSION::SET_DESCRIPTION" => {
                let requested = metadata
                    .get("description")
                    .and_then(Value::as_str)
                    .and_then(|value| normalize_description(Some(value)));
                existing.description == requested
            }
            "FILE_VERSION::SET_KIND" => metadata
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|value| value == existing.kind),
            _ => false,
        };
        if unchanged {
            return Err(LocalError::Invalid(
                "named version already has the requested value".to_string(),
            ));
        }
        let operation_id = Uuid::new_v4().to_string();
        let event = named_version_ref_event(
            &file_item_id,
            event_type,
            version_id,
            metadata,
            None,
            state.head_seq,
        );
        self.append_version_commit(&file_item_id, message, operation_id, event)
            .await
    }

    async fn append_version_commit(
        &self,
        file_item_id: &ItemId,
        message: &str,
        operation_id: String,
        ref_event: RefEventProposal,
    ) -> Result<String> {
        let owner = self
            .core()
            .get_item(self.config().repo_id().clone(), file_item_id.clone())
            .await?
            .ok_or_else(|| LocalError::Integrity(format!("missing FILE {file_item_id}")))?;
        if owner.item_type != "FILE" || owner.lifecycle_state == LifecycleState::Tombstoned {
            return Err(LocalError::Integrity(format!(
                "invalid named-version owner {file_item_id}"
            )));
        }
        if owner.ref_head_seq == 0 {
            let legacy_id = version_set::item_id(self.config().repo_id(), file_item_id)
                .map_err(LocalError::Integrity)?;
            if let Some(legacy) = self
                .core()
                .get_item(self.config().repo_id().clone(), legacy_id.clone())
                .await?
            {
                return Err(LocalError::Integrity(format!(
                    "local format migration did not baseline legacy FILE_VERSION_SET {legacy_id} at head {}",
                    legacy.head_seq
                )));
            }
        }
        if ref_event.owner_item_id != *file_item_id || ref_event.base_ref_seq != owner.ref_head_seq
        {
            return Err(LocalError::Invalid(
                "named-version ref proposal disagrees with its FILE owner head".to_string(),
            ));
        }
        let accepted = self
            .core()
            .append_with_refs(
                self.config().repo_id().clone(),
                CommitProposal {
                    author_id: self.config().author().to_string(),
                    committer_id: self.config().author().to_string(),
                    message: Some(message.to_string()),
                    occurred_at: None,
                    operation_id: Some(operation_id),
                    events: Vec::new(),
                },
                vec![ref_event],
            )
            .await?;
        let commit_id = accepted.commit_id.to_string();
        self.recover()
            .await
            .map_err(|error| LocalError::AcceptedNeedsRecovery {
                commit_id: commit_id.clone(),
                detail: error.to_string(),
            })?;
        self.load_version_set(file_item_id).await.map_err(|error| {
            LocalError::AcceptedNeedsRecovery {
                commit_id: commit_id.clone(),
                detail: error.to_string(),
            }
        })?;
        Ok(commit_id)
    }

    async fn resolve_live_file(&self, caller: &Path, input: &Path) -> Result<(String, ItemId)> {
        let display = normalize_input_path(self.root_path(), caller, input)?;
        let committed = self.committed_tree().await?;
        let node = committed.get(&display).ok_or_else(|| {
            LocalError::Invalid(format!("path is not a tracked live file: {display}"))
        })?;
        if node.kind != NodeKind::File {
            return Err(LocalError::Invalid(format!(
                "path is not a tracked live file: {display}"
            )));
        }
        Ok((display, node.item_id.clone()))
    }

    /// Validate every named-version ledger against the FILE history it labels.
    pub(crate) async fn version_findings(&self, findings: &mut Findings) -> Result<()> {
        let mut items = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .core()
                .list_items(
                    self.config().repo_id().clone(),
                    cursor.clone(),
                    None,
                    PAGE_SIZE,
                )
                .await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map(|item| ItemCursor {
                created_at: item.created_at,
                item_id: item.item_id.clone(),
            });
            let done = page.len() < PAGE_SIZE;
            items.extend(page);
            if done {
                break;
            }
        }
        let owners = items
            .iter()
            .filter(|item| item.item_type == "FILE")
            .map(|item| (item.item_id.clone(), item.lifecycle_state))
            .collect::<HashMap<_, _>>();
        let mut version_owners = items
            .iter()
            .filter(|item| item.item_type == "FILE" && item.ref_head_seq > 0)
            .map(|item| item.item_id.clone())
            .collect::<BTreeSet<_>>();
        for item in items
            .iter()
            .filter(|item| item.item_type == version_set::ITEM_TYPE)
        {
            if item.lifecycle_state != LifecycleState::Live {
                findings.push(format!("FILE_VERSION_SET {} is not LIVE", item.item_id));
            }
            let file_item_id = match version_set::file_item_id_from_name(&item.name) {
                Ok(value) => value,
                Err(error) => {
                    findings.push(format!("FILE_VERSION_SET {}: {error}", item.item_id));
                    continue;
                }
            };
            if !owners.contains_key(&file_item_id) {
                findings.push(format!(
                    "FILE_VERSION_SET {} has no owning FILE {}",
                    item.item_id, file_item_id
                ));
                continue;
            }
            version_owners.insert(file_item_id);
        }
        let mut operation_owners = HashMap::new();
        for file_item_id in version_owners {
            let state = match self.load_version_set(&file_item_id).await {
                Ok(value) => value,
                Err(error) => {
                    findings.push(error.to_string());
                    continue;
                }
            };
            for version in state.versions.values() {
                if let Some((previous_file, previous_version)) = operation_owners.insert(
                    version.operation_id.clone(),
                    (file_item_id.clone(), version.version_id.clone()),
                ) {
                    findings.push(format!(
                        "named-version operation {} is reused by FILE {} version {} and FILE {} version {}",
                        version.operation_id,
                        previous_file,
                        previous_version,
                        file_item_id,
                        version.version_id
                    ));
                }
            }
        }
        Ok(())
    }

    async fn load_version_set(&self, file_item_id: &ItemId) -> Result<VersionSetState> {
        let owner = self
            .core()
            .get_item(self.config().repo_id().clone(), file_item_id.clone())
            .await?
            .ok_or_else(|| LocalError::Integrity(format!("missing FILE {file_item_id}")))?;
        let item_id = version_set::item_id(self.config().repo_id(), file_item_id)
            .map_err(LocalError::Integrity)?;
        let item = self
            .core()
            .get_item(self.config().repo_id().clone(), item_id.clone())
            .await?;
        if let Some(item) = item.as_ref()
            && (item.item_type != version_set::ITEM_TYPE
                || item.lifecycle_state != LifecycleState::Live
                || item.parent_folder_id.is_some()
                || item.name != version_set::item_name(file_item_id))
        {
            return Err(LocalError::Integrity(format!(
                "FILE_VERSION_SET {} has invalid ownership",
                item.item_id
            )));
        }
        let mut state = VersionSetState {
            item_id,
            head_seq: 0,
            legacy_head_seq: 0,
            legacy_head_digest: None,
            replay_commit_id: None,
            replay_commit_base_seq: 0,
            versions: HashMap::new(),
            live_sources: HashMap::new(),
        };
        let revisions = self
            .file_revisions(file_item_id)
            .await?
            .into_iter()
            .map(|revision| (revision.item_seq, revision))
            .collect::<HashMap<_, _>>();
        if let Some(item) = item.as_ref() {
            let mut from_seq = 1;
            loop {
                let events = self
                    .core()
                    .list_events(
                        self.config().repo_id().clone(),
                        state.item_id.clone(),
                        from_seq,
                        PAGE_SIZE,
                    )
                    .await?;
                if events.is_empty() {
                    break;
                }
                for event in &events {
                    apply_version_event(
                        self.config().repo_id(),
                        file_item_id,
                        &revisions,
                        &mut state,
                        event,
                    )?;
                    state.legacy_head_seq = event.item_seq;
                    state.legacy_head_digest = Some(event.event_payload_digest.to_string());
                    from_seq = event.item_seq + 1;
                }
                if events.len() < PAGE_SIZE {
                    break;
                }
            }
            if state.head_seq != item.head_seq {
                return Err(LocalError::Integrity(format!(
                    "FILE_VERSION_SET {} replay head {} differs from item head {}",
                    item.item_id, state.head_seq, item.head_seq
                )));
            }
        }
        let mut from_ref_seq = 1;
        loop {
            let events = self
                .core()
                .list_ref_events(
                    self.config().repo_id().clone(),
                    file_item_id.clone(),
                    from_ref_seq,
                    PAGE_SIZE,
                )
                .await?;
            if events.is_empty() {
                break;
            }
            for event in &events {
                apply_ref_version_event(
                    self.config().repo_id(),
                    file_item_id,
                    &revisions,
                    &mut state,
                    event,
                )?;
                from_ref_seq = event.ref_seq + 1;
            }
            if events.len() < PAGE_SIZE {
                break;
            }
        }
        if owner.ref_head_seq > 0 && state.head_seq != owner.ref_head_seq {
            return Err(LocalError::Integrity(format!(
                "FILE {file_item_id} ref replay head {} differs from item ref head {}",
                state.head_seq, owner.ref_head_seq
            )));
        }
        Ok(state)
    }

    async fn file_revisions(&self, file_item_id: &ItemId) -> Result<Vec<FileRevision>> {
        let mut revisions = Vec::new();
        let mut from_seq = 1;
        loop {
            let events = self
                .core()
                .list_events(
                    self.config().repo_id().clone(),
                    file_item_id.clone(),
                    from_seq,
                    PAGE_SIZE,
                )
                .await?;
            if events.is_empty() {
                break;
            }
            for event in &events {
                if event.event_type == "FILE::SET_CONTENT" {
                    let proposal = parse_event(event)?;
                    if proposal.set_content.is_some() {
                        let (blob_id, size) = event_content(&proposal)?;
                        revisions.push(FileRevision {
                            item_seq: event.item_seq,
                            repo_seq: event.repo_seq.ok_or_else(|| {
                                LocalError::Integrity(format!(
                                    "FILE revision {} has no repository sequence",
                                    event.item_seq
                                ))
                            })?,
                            commit_id: event.commit_id.clone(),
                            commit_event_idx: event.commit_event_idx,
                            blob_id,
                            size,
                        });
                    }
                }
                from_seq = event.item_seq + 1;
            }
            if events.len() < PAGE_SIZE {
                break;
            }
        }
        if revisions.is_empty() {
            return Err(LocalError::Integrity(format!(
                "FILE {file_item_id} has no content revisions"
            )));
        }
        Ok(revisions)
    }
}

fn apply_version_event(
    repo_id: &synck_vcs::types::RepoId,
    file_item_id: &ItemId,
    revisions: &HashMap<i64, FileRevision>,
    state: &mut VersionSetState,
    event: &EventRow,
) -> Result<()> {
    let proposal = parse_event(event)?;
    version_set::validate_identities(repo_id, std::slice::from_ref(&proposal))
        .map_err(LocalError::Integrity)?;
    if proposal.item_id.as_ref() != Some(&state.item_id) {
        return Err(LocalError::Integrity(format!(
            "FILE_VERSION_SET event {} has mismatched item ID",
            event.item_seq
        )));
    }
    if event.item_seq != state.head_seq + 1 {
        return Err(LocalError::Integrity(format!(
            "FILE_VERSION_SET event sequence {} does not follow {}",
            event.item_seq, state.head_seq
        )));
    }
    if state.replay_commit_id.as_ref() != Some(&event.commit_id) {
        state.replay_commit_id = Some(event.commit_id.clone());
        state.replay_commit_base_seq = state.head_seq;
    }
    if event.event_type == version_set::CREATE_EVENT {
        if state.head_seq != 0 || event.item_seq != 1 {
            return Err(LocalError::Integrity(
                "FILE_VERSION_SET is created more than once".to_string(),
            ));
        }
        let create = proposal.create_item.as_ref().ok_or_else(|| {
            LocalError::Integrity("FILE_VERSION_SET::CREATE has no item".to_string())
        })?;
        if version_set::file_item_id_from_name(&create.name).map_err(LocalError::Integrity)?
            != *file_item_id
        {
            return Err(LocalError::Integrity(
                "FILE_VERSION_SET::CREATE names the wrong FILE".to_string(),
            ));
        }
        state.head_seq = event.item_seq;
        return Ok(());
    }
    if state.head_seq == 0
        && !(event.item_seq == 1
            && event.event_type == version_set::VERSION_CREATE_EVENT
            && proposal.base_seq.is_none())
    {
        return Err(LocalError::Integrity(format!(
            "{} precedes FILE_VERSION_SET::CREATE",
            event.event_type
        )));
    }
    let expected_base = if state.replay_commit_base_seq == 0 {
        None
    } else {
        Some(state.replay_commit_base_seq)
    };
    if proposal.base_seq != expected_base {
        return Err(LocalError::Integrity(format!(
            "{} has invalid base sequence {:?}, expected {:?}",
            event.event_type, proposal.base_seq, expected_base
        )));
    }
    let metadata = proposal
        .set_metadata
        .as_ref()
        .and_then(Value::as_object)
        .ok_or_else(|| LocalError::Integrity(format!("{} has no metadata", event.event_type)))?;
    if required_item_id(metadata, "file_item_id")? != *file_item_id {
        return Err(LocalError::Integrity(format!(
            "{} names the wrong FILE",
            event.event_type
        )));
    }
    let version_id = required_uuid(metadata, "version_id")?;
    match event.event_type.as_str() {
        version_set::VERSION_CREATE_EVENT => {
            if Uuid::parse_str(&version_id)
                .expect("required_uuid parsed")
                .get_version_num()
                != 7
            {
                return Err(LocalError::Integrity(
                    "new named-version ID is not UUIDv7".to_string(),
                ));
            }
            if state.versions.contains_key(&version_id) {
                return Err(LocalError::Integrity(format!(
                    "named version {version_id} is created more than once"
                )));
            }
            let operation_id = required_string(metadata, "operation_id")?;
            if state
                .versions
                .values()
                .any(|version| version.operation_id == operation_id)
            {
                return Err(LocalError::Integrity(format!(
                    "named-version operation {operation_id} is reused"
                )));
            }
            let source_item_seq = required_positive_i64(metadata, "source_item_seq")?;
            let source_commit_id = optional_commit_id(metadata, "source_commit_id")?;
            let source_commit_event_idx =
                optional_nonnegative_i32(metadata, "source_commit_event_idx")?;
            let kind = required_kind(metadata)?;
            let label_source = required_label_source(metadata)?;
            let order_key = required_i64(metadata, "order_key")?;
            let description = optional_description(metadata)?;
            let (blob_id, encoded_size) =
                parse_version_content(&proposal).map_err(LocalError::Integrity)?;
            let size = validate_source_transition(
                event,
                &version_id,
                source_item_seq,
                source_commit_id.as_ref(),
                source_commit_event_idx,
                &blob_id,
                encoded_size,
                revisions,
            )?;
            if let Some(existing) = state.live_sources.get(&source_item_seq) {
                return Err(LocalError::Integrity(format!(
                    "named versions {existing} and {version_id} both reference live FILE revision {source_item_seq}"
                )));
            }
            state
                .live_sources
                .insert(source_item_seq, version_id.clone());
            state.versions.insert(
                version_id.clone(),
                StoredVersion {
                    version_id,
                    operation_id,
                    live: true,
                    kind,
                    label_source,
                    order_key,
                    description,
                    source_item_seq,
                    source_commit_id,
                    source_commit_event_idx,
                    blob_id,
                    size,
                    created_seq: event.item_seq,
                    current_seq: event.item_seq,
                    created_at: event.created_at,
                },
            );
        }
        version_set::VERSION_SET_DESCRIPTION_EVENT => {
            let description = required_description(metadata)?;
            let version = live_version_mut(state, &version_id)?;
            version.description = description;
            version.current_seq = event.item_seq;
        }
        version_set::VERSION_SET_KIND_EVENT => {
            let kind = required_kind(metadata)?;
            let version = live_version_mut(state, &version_id)?;
            version.kind = kind;
            version.current_seq = event.item_seq;
        }
        version_set::VERSION_SET_LABEL_SOURCE_EVENT => {
            let label_source = required_label_source(metadata)?;
            let version = live_version_mut(state, &version_id)?;
            version.label_source = label_source;
            version.current_seq = event.item_seq;
        }
        version_set::VERSION_SET_ORDER_EVENT => {
            let order_key = required_i64(metadata, "order_key")?;
            let version = live_version_mut(state, &version_id)?;
            version.order_key = order_key;
            version.current_seq = event.item_seq;
        }
        version_set::VERSION_REPLACE_CONTENT_EVENT => {
            let (previous_source, previous_commit_id, previous_commit_event_idx) = {
                let previous = replay_live_version(state, &version_id)?;
                (
                    previous.source_item_seq,
                    previous.source_commit_id.clone(),
                    previous.source_commit_event_idx,
                )
            };
            let source_item_seq =
                optional_positive_i64(metadata, "source_item_seq")?.unwrap_or(previous_source);
            let source_commit_id =
                optional_commit_id(metadata, "source_commit_id")?.or(previous_commit_id);
            let source_commit_event_idx =
                optional_nonnegative_i32(metadata, "source_commit_event_idx")?
                    .or(previous_commit_event_idx);
            let description_patch =
                if metadata.contains_key("description") || metadata.contains_key("comment") {
                    Some(required_description(metadata)?)
                } else {
                    None
                };
            let (blob_id, encoded_size) =
                parse_version_content(&proposal).map_err(LocalError::Integrity)?;
            let size = validate_source_transition(
                event,
                &version_id,
                source_item_seq,
                source_commit_id.as_ref(),
                source_commit_event_idx,
                &blob_id,
                encoded_size,
                revisions,
            )?;
            if let Some(existing) = state.live_sources.get(&source_item_seq)
                && existing != &version_id
            {
                return Err(LocalError::Integrity(format!(
                    "named versions {existing} and {version_id} both reference live FILE revision {source_item_seq}"
                )));
            }
            if previous_source != source_item_seq {
                state.live_sources.remove(&previous_source);
                state
                    .live_sources
                    .insert(source_item_seq, version_id.clone());
            }
            let version = live_version_mut(state, &version_id)?;
            version.source_item_seq = source_item_seq;
            version.source_commit_id = source_commit_id;
            version.source_commit_event_idx = source_commit_event_idx;
            version.blob_id = blob_id;
            version.size = size;
            if let Some(description) = description_patch {
                version.description = description;
            }
            version.current_seq = event.item_seq;
        }
        version_set::VERSION_TOMBSTONE_EVENT => {
            let source_item_seq = replay_live_version(state, &version_id)?.source_item_seq;
            state.live_sources.remove(&source_item_seq);
            let version = live_version_mut(state, &version_id)?;
            version.live = false;
            version.current_seq = event.item_seq;
        }
        other => {
            return Err(LocalError::Integrity(format!(
                "unsupported FILE_VERSION_SET event {other}"
            )));
        }
    }
    state.head_seq = event.item_seq;
    Ok(())
}

fn apply_ref_version_event(
    repo_id: &synck_vcs::types::RepoId,
    file_item_id: &ItemId,
    revisions: &HashMap<i64, FileRevision>,
    state: &mut VersionSetState,
    event: &RefEventRow,
) -> Result<()> {
    let proposal = decode_ref_version_event(file_item_id, event)?;
    if event.event_type == "FILE_VERSION::LEGACY_BASELINE" {
        let metadata = proposal
            .set_metadata
            .as_ref()
            .and_then(Value::as_object)
            .ok_or_else(|| LocalError::Integrity("legacy baseline has no metadata".to_string()))?;
        let sidecar_id = required_item_id(metadata, "legacy_sidecar_item_id")?;
        let sidecar_head = required_positive_i64(metadata, "legacy_sidecar_head_seq")?;
        let sidecar_digest = required_string(metadata, "legacy_sidecar_head_event_digest")?;
        if proposal.set_content.is_some() || metadata.len() != 3 {
            return Err(LocalError::Integrity(
                "legacy named-version baseline has invalid payload shape".to_string(),
            ));
        }
        if sidecar_id != state.item_id
            || sidecar_head != state.legacy_head_seq
            || event.ref_seq != state.legacy_head_seq
            || state.legacy_head_digest.as_deref() != Some(sidecar_digest.as_str())
            || proposal.base_ref_seq != 0
            || proposal.initial_ref_seq != Some(event.ref_seq)
        {
            return Err(LocalError::Integrity(
                "legacy named-version baseline does not match the frozen sidecar".to_string(),
            ));
        }
        state.head_seq = event.ref_seq;
        return Ok(());
    }
    let suffix = event
        .event_type
        .strip_prefix("FILE_VERSION::")
        .ok_or_else(|| {
            LocalError::Integrity(format!("unsupported ref event {}", event.event_type))
        })?;
    let legacy_event_type = format!("FILE_VERSION_SET::VERSION_{suffix}");
    let synthetic = EventProposal {
        item_id: Some(state.item_id.clone()),
        event_type: legacy_event_type.clone(),
        set_metadata: proposal.set_metadata,
        set_content: proposal.set_content,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: (proposal.base_ref_seq > 0).then_some(proposal.base_ref_seq),
    };
    let payload = canonical_event_proposal_bytes(&synthetic)
        .map_err(|error| LocalError::Integrity(error.to_string()))?;
    apply_version_event(
        repo_id,
        file_item_id,
        revisions,
        state,
        &EventRow {
            repo_id: event.repo_id.clone(),
            item_id: state.item_id.clone(),
            item_seq: event.ref_seq,
            event_type: legacy_event_type,
            commit_id: event.commit_id.clone(),
            commit_event_idx: event.commit_event_idx,
            repo_seq: event.repo_seq,
            payload,
            event_payload_digest: event.event_payload_digest.clone(),
            created_at: event.created_at,
            parent_folder_id: None,
            parent_folder_id_before: None,
        },
    )
}

fn decode_ref_version_event(
    file_item_id: &ItemId,
    event: &RefEventRow,
) -> Result<RefEventProposal> {
    let proposal: RefEventProposal = serde_json::from_slice(&event.payload).map_err(|error| {
        LocalError::Integrity(format!(
            "invalid ref-event payload at ref sequence {}: {error}",
            event.ref_seq
        ))
    })?;
    if proposal.owner_item_id != *file_item_id
        || event.owner_item_id != *file_item_id
        || proposal.event_type != event.event_type
    {
        return Err(LocalError::Integrity(format!(
            "ref event {} identity disagrees with its row",
            event.ref_seq
        )));
    }
    let canonical = canonical_ref_event_proposal_bytes(&proposal)
        .map_err(|error| LocalError::Integrity(error.to_string()))?;
    if canonical != event.payload {
        return Err(LocalError::Integrity(format!(
            "ref event {} payload is not canonical",
            event.ref_seq
        )));
    }
    let digest = compute_sha256_digest_bytes(&event.payload);
    if sha256_prefixed_hex_from_digest_bytes(digest) != event.event_payload_digest.as_str() {
        return Err(LocalError::Integrity(format!(
            "ref event {} payload digest disagrees with its bytes",
            event.ref_seq
        )));
    }
    Ok(proposal)
}

#[allow(clippy::too_many_arguments)]
fn validate_source_transition(
    event: &EventRow,
    version_id: &str,
    source_item_seq: i64,
    source_commit_id: Option<&CommitId>,
    source_commit_event_idx: Option<i32>,
    blob_id: &BlobId,
    encoded_size: Option<i64>,
    revisions: &HashMap<i64, FileRevision>,
) -> Result<i64> {
    let source = revisions.get(&source_item_seq).ok_or_else(|| {
        LocalError::Integrity(format!(
            "named version {version_id} references missing FILE revision {source_item_seq}"
        ))
    })?;
    if source_commit_id.is_some_and(|value| value != &source.commit_id)
        || source_commit_event_idx.is_some_and(|value| value != source.commit_event_idx)
        || blob_id != &source.blob_id
        || encoded_size.is_some_and(|size| size != source.size)
    {
        return Err(LocalError::Integrity(format!(
            "named version {version_id} disagrees with FILE revision {source_item_seq}"
        )));
    }
    let event_repo_seq = event.repo_seq.ok_or_else(|| {
        LocalError::Integrity(format!(
            "named-version event {} has no repository sequence",
            event.item_seq
        ))
    })?;
    if source.repo_seq > event_repo_seq
        || (source.repo_seq == event_repo_seq && source.commit_event_idx >= event.commit_event_idx)
    {
        return Err(LocalError::Integrity(format!(
            "named version {version_id} references FILE revision {source_item_seq} before it exists"
        )));
    }
    Ok(source.size)
}

fn replay_live_version<'a>(
    state: &'a VersionSetState,
    version_id: &str,
) -> Result<&'a StoredVersion> {
    let version = state.versions.get(version_id).ok_or_else(|| {
        LocalError::Integrity(format!(
            "mutation references missing named version {version_id}"
        ))
    })?;
    if !version.live {
        return Err(LocalError::Integrity(format!(
            "mutation references tombstoned named version {version_id}"
        )));
    }
    Ok(version)
}

fn version_entries(
    state: &VersionSetState,
    include_tombstoned: bool,
) -> Result<Vec<NamedVersionEntry>> {
    let mut versions = state.versions.values().collect::<Vec<_>>();
    versions.sort_by(|left, right| {
        left.order_key
            .cmp(&right.order_key)
            .then_with(|| left.created_seq.cmp(&right.created_seq))
            .then_with(|| left.version_id.cmp(&right.version_id))
    });
    let (mut major, mut minor) = (0, 0);
    let mut output = Vec::new();
    for version in versions {
        let label = if version.live {
            version_set::advance_label(&version.kind, &mut major, &mut minor)
                .map_err(LocalError::Integrity)?;
            Some(format!("{major}.{minor:02}"))
        } else {
            None
        };
        if version.live || include_tombstoned {
            output.push(NamedVersionEntry {
                label,
                version_id: version.version_id.clone(),
                lifecycle_state: if version.live { "LIVE" } else { "TOMBSTONED" }.to_string(),
                kind: version.kind.clone(),
                label_source: version.label_source.clone(),
                description: version.description.clone(),
                source_item_seq: version.source_item_seq,
                blob_id: version.blob_id.to_string(),
                size: version.size,
                version_set_seq: version.current_seq,
                created_at: version.created_at,
            });
        }
    }
    Ok(output)
}

fn require_live_version<'a>(
    state: &'a VersionSetState,
    version_id: &str,
) -> Result<&'a StoredVersion> {
    let parsed = Uuid::parse_str(version_id)
        .map_err(|error| LocalError::Invalid(format!("invalid version ID: {error}")))?;
    let canonical = parsed.to_string();
    let version = state
        .versions
        .get(&canonical)
        .ok_or_else(|| LocalError::Invalid(format!("named version does not exist: {canonical}")))?;
    if !version.live {
        return Err(LocalError::Invalid(format!(
            "named version is tombstoned: {canonical}"
        )));
    }
    Ok(version)
}

fn live_version_mut<'a>(
    state: &'a mut VersionSetState,
    version_id: &str,
) -> Result<&'a mut StoredVersion> {
    let version = state.versions.get_mut(version_id).ok_or_else(|| {
        LocalError::Integrity(format!(
            "mutation references missing named version {version_id}"
        ))
    })?;
    if !version.live {
        return Err(LocalError::Integrity(format!(
            "mutation references tombstoned named version {version_id}"
        )));
    }
    Ok(version)
}

fn select_revision(revisions: &[FileRevision], requested: Option<i64>) -> Result<&FileRevision> {
    match requested {
        None => revisions
            .last()
            .ok_or_else(|| LocalError::Integrity("FILE has no content revisions".to_string())),
        Some(value) if value > 0 => revisions
            .iter()
            .find(|revision| revision.item_seq == value)
            .ok_or_else(|| {
                LocalError::Invalid(format!(
                    "item sequence {value} is not a FILE content revision"
                ))
            }),
        Some(_) => Err(LocalError::Invalid(
            "item sequence must be greater than zero".to_string(),
        )),
    }
}

fn parse_event(event: &EventRow) -> Result<EventProposal> {
    serde_json::from_slice(&event.payload).map_err(|error| {
        LocalError::Integrity(format!(
            "invalid event payload at item sequence {}: {error}",
            event.item_seq
        ))
    })
}

fn event_content(event: &EventProposal) -> Result<(BlobId, i64)> {
    let blob_id = match event.set_content.as_ref() {
        Some(EventContent::Blob { blob_id }) => BlobId::parse(blob_id)
            .map_err(|error| LocalError::Integrity(format!("invalid version blob ID: {error}")))?,
        _ => {
            return Err(LocalError::Integrity(
                "version content event has no blob".to_string(),
            ));
        }
    };
    let size = event
        .set_metadata
        .as_ref()
        .and_then(|value| value.pointer("/content/size"))
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or_else(|| LocalError::Integrity("version content has invalid size".to_string()))?;
    Ok((blob_id, size))
}

fn required_item_id(metadata: &Map<String, Value>, key: &str) -> Result<ItemId> {
    let value = required_string(metadata, key)?;
    ItemId::parse(&value).map_err(|error| LocalError::Integrity(format!("invalid {key}: {error}")))
}

fn required_uuid(metadata: &Map<String, Value>, key: &str) -> Result<String> {
    let value = required_string(metadata, key)?;
    Uuid::parse_str(&value)
        .map(|value| value.to_string())
        .map_err(|error| LocalError::Integrity(format!("invalid {key}: {error}")))
}

fn required_string(metadata: &Map<String, Value>, key: &str) -> Result<String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| LocalError::Integrity(format!("FILE_VERSION_SET event requires {key}")))
}

fn required_i64(metadata: &Map<String, Value>, key: &str) -> Result<i64> {
    metadata
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| LocalError::Integrity(format!("FILE_VERSION_SET event requires {key}")))
}

fn required_positive_i64(metadata: &Map<String, Value>, key: &str) -> Result<i64> {
    required_i64(metadata, key).and_then(|value| {
        (value > 0)
            .then_some(value)
            .ok_or_else(|| LocalError::Integrity(format!("{key} must be greater than zero")))
    })
}

fn optional_positive_i64(metadata: &Map<String, Value>, key: &str) -> Result<Option<i64>> {
    match metadata.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .filter(|value| *value > 0)
            .map(Some)
            .ok_or_else(|| LocalError::Integrity(format!("{key} must be greater than zero"))),
    }
}

fn optional_nonnegative_i32(metadata: &Map<String, Value>, key: &str) -> Result<Option<i32>> {
    let value = match metadata.get(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let value = value
        .as_i64()
        .filter(|value| *value >= 0)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| LocalError::Integrity(format!("{key} must be a non-negative i32")))?;
    Ok(Some(value))
}

fn optional_commit_id(metadata: &Map<String, Value>, key: &str) -> Result<Option<CommitId>> {
    match metadata.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => CommitId::parse(value)
            .map(Some)
            .map_err(|error| LocalError::Integrity(format!("invalid {key}: {error}"))),
        Some(_) => Err(LocalError::Integrity(format!(
            "{key} must be a commit ID or null"
        ))),
    }
}

fn required_kind(metadata: &Map<String, Value>) -> Result<String> {
    let value = required_string(metadata, "kind")?;
    version_set::normalize_kind(Some(&value))
        .map(str::to_string)
        .map_err(LocalError::Integrity)
}

fn required_label_source(metadata: &Map<String, Value>) -> Result<String> {
    let value = required_string(metadata, "label_source")?;
    version_set::normalize_label_source(Some(&value))
        .map(str::to_string)
        .map_err(LocalError::Integrity)
}

fn normalize_description(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn optional_description(metadata: &Map<String, Value>) -> Result<Option<String>> {
    if metadata.contains_key("description") || metadata.contains_key("comment") {
        required_description(metadata)
    } else {
        Ok(None)
    }
}

fn required_description(metadata: &Map<String, Value>) -> Result<Option<String>> {
    match metadata
        .get("description")
        .or_else(|| metadata.get("comment"))
    {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(normalize_description(Some(value))),
        Some(_) => Err(LocalError::Integrity(
            "version description must be a string or null".to_string(),
        )),
        None => Err(LocalError::Integrity(
            "version description mutation has no description".to_string(),
        )),
    }
}

fn require_message(message: &str) -> Result<()> {
    if message.is_empty() {
        Err(LocalError::Invalid(
            "commit message must not be empty".to_string(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn ref_replay_rejects_row_payload_identity_and_digest_mismatches() {
        let repo_id = synck_vcs::types::RepoId::new();
        let file_item_id = ItemId::new();
        let proposal = named_version_ref_event(
            &file_item_id,
            "FILE_VERSION::TOMBSTONE",
            &Uuid::now_v7().to_string(),
            Map::new(),
            None,
            0,
        );
        let payload = canonical_ref_event_proposal_bytes(&proposal).unwrap();
        let payload_digest = compute_sha256_digest_bytes(&payload);
        let mut row = RefEventRow {
            repo_id,
            owner_item_id: file_item_id.clone(),
            ref_seq: 1,
            event_type: proposal.event_type.clone(),
            commit_id: CommitId::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
            commit_event_idx: 0,
            repo_seq: Some(1),
            payload,
            event_payload_digest: synck_vcs::types::Sha256Digest::parse(
                sha256_prefixed_hex_from_digest_bytes(payload_digest),
            )
            .unwrap(),
            created_at: synck_vcs::types::now(),
        };
        decode_ref_version_event(&file_item_id, &row).unwrap();

        row.event_type = "FILE_VERSION::SET_DESCRIPTION".to_string();
        assert!(
            decode_ref_version_event(&file_item_id, &row)
                .unwrap_err()
                .to_string()
                .contains("identity disagrees")
        );

        row.event_type = proposal.event_type;
        row.event_payload_digest =
            synck_vcs::types::Sha256Digest::parse(format!("sha256:{}", "0".repeat(64))).unwrap();
        assert!(
            decode_ref_version_event(&file_item_id, &row)
                .unwrap_err()
                .to_string()
                .contains("digest disagrees")
        );
    }

    #[test]
    fn ref_replay_rejects_non_exact_legacy_baseline_shape() {
        let repo_id = synck_vcs::types::RepoId::new();
        let file_item_id = ItemId::new();
        let sidecar_item_id = version_set::item_id(&repo_id, &file_item_id).unwrap();
        let sidecar_digest = format!("sha256:{}", "1".repeat(64));
        let exact = RefEventProposal {
            owner_item_id: file_item_id.clone(),
            event_type: "FILE_VERSION::LEGACY_BASELINE".to_string(),
            set_metadata: Some(serde_json::json!({
                "legacy_sidecar_item_id": sidecar_item_id.as_str(),
                "legacy_sidecar_head_seq": 3,
                "legacy_sidecar_head_event_digest": sidecar_digest,
            })),
            set_content: None,
            base_ref_seq: 0,
            initial_ref_seq: Some(3),
        };

        let mut extra_metadata = exact.clone();
        extra_metadata
            .set_metadata
            .as_mut()
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("unexpected".to_string(), true.into());
        let mut content = exact.clone();
        content.set_content = Some(EventContent::Text {
            text: "unexpected".to_string(),
        });

        for (proposal, accepted) in [(exact, true), (extra_metadata, false), (content, false)] {
            let payload = canonical_ref_event_proposal_bytes(&proposal).unwrap();
            let payload_digest = compute_sha256_digest_bytes(&payload);
            let event = RefEventRow {
                repo_id: repo_id.clone(),
                owner_item_id: file_item_id.clone(),
                ref_seq: 3,
                event_type: proposal.event_type.clone(),
                commit_id: CommitId::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
                commit_event_idx: 0,
                repo_seq: Some(1),
                payload,
                event_payload_digest: synck_vcs::types::Sha256Digest::parse(
                    sha256_prefixed_hex_from_digest_bytes(payload_digest),
                )
                .unwrap(),
                created_at: synck_vcs::types::now(),
            };
            let mut state = VersionSetState {
                item_id: sidecar_item_id.clone(),
                head_seq: 3,
                legacy_head_seq: 3,
                legacy_head_digest: Some(sidecar_digest.clone()),
                replay_commit_id: None,
                replay_commit_base_seq: 0,
                versions: HashMap::new(),
                live_sources: HashMap::new(),
            };
            let result = apply_ref_version_event(
                &repo_id,
                &file_item_id,
                &HashMap::new(),
                &mut state,
                &event,
            );
            if accepted {
                result.unwrap();
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("invalid payload shape")
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_event(
        _repository: &Repository,
        file_item_id: &ItemId,
        version_id: &str,
        operation_id: &str,
        source_item_seq: i64,
        source_commit_id: Option<&CommitId>,
        source_commit_event_idx: Option<i32>,
        blob_id: &BlobId,
        size: i64,
        order_key: i64,
        next_ref_seq: i64,
    ) -> RefEventProposal {
        let mut metadata = Map::new();
        metadata.insert("operation_id".to_string(), operation_id.into());
        metadata.insert("source_item_seq".to_string(), source_item_seq.into());
        if let Some(source_commit_id) = source_commit_id {
            metadata.insert(
                "source_commit_id".to_string(),
                source_commit_id.to_string().into(),
            );
        }
        if let Some(source_commit_event_idx) = source_commit_event_idx {
            metadata.insert(
                "source_commit_event_idx".to_string(),
                i64::from(source_commit_event_idx).into(),
            );
        }
        metadata.insert("kind".to_string(), version_set::KIND_MINOR.into());
        metadata.insert(
            "label_source".to_string(),
            version_set::LABEL_SOURCE_INTERNAL.into(),
        );
        metadata.insert("order_key".to_string(), order_key.into());
        metadata.insert("content".to_string(), serde_json::json!({ "size": size }));
        named_version_ref_event(
            file_item_id,
            "FILE_VERSION::CREATE",
            version_id,
            metadata,
            Some(EventContent::Blob {
                blob_id: blob_id.to_string(),
            }),
            next_ref_seq - 1,
        )
    }

    fn replace_event(
        _repository: &Repository,
        file_item_id: &ItemId,
        version_id: &str,
        source: &FileRevision,
        next_ref_seq: i64,
    ) -> RefEventProposal {
        let mut metadata = Map::new();
        metadata.insert("source_item_seq".to_string(), source.item_seq.into());
        metadata.insert(
            "source_commit_id".to_string(),
            source.commit_id.to_string().into(),
        );
        metadata.insert(
            "source_commit_event_idx".to_string(),
            i64::from(source.commit_event_idx).into(),
        );
        metadata.insert(
            "content".to_string(),
            serde_json::json!({ "size": source.size }),
        );
        named_version_ref_event(
            file_item_id,
            "FILE_VERSION::REPLACE_CONTENT",
            version_id,
            metadata,
            Some(EventContent::Blob {
                blob_id: source.blob_id.to_string(),
            }),
            next_ref_seq - 1,
        )
    }

    fn tombstone_event(
        _repository: &Repository,
        file_item_id: &ItemId,
        version_id: &str,
        next_ref_seq: i64,
    ) -> RefEventProposal {
        named_version_ref_event(
            file_item_id,
            "FILE_VERSION::TOMBSTONE",
            version_id,
            Map::new(),
            None,
            next_ref_seq - 1,
        )
    }

    async fn append(repository: &Repository, message: &str, ref_events: Vec<RefEventProposal>) {
        repository
            .core()
            .append_with_refs(
                repository.config().repo_id().clone(),
                CommitProposal {
                    author_id: "author".to_string(),
                    committer_id: "author".to_string(),
                    message: Some(message.to_string()),
                    occurred_at: None,
                    operation_id: Some(Uuid::new_v4().to_string()),
                    events: Vec::new(),
                },
                ref_events,
            )
            .await
            .unwrap();
        // Seal the commit as an ordinary local write would.
        repository.recover_current().await.unwrap();
    }

    async fn repository_with_file(root: &Path) -> Repository {
        fs::write(root.join("file.txt"), b"first").unwrap();
        crate::init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        repository
    }

    #[tokio::test]
    async fn fsck_rejects_an_invalid_source_even_after_content_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let repository = repository_with_file(root).await;
        fs::write(root.join("file.txt"), b"later").unwrap();
        repository.commit("later").await.unwrap();
        let file_item_id = repository.committed_tree().await.unwrap()["file.txt"]
            .item_id
            .clone();
        let revisions = repository.file_revisions(&file_item_id).await.unwrap();
        let first = &revisions[0];
        let second = &revisions[1];
        let version_id = Uuid::now_v7().to_string();
        let bad_create = create_event(
            &repository,
            &file_item_id,
            &version_id,
            &Uuid::new_v4().to_string(),
            second.item_seq,
            Some(&second.commit_id),
            Some(second.commit_event_idx),
            &first.blob_id,
            second.size,
            version_set::ORDER_KEY_GAP * 2,
            2,
        );
        append(&repository, "bad source", vec![bad_create]).await;
        append(
            &repository,
            "replace bad source",
            vec![replace_event(
                &repository,
                &file_item_id,
                &version_id,
                second,
                3,
            )],
        )
        .await;

        let findings = repository.fsck().await.unwrap();
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("disagrees with FILE revision")),
            "{findings:?}"
        );
    }

    #[tokio::test]
    async fn fsck_rejects_a_named_version_that_forward_references_a_file_revision() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let repository = repository_with_file(root).await;
        let file_item_id = repository.committed_tree().await.unwrap()["file.txt"]
            .item_id
            .clone();
        fs::write(root.join("file.txt"), b"later").unwrap();
        let digest = hex::encode(Sha256::digest(b"later"));
        let blob_id = repository
            .install_object(Path::new("file.txt"), "file.txt", &digest, 5)
            .unwrap();
        let version_id = Uuid::now_v7().to_string();
        append(
            &repository,
            "forward reference",
            vec![create_event(
                &repository,
                &file_item_id,
                &version_id,
                &Uuid::new_v4().to_string(),
                3,
                None,
                Some(0),
                &blob_id,
                5,
                version_set::ORDER_KEY_GAP * 2,
                2,
            )],
        )
        .await;
        repository
            .commit("create referenced revision")
            .await
            .unwrap();

        let findings = repository.fsck().await.unwrap();
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("before it exists")),
            "{findings:?}"
        );
    }

    #[tokio::test]
    async fn fsck_rejects_a_transient_duplicate_live_source() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let repository = repository_with_file(root).await;
        let file_item_id = repository.committed_tree().await.unwrap()["file.txt"]
            .item_id
            .clone();
        let initial_version_id = repository
            .named_versions(root, Path::new("file.txt"), false)
            .await
            .unwrap()[0]
            .version_id
            .clone();
        fs::write(root.join("file.txt"), b"later").unwrap();
        repository.commit("later").await.unwrap();
        let revisions = repository.file_revisions(&file_item_id).await.unwrap();
        let first = &revisions[0];
        let second = &revisions[1];
        let version_id = Uuid::now_v7().to_string();
        append(
            &repository,
            "second label",
            vec![create_event(
                &repository,
                &file_item_id,
                &version_id,
                &Uuid::new_v4().to_string(),
                second.item_seq,
                Some(&second.commit_id),
                Some(second.commit_event_idx),
                &second.blob_id,
                second.size,
                version_set::ORDER_KEY_GAP * 2,
                2,
            )],
        )
        .await;
        append(
            &repository,
            "duplicate source",
            vec![replace_event(
                &repository,
                &file_item_id,
                &version_id,
                first,
                3,
            )],
        )
        .await;
        append(
            &repository,
            "hide duplicate",
            vec![tombstone_event(
                &repository,
                &file_item_id,
                &initial_version_id,
                4,
            )],
        )
        .await;

        let findings = repository.fsck().await.unwrap();
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("both reference live FILE revision")),
            "{findings:?}"
        );
    }

    #[tokio::test]
    async fn shared_replacement_patch_inherits_source_and_resolves_nullable_size() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let repository = repository_with_file(root).await;
        fs::write(root.join("file.txt"), b"later").unwrap();
        repository.commit("later").await.unwrap();
        let file_item_id = repository.committed_tree().await.unwrap()["file.txt"]
            .item_id
            .clone();
        let revision = repository
            .file_revisions(&file_item_id)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let version_id = Uuid::now_v7().to_string();
        let mut create = create_event(
            &repository,
            &file_item_id,
            &version_id,
            &Uuid::new_v4().to_string(),
            revision.item_seq,
            Some(&revision.commit_id),
            Some(revision.commit_event_idx),
            &revision.blob_id,
            revision.size,
            version_set::ORDER_KEY_GAP * 2,
            2,
        );
        create
            .set_metadata
            .as_mut()
            .and_then(Value::as_object_mut)
            .unwrap()
            .insert("content".to_string(), serde_json::json!({ "size": null }));
        append(&repository, "nullable create size", vec![create]).await;

        let mut metadata = Map::new();
        metadata.insert("description".to_string(), "server-shaped patch".into());
        metadata.insert("content".to_string(), serde_json::json!({ "size": null }));
        let replacement = named_version_ref_event(
            &file_item_id,
            "FILE_VERSION::REPLACE_CONTENT",
            &version_id,
            metadata,
            Some(EventContent::Blob {
                blob_id: revision.blob_id.to_string(),
            }),
            2,
        );
        append(&repository, "patch version content", vec![replacement]).await;

        let versions = repository
            .named_versions(root, Path::new("file.txt"), false)
            .await
            .unwrap();
        let version = versions
            .iter()
            .find(|version| version.version_id == version_id)
            .unwrap();
        assert_eq!(version.source_item_seq, revision.item_seq);
        assert_eq!(version.blob_id, revision.blob_id.to_string());
        assert_eq!(version.size, revision.size);
        assert_eq!(version.description.as_deref(), Some("server-shaped patch"));
        assert!(repository.fsck().await.unwrap().is_empty());
        assert_eq!(
            repository
                .storage_report()
                .await
                .unwrap()
                .referenced_objects,
            3
        );

        let archive_parent = tempfile::tempdir().unwrap();
        repository
            .archive(&archive_parent.path().join("archive"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fsck_rejects_repo_wide_operation_reuse_for_the_same_version_id() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("one.txt"), b"first").unwrap();
        fs::write(root.join("two.txt"), b"first").unwrap();
        crate::init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        fs::write(root.join("one.txt"), b"later").unwrap();
        fs::write(root.join("two.txt"), b"later").unwrap();
        repository.commit("later").await.unwrap();
        let tree = repository.committed_tree().await.unwrap();
        let one = tree["one.txt"].item_id.clone();
        let two = tree["two.txt"].item_id.clone();
        let one_revision = repository
            .file_revisions(&one)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let two_revision = repository
            .file_revisions(&two)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let version_id = Uuid::now_v7().to_string();
        let operation_id = Uuid::new_v4().to_string();
        append(
            &repository,
            "reuse operation",
            vec![
                create_event(
                    &repository,
                    &one,
                    &version_id,
                    &operation_id,
                    one_revision.item_seq,
                    Some(&one_revision.commit_id),
                    Some(one_revision.commit_event_idx),
                    &one_revision.blob_id,
                    one_revision.size,
                    version_set::ORDER_KEY_GAP * 2,
                    2,
                ),
                create_event(
                    &repository,
                    &two,
                    &version_id,
                    &operation_id,
                    two_revision.item_seq,
                    Some(&two_revision.commit_id),
                    Some(two_revision.commit_event_idx),
                    &two_revision.blob_id,
                    two_revision.size,
                    version_set::ORDER_KEY_GAP * 2,
                    2,
                ),
            ],
        )
        .await;

        let findings = repository.fsck().await.unwrap();
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("named-version operation")
                    && finding.contains("is reused")),
            "{findings:?}"
        );
    }
}
