use std::sync::Arc;

use base64::Engine as _;
use synck_vcs::config::CoreConfig;
use synck_vcs::errors::StorageError;
use synck_vcs::repo::Repo;
use synck_vcs::state::{StateIndexStore, catch_up_state_index_bounded, initialize_state_index};
use synck_vcs::storage::{SealedCommitInsert, Storage, StorageBackend};
use synck_vcs::types::{
    CommitEventRow, EventInsertRow, RefEventInsertRow, RepoId, sha256_digest_to_digest_bytes,
};
use synck_vcs::verify::{
    SemanticVerifier, VerifyCommit, VerifyCommitEvent, VerifyEvent, VerifyRefEvent,
};
use synck_vcs_sqlite::SqliteStorage;

use crate::{ReplicaCheckpoint, ReplicaCommitGroup, ReplicaError, validate_group};

pub struct SqliteReplica {
    storage: SqliteStorage,
    repo_id: RepoId,
}

impl SqliteReplica {
    pub async fn initialize(
        path: impl Into<std::path::PathBuf>,
        repo_id: RepoId,
    ) -> Result<Self, ReplicaError> {
        let storage = SqliteStorage::new(path);
        storage.init_schema().await.map_err(storage_error)?;
        let mut state = storage
            .state_index_store(repo_id.clone())
            .map_err(|error| storage_error(error.to_string()))?;
        initialize_state_index(&mut state, &repo_id)
            .await
            .map_err(|error| storage_error(error.to_string()))?;
        Ok(Self { storage, repo_id })
    }

    pub fn storage(&self) -> &SqliteStorage {
        &self.storage
    }

    pub async fn install_group(&self, group: &ReplicaCommitGroup) -> Result<bool, ReplicaError> {
        validate_group(group)?;
        let commit = &group.commit;
        let sealed = SealedCommitInsert {
            commit_pk: commit.commit_pk.clone(),
            repo_seq: Some(commit.repo_seq),
            parent_commit_id: commit.parent_commit_id.clone(),
            operation_id: commit.operation_id.clone(),
            format_version: commit.format_version,
            import_job_id: commit.import_job_id.clone(),
            author_id: commit.author_id.clone(),
            committer_id: commit.committer_id.clone(),
            message: commit.message.clone(),
            occurred_at: commit.occurred_at,
            created_at: commit.created_at,
            commit_id: commit.commit_id.clone(),
            events_digest: commit.events_digest.clone(),
            event_count: commit.event_count,
        };
        let mut events = Vec::new();
        let mut ref_events = Vec::new();
        for event in &group.events {
            match event {
                crate::ReplicaEvent::Item {
                    commit_event_idx,
                    item_id,
                    item_seq,
                    event_type,
                    payload_base64,
                    event_payload_digest,
                    created_at,
                    parent_folder_id,
                    parent_folder_id_before,
                } => events.push(EventInsertRow {
                    repo_id: self.repo_id.clone(),
                    commit_pk: commit.commit_pk.clone(),
                    commit_event_idx: *commit_event_idx,
                    item_id: item_id.clone(),
                    item_seq: *item_seq,
                    event_type: event_type.clone(),
                    payload: base64::engine::general_purpose::STANDARD
                        .decode(payload_base64)
                        .expect("validated base64 remains decodable"),
                    event_payload_digest: sha256_digest_to_digest_bytes(event_payload_digest),
                    created_at: *created_at,
                    parent_folder_id: parent_folder_id.clone(),
                    parent_folder_id_before: parent_folder_id_before.clone(),
                }),
                crate::ReplicaEvent::Ref {
                    commit_event_idx,
                    owner_item_id,
                    ref_seq,
                    event_type,
                    payload_base64,
                    event_payload_digest,
                    created_at,
                } => ref_events.push(RefEventInsertRow {
                    repo_id: self.repo_id.clone(),
                    commit_pk: commit.commit_pk.clone(),
                    commit_event_idx: *commit_event_idx,
                    owner_item_id: owner_item_id.clone(),
                    ref_seq: *ref_seq,
                    event_type: event_type.clone(),
                    payload: base64::engine::general_purpose::STANDARD
                        .decode(payload_base64)
                        .expect("validated base64 remains decodable"),
                    event_payload_digest: sha256_digest_to_digest_bytes(event_payload_digest),
                    created_at: *created_at,
                }),
            }
        }
        self.storage
            .install_sealed_history_commit(&self.repo_id, &sealed, &events, &ref_events)
            .await
            .map_err(|error| match error {
                StorageError::Other { detail }
                    if detail.starts_with("sealed history collision for commit ") =>
                {
                    ReplicaError::Corrupt(detail)
                }
                error => storage_error(error),
            })
    }

    pub async fn project_through(&self, target_repo_seq: i64) -> Result<(), ReplicaError> {
        let source = Repo::new(
            Storage::new(Arc::new(self.storage.clone())),
            CoreConfig::default(),
        );
        let mut state = self
            .storage
            .state_index_store(self.repo_id.clone())
            .map_err(|error| storage_error(error.to_string()))?;
        catch_up_state_index_bounded(
            &source,
            &self.repo_id,
            &mut state,
            Some(target_repo_seq),
            None,
        )
        .await
        .map_err(|error| storage_error(error.to_string()))?;
        Ok(())
    }

    pub async fn verify_checkpoint(
        &self,
        checkpoint: &ReplicaCheckpoint,
    ) -> Result<(), ReplicaError> {
        checkpoint.validate()?;
        if checkpoint.repo_id != self.repo_id {
            return Err(ReplicaError::Malformed(
                "checkpoint repository does not match SQLite replica".to_string(),
            ));
        }
        let mut state = self
            .storage
            .state_index_store(self.repo_id.clone())
            .map_err(|error| storage_error(error.to_string()))?;
        let head = state
            .projection_head()
            .await
            .map_err(|error| storage_error(error.to_string()))?
            .ok_or_else(|| storage_error("missing state projection head"))?;
        if head.applied_repo_seq != checkpoint.terminal_repo_seq
            || head.state_root.to_string() != checkpoint.state_root.as_str()
        {
            return Err(ReplicaError::Malformed(
                "SQLite state head does not match checkpoint".to_string(),
            ));
        }
        let source = Repo::new(
            Storage::new(Arc::new(self.storage.clone())),
            CoreConfig::default(),
        );
        let terminal = source
            .list_commits_by_repo_seq(
                self.repo_id.clone(),
                checkpoint.terminal_repo_seq,
                Some(checkpoint.terminal_repo_seq),
                2,
            )
            .await
            .map_err(storage_error)?;
        match (checkpoint.terminal_commit_id.as_ref(), terminal.as_slice()) {
            (None, []) if checkpoint.terminal_repo_seq == 0 => {}
            (Some(expected), [actual]) if &actual.commit_id == expected => {}
            _ => {
                return Err(ReplicaError::Malformed(
                    "SQLite terminal commit does not match checkpoint".to_string(),
                ));
            }
        }
        if checkpoint.terminal_repo_seq < i64::MAX {
            let later = source
                .list_commits_by_repo_seq(
                    self.repo_id.clone(),
                    checkpoint.terminal_repo_seq + 1,
                    None,
                    1,
                )
                .await
                .map_err(storage_error)?;
            if !later.is_empty() {
                return Err(ReplicaError::Corrupt(
                    "SQLite replica contains history beyond checkpoint".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Verify canonical history and its reconstructed Core item graph.
    ///
    /// This is intentionally a publication/startup check, not a per-page check.
    /// Incoming pages are validated before installation; rescanning all retained
    /// history after every page would turn catch-up into quadratic work.
    pub async fn verify_history(&self) -> Result<(), ReplicaError> {
        const PAGE: usize = 1_000;

        let source = Repo::new(
            Storage::new(Arc::new(self.storage.clone())),
            CoreConfig::default(),
        );
        let unsealed: i64 = self
            .storage
            .open_readonly()
            .map_err(storage_error)?
            .query_row(
                "SELECT COUNT(*) FROM vcs_commits WHERE repo_id=?1 AND (commit_id IS NULL OR repo_seq IS NULL)",
                [self.repo_id.as_str()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if unsealed != 0 {
            return Err(ReplicaError::Corrupt(
                "stored replica contains unsealed history".to_string(),
            ));
        }
        let mut verifier = SemanticVerifier::new();
        let mut next_repo_seq = 1_i64;
        loop {
            let commits = source
                .list_commits_by_repo_seq(self.repo_id.clone(), next_repo_seq, None, PAGE)
                .await
                .map_err(storage_error)?;
            if commits.is_empty() {
                break;
            }
            for commit in &commits {
                if commit.repo_id != self.repo_id {
                    return Err(ReplicaError::Corrupt(
                        "stored commit repository is inconsistent".to_string(),
                    ));
                }
                let event_limit = usize::try_from(commit.event_count)
                    .ok()
                    .and_then(|count| count.checked_add(1))
                    .ok_or_else(|| {
                        ReplicaError::Corrupt("stored commit event count is invalid".to_string())
                    })?;
                let events = source
                    .list_commit_events(self.repo_id.clone(), commit.commit_id.clone(), event_limit)
                    .await
                    .map_err(storage_error)?;
                let events = events
                    .into_iter()
                    .map(|event| match event {
                        CommitEventRow::Item(event) => OwnedVerifyEvent::Item(VerifyEvent {
                            commit_event_idx: event.commit_event_idx,
                            item_id: event.item_id,
                            item_seq: event.item_seq,
                            event_type: event.event_type,
                            payload: event.payload,
                            event_payload_digest: sha256_digest_to_digest_bytes(
                                &event.event_payload_digest,
                            )
                            .to_vec(),
                            parent_folder_id: event.parent_folder_id,
                            parent_folder_id_before: event.parent_folder_id_before,
                        }),
                        CommitEventRow::Ref(event) => OwnedVerifyEvent::Ref(VerifyRefEvent {
                            commit_event_idx: event.commit_event_idx,
                            owner_item_id: event.owner_item_id,
                            ref_seq: event.ref_seq,
                            event_type: event.event_type,
                            payload: event.payload,
                            event_payload_digest: sha256_digest_to_digest_bytes(
                                &event.event_payload_digest,
                            )
                            .to_vec(),
                        }),
                    })
                    .collect::<Vec<_>>();
                let findings = verifier.verify_commit_events(
                    &VerifyCommit {
                        commit_id: Some(commit.commit_id.clone()),
                        parent_commit_id: commit.parent_commit_id.clone(),
                        repo_seq: commit.repo_seq,
                        format_version: commit.format_version,
                        import_job_id: commit.import_job_id.clone(),
                        author_id: commit.author_id.clone(),
                        message: commit.message.clone(),
                        event_count: Some(commit.event_count),
                        events_digest: Some(commit.events_digest.clone()),
                    },
                    events.iter().map(OwnedVerifyEvent::as_borrowed),
                );
                if let Some(finding) = findings.into_iter().next() {
                    return Err(ReplicaError::Corrupt(finding));
                }
            }
            let last = commits
                .last()
                .and_then(|commit| commit.repo_seq)
                .ok_or_else(|| ReplicaError::Corrupt("stored commit is unsequenced".to_string()))?;
            next_repo_seq = last
                .checked_add(1)
                .ok_or_else(|| ReplicaError::Corrupt("repository sequence overflow".to_string()))?;
            if commits.len() < PAGE {
                break;
            }
        }

        if let Some(finding) = verifier.finish_replayed().into_iter().next() {
            return Err(ReplicaError::Corrupt(finding));
        }
        Ok(())
    }
}

enum OwnedVerifyEvent {
    Item(VerifyEvent),
    Ref(VerifyRefEvent),
}

impl OwnedVerifyEvent {
    fn as_borrowed(&self) -> VerifyCommitEvent<'_> {
        match self {
            Self::Item(event) => VerifyCommitEvent::Item(event),
            Self::Ref(event) => VerifyCommitEvent::Ref(event),
        }
    }
}

fn storage_error(error: impl ToString) -> ReplicaError {
    ReplicaError::Storage(error.to_string())
}
