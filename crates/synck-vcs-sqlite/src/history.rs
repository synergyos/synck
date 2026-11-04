//! Published history position for sync protocol 1.

use rusqlite::OptionalExtension as _;
use synck_sync::{
    Cursor, HistoryFragment, HistoryPage, HistoryRequest, MAX_PAGE_BYTES, PublishedHeader,
    RepositoryIdentity,
};
use synck_vcs::errors::StorageError;
use synck_vcs::storage::StorageBackend as _;
use synck_vcs::types::{RepoId, Sha256Digest, sha256_prefixed_hex_from_digest_bytes};
use uuid::Uuid;

use crate::storage::{SqliteStorage, map_sqlite_error};

/// Left for the page's `after`, field names and separators.
const PAGE_RESERVE: usize = 4096;

fn other(detail: impl std::fmt::Display) -> StorageError {
    StorageError::Other {
        detail: detail.to_string(),
    }
}

impl SqliteStorage {
    /// `None` until the repository's first publication fixes its generation.
    pub fn history_identity(
        &self,
        repo_id: &RepoId,
    ) -> Result<Option<RepositoryIdentity>, StorageError> {
        let conn = self.open_readonly()?;
        let generation: Option<String> = conn
            .query_row(
                "SELECT generation FROM vcs_history_generations WHERE repo_id=?1",
                [repo_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        generation
            .map(|generation| {
                Ok(RepositoryIdentity {
                    repo_id: repo_id.clone(),
                    generation: Uuid::parse_str(&generation).map_err(other)?,
                })
            })
            .transpose()
    }

    /// The last published position, or genesis when nothing is published.
    pub fn history_head(&self, identity: &RepositoryIdentity) -> Result<Cursor, StorageError> {
        match self.checkpoint(identity, None)? {
            Some(cursor) => Ok(cursor),
            None => Cursor::genesis(identity.clone(), self.encrypted()).map_err(other),
        }
    }

    /// The published position at `repo_seq`, or `None` if it is not published.
    pub fn history_cursor(
        &self,
        identity: &RepositoryIdentity,
        repo_seq: i64,
    ) -> Result<Option<Cursor>, StorageError> {
        self.checkpoint(identity, Some(repo_seq))
    }

    fn checkpoint(
        &self,
        identity: &RepositoryIdentity,
        repo_seq: Option<i64>,
    ) -> Result<Option<Cursor>, StorageError> {
        let conn = self.open_readonly()?;
        let row: Option<(i64, Vec<u8>)> = conn
            .query_row(
                "SELECT repo_seq,checkpoint FROM vcs_checkpoints WHERE repo_id=?1 \
                 AND (?2 IS NULL OR repo_seq=?2) ORDER BY repo_seq DESC LIMIT 1",
                rusqlite::params![identity.repo_id.as_str(), repo_seq],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((repo_seq, checkpoint)) = row else {
            return Ok(None);
        };
        let checkpoint: [u8; 32] = checkpoint
            .try_into()
            .map_err(|_| other("invalid checkpoint digest length"))?;
        Ok(Some(Cursor {
            repository: identity.clone(),
            repo_seq: u64::try_from(repo_seq).map_err(other)?,
            checkpoint: Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(checkpoint))
                .map_err(other)?,
        }))
    }

    /// One page of published history after `request.after`, packed as
    /// protocol 1 requires; `None` when that position is not on this history.
    /// Published history is immutable, so the reads need no shared snapshot.
    pub async fn history_page(
        &self,
        request: &HistoryRequest,
    ) -> Result<Option<HistoryPage>, StorageError> {
        let after = &request.after;
        let cursor = &after.completed;
        let repo_id = &cursor.repository.repo_id;
        if self.history_identity(repo_id)?.as_ref() != Some(&cursor.repository) {
            return Ok(None);
        }
        let mut seq = i64::try_from(cursor.repo_seq).map_err(other)?;
        let known = match seq {
            0 => Some(Cursor::genesis(cursor.repository.clone(), self.encrypted()).map_err(other)?),
            _ => self.history_cursor(&cursor.repository, seq)?,
        };
        if known.as_ref().map(|known| &known.checkpoint) != Some(&cursor.checkpoint) {
            return Ok(None);
        }
        let head = self.history_head(&cursor.repository)?.repo_seq as i64;
        let mut skip = after
            .pending
            .as_ref()
            .map_or(0, |pending| pending.next_event_index as usize);
        let mut events_left = request.limit;
        let mut bytes_left = MAX_PAGE_BYTES - PAGE_RESERVE;
        let mut fragments: Vec<HistoryFragment> = Vec::new();
        let mut incomplete = false;
        while events_left > 0 && seq < head {
            let next = seq + 1;
            let commit = self
                .list_commits_by_repo_seq_readonly(repo_id, next, Some(next), 1)
                .await?
                .pop()
                .ok_or_else(|| other("published commit is missing"))?;
            let total = commit.event_count as usize;
            if let Some(pending) = after.pending.as_ref().filter(|_| fragments.is_empty())
                && (pending.commit_id != commit.commit_id || skip > total)
            {
                return Ok(None);
            }
            let checkpoint = self
                .history_cursor(&cursor.repository, next)?
                .ok_or_else(|| other("published commit has no checkpoint"))?
                .checkpoint;
            let header = PublishedHeader { commit, checkpoint };
            let header_size = serde_json::to_vec(&header).map_err(other)?.len();
            if header_size >= bytes_left {
                break;
            }
            bytes_left -= header_size;
            let want = (total - skip).min(events_left);
            let mut rows = self
                .list_commit_events_readonly(repo_id, &header.commit.commit_id, skip + want)
                .await?;
            if rows.len() != skip + want {
                return Err(other("published commit is missing events"));
            }
            let mut events = Vec::with_capacity(want);
            for row in rows.drain(skip..) {
                let event_size = serde_json::to_vec(&row).map_err(other)?.len() + 1;
                if event_size > bytes_left {
                    break;
                }
                bytes_left -= event_size;
                events.push(row);
            }
            let taken = events.len();
            if taken == 0 && want > 0 {
                break;
            }
            let done = skip + taken == total;
            events_left -= taken.max(1);
            fragments.push(HistoryFragment { header, events });
            if !done {
                incomplete = true;
                break;
            }
            seq = next;
            skip = 0;
        }
        Ok(Some(HistoryPage {
            after: after.clone(),
            fragments,
            has_more: incomplete || seq < head,
        }))
    }
}
