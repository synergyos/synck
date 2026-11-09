//! Proposal staging (`synck/docs/spec/protocol.md`, "Paged proposal upload"
//! and "Expiry, receipts and restart"). Pages stay in `state.sqlite3` until
//! their commit is accepted or the upload expires. Each upload's streaming
//! summary is kept in memory and rebuilt from its pages after a restart; the
//! submission is verified again from the pages themselves. Expiry is set when
//! an upload starts and never extended.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{TimeDelta, Utc};
use rusqlite::{Connection, OptionalExtension as _, params};
use synck_sync::{
    PageAck, ProposalPage, ProposalRecord, ProposalVerifier, ProtocolError, RepositoryIdentity,
    SubmitRequest, ValidatedProposal,
};
use synck_vcs::types::{RepoId, Sha256Digest, Timestamp, canonical_json_bytes};

use super::state::open_existing;
use super::{Error, Result};

const LIFETIME: TimeDelta = TimeDelta::hours(24);

type Key = (RepoId, String);

pub(crate) struct Uploads {
    state_root: PathBuf,
    /// Held while a page is checked and installed, so pages of one upload
    /// arrive in order.
    verifiers: Mutex<HashMap<Key, (ProposalVerifier, Timestamp)>>,
}

fn canonical(records: &[ProposalRecord]) -> Result<Vec<u8>> {
    let value = serde_json::to_value(records).map_err(|error| Error::Invalid(error.to_string()))?;
    Ok(canonical_json_bytes(&value).map_err(ProtocolError::from)?)
}

fn timestamp(seconds: i64) -> Result<Timestamp> {
    Timestamp::from_timestamp(seconds, 0).ok_or_else(|| Error::State("upload expiry out of range".into()))
}

/// Every page of an upload in order, checked by a fresh verifier.
fn replay(
    conn: &Connection,
    repository: &RepositoryIdentity,
    operation_id: &str,
    expires_at: Timestamp,
    now: Timestamp,
    mut each: impl FnMut(Vec<ProposalRecord>),
) -> Result<ProposalVerifier> {
    let mut verifier =
        ProposalVerifier::new(repository.clone(), operation_id.to_string(), expires_at)?;
    let mut pages = conn.prepare(
        "SELECT first, records FROM upload_pages WHERE repo_id = ?1 AND operation_id = ?2 ORDER BY first",
    )?;
    let rows = pages.query_map(params![repository.repo_id.as_str(), operation_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (first, records) = row?;
        let page = ProposalPage {
            repository: repository.clone(),
            operation_id: operation_id.to_string(),
            offset: first as u64,
            records: serde_json::from_slice(&records)
                .map_err(|error| Error::State(format!("staged page: {error}")))?,
        };
        verifier.push(&page, now)?;
        each(page.records);
    }
    Ok(verifier)
}

impl Uploads {
    pub(crate) fn new(state_root: PathBuf) -> Self {
        Self {
            state_root,
            verifiers: Mutex::new(HashMap::new()),
        }
    }

    /// Install one page durably before acknowledging it. A retry of an
    /// installed page gets its original acknowledgement only if identical.
    pub(crate) fn put_page(
        &self,
        repository: &RepositoryIdentity,
        page: &ProposalPage,
    ) -> Result<PageAck> {
        if page.repository != *repository {
            return Err(ProtocolError::Generation.into());
        }
        let records = canonical(&page.records)?;
        let now = Utc::now();
        let mut verifiers = self.verifiers.lock().expect("upload verifiers");
        let mut conn = open_existing(&self.state_root)?;
        conn.execute("DELETE FROM uploads WHERE expires_at <= ?1", [now.timestamp()])?;
        verifiers.retain(|_, (_, expires_at)| *expires_at > now);
        let key = (repository.repo_id.clone(), page.operation_id.clone());
        let upload: Option<(String, i64)> = conn
            .query_row(
                "SELECT generation, expires_at FROM uploads WHERE repo_id = ?1 AND operation_id = ?2",
                params![key.0.as_str(), key.1],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (verifier, expires_at, started) = match upload {
            None if page.offset != 0 => return Err(ProtocolError::UploadExpired.into()),
            None => {
                let expires_at = timestamp((now + LIFETIME).timestamp())?;
                let verifier =
                    ProposalVerifier::new(repository.clone(), page.operation_id.clone(), expires_at)?;
                (verifier, expires_at, false)
            }
            Some((generation, _)) if generation != repository.generation.to_string() => {
                return Err(ProtocolError::Generation.into());
            }
            Some((_, expires_at)) => {
                let expires_at = timestamp(expires_at)?;
                let verifier = match verifiers.get(&key) {
                    Some((verifier, _)) => verifier.clone(),
                    None => replay(&conn, repository, &key.1, expires_at, now, |_| {})?,
                };
                (verifier, expires_at, true)
            }
        };
        if page.offset < verifier.next_offset() {
            let installed: Option<(i64, Vec<u8>)> = conn
                .query_row(
                    "SELECT next, records FROM upload_pages \
                     WHERE repo_id = ?1 AND operation_id = ?2 AND first = ?3",
                    params![key.0.as_str(), key.1, page.offset as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            return match installed {
                Some((next, installed)) if installed == records => Ok(PageAck {
                    next_offset: next as u64,
                    expires_at,
                }),
                _ => Err(Error::Invalid(
                    "a different page is installed at this offset".into(),
                )),
            };
        }
        let mut next = verifier;
        next.push(page, now)?;
        let txn = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !started {
            txn.execute(
                "INSERT INTO uploads (repo_id, operation_id, generation, expires_at) VALUES (?1, ?2, ?3, ?4)",
                params![key.0.as_str(), key.1, repository.generation.to_string(), expires_at.timestamp()],
            )?;
        }
        txn.execute(
            "INSERT INTO upload_pages (repo_id, operation_id, first, next, records) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key.0.as_str(), key.1, page.offset as i64, next.next_offset() as i64, records],
        )?;
        txn.commit()?;
        let ack = PageAck {
            next_offset: next.next_offset(),
            expires_at,
        };
        verifiers.insert(key, (next, expires_at));
        Ok(ack)
    }

    /// The staged records of `request`, verified against it now.
    pub(crate) fn staged(
        &self,
        request: &SubmitRequest,
    ) -> Result<(ValidatedProposal, Vec<ProposalRecord>)> {
        let now = Utc::now();
        let conn = open_existing(&self.state_root)?;
        let expires_at: i64 = conn
            .query_row(
                "SELECT expires_at FROM uploads WHERE repo_id = ?1 AND operation_id = ?2 AND generation = ?3",
                params![
                    request.repository.repo_id.as_str(),
                    request.operation_id,
                    request.repository.generation.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(ProtocolError::UploadExpired)?;
        let mut records = Vec::new();
        let verifier = replay(
            &conn,
            &request.repository,
            &request.operation_id,
            timestamp(expires_at)?,
            now,
            |page| records.extend(page),
        )?;
        Ok((verifier.finish(request, now)?, records))
    }

    /// Drop an accepted upload's pages.
    pub(crate) fn remove(&self, repo_id: &RepoId, operation_id: &str) -> Result<()> {
        let mut verifiers = self.verifiers.lock().expect("upload verifiers");
        open_existing(&self.state_root)?.execute(
            "DELETE FROM uploads WHERE repo_id = ?1 AND operation_id = ?2",
            params![repo_id.as_str(), operation_id],
        )?;
        verifiers.remove(&(repo_id.clone(), operation_id.to_string()));
        Ok(())
    }

    /// Recorded before acceptance, so a receipt always has its digest.
    pub(crate) fn submit(
        &self,
        repo_id: &RepoId,
        operation_id: &str,
        request_digest: &Sha256Digest,
    ) -> Result<()> {
        let connection = open_existing(&self.state_root)?;
        connection.execute(
            "INSERT OR IGNORE INTO submissions (repo_id, operation_id, request_digest) VALUES (?1, ?2, ?3)",
            params![repo_id.as_str(), operation_id, request_digest.as_str()],
        )?;
        let stored: String = connection.query_row(
            "SELECT request_digest FROM submissions WHERE repo_id=?1 AND operation_id=?2",
            params![repo_id.as_str(),operation_id],|row|row.get(0),
        )?;
        if stored != request_digest.as_str() { return Err(Error::OperationReused); }
        Ok(())
    }

    pub(crate) fn submitted(&self, repo_id: &RepoId, operation_id: &str) -> Result<Option<Sha256Digest>> {
        let digest: Option<String> = open_existing(&self.state_root)?
            .query_row(
                "SELECT request_digest FROM submissions WHERE repo_id = ?1 AND operation_id = ?2",
                params![repo_id.as_str(), operation_id],
                |row| row.get(0),
            )
            .optional()?;
        digest
            .map(|digest| Sha256Digest::parse(digest).map_err(|error| Error::State(error.to_string())))
            .transpose()
    }
}
