use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{BufReader, Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fastcdc::v2020::{FastCDC, Normalization};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use synck_vcs_sqlite::{keep_wal_on_close, open_connection};
use sha2::{Digest, Sha256};
use synck_vcs::types::{ObjectId, RepoId};

use crate::error::{LocalError, Result};
use crate::repository::{is_link_or_reparse, sync_file};

#[cfg(test)]
thread_local! {
    static DURABILITY_CHECKPOINTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) const OBJECT_STORE_FILE: &str = "object-store.sqlite3";
const APPLICATION_ID: i64 = 0x534f_424a;
const USER_VERSION: i64 = 2;
pub(super) const MIN_CHUNK: u32 = 32 * 1024;
pub(super) const AVG_CHUNK: u32 = 64 * 1024;
pub(crate) const MAX_CHUNK: usize = 256 * 1024;
const ZSTD_LEVEL: i32 = 1;
// Only changed files have a base, so level 3 costs a commit little. In a
// prototype over the 311 git.git release tags it stored 12.4 MB of chunks where
// level 1 stored 18.5 MB.
const DELTA_ZSTD_LEVEL: i32 = 3;
/// Reading a chunk decodes its whole chain. Replaying those tags, chains of up
/// to 24 deltas stored 13.0 MB and `fsck` took 4.0 s; 49 stored 11.2 MB in 4.7 s.
const MAX_DELTA_DEPTH: usize = 24;
const MAX_BATCH_PAYLOAD: usize = 8 * 1024 * 1024;
const MAX_BATCH_ROWS: usize = 1024;

#[cfg(test)]
mod ooxml_layout_tests;

const SCHEMA: &str = include_str!("object-store.sql");
/// Version 2 adds `chunk_bases`; a version 1 store has no deltas.
const UPGRADE_VERSION_1: &str = "
CREATE TABLE chunk_bases (
    chunk_digest BLOB PRIMARY KEY CHECK (length(chunk_digest) = 32),
    base_digest BLOB NOT NULL CHECK (length(base_digest) = 32),
    CHECK (chunk_digest <> base_digest),
    FOREIGN KEY (chunk_digest) REFERENCES chunks(digest),
    FOREIGN KEY (base_digest) REFERENCES chunks(digest)
) STRICT, WITHOUT ROWID;
CREATE INDEX idx_chunk_bases_base_digest ON chunk_bases(base_digest);
";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StorageMode {
    #[default]
    WholeFile,
    Chunked,
}

pub(crate) struct ChunkStore {
    path: PathBuf,
    repo_id: String,
    /// The schema was checked by an earlier open of this value. Comparing it
    /// reads the whole catalog, and a commit or a pack opens per object.
    schema_checked: AtomicBool,
}

/// Reads blobs through one open connection.
pub(crate) struct ChunkReader {
    store: ChunkStore,
    connection: Connection,
}

impl ChunkReader {
    pub(crate) fn copy_blob<W: Write>(
        &self,
        blob_id: &ObjectId,
        expected_size: u64,
        target: &mut W,
    ) -> Result<()> {
        self.store
            .copy_blob_from(&self.connection, blob_id, expected_size, target)
    }

    /// A complete blob's size, or `None` when it is not stored.
    pub(crate) fn blob_size(&self, id: &ObjectId) -> Result<Option<u64>> {
        let digest = blob_digest(id)?;
        let size: Option<i64> = self
            .connection
            .query_row(
                "SELECT size_bytes FROM blobs WHERE digest=?1 AND complete=1",
                [digest.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        size.map(|size| u64::try_from(size).map_err(|_| LocalError::Integrity("negative blob size".into())))
            .transpose()
    }
}

struct PendingChunk {
    ordinal: i64,
    digest: [u8; 32],
    raw: Vec<u8>,
    codec: &'static str,
    payload: Vec<u8>,
    base: Option<[u8; 32]>,
    insert_chunk: bool,
}

impl ChunkStore {
    pub(crate) fn new(path: PathBuf, repo_id: &RepoId) -> Self {
        Self {
            path,
            repo_id: repo_id.as_str().to_string(),
            schema_checked: AtomicBool::new(false),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn create(path: &Path, repo_id: &RepoId) -> Result<()> {
        let seed = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(|source| LocalError::Io {
                action: "create object database",
                path: path.to_path_buf(),
                source,
            })?;
        drop(seed);
        let open_path = resolved_open_path(path)?;
        let connection = open_connection(
            open_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        // Chunks average 64 KiB and overflow their row's page. Importing 906 MB
        // took 4.3 s into 911 MB with 4 KiB pages, 2.9 s into 955 MB with 32 KiB
        // and 3.0 s into 1042 MB with 64 KiB.
        connection.execute_batch(
            "PRAGMA page_size = 32768;
             PRAGMA auto_vacuum = INCREMENTAL;
             PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA synchronous = FULL;
             BEGIN IMMEDIATE;",
        )?;
        let result = (|| -> Result<()> {
            connection.execute_batch(SCHEMA)?;
            connection.execute(
                "INSERT INTO store_identity(singleton, repo_id) VALUES (1, ?1)",
                [repo_id.as_str()],
            )?;
            connection.pragma_update(None, "application_id", APPLICATION_ID)?;
            connection.pragma_update(None, "user_version", USER_VERSION)?;
            connection.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE)")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = connection.execute_batch("ROLLBACK");
        }
        drop(connection);
        result?;
        sync_file(path)
    }

    pub(crate) fn open_readonly(&self) -> Result<Connection> {
        validate_regular_file(&self.path)?;
        let open_path = resolved_open_path(&self.path)?;
        let connection = open_connection(
            open_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let user_version: i64 =
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if user_version == 1 {
            drop(connection);
            drop(self.open_writable()?);
            return self.open_readonly();
        }
        self.validate_connection(&connection)?;
        Ok(connection)
    }

    pub(crate) fn open_writable(&self) -> Result<Connection> {
        validate_regular_file(&self.path)?;
        let open_path = resolved_open_path(&self.path)?;
        let connection = open_connection(
            open_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        // Staging transactions do not flush one by one. Nothing refers to a blob
        // until `checkpoint` has flushed the whole WAL: `complete_blob` does it for
        // each blob, and a commit does it once before its history transaction.
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA synchronous = NORMAL;",
        )?;
        keep_wal_on_close(&connection)?;
        // Content is large, and every write path ends with `checkpoint`. Each
        // automatic checkpoint fully flushes the drive twice and stalls the other
        // writer, so let the WAL grow to 1 GiB (32768 pages of 32 KiB) first.
        connection.execute_batch(
            "PRAGMA wal_autocheckpoint = 32768;
             PRAGMA journal_size_limit = 67108864;",
        )?;
        // A commit captures files on several threads, each with its own writer.
        connection.busy_timeout(Duration::from_secs(5))?;
        let foreign_keys: i64 =
            connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if foreign_keys != 1 || synchronous != 1 {
            return Err(LocalError::Integrity(
                "object database connection pragmas were not applied".to_string(),
            ));
        }
        upgrade_version_1(&connection)?;
        self.validate_connection(&connection)?;
        Ok(connection)
    }

    fn validate_connection(&self, connection: &Connection) -> Result<()> {
        let application_id: i64 =
            connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let user_version: i64 =
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let page_size: i64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
        let auto_vacuum: i64 =
            connection.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?;
        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if application_id != APPLICATION_ID
            || user_version != USER_VERSION
            // Stores created before 0.2.1 use 4 KiB pages.
            || !matches!(page_size, 4096 | 32768)
            || auto_vacuum != 2
            || !journal_mode.eq_ignore_ascii_case("wal")
        {
            return Err(LocalError::Integrity(
                "invalid object database format pragmas".to_string(),
            ));
        }
        if !self.schema_checked.load(Ordering::Relaxed) {
            validate_schema(connection)?;
            self.schema_checked.store(true, Ordering::Relaxed);
        }
        let identities: i64 =
            connection.query_row("SELECT count(*) FROM store_identity", [], |row| row.get(0))?;
        let repo_id: Option<String> = connection
            .query_row(
                "SELECT repo_id FROM store_identity WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if identities != 1 || repo_id.as_deref() != Some(self.repo_id.as_str()) {
            return Err(LocalError::Integrity(
                "object database repository identity mismatch".to_string(),
            ));
        }
        Ok(())
    }

    fn validate_physical(&self, connection: &Connection) -> Result<()> {
        let integrity: String =
            connection.pragma_query_value(None, "integrity_check", |row| row.get(0))?;
        if integrity != "ok" {
            return Err(LocalError::Integrity(format!(
                "object database integrity check failed: {integrity}"
            )));
        }
        let foreign_key_errors: i64 =
            connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if foreign_key_errors != 0 {
            return Err(LocalError::Integrity(
                "object database contains foreign-key violations".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn recover_incomplete(&self) -> Result<()> {
        let connection = self.open_writable()?;
        let mut statement =
            connection.prepare("SELECT digest FROM blobs WHERE complete = 0 ORDER BY digest")?;
        let digests = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        if digests.is_empty() {
            return Ok(());
        }
        for digest in digests {
            delete_incomplete_blob(&connection, &digest, true)?;
        }
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(connection);
        sync_file(&self.path)
    }

    pub(crate) fn contains_complete(&self, digest: &[u8; 32], size: u64) -> Result<bool> {
        let connection = self.open_readonly()?;
        let found: Option<i64> = connection
            .query_row(
                "SELECT size_bytes FROM blobs WHERE digest = ?1 AND complete = 1",
                [digest.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        match found {
            None => Ok(false),
            Some(stored) if stored == i64::try_from(size).unwrap_or(-1) => Ok(true),
            Some(_) => Err(LocalError::Integrity(
                "complete blob has an inconsistent logical size".to_string(),
            )),
        }
    }

    pub(crate) fn stage_blob<R: Read>(
        &self,
        expected_digest: [u8; 32],
        expected_size: u64,
        reader: R,
    ) -> Result<([u8; 32], u64)> {
        let mut chunker = StreamingChunker::new(reader);
        self.stage_blob_chunks(expected_digest, expected_size, None, || chunker.next_chunk())
    }

    /// `base` is the previous version of the same file. Its chunks are the
    /// delta bases for new chunks in the same places.
    pub(crate) fn stage_blob_regions<R: Read>(
        &self,
        expected_digest: [u8; 32],
        expected_size: u64,
        reader: R,
        region_lengths: &[u64],
        base: Option<&[u8; 32]>,
    ) -> Result<([u8; 32], u64)> {
        let total = region_lengths.iter().try_fold(0_u64, |total, length| {
            if *length == 0 {
                None
            } else {
                total.checked_add(*length)
            }
        });
        if total != Some(expected_size) {
            return Err(LocalError::Integrity(
                "chunk regions do not cover the object".to_string(),
            ));
        }
        let mut chunker = RegionStreamingChunker::new(reader, region_lengths);
        self.stage_blob_chunks(expected_digest, expected_size, base, || chunker.next_chunk())
    }

    fn stage_blob_chunks<F>(
        &self,
        expected_digest: [u8; 32],
        expected_size: u64,
        base: Option<&[u8; 32]>,
        mut next_chunk: F,
    ) -> Result<([u8; 32], u64)>
    where
        F: FnMut() -> Result<Option<Vec<u8>>>,
    {
        let expected_size_i64 = i64::try_from(expected_size)
            .map_err(|_| LocalError::Invalid("file is too large for local storage".to_string()))?;
        let connection = self.open_writable()?;
        delete_incomplete_blob(&connection, &expected_digest, false)?;
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let prepare_result = (|| -> Result<()> {
            connection.execute(
                "INSERT INTO blobs(digest, size_bytes, complete) VALUES (?1, ?2, 0)",
                params![expected_digest.as_slice(), expected_size_i64],
            )?;
            connection.execute_batch("COMMIT")?;
            Ok(())
        })();
        if prepare_result.is_err() {
            let _ = connection.execute_batch("ROLLBACK");
            return prepare_result.map(|()| (expected_digest, expected_size));
        }
        let base_chunks = match base {
            Some(base) => complete_blob_chunks(&connection, base)?,
            None => Vec::new(),
        };

        let mut full_hasher = Sha256::new();
        let mut full_size = 0_u64;
        let mut ordinal = 0_i64;
        let mut batch: Vec<PendingChunk> = Vec::new();
        let mut pending_chunks = HashMap::new();
        let mut batch_payload = 0_usize;
        while let Some(raw) = next_chunk()? {
            full_hasher.update(&raw);
            full_size = full_size
                .checked_add(raw.len() as u64)
                .ok_or_else(|| LocalError::Invalid("file size overflow".to_string()))?;
            let digest: [u8; 32] = Sha256::digest(&raw).into();
            let pending_index: Option<usize> = pending_chunks.get(&digest).copied();
            let (codec, payload, chunk_base, insert_chunk) = if let Some(index) = pending_index {
                if batch[index].raw != raw {
                    return Err(LocalError::Integrity(
                        "chunk digest collision within write batch".to_string(),
                    ));
                }
                ("RAW", Vec::new(), None, false)
            } else if let Some((existing, _)) = read_chunk(&connection, &digest)? {
                if existing != raw {
                    return Err(LocalError::Integrity(
                        "chunk digest collision or corrupt reused chunk".to_string(),
                    ));
                }
                ("RAW", Vec::new(), None, false)
            } else {
                let delta = match base_chunks
                    .get(ordinal as usize)
                    .or(base_chunks.last())
                {
                    Some(base) => match read_chunk(&connection, base)? {
                        Some((base_raw, depth)) if depth < MAX_DELTA_DEPTH => {
                            encode_delta(&raw, &base_raw)?.map(|payload| (payload, *base))
                        }
                        _ => None,
                    },
                    None => None,
                };
                match delta {
                    Some((payload, base)) => ("ZSTD", payload, Some(base), true),
                    None => {
                        let (codec, payload) = encode_chunk(&raw)?;
                        (codec, payload, None, true)
                    }
                }
            };
            // A pending chunk also holds its raw bytes.
            let stored_payload = payload.len() + raw.len();
            if !batch.is_empty() && batch_must_flush(batch.len(), batch_payload, stored_payload) {
                flush_batch(&connection, &expected_digest, &batch)?;
                batch.clear();
                pending_chunks.clear();
                batch_payload = 0;
            }
            batch_payload += stored_payload;
            batch.push(PendingChunk {
                ordinal,
                digest,
                raw,
                codec,
                payload,
                base: chunk_base,
                insert_chunk,
            });
            if insert_chunk {
                pending_chunks.insert(digest, batch.len() - 1);
            }
            ordinal = ordinal
                .checked_add(1)
                .ok_or_else(|| LocalError::Invalid("chunk ordinal overflow".to_string()))?;
        }
        if !batch.is_empty() {
            flush_batch(&connection, &expected_digest, &batch)?;
        }
        Ok((full_hasher.finalize().into(), full_size))
    }

    /// Replace a private staging key with the streamed digest. The object stays
    /// incomplete until the caller has validated its source and byte count.
    /// Give staged bytes their digest. Identical bytes have one ID, so this returns
    /// false, and drops the staging, when that object is already complete.
    pub(crate) fn address_staged_blob(&self, staging: &[u8;32], digest: &[u8;32], size: u64) -> Result<bool> {
        if staging == digest { return Ok(true); }
        let mut connection = self.open_writable()?;
        let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let complete: Option<bool> = transaction
            .query_row("SELECT complete FROM blobs WHERE digest=?1", [digest.as_slice()], |row| row.get(0))
            .optional()?;
        if complete == Some(true) {
            transaction.execute("DELETE FROM blob_chunks WHERE blob_digest=?1", [staging.as_slice()])?;
            transaction.execute("DELETE FROM blobs WHERE digest=?1", [staging.as_slice()])?;
            transaction.commit()?;
            return Ok(false);
        }
        if complete.is_some() {
            transaction.execute("DELETE FROM blob_chunks WHERE blob_digest=?1", [digest.as_slice()])?;
            transaction.execute("DELETE FROM blobs WHERE digest=?1", [digest.as_slice()])?;
        }
        let changed = transaction.execute(
            "INSERT INTO blobs(digest,size_bytes,complete) SELECT ?1,size_bytes,0 FROM blobs WHERE digest=?2 AND size_bytes=?3 AND complete=0",
            params![digest.as_slice(), staging.as_slice(), i64::try_from(size).map_err(|_| LocalError::Invalid("object too large".into()))?],
        )?;
        if changed != 1 { return Err(LocalError::Integrity("staged object disappeared".into())); }
        transaction.execute("UPDATE blob_chunks SET blob_digest=?1 WHERE blob_digest=?2", params![digest.as_slice(),staging.as_slice()])?;
        transaction.execute("DELETE FROM blobs WHERE digest=?1", [staging.as_slice()])?;
        transaction.commit()?;
        Ok(true)
    }

    pub(crate) fn complete_blob(&self, digest: &[u8; 32], size: u64) -> Result<()> {
        self.complete_blob_before_batch_checkpoint(digest, size)?;
        self.checkpoint()
    }

    pub(crate) fn complete_blob_before_batch_checkpoint(
        &self,
        digest: &[u8; 32],
        size: u64,
    ) -> Result<()> {
        let size = i64::try_from(size)
            .map_err(|_| LocalError::Invalid("file is too large for local storage".to_string()))?;
        let connection = self.open_writable()?;
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let changed = connection.execute(
            "UPDATE blobs SET complete = 1
             WHERE digest = ?1 AND size_bytes = ?2 AND complete = 0",
            params![digest.as_slice(), size],
        )?;
        if changed != 1 {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(LocalError::Integrity(
                "incomplete blob disappeared before completion".to_string(),
            ));
        }
        connection.execute_batch("COMMIT")?;
        Ok(())
    }

    pub(crate) fn verify_blob(&self, blob_id: &ObjectId, expected_size: u64) -> Result<()> {
        self.copy_blob(blob_id, expected_size, &mut std::io::sink())
    }

    pub(crate) fn copy_blob<W: Write>(
        &self,
        blob_id: &ObjectId,
        expected_size: u64,
        target: &mut W,
    ) -> Result<()> {
        let connection = self.open_readonly()?;
        self.copy_blob_from(&connection, blob_id, expected_size, target)
    }

    /// Opening and validating the store costs about a millisecond, more than
    /// reading a typical small file; a reader pays it once for many blobs.
    pub(crate) fn reader(self) -> Result<ChunkReader> {
        let connection = self.open_readonly()?;
        Ok(ChunkReader {
            store: self,
            connection,
        })
    }

    fn copy_blob_from<W: Write>(
        &self,
        connection: &Connection,
        blob_id: &ObjectId,
        expected_size: u64,
        target: &mut W,
    ) -> Result<()> {
        let digest = blob_digest(blob_id)?;
        let stored_size: Option<i64> = connection
            .query_row(
                "SELECT size_bytes FROM blobs WHERE digest = ?1 AND complete = 1",
                [digest.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        let stored_size = stored_size.ok_or_else(|| {
            LocalError::Integrity(format!(
                "referenced object is missing: {}",
                blob_id.as_str()
            ))
        })?;
        if stored_size != i64::try_from(expected_size).unwrap_or(-1) {
            return Err(LocalError::Integrity(format!(
                "object size mismatch: {}",
                blob_id.as_str()
            )));
        }
        let mut statement = connection.prepare(
            "SELECT bc.ordinal, c.digest, c.raw_size, c.codec, c.payload, cb.base_digest
             FROM blob_chunks bc
             JOIN chunks c ON c.digest = bc.chunk_digest
             LEFT JOIN chunk_bases cb ON cb.chunk_digest = c.digest
             WHERE bc.blob_digest = ?1
             ORDER BY bc.ordinal",
        )?;
        let mut rows = statement.query([digest.as_slice()])?;
        let mut next_ordinal = 0_i64;
        let mut total = 0_u64;
        let mut full_hasher = Sha256::new();
        while let Some(row) = rows.next()? {
            let ordinal: i64 = row.get(0)?;
            if ordinal != next_ordinal {
                return Err(LocalError::Integrity(
                    "blob chunk ordinals are not contiguous".to_string(),
                ));
            }
            next_ordinal += 1;
            let chunk_digest: Vec<u8> = row.get(1)?;
            let raw_size: i64 = row.get(2)?;
            let codec: String = row.get(3)?;
            let payload: Vec<u8> = row.get(4)?;
            let base: Option<Vec<u8>> = row.get(5)?;
            let raw = decode_stored_chunk(connection, &codec, raw_size, &payload, base)?;
            let actual_digest: [u8; 32] = Sha256::digest(&raw).into();
            if chunk_digest.as_slice() != actual_digest {
                return Err(LocalError::Integrity("chunk digest mismatch".to_string()));
            }
            total = total
                .checked_add(raw.len() as u64)
                .ok_or_else(|| LocalError::Integrity("blob size overflow".to_string()))?;
            full_hasher.update(&raw);
            target.write_all(&raw).map_err(|source| LocalError::Io {
                action: "write reconstructed object",
                path: self.path.clone(),
                source,
            })?;
        }
        if total != expected_size || <[u8; 32]>::from(full_hasher.finalize()) != digest {
            return Err(LocalError::Integrity(format!(
                "whole object verification failed: {}",
                blob_id.as_str()
            )));
        }
        Ok(())
    }

    pub(crate) fn holds_blob(&self, id: &ObjectId) -> Result<bool> {
        let digest = blob_digest(id)?;
        let held = self
            .open_readonly()?
            .query_row(
                "SELECT 1 FROM blobs WHERE digest=?1 AND complete=1",
                [digest.as_slice()],
                |_| Ok(()),
            )
            .optional()?;
        Ok(held.is_some())
    }

    pub(crate) fn blob_size(&self, id: &ObjectId) -> Result<u64> {
        let digest = blob_digest(id)?;
        let size: i64 = self.open_readonly()?.query_row(
            "SELECT size_bytes FROM blobs WHERE digest=?1 AND complete=1",
            [digest.as_slice()],
            |row| row.get(0),
        )?;
        u64::try_from(size).map_err(|_| LocalError::Integrity("negative blob size".into()))
    }

    pub(crate) fn store_bytes(&self, bytes: &[u8]) -> Result<String> {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let size = bytes.len() as u64;
        if self.contains_complete(&digest, size)? {
            self.verify_blob(
                &ObjectId::parse(format!("sha256:{}", hex::encode(digest)))
                    .map_err(|e| LocalError::Integrity(e.to_string()))?,
                size,
            )?;
        } else {
            self.stage_blob(digest, size, bytes)?;
            self.complete_blob(&digest, size)?;
        }
        Ok(hex::encode(digest))
    }

    /// Repack exact stored bytes; object IDs never change.
    pub(crate) fn store_object_file(&self, source: &Path) -> Result<(String, u64)> {
        let mut file = crate::repository::open_regular_file_nofollow(source)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|e| LocalError::Invalid(e.to_string()))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
            size += count as u64;
        }
        let digest: [u8; 32] = hasher.finalize().into();
        if self.contains_complete(&digest, size)? {
            self.verify_blob(
                &ObjectId::parse(format!("sha256:{}", hex::encode(digest)))
                    .map_err(|e| LocalError::Integrity(e.to_string()))?,
                size,
            )?;
            return Ok((hex::encode(digest), size));
        }
        file.rewind()
            .map_err(|e| LocalError::Invalid(e.to_string()))?;
        use crate::ooxml::{OoxmlKind, chunk_region_lengths_at};
        let mut regions = None;
        for kind in [OoxmlKind::Docx, OoxmlKind::Xlsx, OoxmlKind::Pptx] {
            regions = chunk_region_lengths_at(file.try_clone().map_err(|e| LocalError::Invalid(e.to_string()))?, size, kind, 0);
            if regions.is_some() { break; }
        }
        let boundaries = if size > 0 { regions.unwrap_or_else(|| vec![size]) } else { Vec::new() };
        // Office inspection may move the shared file offset.
        file.rewind()
            .map_err(|e| LocalError::Invalid(e.to_string()))?;
        let captured = self.stage_blob_regions(digest, size, file, &boundaries, None)?;
        if captured != (digest, size) {
            return Err(LocalError::Invalid(
                "file changed while being captured".into(),
            ));
        }
        self.complete_blob(&digest, size)?;
        Ok((hex::encode(digest), size))
    }

    pub(crate) fn inventory(&self) -> Result<BTreeMap<String, u64>> {
        let connection = self.open_readonly()?;
        Self::inventory_from(&connection)
    }

    fn inventory_from(connection: &Connection) -> Result<BTreeMap<String, u64>> {
        let mut statement = connection
            .prepare("SELECT digest, size_bytes FROM blobs WHERE complete = 1 ORDER BY digest")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut inventory = BTreeMap::new();
        for row in rows {
            let (digest, size) = row?;
            let digest: [u8; 32] = digest.try_into().map_err(|_| {
                LocalError::Integrity("object database contains an invalid digest".to_string())
            })?;
            let size = u64::try_from(size).map_err(|_| {
                LocalError::Integrity("object database contains a negative size".to_string())
            })?;
            inventory.insert(hex::encode(digest), size);
        }
        Ok(inventory)
    }

    pub(crate) fn verify_all(&self) -> Result<BTreeMap<String, u64>> {
        let connection = self.open_readonly()?;
        self.verify_all_from(&connection)
    }

    fn verify_all_from(&self, connection: &Connection) -> Result<BTreeMap<String, u64>> {
        self.validate_physical(connection)?;
        // Complete blobs verify their chunks below. Check the remaining chunks
        // here too, including those belonging only to interrupted writes.
        let mut chunks = connection.prepare(
            "SELECT digest, raw_size, codec, payload, cb.base_digest FROM chunks
             LEFT JOIN chunk_bases cb ON cb.chunk_digest = chunks.digest
             WHERE NOT EXISTS (
                 SELECT 1 FROM blob_chunks bc JOIN blobs b ON b.digest = bc.blob_digest
                 WHERE bc.chunk_digest = chunks.digest AND b.complete = 1
             ) ORDER BY digest",
        )?;
        let rows = chunks.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
            ))
        })?;
        for row in rows {
            let (digest, raw_size, codec, payload, base) = row?;
            let raw = decode_stored_chunk(connection, &codec, raw_size, &payload, base)?;
            let actual_digest: [u8; 32] = Sha256::digest(&raw).into();
            if digest.as_slice() != actual_digest {
                return Err(LocalError::Integrity("chunk digest mismatch".to_string()));
            }
        }
        drop(chunks);
        let inventory = Self::inventory_from(connection)?;
        for (digest, size) in &inventory {
            let blob_id = ObjectId::parse(format!("sha256:{digest}"))
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
            let mut writer = synck_vcs::object::VerifyingWriter::new(blob_id.clone(), std::io::sink());
            self.copy_blob_from(connection, &blob_id, *size, &mut writer)?;
            writer
                .finish()
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
        }
        Ok(inventory)
    }

    // The destination repository verifier checks the payloads before publication.
    // This additional check enforces the archive-only physical closure rules.
    pub(crate) fn verify_archive_closure(&self) -> Result<()> {
        let connection = self.open_readonly()?;
        let incomplete: i64 =
            connection.query_row("SELECT count(*) FROM blobs WHERE complete = 0", [], |row| {
                row.get(0)
            })?;
        let orphan_chunks: i64 = connection.query_row(
            "SELECT count(*) FROM chunks
             WHERE NOT EXISTS (
                 SELECT 1 FROM blob_chunks WHERE chunk_digest = chunks.digest
             ) AND NOT EXISTS (
                 SELECT 1 FROM chunk_bases WHERE base_digest = chunks.digest
             )",
            [],
            |row| row.get(0),
        )?;
        if incomplete != 0 || orphan_chunks != 0 {
            return Err(LocalError::Integrity(
                "archive object database contains incomplete blobs or orphan chunks".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn gc(&self, reachable: &BTreeSet<String>) -> Result<(u64, u64)> {
        let inventory = self.inventory()?;
        let mut removed_objects = 0_u64;
        let mut removed_bytes = 0_u64;
        let unreachable = inventory
            .iter()
            .filter(|(digest, _)| !reachable.contains(*digest))
            .map(|(digest, size)| (digest.clone(), *size))
            .collect::<Vec<_>>();
        let connection = self.open_writable()?;
        self.validate_physical(&connection)?;
        delete_blob_batches(
            &connection,
            &unreachable
                .iter()
                .map(|(digest, _)| digest.clone())
                .collect::<Vec<_>>(),
        )?;
        for (_, size) in &unreachable {
            removed_objects += 1;
            removed_bytes = removed_bytes
                .checked_add(*size)
                .ok_or_else(|| LocalError::Integrity("removed byte count overflow".to_string()))?;
        }
        delete_orphan_chunks(&connection)?;
        connection.execute_batch(
            "PRAGMA incremental_vacuum;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )?;
        drop(connection);
        sync_file(&self.path)?;
        Ok((removed_objects, removed_bytes))
    }

    pub(crate) fn checkpoint(&self) -> Result<()> {
        #[cfg(test)]
        DURABILITY_CHECKPOINTS.with(|count| count.set(count.get() + 1));
        let connection = self.open_writable()?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(connection);
        sync_file(&self.path)
    }

    #[cfg(test)]
    pub(crate) fn durability_checkpoint_count(&self) -> usize {
        DURABILITY_CHECKPOINTS.with(std::cell::Cell::get)
    }

    pub(crate) fn backup_to(&self, destination: &Path) -> Result<()> {
        let source = self.open_readonly()?;
        let open_destination = resolved_open_path(destination)?;
        let mut destination_connection = open_connection(
            open_destination,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let backup = rusqlite::backup::Backup::new(&source, &mut destination_connection)?;
        // The caller holds the repository lock and the destination is private.
        // Do not throttle every 512 KiB as if this were an online service backup.
        backup.run_to_completion(i32::MAX, Duration::from_millis(10), None)?;
        drop(backup);
        drop(destination_connection);
        drop(source);
        sync_file(destination)
    }

    pub(crate) fn prune_to(&self, keep: &BTreeSet<String>) -> Result<()> {
        let inventory = self.inventory()?;
        let remove = inventory
            .keys()
            .filter(|digest| !keep.contains(*digest))
            .cloned()
            .collect::<Vec<_>>();
        let connection = self.open_writable()?;
        delete_blob_batches(&connection, &remove)?;
        delete_orphan_chunks(&connection)?;
        connection.execute_batch(
            "PRAGMA incremental_vacuum;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )?;
        drop(connection);
        sync_file(&self.path)
    }
}

fn delete_blob_batches(connection: &Connection, digests: &[String]) -> Result<()> {
    for batch in digests.chunks(MAX_BATCH_ROWS) {
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            for digest in batch {
                let bytes = hex::decode(digest)
                    .map_err(|error| LocalError::Integrity(error.to_string()))?;
                let changed = connection.execute(
                    "UPDATE blobs SET complete = 0
                     WHERE digest = ?1 AND complete = 1",
                    [bytes],
                )?;
                if changed != 1 {
                    return Err(LocalError::Integrity(
                        "logical blob disappeared before deletion".to_string(),
                    ));
                }
            }
            connection.execute_batch("COMMIT")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = connection.execute_batch("ROLLBACK");
        }
        result?;
        for digest in batch {
            let bytes =
                hex::decode(digest).map_err(|error| LocalError::Integrity(error.to_string()))?;
            delete_incomplete_blob(connection, &bytes, true)?;
        }
    }
    Ok(())
}

fn delete_incomplete_blob(
    connection: &Connection,
    digest: &[u8],
    require_present: bool,
) -> Result<()> {
    loop {
        let deleted = connection.execute(
            "DELETE FROM blob_chunks
             WHERE blob_digest = ?1
               AND ordinal IN (
                   SELECT bc.ordinal FROM blob_chunks bc
                   JOIN blobs b ON b.digest = bc.blob_digest
                   WHERE bc.blob_digest = ?1 AND b.complete = 0
                   ORDER BY bc.ordinal LIMIT ?2
               )",
            params![digest, MAX_BATCH_ROWS as i64],
        )?;
        if deleted == 0 {
            break;
        }
    }
    let deleted = connection.execute(
        "DELETE FROM blobs WHERE digest = ?1 AND complete = 0",
        [digest],
    )?;
    if require_present && deleted != 1 {
        return Err(LocalError::Integrity(
            "incomplete blob disappeared during deletion".to_string(),
        ));
    }
    Ok(())
}

fn delete_orphan_chunks(connection: &Connection) -> Result<()> {
    loop {
        let mut statement = connection.prepare(
            "SELECT digest, length(payload) FROM chunks
             WHERE NOT EXISTS (
                 SELECT 1 FROM blob_chunks WHERE chunk_digest = chunks.digest
             ) AND NOT EXISTS (
                 SELECT 1 FROM chunk_bases WHERE base_digest = chunks.digest
             )
             ORDER BY digest LIMIT ?1",
        )?;
        let rows = statement.query_map([MAX_BATCH_ROWS as i64], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut digests = Vec::new();
        let mut payload_bytes = 0_usize;
        for row in rows {
            let (digest, payload_size) = row?;
            let payload_size = usize::try_from(payload_size).map_err(|_| {
                LocalError::Integrity("chunk has an invalid payload size".to_string())
            })?;
            if !digests.is_empty() && payload_bytes.saturating_add(payload_size) > MAX_BATCH_PAYLOAD
            {
                break;
            }
            payload_bytes += payload_size;
            digests.push(digest);
        }
        drop(statement);
        if digests.is_empty() {
            return Ok(());
        }

        connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            for digest in &digests {
                // Its base becomes an orphan in turn, unless something else needs it.
                connection.execute("DELETE FROM chunk_bases WHERE chunk_digest = ?1", [digest])?;
                let deleted =
                    connection.execute("DELETE FROM chunks WHERE digest = ?1", [digest])?;
                if deleted != 1 {
                    return Err(LocalError::Integrity(
                        "orphan chunk disappeared during deletion".to_string(),
                    ));
                }
            }
            connection.execute_batch("COMMIT")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = connection.execute_batch("ROLLBACK");
        }
        result?;
    }
}

fn validate_regular_file(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(LocalError::Integrity(format!(
                "object database is missing: {}",
                path.display()
            )));
        }
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect object database",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() || is_link_or_reparse(&metadata) {
        return Err(LocalError::Integrity(format!(
            "object database is not a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn resolved_open_path(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        LocalError::Invalid(format!(
            "object database path has no file name: {}",
            path.display()
        ))
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(parent
        .canonicalize()
        .map_err(|source| LocalError::Io {
            action: "resolve object database parent",
            path: parent.to_path_buf(),
            source,
        })?
        .join(name))
}

fn validate_schema(connection: &Connection) -> Result<()> {
    if &schema_objects(connection)? != expected_schema_objects() {
        return Err(LocalError::Integrity(
            "object SQLite schema definition does not match version 1".to_string(),
        ));
    }
    Ok(())
}

fn expected_schema_objects() -> &'static synck_vcs_sqlite::schema::SchemaShape {
    static EXPECTED: OnceLock<synck_vcs_sqlite::schema::SchemaShape> = OnceLock::new();
    EXPECTED.get_or_init(|| {
        let connection = Connection::open_in_memory().expect("open expected object schema");
        connection.execute_batch(SCHEMA).expect("install expected object schema");
        schema_objects(&connection).expect("read expected object schema")
    })
}

fn schema_objects(connection: &Connection) -> Result<synck_vcs_sqlite::schema::SchemaShape> {
    Ok(synck_vcs_sqlite::schema::inspect(connection, "")?)
}

fn flush_batch(
    connection: &Connection,
    blob_digest: &[u8; 32],
    batch: &[PendingChunk],
) -> Result<()> {
    connection.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        for chunk in batch {
            if chunk.insert_chunk {
                let raw_size = i64::try_from(chunk.raw.len()).expect("chunk size is bounded");
                let inserted = connection.execute(
                    "INSERT INTO chunks(digest, raw_size, codec, payload)
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT(digest) DO NOTHING",
                    params![chunk.digest.as_slice(), raw_size, chunk.codec, &chunk.payload],
                )?;
                if inserted == 1 {
                    if let Some(base) = chunk.base {
                        connection.execute(
                            "INSERT INTO chunk_bases(chunk_digest, base_digest) VALUES (?1, ?2)",
                            params![chunk.digest.as_slice(), base.as_slice()],
                        )?;
                    }
                } else if read_chunk(connection, &chunk.digest)?.map(|(raw, _)| raw)
                    != Some(chunk.raw.clone())
                {
                    // Another capture in this commit stored it first.
                    return Err(LocalError::Integrity(
                        "chunk digest collision between captures".to_string(),
                    ));
                }
            }
            connection.execute(
                "INSERT INTO blob_chunks(blob_digest, ordinal, chunk_digest)
                 VALUES (?1, ?2, ?3)",
                params![
                    blob_digest.as_slice(),
                    chunk.ordinal,
                    chunk.digest.as_slice()
                ],
            )?;
        }
        connection.execute_batch("COMMIT")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = connection.execute_batch("ROLLBACK");
    }
    result
}

/// The chunk IDs of a complete blob, in order.
fn complete_blob_chunks(connection: &Connection, blob: &[u8; 32]) -> Result<Vec<[u8; 32]>> {
    let mut statement = connection.prepare(
        "SELECT bc.chunk_digest FROM blob_chunks bc
         JOIN blobs b ON b.digest = bc.blob_digest
         WHERE bc.blob_digest = ?1 AND b.complete = 1
         ORDER BY bc.ordinal",
    )?;
    let digests = statement
        .query_map([blob.as_slice()], |row| row.get::<_, Vec<u8>>(0))?
        .map(|digest| {
            digest?.try_into().map_err(|_| {
                LocalError::Integrity("object database contains an invalid digest".to_string())
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(digests)
}

/// A chunk's bytes and how many deltas lie between it and a full chunk.
fn read_chunk(connection: &Connection, digest: &[u8; 32]) -> Result<Option<(Vec<u8>, usize)>> {
    let mut statement = connection.prepare_cached(
        "WITH RECURSIVE chain(digest, depth) AS (
             SELECT ?1, 0
             UNION ALL
             SELECT cb.base_digest, chain.depth + 1
             FROM chain JOIN chunk_bases cb ON cb.chunk_digest = chain.digest
             WHERE chain.depth <= ?2
         )
         SELECT chain.depth, c.raw_size, c.codec, c.payload
         FROM chain JOIN chunks c ON c.digest = chain.digest
         ORDER BY chain.depth DESC",
    )?;
    let mut rows = statement.query(params![digest.as_slice(), MAX_DELTA_DEPTH as i64])?;
    let mut raw: Option<Vec<u8>> = None;
    let mut chain_depth = None;
    let mut expected = 0;
    while let Some(row) = rows.next()? {
        let depth: usize = row.get::<_, i64>(0)? as usize;
        let raw_size: i64 = row.get(1)?;
        let codec: String = row.get(2)?;
        let payload: Vec<u8> = row.get(3)?;
        let chain_depth = *chain_depth.get_or_insert(depth);
        if depth > MAX_DELTA_DEPTH || depth != chain_depth - expected {
            return Err(LocalError::Integrity("invalid chunk delta chain".to_string()));
        }
        expected += 1;
        raw = Some(decode_chunk(&codec, raw_size, &payload, raw.as_deref())?);
    }
    match (raw, chain_depth) {
        (None, _) => Ok(None),
        (Some(raw), Some(depth)) if expected == depth + 1 => Ok(Some((raw, depth))),
        _ => Err(LocalError::Integrity("invalid chunk delta chain".to_string())),
    }
}

fn decode_stored_chunk(
    connection: &Connection,
    codec: &str,
    raw_size: i64,
    payload: &[u8],
    base: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let Some(base) = base else {
        return decode_chunk(codec, raw_size, payload, None);
    };
    let base: [u8; 32] = base
        .try_into()
        .map_err(|_| LocalError::Integrity("invalid chunk base digest".to_string()))?;
    let (base, _) = read_chunk(connection, &base)?
        .ok_or_else(|| LocalError::Integrity("chunk base is missing".to_string()))?;
    decode_chunk(codec, raw_size, payload, Some(&base))
}

/// `dictionary` is the base chunk's bytes when the chunk is a delta.
pub(crate) fn decode_chunk(
    codec: &str,
    raw_size: i64,
    payload: &[u8],
    dictionary: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let raw_size = usize::try_from(raw_size)
        .ok()
        .filter(|size| (1..=MAX_CHUNK).contains(size))
        .ok_or_else(|| LocalError::Integrity("invalid chunk raw size".to_string()))?;
    match codec {
        "RAW" if dictionary.is_some() => Err(LocalError::Integrity(
            "a delta chunk must be compressed".to_string(),
        )),
        "RAW" => {
            if payload.len() != raw_size {
                return Err(LocalError::Integrity(
                    "raw chunk payload size mismatch".to_string(),
                ));
            }
            Ok(payload.to_vec())
        }
        "ZSTD" => {
            if payload.is_empty() || payload.len() * 100 >= raw_size * 95 {
                return Err(LocalError::Integrity(
                    "invalid compressed chunk payload size".to_string(),
                ));
            }
            let frame_size = zstd::zstd_safe::find_frame_compressed_size(payload)
                .map_err(|code| LocalError::Integrity(format!("invalid Zstd frame: {code:?}")))?;
            if frame_size != payload.len() {
                return Err(LocalError::Integrity(
                    "compressed chunk contains trailing or concatenated frames".to_string(),
                ));
            }
            let mut decoder = zstd::stream::read::Decoder::with_dictionary(
                Cursor::new(payload),
                dictionary.unwrap_or_default(),
            )
            .map_err(|error| LocalError::Integrity(format!("invalid Zstd frame: {error}")))?;
            decoder
                .window_log_max(MAX_CHUNK.ilog2())
                .map_err(|error| LocalError::Integrity(format!("invalid Zstd window: {error}")))?;
            let mut decoder = decoder.single_frame().take((raw_size + 1) as u64);
            let mut raw = Vec::with_capacity(raw_size);
            decoder.read_to_end(&mut raw).map_err(|error| {
                LocalError::Integrity(format!("cannot decompress chunk: {error}"))
            })?;
            if raw.len() != raw_size {
                return Err(LocalError::Integrity(
                    "decompressed chunk size mismatch".to_string(),
                ));
            }
            Ok(raw)
        }
        _ => Err(LocalError::Integrity(format!(
            "unknown chunk codec {codec}"
        ))),
    }
}

fn encode_chunk(raw: &[u8]) -> Result<(&'static str, Vec<u8>)> {
    let compressed = zstd::bulk::compress(raw, ZSTD_LEVEL)
        .map_err(|error| LocalError::Invalid(format!("cannot compress chunk: {error}")))?;
    if compression_is_profitable(raw.len(), compressed.len()) {
        Ok(("ZSTD", compressed))
    } else {
        Ok(("RAW", raw.to_vec()))
    }
}

/// Compress `raw` with `base` as a raw content dictionary, when that pays.
fn encode_delta(raw: &[u8], base: &[u8]) -> Result<Option<Vec<u8>>> {
    let compressed = zstd::bulk::Compressor::with_dictionary(DELTA_ZSTD_LEVEL, base)
        .and_then(|mut compressor| compressor.compress(raw))
        .map_err(|error| LocalError::Invalid(format!("cannot compress chunk: {error}")))?;
    Ok(compression_is_profitable(raw.len(), compressed.len()).then_some(compressed))
}

fn upgrade_version_1(connection: &Connection) -> Result<()> {
    let user_version: i64 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if user_version != 1 {
        return Ok(());
    }
    connection.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        let user_version: i64 =
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if user_version == 1 {
            connection.execute_batch(UPGRADE_VERSION_1)?;
            connection.pragma_update(None, "user_version", USER_VERSION)?;
        }
        connection.execute_batch("COMMIT")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = connection.execute_batch("ROLLBACK");
    }
    result
}

fn compression_is_profitable(raw_size: usize, stored_size: usize) -> bool {
    stored_size * 100 < raw_size * 95
}

fn batch_must_flush(rows: usize, payload: usize, next_payload: usize) -> bool {
    rows == MAX_BATCH_ROWS || payload.saturating_add(next_payload) > MAX_BATCH_PAYLOAD
}

pub(crate) fn blob_digest(blob_id: &ObjectId) -> Result<[u8; 32]> {
    let digest = blob_id
        .as_str()
        .strip_prefix("sha256:")
        .ok_or_else(|| LocalError::Integrity("unsupported blob digest".to_string()))?;
    let bytes = hex::decode(digest).map_err(|error| LocalError::Integrity(error.to_string()))?;
    bytes
        .try_into()
        .map_err(|_| LocalError::Integrity("invalid blob digest length".to_string()))
}

struct StreamingChunker<R> {
    reader: BufReader<R>,
    buffer: Vec<u8>,
    start: usize,
    eof: bool,
}

struct RegionStreamingChunker<'a, R> {
    reader: BufReader<R>,
    region_lengths: &'a [u64],
    region_index: usize,
    region_remaining: u64,
    buffer: Vec<u8>,
    start: usize,
}

impl<'a, R: Read> RegionStreamingChunker<'a, R> {
    fn new(reader: R, region_lengths: &'a [u64]) -> Self {
        Self {
            reader: BufReader::new(reader),
            region_lengths,
            region_index: 0,
            region_remaining: region_lengths.first().copied().unwrap_or(0),
            buffer: Vec::with_capacity(2 * MAX_CHUNK),
            start: 0,
        }
    }

    fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.start == self.buffer.len() && self.region_remaining == 0 {
            self.region_index += 1;
            let Some(length) = self.region_lengths.get(self.region_index) else {
                let mut trailing = [0_u8; 1];
                let read = self
                    .reader
                    .read(&mut trailing)
                    .map_err(|source| LocalError::Io {
                        action: "read object source",
                        path: PathBuf::from("<object source>"),
                        source,
                    })?;
                if read != 0 {
                    return Err(LocalError::Invalid(
                        "object continues beyond its chunk regions".to_string(),
                    ));
                }
                return Ok(None);
            };
            self.region_remaining = *length;
            self.buffer.clear();
            self.start = 0;
        } else if self.start != 0 && self.buffer.len() - self.start < MAX_CHUNK {
            self.buffer.copy_within(self.start.., 0);
            self.buffer.truncate(self.buffer.len() - self.start);
            self.start = 0;
        }

        while self.region_remaining != 0 && self.buffer.len() - self.start < MAX_CHUNK {
            let capacity = 2 * MAX_CHUNK - self.buffer.len();
            let count = capacity
                .min(128 * 1024)
                .min(usize::try_from(self.region_remaining).unwrap_or(usize::MAX));
            let old_len = self.buffer.len();
            self.buffer.resize(old_len + count, 0);
            let read = self
                .reader
                .read(&mut self.buffer[old_len..])
                .map_err(|source| LocalError::Io {
                    action: "read object source",
                    path: PathBuf::from("<object source>"),
                    source,
                })?;
            self.buffer.truncate(old_len + read);
            if read == 0 {
                return Err(LocalError::Invalid(
                    "object ended before its chunk regions".to_string(),
                ));
            }
            self.region_remaining -= read as u64;
        }

        let chunk = FastCDC::with_level(
            &self.buffer[self.start..],
            MIN_CHUNK,
            AVG_CHUNK,
            MAX_CHUNK as u32,
            Normalization::Level1,
        )
        .next()
        .expect("a nonempty region buffer emits one chunk");
        let chunk_start = self.start;
        self.start += chunk.length;
        Ok(Some(self.buffer[chunk_start..self.start].to_vec()))
    }
}

impl<R: Read> StreamingChunker<R> {
    fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            buffer: Vec::with_capacity(2 * MAX_CHUNK),
            start: 0,
            eof: false,
        }
    }

    fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.start != 0 && self.buffer.len() - self.start < MAX_CHUNK {
            self.buffer.copy_within(self.start.., 0);
            self.buffer.truncate(self.buffer.len() - self.start);
            self.start = 0;
        }
        while !self.eof && self.buffer.len() - self.start < MAX_CHUNK {
            let remaining = 2 * MAX_CHUNK - self.buffer.len();
            let mut temporary = vec![0_u8; remaining.min(128 * 1024)];
            let count = self
                .reader
                .read(&mut temporary)
                .map_err(|source| LocalError::Io {
                    action: "read object source",
                    path: PathBuf::from("<object source>"),
                    source,
                })?;
            if count == 0 {
                self.eof = true;
            } else {
                self.buffer.extend_from_slice(&temporary[..count]);
            }
        }
        if self.start == self.buffer.len() {
            return Ok(None);
        }
        let chunk = FastCDC::with_level(
            &self.buffer[self.start..],
            MIN_CHUNK,
            AVG_CHUNK,
            MAX_CHUNK as u32,
            Normalization::Level1,
        )
        .next()
        .expect("a nonempty buffer emits one chunk");
        if !self.eof && chunk.length > MAX_CHUNK {
            return Err(LocalError::Integrity(
                "FastCDC emitted an oversized chunk".to_string(),
            ));
        }
        let chunk_start = self.start;
        self.start += chunk.length;
        Ok(Some(self.buffer[chunk_start..self.start].to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_all_checks_each_object_against_its_id() {
        for bytes in [b"payload".to_vec(), Vec::new()] {
            let temporary = tempfile::tempdir().unwrap();
            let path = temporary.path().join("objects.sqlite3");
            let repo_id = RepoId::new();
            ChunkStore::create(&path, &repo_id).unwrap();
            let store = ChunkStore::new(path, &repo_id);
            let digest = Sha256::digest(&bytes).into();
            store.stage_blob(digest, bytes.len() as u64, Cursor::new(&bytes)).unwrap();
            store.complete_blob(&digest, bytes.len() as u64).unwrap();
            assert!(store.verify_all().is_ok());
        }
    }

    #[test]
    fn verify_all_checks_chunks_outside_complete_blobs() {
        for orphan in [false, true] {
            let temporary = tempfile::tempdir().unwrap();
            let path = temporary.path().join("objects.sqlite3");
            let repo_id = RepoId::new();
            ChunkStore::create(&path, &repo_id).unwrap();
            let store = ChunkStore::new(path, &repo_id);
            let bytes = b"interrupted payload";
            let digest = Sha256::digest(bytes).into();
            store.stage_blob(digest, bytes.len() as u64, Cursor::new(bytes)).unwrap();
            let connection = store.open_writable().unwrap();
            if orphan {
                // Leave a valid, unreferenced chunk rather than an incomplete blob.
                connection.execute("DELETE FROM blobs", []).unwrap();
            }
            assert!(store.verify_all().is_ok());
            assert!(store.verify_archive_closure().is_err());
            connection.execute("UPDATE chunks SET payload = zeroblob(raw_size), codec = 'RAW'", []).unwrap();
            assert!(store.verify_all().unwrap_err().to_string().contains("chunk digest mismatch"));
        }
    }

    #[test]
    fn backup_includes_committed_wal_pages() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.sqlite3");
        let destination = temporary.path().join("copy.sqlite3");
        let repo_id = RepoId::new();
        ChunkStore::create(&source, &repo_id).unwrap();
        let store = ChunkStore::new(source, &repo_id);
        let connection = store.open_writable().unwrap();
        connection.execute_batch("PRAGMA wal_autocheckpoint = 0").unwrap();
        let bytes = b"committed only in WAL";
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        connection.execute(
            "INSERT INTO chunks(digest,raw_size,codec,payload) VALUES(?1,?2,'RAW',?3)",
            params![digest.as_slice(), bytes.len() as i64, bytes.as_slice()],
        ).unwrap();
        // Keep the writer open so closing the last connection cannot checkpoint.
        store.backup_to(&destination).unwrap();
        let copy = ChunkStore::new(destination, &repo_id);
        let actual: Vec<u8> = copy.open_readonly().unwrap().query_row(
            "SELECT payload FROM chunks WHERE digest=?1", [digest.as_slice()], |row| row.get(0),
        ).unwrap();
        assert_eq!(actual, bytes);
        copy.verify_all().unwrap();
    }

    struct ShortReader<'a> {
        bytes: &'a [u8],
        position: usize,
        maximum: usize,
    }

    impl Read for ShortReader<'_> {
        fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
            let count = target
                .len()
                .min(self.maximum)
                .min(self.bytes.len() - self.position);
            target[..count].copy_from_slice(&self.bytes[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        }
    }

    #[test]
    fn equivalent_object_schema_is_accepted_but_changed_keys_are_rejected() {
        assert_eq!(SCHEMA, include_str!("../../../docs/spec/object-store.sql"));
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(&SCHEMA.replace("INTEGER NOT NULL", "INT NOT NULL").replace(
            "idx_blob_chunks_chunk_digest", "different_performance_index_name"
        )).unwrap();
        connection.execute_batch("CREATE INDEX extra_index ON chunks(raw_size)").unwrap();
        validate_schema(&connection).unwrap();
        connection.execute_batch("CREATE UNIQUE INDEX changed_key ON blobs(size_bytes)").unwrap();
        assert!(validate_schema(&connection).is_err());
    }

    #[test]
    fn streaming_boundaries_match_whole_buffer_fastcdc() {
        for length in [
            0,
            1,
            MIN_CHUNK as usize - 1,
            MIN_CHUNK as usize,
            AVG_CHUNK as usize + 17,
            MAX_CHUNK - 1,
            MAX_CHUNK,
            2 * MAX_CHUNK + 37,
            7 * MAX_CHUNK + 991,
        ] {
            let bytes = (0..length)
                .map(|index| ((index.wrapping_mul(31) + index / 97) & 0xff) as u8)
                .collect::<Vec<_>>();
            let expected = FastCDC::with_level(
                &bytes,
                MIN_CHUNK,
                AVG_CHUNK,
                MAX_CHUNK as u32,
                Normalization::Level1,
            )
            .map(|chunk| chunk.length)
            .collect::<Vec<_>>();
            let mut chunker = StreamingChunker::new(Cursor::new(&bytes));
            let mut actual = Vec::new();
            while let Some(chunk) = chunker.next_chunk().unwrap() {
                actual.push(chunk.len());
            }
            assert_eq!(actual, expected, "length {length}");

            for maximum in [17, 4093, MAX_CHUNK - 1] {
                let reader = ShortReader {
                    bytes: &bytes,
                    position: 0,
                    maximum,
                };
                let mut chunker = StreamingChunker::new(reader);
                let mut actual = Vec::new();
                while let Some(chunk) = chunker.next_chunk().unwrap() {
                    actual.push(chunk.len());
                }
                assert_eq!(actual, expected, "length {length}, short read {maximum}");
            }
        }
    }

    #[test]
    fn region_streaming_matches_independent_fastcdc_runs() {
        let region_lengths = [17_u64, u64::from(MIN_CHUNK), (3 * MAX_CHUNK + 91) as u64];
        let length = usize::try_from(region_lengths.iter().sum::<u64>()).unwrap();
        let bytes = (0..length)
            .map(|index| ((index.wrapping_mul(31) + index / 97) & 0xff) as u8)
            .collect::<Vec<_>>();
        let mut expected = Vec::new();
        let mut start = 0_usize;
        for length in region_lengths {
            let end = start + usize::try_from(length).unwrap();
            expected.extend(
                FastCDC::with_level(
                    &bytes[start..end],
                    MIN_CHUNK,
                    AVG_CHUNK,
                    MAX_CHUNK as u32,
                    Normalization::Level1,
                )
                .map(|chunk| {
                    bytes[start + chunk.offset..start + chunk.offset + chunk.length].to_vec()
                }),
            );
            start = end;
        }

        for maximum in [4093, MAX_CHUNK - 1] {
            let reader = ShortReader {
                bytes: &bytes,
                position: 0,
                maximum,
            };
            let mut chunker = RegionStreamingChunker::new(reader, &region_lengths);
            let mut actual = Vec::new();
            while let Some(chunk) = chunker.next_chunk().unwrap() {
                actual.push(chunk);
            }
            assert_eq!(actual, expected, "short read {maximum}");
            assert_eq!(actual.concat(), bytes);
        }
    }

    #[test]
    fn region_streaming_rejects_early_eof() {
        let mut chunker = RegionStreamingChunker::new(Cursor::new(b"short"), &[6]);
        let error = chunker.next_chunk().unwrap_err();
        assert!(error.to_string().contains("ended before its chunk regions"));
    }

    #[test]
    fn region_streaming_rejects_trailing_input() {
        let mut chunker = RegionStreamingChunker::new(Cursor::new(b"longer"), &[4]);
        assert_eq!(chunker.next_chunk().unwrap().unwrap(), b"long");
        let error = chunker.next_chunk().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("continues beyond its chunk regions")
        );
    }

    #[test]
    fn codec_rejects_trailing_and_concatenated_frames() {
        let raw = vec![b'x'; 64 * 1024];
        let compressed = zstd::bulk::compress(&raw, ZSTD_LEVEL).unwrap();
        assert_eq!(
            decode_chunk("ZSTD", raw.len() as i64, &compressed, None).unwrap(),
            raw
        );

        let mut trailing = compressed.clone();
        trailing.push(0);
        assert!(decode_chunk("ZSTD", raw.len() as i64, &trailing, None).is_err());

        let mut concatenated = compressed.clone();
        concatenated.extend_from_slice(&compressed);
        assert!(decode_chunk("ZSTD", raw.len() as i64, &concatenated, None).is_err());
        assert!(decode_chunk("ZSTD", raw.len() as i64 + 1, &compressed, None).is_err());
        assert!(decode_chunk("ZSTD", raw.len() as i64 - 1, &compressed, None).is_err());
        assert!(
            decode_chunk(
                "ZSTD",
                raw.len() as i64,
                &compressed[..compressed.len() - 1],
                None,
            )
            .is_err()
        );
        assert!(decode_chunk("UNKNOWN", raw.len() as i64, &compressed, None).is_err());
        assert!(decode_chunk("RAW", 0, &[], None).is_err());
    }

    #[test]
    fn codec_rejects_a_frame_with_an_oversized_window() {
        let raw = vec![b'x'; 64 * 1024];
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL).unwrap();
        encoder.window_log(MAX_CHUNK.ilog2() + 1).unwrap();
        encoder.include_contentsize(false).unwrap();
        encoder.write_all(&raw).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut permissive = zstd::stream::read::Decoder::new(Cursor::new(&compressed)).unwrap();
        permissive.window_log_max(MAX_CHUNK.ilog2() + 1).unwrap();
        let mut decoded = Vec::new();
        permissive.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, raw);

        let error = decode_chunk("ZSTD", raw.len() as i64, &compressed, None).unwrap_err();
        assert!(error.to_string().contains("too much memory"), "{error}");
    }

    #[test]
    fn codec_uses_zstd_only_when_it_is_profitable() {
        let incompressible = (0_u8..=u8::MAX).collect::<Vec<_>>();
        let (codec, payload) = encode_chunk(&incompressible).unwrap();
        assert_eq!(codec, "RAW");
        assert_eq!(payload, incompressible);

        let compressible = vec![b'x'; 64 * 1024];
        let (codec, payload) = encode_chunk(&compressible).unwrap();
        assert_eq!(codec, "ZSTD");
        assert!(payload.len() * 100 < compressible.len() * 95);
        assert_eq!(
            decode_chunk(codec, compressible.len() as i64, &payload, None).unwrap(),
            compressible
        );
    }

    #[test]
    fn compression_and_batch_boundaries_are_strict() {
        assert!(!compression_is_profitable(100, 95));
        assert!(compression_is_profitable(100, 94));
        assert!(!batch_must_flush(1023, MAX_BATCH_PAYLOAD - 1, 1));
        assert!(batch_must_flush(1024, 0, 1));
        assert!(batch_must_flush(1, MAX_BATCH_PAYLOAD, 1));
    }

    #[test]
    fn writable_connections_enforce_foreign_keys_and_full_sync() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("objects.sqlite3");
        let repo_id = RepoId::new();
        ChunkStore::create(&path, &repo_id).unwrap();
        let store = ChunkStore::new(path, &repo_id);
        let connection = store.open_writable().unwrap();
        let foreign_keys: i64 = connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        let synchronous: i64 = connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        assert_eq!(synchronous, 1);
        drop(connection);

        let bytes = b"stored bytes";
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        assert_eq!(
            store
                .stage_blob(digest, bytes.len() as u64, Cursor::new(bytes))
                .unwrap(),
            (digest, bytes.len() as u64)
        );
        store.complete_blob(&digest, bytes.len() as u64).unwrap();
        let connection = store.open_writable().unwrap();
        connection
            .execute("DELETE FROM blobs WHERE digest = ?1", [digest.as_slice()])
            .unwrap();
        let mappings: i64 = connection
            .query_row("SELECT count(*) FROM blob_chunks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mappings, 0);
    }

    #[test]
    fn format_identity_and_repository_identity_fail_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("objects.sqlite3");
        let repo_id = RepoId::new();
        ChunkStore::create(&path, &repo_id).unwrap();
        let store = ChunkStore::new(path.clone(), &repo_id);
        let connection = store.open_readonly().unwrap();
        let columns = connection
            .prepare("SELECT name FROM pragma_table_info('store_identity') ORDER BY cid")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(columns, ["singleton", "repo_id"]);
        drop(connection);

        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", USER_VERSION + 1).unwrap();
        drop(connection);
        assert!(store.open_readonly().is_err());

        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", USER_VERSION)
            .unwrap();
        connection
            .execute(
                "UPDATE store_identity SET repo_id = ?1",
                [RepoId::new().as_str()],
            )
            .unwrap();
        drop(connection);
        assert!(store.open_readonly().is_err());
    }

    #[test]
    fn valid_existing_chunk_encoding_is_reused_without_rewrite() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("objects.sqlite3");
        let repo_id = RepoId::new();
        ChunkStore::create(&path, &repo_id).unwrap();
        let store = ChunkStore::new(path, &repo_id);
        let bytes = vec![b'x'; 64 * 1024];
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        store
            .stage_blob(digest, bytes.len() as u64, Cursor::new(&bytes))
            .unwrap();
        store.complete_blob(&digest, bytes.len() as u64).unwrap();

        let connection = store.open_writable().unwrap();
        connection
            .execute("DELETE FROM blobs WHERE digest = ?1", [digest.as_slice()])
            .unwrap();
        connection
            .execute(
                "UPDATE chunks SET codec = 'RAW', payload = ?1 WHERE digest = ?2",
                params![&bytes, digest.as_slice()],
            )
            .unwrap();
        drop(connection);

        store
            .stage_blob(digest, bytes.len() as u64, Cursor::new(&bytes))
            .unwrap();
        store.complete_blob(&digest, bytes.len() as u64).unwrap();
        let connection = store.open_readonly().unwrap();
        let codec: String = connection
            .query_row(
                "SELECT codec FROM chunks WHERE digest = ?1",
                [digest.as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(codec, "RAW");
    }

    #[test]
    fn gc_deletes_across_fixed_batch_boundaries() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("objects.sqlite3");
        let repo_id = RepoId::new();
        ChunkStore::create(&path, &repo_id).unwrap();
        let store = ChunkStore::new(path, &repo_id);

        for index in 0..=MAX_BATCH_ROWS {
            let bytes = index.to_le_bytes();
            let digest: [u8; 32] = Sha256::digest(bytes).into();
            store
                .stage_blob(digest, bytes.len() as u64, Cursor::new(bytes))
                .unwrap();
            store.complete_blob(&digest, bytes.len() as u64).unwrap();
        }

        let (removed_objects, removed_bytes) = store.gc(&BTreeSet::new()).unwrap();
        assert_eq!(removed_objects, (MAX_BATCH_ROWS + 1) as u64);
        assert_eq!(
            removed_bytes,
            ((MAX_BATCH_ROWS + 1) * std::mem::size_of::<usize>()) as u64
        );
        assert!(store.inventory().unwrap().is_empty());
        let connection = store.open_readonly().unwrap();
        let chunks: i64 = connection
            .query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(chunks, 0);
    }

    fn store_version(store: &ChunkStore, bytes: &[u8], base: Option<&[u8; 32]>) -> [u8; 32] {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        store
            .stage_blob_regions(digest, bytes.len() as u64, bytes, &[bytes.len() as u64], base)
            .unwrap();
        store.complete_blob(&digest, bytes.len() as u64).unwrap();
        digest
    }

    fn text_version(version: usize) -> Vec<u8> {
        (0..2000)
            .map(|line| format!("line {line} of version {}\n", if line == 1000 { version } else { 0 }))
            .collect::<String>()
            .into_bytes()
    }

    fn count(store: &ChunkStore, table: &str) -> i64 {
        store
            .open_readonly()
            .unwrap()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_new_version_is_stored_against_the_previous_one_and_gc_keeps_its_base() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("objects.sqlite3");
        let repo_id = RepoId::new();
        ChunkStore::create(&path, &repo_id).unwrap();
        let store = ChunkStore::new(path, &repo_id);
        let (first, second) = (text_version(1), text_version(2));
        let first_digest = store_version(&store, &first, None);
        let second_digest = store_version(&store, &second, Some(&first_digest));
        assert_eq!(count(&store, "chunk_bases"), 1);
        let (full, delta): (i64, i64) = store
            .open_readonly()
            .unwrap()
            .query_row(
                "SELECT max(length(payload)), min(length(payload)) FROM chunks",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(delta * 20 < full, "delta {delta} bytes, full {full}");
        store.verify_all().unwrap();

        // The first version goes; its chunk stays as the second's base.
        let second_id = ObjectId::from_digest(second_digest);
        store
            .gc(&BTreeSet::from([hex::encode(second_digest)]))
            .unwrap();
        assert_eq!(count(&store, "chunks"), 2);
        let mut read = Vec::new();
        store.copy_blob(&second_id, second.len() as u64, &mut read).unwrap();
        assert_eq!(read, second);
        store.verify_archive_closure().unwrap();

        store.gc(&BTreeSet::new()).unwrap();
        assert_eq!(count(&store, "chunks"), 0);
        assert_eq!(count(&store, "chunk_bases"), 0);
    }

    #[test]
    fn a_largest_delta_fits_the_frame_window() {
        let text = |seed: u64| {
            let mut state = seed;
            let mut bytes = Vec::with_capacity(MAX_CHUNK);
            while bytes.len() < MAX_CHUNK {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                bytes.extend_from_slice(format!("{:x} ", state >> 40).as_bytes());
            }
            bytes.truncate(MAX_CHUNK);
            bytes
        };
        let base = text(1);
        let mut raw = base.clone();
        raw[MAX_CHUNK / 2..MAX_CHUNK / 2 + 8].copy_from_slice(b"changed!");
        let payload = encode_delta(&raw, &base).unwrap().unwrap();
        assert!(payload.len() < 1024, "{} bytes", payload.len());
        assert_eq!(decode_chunk("ZSTD", raw.len() as i64, &payload, Some(&base)).unwrap(), raw);
        assert!(decode_chunk("ZSTD", raw.len() as i64, &payload, None).is_err());
    }

    #[test]
    fn delta_chains_restart_with_a_full_chunk() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("objects.sqlite3");
        let repo_id = RepoId::new();
        ChunkStore::create(&path, &repo_id).unwrap();
        let store = ChunkStore::new(path, &repo_id);
        let mut base = None;
        for version in 0..=MAX_DELTA_DEPTH + 1 {
            base = Some(store_version(&store, &text_version(version), base.as_ref()));
        }
        assert_eq!(count(&store, "chunk_bases"), MAX_DELTA_DEPTH as i64);
        let connection = store.open_readonly().unwrap();
        let latest = complete_blob_chunks(&connection, &base.unwrap()).unwrap();
        assert_eq!(read_chunk(&connection, &latest[0]).unwrap().unwrap().1, 0);
        store.verify_all().unwrap();
    }

    #[test]
    fn a_version_1_store_gains_chunk_bases_when_opened() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("objects.sqlite3");
        let repo_id = RepoId::new();
        ChunkStore::create(&path, &repo_id).unwrap();
        let store = ChunkStore::new(path.clone(), &repo_id);
        let digest = store_version(&store, b"before the upgrade", None);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP INDEX idx_chunk_bases_base_digest;
                 DROP TABLE chunk_bases;
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);
        let mut read = Vec::new();
        store
            .copy_blob(&ObjectId::from_digest(digest), 18, &mut read)
            .unwrap();
        assert_eq!(read, b"before the upgrade");
        let version: i64 = store
            .open_readonly()
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, USER_VERSION);
    }

    #[test]
    fn large_blob_mapping_cleanup_is_restartable() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("objects.sqlite3");
        let repo_id = RepoId::new();
        ChunkStore::create(&path, &repo_id).unwrap();
        let store = ChunkStore::new(path, &repo_id);
        let mapping_count = MAX_BATCH_ROWS + 1;
        let bytes = vec![b'x'; mapping_count];
        let blob_digest: [u8; 32] = Sha256::digest(&bytes).into();
        let chunk_digest: [u8; 32] = Sha256::digest(b"x").into();

        insert_repeated_test_blob(&store, &blob_digest, &chunk_digest, mapping_count, true);

        let (removed_objects, removed_bytes) = store.gc(&BTreeSet::new()).unwrap();
        assert_eq!(removed_objects, 1);
        assert_eq!(removed_bytes, mapping_count as u64);

        insert_repeated_test_blob(&store, &blob_digest, &chunk_digest, mapping_count, false);

        store.recover_incomplete().unwrap();
        let connection = store.open_readonly().unwrap();
        for table in ["blobs", "blob_chunks"] {
            let count: i64 = connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
    }

    fn insert_repeated_test_blob(
        store: &ChunkStore,
        blob_digest: &[u8; 32],
        chunk_digest: &[u8; 32],
        mapping_count: usize,
        complete: bool,
    ) {
        let connection = store.open_writable().unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        connection
            .execute(
                "INSERT INTO blobs(digest, size_bytes, complete) VALUES (?1, ?2, ?3)",
                params![
                    blob_digest.as_slice(),
                    mapping_count as i64,
                    i64::from(complete)
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO chunks(digest, raw_size, codec, payload)
                 VALUES (?1, 1, 'RAW', ?2)",
                params![chunk_digest.as_slice(), b"x"],
            )
            .unwrap();
        for ordinal in 0..mapping_count {
            connection
                .execute(
                    "INSERT INTO blob_chunks(blob_digest, ordinal, chunk_digest)
                     VALUES (?1, ?2, ?3)",
                    params![
                        blob_digest.as_slice(),
                        ordinal as i64,
                        chunk_digest.as_slice()
                    ],
                )
                .unwrap();
        }
        connection.execute_batch("COMMIT").unwrap();
    }
}
