use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use synck_file_folder::file_versions as version_set;
use synck_vcs::types::{
    CommitId, CommitProposal, EventProposal, EventRow, ItemCursor, ItemId, LifecycleState,
    ObjectId, RefEventProposal, RefEventRow,
};
use uuid::Uuid;

use crate::commands::normalize_input_path;
use crate::error::{LocalError, Result};
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

/// A FILE's labels replayed from its ref ledger, with the content revisions
/// they name.
struct VersionSetState {
    ledger: version_set::Ledger,
    created_at: HashMap<String, DateTime<Utc>>,
    revisions: HashMap<i64, FileRevision>,
}

struct FileRevision {
    item_seq: i64,
    repo_seq: i64,
    commit_event_idx: i32,
    blob_id: ObjectId,
    size: i64,
}

impl VersionSetState {
    fn revision(&self, label: &version_set::Label) -> Result<&FileRevision> {
        self.revisions.get(&label.source_item_seq).ok_or_else(|| {
            LocalError::Integrity(format!(
                "named version {} references missing FILE revision {}",
                label.version_id, label.source_item_seq
            ))
        })
    }
}

fn named_version_ref_event(
    file_item_id: &ItemId,
    event_type: &'static str,
    version_id: &str,
    mut metadata: Map<String, Value>,
    base_ref_seq: i64,
) -> Result<RefEventProposal> {
    metadata.insert(
        "version_id".to_string(),
        Value::String(version_id.to_string()),
    );
    Ok(RefEventProposal {
        owner_item_id: file_item_id.clone(),
        event_type: event_type.to_string(),
        set_metadata: Some(
            synck_vcs::metadata::MetadataRef::new(Value::Object(metadata))
                .map_err(|e| LocalError::Invalid(e.to_string()))?,
        ),
        set_content: None,
        base_ref_seq,
    })
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
        let live = || state.ledger.labels.values().filter(|label| label.live);
        if live().any(|label| label.source_item_seq == revision.item_seq) {
            return Err(LocalError::Invalid(format!(
                "file revision {} already has a live named version",
                revision.item_seq
            )));
        }
        let kind = version_set::normalize_kind(Some(kind)).map_err(LocalError::Invalid)?;
        let order_key = live().map(|label| label.order_key).max().map_or(
            Ok(version_set::ORDER_KEY_GAP),
            |value| {
                value
                    .checked_add(version_set::ORDER_KEY_GAP)
                    .ok_or_else(|| {
                        LocalError::Invalid("file version ordering is exhausted".to_string())
                    })
            },
        )?;
        let version_id = Uuid::now_v7().to_string();
        let mut metadata = Map::new();
        metadata.insert("source_item_seq".to_string(), revision.item_seq.into());
        metadata.insert("kind".to_string(), kind.into());
        metadata.insert(
            "label_source".to_string(),
            version_set::LABEL_SOURCE_INTERNAL.into(),
        );
        metadata.insert("order_key".to_string(), order_key.into());
        if let Some(description) = normalize_description(description) {
            metadata.insert("description".to_string(), description.into());
        }
        let event = named_version_ref_event(
            &file_item_id,
            version_set::VERSION_CREATE_EVENT,
            &version_id,
            metadata,
            state.ledger.head_seq,
        )?;
        self.append_version_commit(&file_item_id, message, event)
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
            version_set::VERSION_SET_DESCRIPTION_EVENT,
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
            version_set::VERSION_SET_KIND_EVENT,
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
            version_set::VERSION_TOMBSTONE_EVENT,
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
        self.pull_if_behind().await?;
        let (display, file_item_id) = self.resolve_live_file(caller, input).await?;
        let state = self.load_version_set(&file_item_id).await?;
        let revision = state.revision(require_live_version(&state, version_id)?)?;
        self.guard_agent_paths(&[&display])?;
        self.write_working_file(&display, &revision.blob_id, revision.size, true)?;
        crate::commands::sync_display_parent(self.root_path(), &display)?;
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
            version_set::VERSION_SET_DESCRIPTION_EVENT => {
                let requested = metadata
                    .get("description")
                    .and_then(Value::as_str)
                    .and_then(|value| normalize_description(Some(value)));
                existing.description == requested
            }
            version_set::VERSION_SET_KIND_EVENT => metadata
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
        let event = named_version_ref_event(
            &file_item_id,
            event_type,
            &existing.version_id,
            metadata,
            state.ledger.head_seq,
        )?;
        self.append_version_commit(&file_item_id, message, event)
            .await
    }

    async fn append_version_commit(
        &self,
        file_item_id: &ItemId,
        message: &str,
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
                    operation_id: Some(Uuid::new_v4().to_string()),
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
        for item in items
            .iter()
            .filter(|item| item.item_type == "FILE" && item.ref_head_seq > 0)
        {
            if let Err(error) = self.load_version_set(&item.item_id).await {
                findings.push(error.to_string());
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
        let mut state = VersionSetState {
            ledger: version_set::Ledger::default(),
            created_at: HashMap::new(),
            revisions: self
                .file_revisions(file_item_id)
                .await?
                .into_iter()
                .map(|revision| (revision.item_seq, revision))
                .collect(),
        };
        // Every ref event of one commit has the head before that commit as
        // its base.
        let mut commit: Option<(CommitId, i64)> = None;
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
                if commit.as_ref().is_none_or(|(id, _)| id != &event.commit_id) {
                    commit = Some((event.commit_id.clone(), state.ledger.head_seq));
                }
                let base = commit.as_ref().expect("set above").1;
                apply_ref_version_event(file_item_id, &mut state, base, event)?;
                from_ref_seq = event.ref_seq + 1;
            }
            if events.len() < PAGE_SIZE {
                break;
            }
        }
        if owner.ref_head_seq > 0 && state.ledger.head_seq != owner.ref_head_seq {
            return Err(LocalError::Integrity(format!(
                "FILE {file_item_id} ref replay head {} differs from item ref head {}",
                state.ledger.head_seq, owner.ref_head_seq
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
                if synck_file_folder::is_file_content_event(&event.event_type) {
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

/// Applies one ref event of a FILE's ledger, whose commit's events share the
/// base `base_ref_seq`.
fn apply_ref_version_event(
    file_item_id: &ItemId,
    state: &mut VersionSetState,
    base_ref_seq: i64,
    event: &RefEventRow,
) -> Result<()> {
    let proposal = decode_ref_version_event(file_item_id, event)?;
    if proposal.set_content.is_some() {
        return Err(LocalError::Integrity(
            "named-version refs cannot hold content objects".into(),
        ));
    }
    if event.ref_seq != state.ledger.head_seq + 1 {
        return Err(LocalError::Integrity(
            "noncontiguous FILE ref ledger".into(),
        ));
    }
    if proposal.base_ref_seq != base_ref_seq {
        return Err(LocalError::Integrity(
            "invalid FILE ref base sequence".into(),
        ));
    }
    let metadata = proposal
        .set_metadata
        .as_ref()
        .and_then(synck_vcs::metadata::MetadataRef::value)
        .and_then(Value::as_object)
        .ok_or_else(|| LocalError::Integrity(format!("{} has no metadata", event.event_type)))?;
    let event_repo_seq = event.repo_seq.ok_or_else(|| {
        LocalError::Integrity(format!(
            "named-version event {} has no repository sequence",
            event.ref_seq
        ))
    })?;
    let revisions = &state.revisions;
    state
        .ledger
        .apply(&event.event_type, metadata, |source| {
            let revision = revisions.get(&source.item_seq).ok_or_else(|| {
                format!(
                    "named version {} references missing FILE revision {}",
                    source.version_id, source.item_seq
                )
            })?;
            if (revision.repo_seq, revision.commit_event_idx)
                >= (event_repo_seq, event.commit_event_idx)
            {
                return Err(format!(
                    "named version {} references FILE revision {} before it exists",
                    source.version_id, source.item_seq
                ));
            }
            Ok(())
        })
        .map_err(LocalError::Integrity)?;
    if event.event_type == version_set::VERSION_CREATE_EVENT
        && let Some(version_id) = metadata.get("version_id").and_then(Value::as_str)
    {
        state
            .created_at
            .insert(version_id.to_string(), event.created_at);
    }
    Ok(())
}

fn decode_ref_version_event(
    file_item_id: &ItemId,
    event: &RefEventRow,
) -> Result<RefEventProposal> {
    let proposal: RefEventProposal = event.proposal().map_err(|error| {
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
    Ok(proposal)
}

fn version_entries(
    state: &VersionSetState,
    include_tombstoned: bool,
) -> Result<Vec<NamedVersionEntry>> {
    let mut labels = state.ledger.labels.values().collect::<Vec<_>>();
    labels.sort_by(|left, right| {
        (left.order_key, left.created_ref_seq, &left.version_id).cmp(&(
            right.order_key,
            right.created_ref_seq,
            &right.version_id,
        ))
    });
    let (mut major, mut minor) = (0, 0);
    let mut output = Vec::new();
    for label in labels {
        let name = if label.live {
            version_set::advance_label(&label.kind, &mut major, &mut minor)
                .map_err(LocalError::Integrity)?;
            Some(format!("{major}.{minor:02}"))
        } else {
            None
        };
        if label.live || include_tombstoned {
            let revision = state.revision(label)?;
            output.push(NamedVersionEntry {
                label: name,
                version_id: label.version_id.clone(),
                lifecycle_state: if label.live { "LIVE" } else { "TOMBSTONED" }.to_string(),
                kind: label.kind.clone(),
                label_source: label.label_source.clone(),
                description: label.description.clone(),
                source_item_seq: label.source_item_seq,
                blob_id: revision.blob_id.to_string(),
                size: revision.size,
                version_set_seq: label.current_ref_seq,
                created_at: state.created_at[&label.version_id],
            });
        }
    }
    Ok(output)
}

fn require_live_version<'a>(
    state: &'a VersionSetState,
    version_id: &str,
) -> Result<&'a version_set::Label> {
    let parsed = Uuid::parse_str(version_id)
        .map_err(|error| LocalError::Invalid(format!("invalid version ID: {error}")))?;
    let canonical = parsed.to_string();
    let label =
        state.ledger.labels.get(&canonical).ok_or_else(|| {
            LocalError::Invalid(format!("named version does not exist: {canonical}"))
        })?;
    if !label.live {
        return Err(LocalError::Invalid(format!(
            "named version is tombstoned: {canonical}"
        )));
    }
    Ok(label)
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
    event.proposal().map_err(|error| {
        LocalError::Integrity(format!(
            "invalid event payload at item sequence {}: {error}",
            event.item_seq
        ))
    })
}

fn event_content(event: &EventProposal) -> Result<(ObjectId, i64)> {
    let blob_id = match event
        .set_content
        .as_ref()
        .and_then(|content| content.only("bytes"))
    {
        Some(blob_id) => blob_id.clone(),
        _ => {
            return Err(LocalError::Integrity(
                "version content event has no blob".to_string(),
            ));
        }
    };
    let size = event
        .set_metadata
        .as_ref()
        .and_then(synck_vcs::metadata::MetadataRef::value)
        .and_then(|value| value.pointer("/content/size"))
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or_else(|| LocalError::Integrity("version content has invalid size".to_string()))?;
    Ok((blob_id, size))
}

fn normalize_description(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
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
    fn ref_replay_rejects_row_identity_and_payload_mismatches() {
        let repo_id = synck_vcs::types::RepoId::new();
        let file_item_id = ItemId::new();
        let proposal = named_version_ref_event(
            &file_item_id,
            "FILE_VERSION::TOMBSTONE",
            &Uuid::now_v7().to_string(),
            Map::new(),
            0,
        )
        .unwrap();
        let payload = synck_vcs::types::canonical_ref_event_proposal_bytes(&proposal).unwrap();
        let mut row = RefEventRow {
            metadata: proposal
                .set_metadata
                .as_ref()
                .and_then(|value| value.payload())
                .cloned(),
            repo_id,
            owner_item_id: file_item_id.clone(),
            ref_seq: 1,
            event_type: proposal.event_type.clone(),
            commit_id: CommitId::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
            commit_event_idx: 0,
            repo_seq: Some(1),
            payload,
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
        row.payload.push(0);
        assert!(
            decode_ref_version_event(&file_item_id, &row)
                .unwrap_err()
                .to_string()
                .contains("invalid ref-event payload")
        );
    }

    fn create_event(
        file_item_id: &ItemId,
        version_id: &str,
        source_item_seq: i64,
        order_key: i64,
        next_ref_seq: i64,
    ) -> RefEventProposal {
        let mut metadata = Map::new();
        metadata.insert("source_item_seq".to_string(), source_item_seq.into());
        metadata.insert("kind".to_string(), version_set::KIND_MINOR.into());
        metadata.insert(
            "label_source".to_string(),
            version_set::LABEL_SOURCE_INTERNAL.into(),
        );
        metadata.insert("order_key".to_string(), order_key.into());
        named_version_ref_event(
            file_item_id,
            "FILE_VERSION::CREATE",
            version_id,
            metadata,
            next_ref_seq - 1,
        )
        .unwrap()
    }

    fn replace_event(
        file_item_id: &ItemId,
        version_id: &str,
        source: &FileRevision,
        next_ref_seq: i64,
    ) -> RefEventProposal {
        let mut metadata = Map::new();
        metadata.insert("source_item_seq".to_string(), source.item_seq.into());
        named_version_ref_event(
            file_item_id,
            "FILE_VERSION::SET_REVISION",
            version_id,
            metadata,
            next_ref_seq - 1,
        )
        .unwrap()
    }

    fn tombstone_event(
        file_item_id: &ItemId,
        version_id: &str,
        next_ref_seq: i64,
    ) -> RefEventProposal {
        named_version_ref_event(
            file_item_id,
            "FILE_VERSION::TOMBSTONE",
            version_id,
            Map::new(),
            next_ref_seq - 1,
        )
        .unwrap()
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
    async fn fsck_rejects_a_named_version_that_forward_references_a_file_revision() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let repository = repository_with_file(root).await;
        let file_item_id = repository.committed_tree().await.unwrap()["file.txt"]
            .item_id
            .clone();
        fs::write(root.join("file.txt"), b"later").unwrap();
        let digest = hex::encode(Sha256::digest(b"later"));
        repository
            .install_object(Path::new("file.txt"), "file.txt", &digest, 5)
            .unwrap();
        let version_id = Uuid::now_v7().to_string();
        append(
            &repository,
            "forward reference",
            vec![create_event(
                &file_item_id,
                &version_id,
                2,
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
                &file_item_id,
                &version_id,
                second.item_seq,
                version_set::ORDER_KEY_GAP * 2,
                2,
            )],
        )
        .await;
        append(
            &repository,
            "duplicate source",
            vec![replace_event(&file_item_id, &version_id, first, 3)],
        )
        .await;
        append(
            &repository,
            "hide duplicate",
            vec![tombstone_event(&file_item_id, &initial_version_id, 4)],
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
}
