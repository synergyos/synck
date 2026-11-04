//! Published history position for sync protocol 1.

use rusqlite::OptionalExtension as _;
use synck_sync::{Cursor, RepositoryIdentity};
use synck_vcs::errors::StorageError;
use synck_vcs::types::{RepoId, Sha256Digest, sha256_prefixed_hex_from_digest_bytes};
use uuid::Uuid;

use crate::storage::{SqliteStorage, map_sqlite_error};

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
}
