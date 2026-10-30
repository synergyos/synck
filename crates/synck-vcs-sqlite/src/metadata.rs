use rusqlite::{Connection, OptionalExtension, params};
use synck_vcs::errors::StorageError;
use synck_vcs::metadata::Metadata;
use synck_vcs::object::ENCRYPTED_PREFIX;
use synck_vcs::types::{ObjectId, RepoId};

use crate::SqliteStorage;

fn error(value: impl std::fmt::Display) -> StorageError {
    StorageError::Other {
        detail: value.to_string(),
    }
}

impl SqliteStorage {
    pub fn verify_metadata_objects(&self, repo: &RepoId) -> Result<u64, StorageError> {
        let connection = self.open_readonly()?;
        let mut statement = connection.prepare(
            "SELECT object_id,payload FROM vcs_metadata_objects WHERE repo_id=?1 ORDER BY object_id",
        ).map_err(error)?;
        let mut rows = statement.query([repo.as_str()]).map_err(error)?;
        let mut count = 0;
        while let Some(row) = rows.next().map_err(error)? {
            let digest: Vec<u8> = row.get(0).map_err(error)?;
            let digest: [u8; 32] = digest
                .try_into()
                .map_err(|_| error("invalid metadata object ID"))?;
            let payload: Vec<u8> = row.get(1).map_err(error)?;
            if payload.starts_with(ENCRYPTED_PREFIX) != self.encrypted() {
                return Err(error("metadata object has the wrong form for this repository"));
            }
            Metadata::verify_any(&ObjectId::from_digest(digest), payload).map_err(error)?;
            count += 1;
        }
        Ok(count)
    }

    /// Publish before accepting an event that refers to this payload. An unused
    /// object is harmless; an accepted pointer must never precede its bytes.
    pub fn put_metadata(&self, repo: &RepoId, metadata: &Metadata) -> Result<(), StorageError> {
        let mut connection = self.open_writable()?;
        let transaction = connection.transaction().map_err(error)?;
        insert(&transaction, repo, metadata)?;
        transaction.commit().map_err(error)
    }

    /// Absence is a valid state; corrupt present bytes are an error. An encrypted
    /// object has no value to return without its key.
    pub fn get_metadata(
        &self,
        repo: &RepoId,
        id: &ObjectId,
    ) -> Result<Option<Metadata>, StorageError> {
        match self.stored_metadata(repo, id)? {
            Some(bytes) => Metadata::verify_any(id, bytes).map_err(error),
            None => Ok(None),
        }
    }

    /// Whether a valid object is stored, in either form.
    pub fn has_metadata(&self, repo: &RepoId, id: &ObjectId) -> Result<bool, StorageError> {
        match self.stored_metadata(repo, id)? {
            Some(bytes) => Metadata::verify_any(id, bytes).map(|_| true).map_err(error),
            None => Ok(false),
        }
    }

    fn stored_metadata(&self, repo: &RepoId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        self.open_readonly()?
            .query_row(
                "SELECT payload FROM vcs_metadata_objects WHERE repo_id=?1 AND object_id=?2",
                params![repo.as_str(), id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(error)
    }
}

pub(crate) fn insert(
    connection: &Connection,
    repo: &RepoId,
    metadata: &Metadata,
) -> Result<(), StorageError> {
    insert_payload(connection, repo, &metadata.id(), metadata.payload())
}

/// Store verified bytes of either form.
pub(crate) fn insert_payload(
    connection: &Connection,
    repo: &RepoId,
    id: &ObjectId,
    payload: &[u8],
) -> Result<(), StorageError> {
    let existing: Option<Vec<u8>> = connection
        .query_row(
            "SELECT payload FROM vcs_metadata_objects WHERE repo_id=?1 AND object_id=?2",
            params![repo.as_str(), id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()
        .map_err(error)?;
    if let Some(existing) = existing {
        if existing != payload {
            return Err(error(
                "metadata object digest collision or corrupt stored bytes",
            ));
        }
        return Ok(());
    }
    connection
        .execute(
            "INSERT INTO vcs_metadata_objects(repo_id,object_id,payload) VALUES(?1,?2,?3)",
            params![repo.as_str(), id.as_bytes().as_slice(), payload],
        )
        .map_err(error)?;
    Ok(())
}
