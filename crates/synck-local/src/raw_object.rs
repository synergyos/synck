//! Bounded-memory capture of PLAIN objects, shared by bare and checked-out repositories.
use crate::{
    chunk_store::ChunkStore,
    error::{IoContext, LocalError, Result},
    objects::verify_object_file,
    repository::{rename_noreplace, sync_directory},
};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use synck_vcs::types::ObjectId;

pub(crate) struct Captured {
    pub id: ObjectId,
    pub plain_digest: String,
    pub size: u64,
}

struct CountingReader<R> {
    source: R,
    size: u64,
}
impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
        let count = self.source.read(target)?;
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
    let mut reader = CountingReader { source, size: 0 };
    let digest = if let Some(store) = store {
        let staging_key: [u8; 32] = Sha256::digest(uuid::Uuid::new_v4().as_bytes()).into();
        let boundaries = if size > 0 {
            regions.unwrap_or_else(|| vec![size])
        } else {
            Vec::new()
        };
        let (digest, stored_size) =
            store.stage_blob_regions(staging_key, size, &mut reader, &boundaries)?;
        if stored_size != size || reader.size != size {
            return Err(LocalError::Invalid(
                "file changed while being captured".into(),
            ));
        }
        if !store.address_staged_blob(&staging_key, &digest, size)? {
            store.verify_blob(&ObjectId::from_digest(digest), size)?;
        } else if checkpoint {
            store.complete_blob(&digest, size)?;
        } else {
            store.complete_blob_before_batch_checkpoint(&digest, size)?;
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
            match rename_noreplace(&temporary, &destination) {
                Ok(()) => sync_directory(objects)?,
                // Identical bytes have one ID, and are stored once.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    verify_object_file(&destination, &hex::encode(digest), size)?;
                    std::fs::remove_file(&temporary).at("remove object staging", &temporary)?;
                }
                Err(error) => return Err(error).at("publish object", &destination),
            }
            Ok(digest)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?
    };
    Ok(Captured {
        id: ObjectId::from_digest(digest),
        plain_digest: hex::encode(digest),
        size: reader.size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(
        captured: &Captured,
        objects: &Path,
        store: Option<&ChunkStore>,
    ) -> Vec<u8> {
        let mut output = Vec::new();
        match store {
            Some(store) => store.copy_blob(&captured.id, captured.size, &mut output).unwrap(),
            None => output = std::fs::read(objects.join(&captured.plain_digest)).unwrap(),
        }
        output
    }

    #[test]
    fn empty_objects_work_in_both_storage_modes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let repo = synck_vcs::types::RepoId::new();
        ChunkStore::create(&path, &repo).unwrap();
        let store = ChunkStore::new(path, &repo);
        for mode in [None, Some(&store)] {
            let captured = capture(&b""[..], 0, None, directory.path(), mode, true).unwrap();
            assert!(stored(&captured, directory.path(), mode).is_empty());
            assert_eq!(captured.plain_digest, hex::encode(Sha256::digest(b"")));
            synck_vcs::object::verify_stored(&captured.id, synck_vcs::object::Form::Plain, b"")
                .unwrap();
        }
    }

    #[test]
    fn identical_bytes_share_one_id_in_both_storage_modes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.sqlite3");
        let repo = synck_vcs::types::RepoId::new();
        ChunkStore::create(&path, &repo).unwrap();
        let store = ChunkStore::new(path, &repo);
        let bytes = (0..700_000).map(|i| (i * 17) as u8).collect::<Vec<_>>();
        for mode in [None, Some(&store)] {
            let capture_bytes = || {
                capture(bytes.as_slice(), bytes.len() as u64, None, directory.path(), mode, true)
                    .unwrap()
            };
            let (first, second) = (capture_bytes(), capture_bytes());
            assert_eq!(first.id, second.id);
            assert_eq!(first.plain_digest, hex::encode(Sha256::digest(&bytes)));
            assert_eq!(stored(&second, directory.path(), mode), bytes);
        }
        let count: i64 = store
            .open_readonly()
            .unwrap()
            .query_row("SELECT count(*) FROM blobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
