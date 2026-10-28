//! Bounded-memory RAW capture shared by bare and checked-out repositories.
use crate::{
    chunk_store::ChunkStore,
    error::{IoContext, LocalError, Result},
    repository::{rename_noreplace, sync_directory},
};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use synck_vcs::{
    object::{Codec, HEADER_BYTES, Header},
    types::ObjectId,
};

pub(crate) struct Captured {
    pub id: ObjectId,
    pub plain_digest: String,
    pub size: u64,
}

struct RawReader<R> {
    source: R,
    header: Header,
    position: usize,
    plain: Sha256,
    size: u64,
}
impl<R: Read> Read for RawReader<R> {
    fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
        if self.position < HEADER_BYTES {
            let count = target.len().min(HEADER_BYTES - self.position);
            target[..count]
                .copy_from_slice(&self.header.as_bytes()[self.position..self.position + count]);
            self.position += count;
            return Ok(count);
        }
        let count = self.source.read(target)?;
        self.plain.update(&target[..count]);
        self.size = self
            .size
            .checked_add(count as u64)
            .ok_or_else(|| std::io::Error::other("file size overflow"))?;
        Ok(count)
    }
}

pub(crate) fn capture<R: Read>(
    source: R,
    size: u64,
    regions: Option<Vec<u64>>,
    objects: &Path,
    store: Option<&ChunkStore>,
    checkpoint: bool,
) -> Result<Captured> {
    let header = Header::new(Codec::Raw).map_err(|e| LocalError::Invalid(e.to_string()))?;
    let staging_key: [u8; 32] = Sha256::digest(header.as_bytes()).into();
    let mut reader = RawReader {
        source,
        header,
        position: 0,
        plain: Sha256::new(),
        size: 0,
    };
    let total = size
        .checked_add(HEADER_BYTES as u64)
        .ok_or_else(|| LocalError::Invalid("file too large".into()))?;
    let digest = if let Some(store) = store {
        let mut boundaries = vec![HEADER_BYTES as u64];
        if size > 0 {
            boundaries.extend(regions.unwrap_or_else(|| vec![size]));
        }
        let (digest, stored_size) =
            store.stage_blob_regions(staging_key, total, &mut reader, &boundaries)?;
        if stored_size != total || reader.size != size {
            return Err(LocalError::Invalid(
                "file changed while being captured".into(),
            ));
        }
        store.address_staged_blob(&staging_key, &digest, total)?;
        if checkpoint {
            store.complete_blob(&digest, total)?;
        } else {
            store.complete_blob_before_batch_checkpoint(&digest, total)?;
        }
        digest
    } else {
        let temporary = objects.join(format!(".pending-{}", uuid::Uuid::new_v4()));
        let result = (|| -> Result<[u8; 32]> {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .at("create object staging", &temporary)?;
            let mut output = std::io::BufWriter::new(file);
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 128 * 1024];
            loop {
                let count = reader.read(&mut buffer).at("capture object", &temporary)?;
                if count == 0 {
                    break;
                }
                output
                    .write_all(&buffer[..count])
                    .at("write object", &temporary)?;
                hasher.update(&buffer[..count]);
            }
            if reader.size != size {
                return Err(LocalError::Invalid(
                    "file changed while being captured".into(),
                ));
            }
            output.flush().at("flush object", &temporary)?;
            output.get_ref().sync_all().at("sync object", &temporary)?;
            drop(output);
            let digest: [u8; 32] = hasher.finalize().into();
            let destination = objects.join(hex::encode(digest));
            rename_noreplace(&temporary, &destination).at("publish object", &destination)?;
            sync_directory(objects)?;
            Ok(digest)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?
    };
    Ok(Captured {
        id: ObjectId::from_digest(digest),
        plain_digest: hex::encode(reader.plain.finalize()),
        size: reader.size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_raw_objects_work_in_both_storage_modes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let repo = synck_vcs::types::RepoId::new();
        ChunkStore::create(&path, &repo).unwrap();
        let store = ChunkStore::new(path, &repo);
        for mode in [None, Some(&store)] {
            let captured = capture(&b""[..], 0, None, directory.path(), mode, true).unwrap();
            let mut stored = Vec::new();
            if let Some(store) = mode {
                store.copy_blob(&captured.id, HEADER_BYTES as u64, &mut stored).unwrap();
            } else {
                stored = std::fs::read(directory.path().join(hex::encode(captured.id.as_bytes()))).unwrap();
            }
            let object = synck_vcs::object::Object::verify(&captured.id, stored).unwrap();
            assert!(object.bytes().is_empty());
            assert_eq!(captured.plain_digest, hex::encode(Sha256::digest(b"")));
        }
    }

    #[test]
    fn salted_captures_deduplicate_plain_chunks_and_decode_exactly() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let repo = synck_vcs::types::RepoId::new();
        ChunkStore::create(&path, &repo).unwrap();
        let store = ChunkStore::new(path, &repo);
        let bytes = (0..700_000).map(|i| (i * 17) as u8).collect::<Vec<_>>();
        let first = capture(
            bytes.as_slice(),
            bytes.len() as u64,
            None,
            directory.path(),
            Some(&store),
            true,
        )
        .unwrap();
        let count = || {
            store
                .open_readonly()
                .unwrap()
                .query_row("SELECT count(*) FROM chunks", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        };
        let before = count();
        let second = capture(
            bytes.as_slice(),
            bytes.len() as u64,
            None,
            directory.path(),
            Some(&store),
            true,
        )
        .unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(first.plain_digest, second.plain_digest);
        assert_eq!(count(), before + 1, "only the salted header adds a chunk");
        for captured in [first, second] {
            let mut output = Vec::new();
            let mut decoder = synck_vcs::object::Decoder::new(captured.id.clone(), &mut output);
            store
                .copy_blob(
                    &captured.id,
                    captured.size + HEADER_BYTES as u64,
                    &mut decoder,
                )
                .unwrap();
            assert_eq!(decoder.finish().unwrap(), (bytes.len() as u64, Codec::Raw));
            assert_eq!(output, bytes);
        }
    }
}
