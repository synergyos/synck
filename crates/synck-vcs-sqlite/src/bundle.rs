//! Transactional installation of existing canonical bundles into an empty repository.
//!
//! Import preserves source history and sequencing modes. It neither grants writer
//! authority nor supplies referenced content bytes or application projections.
use std::io::BufRead;
use std::sync::Arc;

use rusqlite::params;
use synck_vcs::bundle::{
    BundleCommitRecord, BundleHeader, BundleRecord, BundleSchema, ImportFailure,
    bounded_commit_payload_bytes, parse_rfc3339_utc, read_header, read_record,
    validate_bundle_event_payload_json_and_digest,
    validate_bundle_ref_event_payload_json_and_digest, validate_import_repo_seq,
};
use synck_vcs::config::CoreConfig;
use synck_vcs::repo::Repo;
use synck_vcs::state::catch_up_state_index;
use synck_vcs::storage::{SealedCommitInsert, Storage, StorageBackend, StorageTxn};
use synck_vcs::types::{
    CommitId, CommitPk, EventInsertRow, ItemId, ItemRow, RefEventInsertRow, RepoId, Sha256Digest,
    Timestamp,
};
use synck_vcs::verify::{
    SemanticVerifier, VerifyCommit, VerifyCommitEvent, VerifyEvent, VerifyItem, VerifyRefEvent,
};

use crate::storage::SqliteTxn;
use crate::{SqliteStorage, format_timestamp};

#[derive(Debug)]
pub struct BundleImportReport {
    pub repo_id: RepoId,
    /// Physical source-schema provenance; the target retains its SQLite schema.
    pub source_schema: BundleSchema,
    pub terminal_repo_seq: i64,
    pub commits: usize,
    pub items: usize,
}

enum PendingEvent {
    Item(VerifyEvent, Timestamp),
    Ref(VerifyRefEvent, Timestamp),
}

impl PendingEvent {
    fn borrowed(&self) -> VerifyCommitEvent<'_> {
        match self {
            Self::Item(event, _) => VerifyCommitEvent::Item(event),
            Self::Ref(event, _) => VerifyCommitEvent::Ref(event),
        }
    }
}

struct PendingCommit {
    checkpoint: Sha256Digest,
    row: SealedCommitInsert,
    events: Vec<PendingEvent>,
    payload_bytes: usize,
}

impl PendingCommit {
    fn new(record: BundleCommitRecord) -> Result<Self, String> {
        if record.event_count < 1 {
            return Err("commit event_count exceeds supported canonical bounds".into());
        }
        Ok(Self {
            checkpoint: record.checkpoint,
            row: SealedCommitInsert {
                // The bundle has no physical commit_pk. Allocate a target surrogate;
                // canonical commit/item/ref identities and sequences are unchanged.
                commit_pk: CommitPk::new(),
                repo_seq: Some(record.repo_seq),
                operation_id: record.operation_id,
                format_version: record.format_version,
                import_job_id: record.import_job_id,
                author_id: record.author_id,
                committer_id: record.committer_id,
                message: record.message,
                occurred_at: record
                    .occurred_at
                    .as_deref()
                    .map(parse_rfc3339_utc)
                    .transpose()?,
                created_at: parse_rfc3339_utc(&record.created_at)?,
                commit_id: CommitId::parse(record.commit_id).map_err(|error| error.to_string())?,
                events_digest: Sha256Digest::parse(record.events_digest)
                    .map_err(|error| error.to_string())?,
                event_count: record.event_count,
            },
            events: Vec::new(),
            payload_bytes: 0,
        })
    }

    fn check_event(
        &mut self,
        commit_id: &str,
        repo_seq: i64,
        index: i32,
        bytes: usize,
    ) -> Result<(), String> {
        if self.row.commit_id.as_str() != commit_id
            || self.row.repo_seq != Some(repo_seq)
            || usize::try_from(index).ok() != Some(self.events.len())
        {
            return Err("bundle event commit identity/index mismatch".into());
        }
        self.payload_bytes = bounded_commit_payload_bytes(self.payload_bytes, bytes)?;
        Ok(())
    }

    async fn install(
        self,
        txn: &mut SqliteTxn,
        repo_id: &RepoId,
        verifier: &mut SemanticVerifier,
    ) -> Result<(), String> {
        let row = self.row;
        let commit = VerifyCommit {
            commit_id: Some(row.commit_id.clone()),
            repo_seq: row.repo_seq,
            format_version: row.format_version,
            import_job_id: row.import_job_id.clone(),
            committer_id: row.committer_id.clone(),
            occurred_at: row.occurred_at,
            author_id: row.author_id.clone(),
            message: row.message.clone(),
            event_count: Some(row.event_count),
            events_digest: Some(row.events_digest.clone()),
        };
        let findings =
            verifier.verify_commit_events(&commit, self.events.iter().map(PendingEvent::borrowed));
        if let Some(finding) = findings.into_iter().next() {
            return Err(finding);
        }
        txn.insert_commits_sealed(repo_id, std::slice::from_ref(&row))
            .await
            .map_err(detail)?;
        let published: Vec<u8> = txn
            .conn()
            .map_err(detail)?
            .query_row(
                "SELECT checkpoint FROM vcs_checkpoints WHERE repo_id=?1 AND repo_seq=?2",
                params![repo_id.as_str(), row.repo_seq],
                |row| row.get(0),
            )
            .map_err(detail)?;
        if published != synck_vcs::types::sha256_digest_to_digest_bytes(&self.checkpoint) {
            return Err("bundle checkpoint mismatch".into());
        }
        for event in self.events {
            match event {
                PendingEvent::Item(event, created_at) => txn
                    .append_event(&EventInsertRow {
                        repo_id: repo_id.clone(),
                        commit_pk: row.commit_pk.clone(),
                        commit_event_idx: event.commit_event_idx,
                        item_id: event.item_id,
                        item_seq: event.item_seq,
                        event_type: event.event_type,
                        payload: event.payload,
                        metadata: None,
                        event_payload_digest: event
                            .event_payload_digest
                            .try_into()
                            .expect("validated SHA-256"),
                        created_at,
                        parent_folder_id: event.parent_folder_id,
                        parent_folder_id_before: event.parent_folder_id_before,
                    })
                    .await
                    .map_err(detail)?,
                PendingEvent::Ref(event, created_at) => txn
                    .append_ref_event(&RefEventInsertRow {
                        repo_id: repo_id.clone(),
                        commit_pk: row.commit_pk.clone(),
                        commit_event_idx: event.commit_event_idx,
                        owner_item_id: event.owner_item_id,
                        ref_seq: event.ref_seq,
                        event_type: event.event_type,
                        payload: event.payload,
                        metadata: None,
                        event_payload_digest: event
                            .event_payload_digest
                            .try_into()
                            .expect("validated SHA-256"),
                        created_at,
                    })
                    .await
                    .map_err(detail)?,
            }
        }
        Ok(())
    }
}

fn detail(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn item_id(value: String) -> Result<ItemId, String> {
    ItemId::parse(value).map_err(detail)
}
fn optional_item_id(value: Option<String>) -> Result<Option<ItemId>, String> {
    value.map(item_id).transpose()
}

impl SqliteStorage {
    /// Install a canonical bundle into an empty target repository and build its
    /// historical indexes. The caller must hold the target's exclusive repository
    /// lock until completion, including historical catch-up.
    ///
    /// Every pre-commit failure rolls back all imported rows; an initialized empty
    /// schema may remain and can be retried. A nonempty target is always refused,
    /// including an exact repeat: this API never overwrites or merges history.
    /// `AcceptedStateIncomplete` means canonical data committed; resume only
    /// `project_imported_history`, not this import. Commit failures are reported
    /// conservatively as `CommitOutcomeUnknown` and require inspection.
    pub async fn import_bundle(
        &self,
        reader: &mut dyn BufRead,
    ) -> Result<BundleImportReport, ImportFailure> {
        let header = read_header(reader).map_err(ImportFailure::BeforeCommit)?;
        self.init_schema()
            .await
            .map_err(|error| ImportFailure::BeforeCommit(detail(error)))?;
        let mut txn = self
            .begin_sqlite_txn()
            .map_err(|error| ImportFailure::BeforeCommit(detail(error)))?;
        let report = install(&mut txn, &header, reader)
            .await
            .map_err(ImportFailure::BeforeCommit)?;
        Box::new(txn)
            .commit()
            .await
            .map_err(|error| ImportFailure::CommitOutcomeUnknown {
                repo_id: header.repo_id().clone(),
                detail: detail(error),
            })?;
        self.project_imported_history(header.repo_id()).await?;
        Ok(report)
    }

    /// Resume derived historical indexing after a committed import. Canonical
    /// rows are not changed. The caller must hold the exclusive repository lock.
    pub async fn project_imported_history(&self, repo_id: &RepoId) -> Result<(), ImportFailure> {
        let incomplete = |error| ImportFailure::AcceptedStateIncomplete {
            repo_id: repo_id.clone(),
            detail: error,
        };
        let mut state = self
            .state_index_store(repo_id.clone())
            .map_err(|error| incomplete(detail(error)))?;
        let core = Repo::new(Storage::new(Arc::new(self.clone())), CoreConfig::default());
        catch_up_state_index(&core, repo_id, &mut state, None)
            .await
            .map_err(|error| incomplete(detail(error)))?;
        Ok(())
    }
}

async fn install(
    txn: &mut SqliteTxn,
    header: &BundleHeader,
    reader: &mut dyn BufRead,
) -> Result<BundleImportReport, String> {
    let repo_id = header.repo_id();
    for table in [
        "vcs_metadata_objects",
        "vcs_commits",
        "vcs_checkpoints",
        "vcs_events",
        "vcs_ref_events",
        "vcs_items",
        "vcs_federation_domains",
        "vcs_repo_activity_heads",
    ] {
        let occupied: bool = txn
            .conn()
            .map_err(detail)?
            .query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE repo_id=?1)"),
                [repo_id.as_str()],
                |row| row.get(0),
            )
            .map_err(detail)?;
        if occupied {
            return Err(format!("bundle target repository is not empty: {table}"));
        }
    }
    txn.conn().map_err(detail)?.execute(
        "INSERT INTO vcs_history_generations(repo_id,generation) VALUES (?1,?2) ON CONFLICT(repo_id) DO UPDATE SET generation=excluded.generation",
        params![repo_id.as_str(), header.history_generation()],
    ).map_err(detail)?;
    let mut verifier = SemanticVerifier::new();
    let mut pending: Option<PendingCommit> = None;
    let mut last_repo_seq = None;
    let mut items = Vec::new();
    let mut commits = 0;
    let mut items_started = false;
    let mut domains_started = false;
    let mut metadata_ids = std::collections::HashSet::new();
    while let Some(record) = read_record(reader, header.bundle_format_version())? {
        match record {
            BundleRecord::Metadata(record) => {
                if last_repo_seq.is_some() || items_started || domains_started || pending.is_some() {
                    return Err("metadata objects must precede history".into());
                }
                if !metadata_ids.insert(record.object_id.clone()) {
                    return Err("duplicate metadata object".into());
                }
                if record.payload_hex.len() > 2 * (synck_vcs::object::HEADER_BYTES + synck_vcs::metadata::MAX_METADATA_BYTES)
                    || !record.payload_hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) {
                    return Err("invalid metadata payload hex".into());
                }
                let bytes = hex::decode(record.payload_hex).map_err(detail)?;
                let metadata = synck_vcs::metadata::Metadata::verify(&record.object_id, bytes).map_err(detail)?;
                crate::metadata::insert(txn.conn().map_err(detail)?, repo_id, &metadata).map_err(detail)?;
            }
            BundleRecord::Commit(record) => {
                if items_started || domains_started || pending.is_some() {
                    return Err("commit after items/domains or incomplete commit".into());
                }
                last_repo_seq = Some(validate_import_repo_seq(last_repo_seq, record.repo_seq)?);
                pending = Some(PendingCommit::new(record)?);
            }
            BundleRecord::Event(record) => {
                let current = pending.as_mut().ok_or("event without preceding commit")?;
                current.check_event(
                    &record.commit_id,
                    record.repo_seq,
                    record.commit_event_idx,
                    record.payload_json.len(),
                )?;
                let hash = validate_bundle_event_payload_json_and_digest(
                    &record.payload_json,
                    &record.event_payload_digest,
                )?;
                current.events.push(PendingEvent::Item(
                    VerifyEvent {
                        commit_event_idx: record.commit_event_idx,
                        item_id: item_id(record.item_id)?,
                        item_seq: record.item_seq,
                        event_type: record.event_type,
                        payload: record.payload_json.into_bytes(),
                        event_payload_digest: hash.to_vec(),
                        parent_folder_id: optional_item_id(record.parent_folder_id)?,
                        parent_folder_id_before: optional_item_id(record.parent_folder_id_before)?,
                    },
                    parse_rfc3339_utc(&record.created_at)?,
                ));
            }
            BundleRecord::RefEvent(record) => {
                let current = pending
                    .as_mut()
                    .ok_or("ref event without preceding commit")?;
                current.check_event(
                    &record.commit_id,
                    record.repo_seq,
                    record.commit_event_idx,
                    record.payload_json.len(),
                )?;
                let hash = validate_bundle_ref_event_payload_json_and_digest(
                    &record.payload_json,
                    &record.event_payload_digest,
                )?;
                current.events.push(PendingEvent::Ref(
                    VerifyRefEvent {
                        commit_event_idx: record.commit_event_idx,
                        owner_item_id: item_id(record.owner_item_id)?,
                        ref_seq: record.ref_seq,
                        event_type: record.event_type,
                        payload: record.payload_json.into_bytes(),
                        event_payload_digest: hash.to_vec(),
                    },
                    parse_rfc3339_utc(&record.created_at)?,
                ));
            }
            BundleRecord::Item(record) => {
                if domains_started || pending.is_some() {
                    return Err("item after domains or incomplete commit".into());
                }
                items_started = true;
                let item = ItemRow {
                    repo_id: repo_id.clone(),
                    item_id: item_id(record.item_id)?,
                    item_type: record.item_type,
                    parent_folder_id: optional_item_id(record.parent_folder_id)?,
                    name: record.name,
                    name_key: record.name_key,
                    head_seq: record.head_seq,
                    ref_head_seq: record.ref_head_seq,
                    lifecycle_state: serde_json::from_value(serde_json::json!(
                        record.lifecycle_state
                    ))
                    .map_err(detail)?,
                    sequencing_mode: serde_json::from_value(serde_json::json!(
                        record.sequencing_mode
                    ))
                    .map_err(detail)?,
                    created_at: parse_rfc3339_utc(&record.created_at)?,
                    created_by: record.created_by,
                    federation_stream_id: record.federation_stream_id,
                };
                if txn
                    .get_item(repo_id, &item.item_id)
                    .await
                    .map_err(detail)?
                    .is_some()
                {
                    return Err("duplicate item record".into());
                }
                txn.put_item(&item).await.map_err(detail)?;
                items.push(VerifyItem {
                    item_id: item.item_id,
                    item_type: item.item_type,
                    parent_folder_id: item.parent_folder_id,
                    name: item.name,
                    name_key: item.name_key,
                    head_seq: item.head_seq,
                    ref_head_seq: item.ref_head_seq,
                    lifecycle_state: item.lifecycle_state,
                    sequencing_mode: item.sequencing_mode,
                    federation_stream_id: item.federation_stream_id,
                });
            }
            BundleRecord::FederationDomain(record) => {
                if pending.is_some() {
                    return Err("domain after incomplete commit".into());
                }
                domains_started = true;
                txn.conn().map_err(detail)?.execute(
                    "INSERT INTO vcs_federation_domains (repo_id,domain_id,last_domain_seq,created_at,updated_at) VALUES (?1,?2,?3,?4,?5)",
                    params![repo_id.as_str(), record.domain_id, record.last_domain_seq,
                        format_timestamp(parse_rfc3339_utc(&record.created_at)?),
                        format_timestamp(parse_rfc3339_utc(&record.updated_at)?)],
                ).map_err(detail)?;
            }
        }
        if pending
            .as_ref()
            .is_some_and(|current| current.events.len() == current.row.event_count as usize)
        {
            pending
                .take()
                .expect("complete commit exists")
                .install(txn, repo_id, &mut verifier)
                .await?;
            commits += 1;
        }
    }
    if pending.is_some() {
        return Err("bundle ended with an incomplete commit".into());
    }
    let item_count = items.len();
    if let Some(finding) = verifier.finish(&items).into_iter().next() {
        return Err(finding);
    }
    let terminal_repo_seq = last_repo_seq.unwrap_or(0);
    let next_repo_seq = terminal_repo_seq
        .checked_add(1)
        .ok_or("repository sequence exhausted")?;
    txn.conn().map_err(detail)?.execute(
        "INSERT INTO vcs_repo_activity_heads (repo_id,next_repo_seq,updated_at) VALUES (?1,?2,?3)",
        params![repo_id.as_str(), next_repo_seq, format_timestamp(chrono::Utc::now())],
    ).map_err(detail)?;
    Ok(BundleImportReport {
        repo_id: repo_id.clone(),
        source_schema: BundleSchema {
            subsystem: header.source_schema().subsystem.clone(),
            schema_id: header.source_schema().schema_id.clone(),
        },
        terminal_repo_seq,
        commits,
        items: item_count,
    })
}
