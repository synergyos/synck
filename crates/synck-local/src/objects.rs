use std::fs::{self, File, OpenOptions as StdOpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use cap_fs_ext::{FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::fs::OpenOptions;
use sha2::{Digest, Sha256};
use synck_vcs::types::BlobId;
use uuid::Uuid;

use crate::chunk_store::StorageMode;
use crate::error::{IoContext, LocalError, Result};
use crate::ooxml::{OoxmlKind, chunk_region_lengths};
use crate::repository::{
    Repository, is_cap_link_or_reparse, is_link_or_reparse, open_regular_file_nofollow,
    sync_directory, sync_file,
};

#[derive(Debug, Eq, PartialEq)]
pub struct StorageReport {
    pub referenced_objects: u64,
    pub referenced_bytes: u64,
    pub unreachable_objects: u64,
    pub unreachable_bytes: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub struct GcReport {
    pub removed_objects: u64,
    pub removed_bytes: u64,
}

impl Repository {
    pub async fn optimize_storage(&mut self) -> Result<bool> {
        self.recover_current().await?;
        let mut portable = self.portable_view()?;
        let result = portable.optimize_storage().await;
        self.storage_mode = portable.storage_mode();
        self.config_mut().storage_mode = Some(self.storage_mode);
        result
    }

    pub async fn storage_report(&self) -> Result<StorageReport> {
        self.recover_current().await?;
        self.portable_view()?.storage_report().await
    }

    pub async fn gc(&self) -> Result<GcReport> {
        self.recover_current().await?;
        self.portable_view()?.gc().await
    }

    #[cfg(test)]
    pub(crate) fn install_object(
        &self,
        relative: &Path,
        display: &str,
        expected_digest: &str,
        expected_size: u64,
    ) -> Result<BlobId> {
        self.install_object_from_inner(
            self.root(),
            relative,
            display,
            expected_digest,
            expected_size,
            true,
        )
    }

    pub(crate) fn install_object_before_commit(
        &self,
        relative: &Path,
        display: &str,
        expected_digest: &str,
        expected_size: u64,
    ) -> Result<BlobId> {
        self.install_object_from_inner(
            self.root(),
            relative,
            display,
            expected_digest,
            expected_size,
            false,
        )
    }

    pub(crate) fn checkpoint_commit_objects(&self) -> Result<()> {
        if self.storage_mode() == StorageMode::Chunked {
            self.object_store().checkpoint()?;
        }
        Ok(())
    }

    fn install_object_from_inner(
        &self,
        root: &cap_std::fs::Dir,
        relative: &Path,
        display: &str,
        expected_digest: &str,
        expected_size: u64,
        checkpoint_chunk_store: bool,
    ) -> Result<BlobId> {
        let mut source_options = OpenOptions::new();
        source_options.read(true).follow(FollowSymlinks::No);
        let source = root
            .open_with(relative, &source_options)
            .at("open", display)?;
        let before = source.metadata().at("inspect", display)?;
        if !before.file_type().is_file() || is_cap_link_or_reparse(&before) {
            return Err(LocalError::Invalid(format!(
                "unsupported filesystem entry: {display}"
            )));
        }
        let identity = metadata_identity(&before, display)?;

        if self.storage_mode() == StorageMode::Chunked {
            let digest_bytes: [u8; 32] = hex::decode(expected_digest)
                .map_err(|error| LocalError::Invalid(format!("invalid expected digest: {error}")))?
                .try_into()
                .map_err(|_| LocalError::Invalid("invalid expected digest length".to_string()))?;
            let store = self.object_store();
            let was_complete = store.contains_complete(&digest_bytes, expected_size)?;
            let (digest, size) = if was_complete {
                let blob_id = BlobId::parse(format!("sha256:{expected_digest}"))
                    .map_err(|error| LocalError::Invalid(error.to_string()))?;
                store.verify_blob(&blob_id, expected_size)?;
                hash_reader(source, display)?
            } else {
                let regions = if let Some(kind) = OoxmlKind::from_path(relative) {
                    let validation_source = source.try_clone().at("open", display)?.into_std();
                    chunk_region_lengths(validation_source, expected_size, kind)
                } else {
                    None
                };
                if let Some(regions) = regions {
                    store.stage_blob_regions(digest_bytes, expected_size, source, &regions)?
                } else {
                    store.stage_blob(digest_bytes, expected_size, source)?
                }
            };
            let after = root.symlink_metadata(relative).at("inspect", display)?;
            if !after.file_type().is_file()
                || is_cap_link_or_reparse(&after)
                || identity != metadata_identity(&after, display)?
                || digest != digest_bytes
                || size != expected_size
            {
                return Err(LocalError::Invalid(format!(
                    "file changed while being captured: {display}"
                )));
            }
            if !was_complete {
                if checkpoint_chunk_store {
                    store.complete_blob(&digest_bytes, expected_size)?;
                } else {
                    store.complete_blob_before_batch_checkpoint(&digest_bytes, expected_size)?;
                }
            }
            return BlobId::parse(format!("sha256:{expected_digest}"))
                .map_err(|error| LocalError::Invalid(error.to_string()));
        }

        let temporary_path = self
            .metadata_path()
            .join("tmp")
            .join(format!("object-{}", Uuid::new_v4()));
        let temporary = StdOpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
            .at("create", &temporary_path)?;
        let capture = capture(source, temporary, display);
        let (digest, size) = match capture {
            Ok(value) => value,
            Err(error) => {
                let _ = fs::remove_file(&temporary_path);
                return Err(error);
            }
        };
        let after = root.symlink_metadata(relative).at("inspect", display)?;
        if !after.file_type().is_file()
            || is_cap_link_or_reparse(&after)
            || identity != metadata_identity(&after, display)?
            || digest != expected_digest
            || size != expected_size
        {
            let _ = fs::remove_file(&temporary_path);
            return Err(LocalError::Invalid(format!(
                "file changed while being captured: {display}"
            )));
        }

        let blob_id =
            BlobId::parse(format!("sha256:{digest}")).expect("a SHA-256 digest is a valid blob ID");
        let final_path = self.object_path(&blob_id);
        let prefix_directory = final_path
            .parent()
            .expect("object path always has a prefix directory");
        match fs::create_dir(prefix_directory) {
            Ok(()) => sync_directory(self.metadata_path().join("objects").as_path())?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata =
                    fs::symlink_metadata(prefix_directory).at("inspect", prefix_directory)?;
                if !metadata.file_type().is_dir() || is_link_or_reparse(&metadata) {
                    let _ = fs::remove_file(&temporary_path);
                    return Err(LocalError::Integrity(format!(
                        "object prefix is not a directory: {}",
                        prefix_directory.display()
                    )));
                }
            }
            Err(source) => {
                let _ = fs::remove_file(&temporary_path);
                return Err(LocalError::Io {
                    action: "create object directory",
                    path: prefix_directory.to_path_buf(),
                    source,
                });
            }
        }

        match fs::hard_link(&temporary_path, &final_path) {
            Ok(()) => {
                sync_file(&final_path)?;
                sync_directory(prefix_directory)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_object_file(&final_path, &digest, size)?;
            }
            Err(source) => {
                let _ = fs::remove_file(&temporary_path);
                return Err(LocalError::Io {
                    action: "install object",
                    path: final_path,
                    source,
                });
            }
        }
        fs::remove_file(&temporary_path).at("remove", &temporary_path)?;
        Ok(blob_id)
    }

    pub(crate) fn verify_blob(&self, blob_id: &BlobId, size: u64) -> Result<()> {
        match self.storage_mode() {
            StorageMode::WholeFile => {
                let digest = blob_id
                    .as_str()
                    .strip_prefix("sha256:")
                    .expect("BlobId always has a sha256 prefix");
                verify_object_file(&self.object_path(blob_id), digest, size)
            }
            StorageMode::Chunked => self.object_store().verify_blob(blob_id, size),
        }
    }

    pub(crate) fn copy_blob<W: Write>(
        &self,
        blob_id: &BlobId,
        size: u64,
        target: &mut W,
    ) -> Result<()> {
        match self.storage_mode() {
            StorageMode::WholeFile => {
                copy_whole_file_blob(&self.object_path(blob_id), blob_id, size, target)
            }
            StorageMode::Chunked => self.object_store().copy_blob(blob_id, size, target),
        }
    }

    pub(crate) fn object_path(&self, blob_id: &BlobId) -> PathBuf {
        let digest = blob_id
            .as_str()
            .strip_prefix("sha256:")
            .expect("BlobId always has a sha256 prefix");
        self.metadata_path().join("objects").join(digest)
    }
}

fn capture(source: cap_std::fs::File, target: File, display: &str) -> Result<(String, u64)> {
    let mut source = BufReader::new(source);
    let mut target = BufWriter::new(target);
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 128 * 1_024];
    loop {
        let count = source.read(&mut buffer).at("read", display)?;
        if count == 0 {
            break;
        }
        target
            .write_all(&buffer[..count])
            .at("write object", display)?;
        hasher.update(&buffer[..count]);
        size = size.checked_add(count as u64).ok_or_else(|| {
            LocalError::Invalid(format!("file size overflow while capturing {display}"))
        })?;
    }
    target.flush().at("flush object", display)?;
    target.get_ref().sync_all().at("sync object", display)?;
    Ok((hex::encode(hasher.finalize()), size))
}

fn hash_reader<R: Read>(mut source: R, display: &str) -> Result<([u8; 32], u64)> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 128 * 1_024];
    loop {
        let count = source.read(&mut buffer).at("read", display)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size = size.checked_add(count as u64).ok_or_else(|| {
            LocalError::Invalid(format!("file size overflow while capturing {display}"))
        })?;
    }
    Ok((hasher.finalize().into(), size))
}

fn copy_whole_file_blob<W: Write>(
    path: &Path,
    blob_id: &BlobId,
    expected_size: u64,
    target: &mut W,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path).at("inspect", path)?;
    if !metadata.file_type().is_file()
        || is_link_or_reparse(&metadata)
        || metadata.len() != expected_size
    {
        return Err(LocalError::Integrity(format!(
            "object is not a regular file of the expected size: {}",
            path.display()
        )));
    }
    let mut source = BufReader::new(open_regular_file_nofollow(path)?);
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 128 * 1_024];
    loop {
        let count = source.read(&mut buffer).at("read", path)?;
        if count == 0 {
            break;
        }
        target.write_all(&buffer[..count]).at("copy object", path)?;
        hasher.update(&buffer[..count]);
        size += count as u64;
    }
    let digest = blob_id
        .as_str()
        .strip_prefix("sha256:")
        .expect("BlobId always has a sha256 prefix");
    if size != expected_size || hex::encode(hasher.finalize()) != digest {
        return Err(LocalError::Integrity(format!(
            "object verification failed: {}",
            path.display()
        )));
    }
    Ok(())
}

fn metadata_identity(
    metadata: &cap_std::fs::Metadata,
    display: &str,
) -> Result<(u64, u64, u64, cap_std::time::SystemTime)> {
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.modified().at("read modification time", display)?,
    ))
}

pub(crate) fn verify_object_file(path: &Path, expected_digest: &str, size: u64) -> Result<()> {
    let metadata = fs::symlink_metadata(path).at("inspect", path)?;
    if !metadata.file_type().is_file() || is_link_or_reparse(&metadata) {
        return Err(LocalError::Integrity(format!(
            "object is not a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() != size {
        return Err(LocalError::Integrity(format!(
            "object size mismatch: {}",
            path.display()
        )));
    }
    let mut file = BufReader::new(open_regular_file_nofollow(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 128 * 1_024];
    loop {
        let count = file.read(&mut buffer).at("read", path)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if hex::encode(hasher.finalize()) != expected_digest {
        return Err(LocalError::Integrity(format!(
            "object digest mismatch: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcdc::v2020::{FastCDC, Normalization};
    use synck_vcs::types::{
        COMMIT_FORMAT_VERSION, CommitPk, CommitProposal, canonical_commit_bytes_for_hash,
        canonical_event_proposal_bytes, commit_id_to_digest_bytes, compute_commit_id,
        compute_sha256_digest_bytes, sha256_digest_from_digest_bytes,
    };
    use synck_vcs_sqlite::format_timestamp;

    use crate::chunk_store::{AVG_CHUNK, MAX_CHUNK, MIN_CHUNK};

    fn mapping_digests(repository: &Repository, digest: &[u8; 32]) -> Vec<Vec<u8>> {
        let connection = repository.object_store().open_readonly().unwrap();
        connection
            .prepare(
                "SELECT chunk_digest FROM blob_chunks
                 WHERE blob_digest = ?1 ORDER BY ordinal",
            )
            .unwrap()
            .query_map([digest.as_slice()], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn chunk_digests(bytes: &[u8], regions: &[u64]) -> Vec<Vec<u8>> {
        let mut start = 0_usize;
        let mut digests = Vec::new();
        for length in regions {
            let end = start + usize::try_from(*length).unwrap();
            digests.extend(
                FastCDC::with_level(
                    &bytes[start..end],
                    MIN_CHUNK,
                    AVG_CHUNK,
                    MAX_CHUNK as u32,
                    Normalization::Level1,
                )
                .map(|chunk| {
                    Sha256::digest(
                        &bytes[start + chunk.offset..start + chunk.offset + chunk.length],
                    )
                    .to_vec()
                }),
            );
            start = end;
        }
        assert_eq!(start, bytes.len());
        digests
    }

    #[tokio::test]
    async fn new_valid_office_blobs_use_grouped_regions_by_default() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        crate::init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        let fixtures = [
            (
                OoxmlKind::Docx,
                "document.DOCX",
                "assets/templates/empty.docx",
            ),
            (
                OoxmlKind::Xlsx,
                "workbook.xlsx",
                "assets/templates/empty.xlsx",
            ),
            (
                OoxmlKind::Pptx,
                "slides.PpTx",
                "assets/templates/empty.pptx",
            ),
        ];
        let mut differs_from_generic = false;
        for (kind, name, fixture) in fixtures {
            let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture);
            let bytes = fs::read(&source).unwrap();
            let path = root.join(name);
            fs::write(&path, &bytes).unwrap();
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            let regions =
                chunk_region_lengths(File::open(&path).unwrap(), bytes.len() as u64, kind).unwrap();
            let expected = chunk_digests(&bytes, &regions);
            let generic = chunk_digests(&bytes, &[bytes.len() as u64]);

            repository
                .install_object(
                    Path::new(name),
                    name,
                    &hex::encode(digest),
                    bytes.len() as u64,
                )
                .unwrap();

            assert_eq!(mapping_digests(&repository, &digest), expected, "{name}");
            differs_from_generic |= expected != generic;
            repository
                .verify_blob(
                    &BlobId::parse(format!("sha256:{}", hex::encode(digest))).unwrap(),
                    bytes.len() as u64,
                )
                .unwrap();
        }
        assert!(differs_from_generic);
    }

    #[tokio::test]
    async fn complete_repository_office_writes_conversion_and_copy_use_grouped_regions() {
        let temporary = tempfile::tempdir().unwrap();
        for mode in [StorageMode::WholeFile, StorageMode::Chunked] {
            let root = temporary.path().join(format!("{mode:?}"));
            crate::portable::init_repository_with_mode(&root, "author", mode)
                .await
                .unwrap();
            let mut portable = crate::portable::PortableRepository::open(&root).unwrap();
            for name in ["empty.docx", "empty.xlsx", "empty.pptx"] {
                let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("assets/templates")
                    .join(name);
                portable.create_file(None, name, &source).await.unwrap();
            }
            drop(portable);
            let mut repository = Repository::discover(&root).await.unwrap();
            repository.optimize_storage().await.unwrap();
            for name in ["empty.docx", "empty.xlsx", "empty.pptx"] {
                let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("assets/templates")
                    .join(name);
                let bytes = fs::read(&source).unwrap();
                let digest: [u8; 32] = Sha256::digest(&bytes).into();
                let regions = chunk_region_lengths(
                    File::open(&source).unwrap(),
                    bytes.len() as u64,
                    OoxmlKind::from_path(&source).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    mapping_digests(&repository, &digest),
                    chunk_digests(&bytes, &regions)
                );
            }
            drop(repository);
            let copy = temporary.path().join(format!("copy-{mode:?}"));
            crate::portable::PortableRepository::open(&root)
                .unwrap()
                .archive(&copy)
                .await
                .unwrap();
            let source_db =
                rusqlite::Connection::open(root.join(".synck/object-store.sqlite3")).unwrap();
            let copy_db =
                rusqlite::Connection::open(copy.join(".synck/object-store.sqlite3")).unwrap();
            let mappings = |connection: &rusqlite::Connection| {
                connection.prepare("SELECT blob_digest,ordinal,chunk_digest FROM blob_chunks ORDER BY blob_digest,ordinal").unwrap()
                    .query_map([], |r| Ok((r.get::<_,Vec<u8>>(0)?, r.get::<_,i64>(1)?, r.get::<_,Vec<u8>>(2)?))).unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>().unwrap()
            };
            assert_eq!(mappings(&source_db), mappings(&copy_db));
        }
    }

    #[tokio::test]
    async fn office_capture_rejects_bytes_beyond_the_expected_snapshot() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        crate::init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/templates/empty.docx");
        let expected = fs::read(fixture).unwrap();
        let digest: [u8; 32] = Sha256::digest(&expected).into();
        let mut changed = expected.clone();
        changed.extend_from_slice(b"changed after scan");
        fs::write(root.join("document.docx"), changed).unwrap();

        let error = repository
            .install_object(
                Path::new("document.docx"),
                "document.docx",
                &hex::encode(digest),
                expected.len() as u64,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("continues beyond its chunk regions")
        );
        assert!(
            !repository
                .object_store()
                .contains_complete(&digest, expected.len() as u64)
                .unwrap()
        );
    }

    #[tokio::test]
    async fn ineligible_office_names_fall_back_and_complete_blobs_keep_their_mapping() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        crate::init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();

        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/templates/empty.docx");
        let office = fs::read(fixture).unwrap();
        let office_digest: [u8; 32] = Sha256::digest(&office).into();
        fs::write(root.join("first.zip"), &office).unwrap();
        repository
            .install_object(
                Path::new("first.zip"),
                "first.zip",
                &hex::encode(office_digest),
                office.len() as u64,
            )
            .unwrap();
        let generic_mapping = mapping_digests(&repository, &office_digest);
        assert_eq!(
            generic_mapping,
            chunk_digests(&office, &[office.len() as u64])
        );

        fs::write(root.join("later.docx"), &office).unwrap();
        repository
            .install_object(
                Path::new("later.docx"),
                "later.docx",
                &hex::encode(office_digest),
                office.len() as u64,
            )
            .unwrap();
        assert_eq!(
            mapping_digests(&repository, &office_digest),
            generic_mapping
        );

        let invalid = (0..(3 * MAX_CHUNK + 91))
            .map(|index| ((index.wrapping_mul(31) + index / 97) & 0xff) as u8)
            .collect::<Vec<_>>();
        let invalid_digest: [u8; 32] = Sha256::digest(&invalid).into();
        fs::write(root.join("invalid.docx"), &invalid).unwrap();
        repository
            .install_object(
                Path::new("invalid.docx"),
                "invalid.docx",
                &hex::encode(invalid_digest),
                invalid.len() as u64,
            )
            .unwrap();
        assert_eq!(
            mapping_digests(&repository, &invalid_digest),
            chunk_digests(&invalid, &[invalid.len() as u64])
        );
    }

    #[tokio::test]
    async fn failed_append_leaves_an_object_that_gc_removes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("file"), b"uncommitted").unwrap();
        crate::init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        let baseline = repository.storage_report().await.unwrap();
        let digest = hex::encode(Sha256::digest(b"uncommitted"));
        repository
            .install_object(Path::new("file"), "file", &digest, 11)
            .unwrap();

        let error = repository
            .core()
            .append(
                repository.config().repo_id().clone(),
                CommitProposal {
                    author_id: "author".to_string(),
                    committer_id: "author".to_string(),
                    message: Some("must fail".to_string()),
                    occurred_at: None,
                    operation_id: Some(Uuid::new_v4().to_string()),
                    events: Vec::new(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("empty commit"));

        assert_eq!(
            repository.storage_report().await.unwrap(),
            StorageReport {
                referenced_objects: baseline.referenced_objects,
                referenced_bytes: baseline.referenced_bytes,
                unreachable_objects: 1,
                unreachable_bytes: 11,
            }
        );
        assert_eq!(
            repository.gc().await.unwrap(),
            GcReport {
                removed_objects: 1,
                removed_bytes: 11,
            }
        );
        assert_eq!(
            repository
                .storage_report()
                .await
                .unwrap()
                .unreachable_objects,
            0
        );
    }

    #[tokio::test]
    async fn reachability_includes_objects_referenced_only_by_version_sidecars() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("version"), b"named only").unwrap();
        crate::init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        let baseline = repository.storage_report().await.unwrap();
        let digest = hex::encode(Sha256::digest(b"named only"));
        repository
            .install_object(Path::new("version"), "version", &digest, 10)
            .unwrap();
        let file_item_id = repository.committed_tree().await.unwrap()[".synckignore"]
            .item_id
            .clone();
        let version_id = Uuid::now_v7().to_string();
        let events = synck_file_folder::file_version_set::automatic_initial_events(
            repository.config().repo_id(),
            &file_item_id,
            &version_id,
            true,
            2,
            0,
            format!("sha256:{digest}"),
            Some(10),
        )
        .unwrap();

        // Normal append rejects frozen sidecars. Seed canonical immutable
        // history at the storage boundary so this test exercises reachability.
        let payloads = events
            .iter()
            .map(|event| canonical_event_proposal_bytes(event).unwrap())
            .collect::<Vec<_>>();
        let event_digests = payloads
            .iter()
            .map(|payload| compute_sha256_digest_bytes(payload))
            .collect::<Vec<_>>();
        let mut event_stream = Sha256::new();
        for event_digest in &event_digests {
            event_stream.update([synck_vcs::types::COMMIT_EVENT_FAMILY_ITEM]);
            event_stream.update(event_digest);
        }
        let events_digest_bytes: [u8; 32] = event_stream.finalize().into();
        let events_digest = sha256_digest_from_digest_bytes(&events_digest_bytes).unwrap();
        let author = "author".to_string();
        let message = "version sidecar";
        let commit_bytes = canonical_commit_bytes_for_hash(
            &author,
            &author,
            None,
            Some(message),
            None,
            events.len(),
            &events_digest,
        )
        .unwrap();
        let commit_id = commit_id_to_digest_bytes(&compute_commit_id(&commit_bytes)).to_vec();
        let commit_pk = CommitPk::new();
        let sidecar_item_id = events[0].item_id.as_ref().unwrap();
        let sidecar_name = events[0].create_item.as_ref().unwrap().name.as_str();
        let created_at = format_timestamp(synck_vcs::types::now());
        let repo_id = repository.config().repo_id().as_str();
        let mut connection = repository.storage().open_writable().unwrap();
        let transaction = connection.transaction().unwrap();
        let repo_seq: i64 = transaction
            .query_row(
                "SELECT COALESCE(max(repo_seq), 0) + 1 FROM vcs_commits WHERE repo_id = ?1",
                [repo_id],
                |row| row.get(0),
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO vcs_commits (
                     repo_id, commit_pk, commit_id, repo_seq, operation_id,
                     format_version, author_id, committer_id, message, occurred_at, created_at,
                     event_count, events_digest, import_job_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, NULL, ?9, ?10, ?11, NULL)",
                rusqlite::params![
                    repo_id,
                    commit_pk.as_str(),
                    commit_id,
                    repo_seq,
                    Uuid::new_v4().to_string(),
                    COMMIT_FORMAT_VERSION,
                    author,
                    message,
                    created_at,
                    i64::try_from(events.len()).unwrap(),
                    events_digest_bytes.as_slice(),
                ],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO vcs_items (
                     repo_id, item_id, item_type, parent_folder_id, name, name_key,
                     head_seq, ref_head_seq, lifecycle_state, created_at, created_by,
                     sequencing_mode, federation_stream_id
                 ) VALUES (?1, ?2, 'FILE_VERSION_SET', NULL, ?3, ?3, ?4, 0,
                           'LIVE', ?5, ?6, 'LOCAL', NULL)",
                rusqlite::params![
                    repo_id,
                    sidecar_item_id.as_str(),
                    sidecar_name,
                    i64::try_from(events.len()).unwrap(),
                    created_at,
                    author,
                ],
            )
            .unwrap();
        for (index, ((event, payload), event_digest)) in
            events.iter().zip(&payloads).zip(&event_digests).enumerate()
        {
            transaction
                .execute(
                    "INSERT INTO vcs_events (
                         repo_id, commit_pk, commit_event_idx, item_id, item_seq, event_type,
                         payload, event_payload_digest, created_at, parent_folder_id,
                         parent_folder_id_before
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL)",
                    rusqlite::params![
                        repo_id,
                        commit_pk.as_str(),
                        i64::try_from(index).unwrap(),
                        sidecar_item_id.as_str(),
                        i64::try_from(index + 1).unwrap(),
                        event.event_type,
                        payload,
                        event_digest.as_slice(),
                        created_at,
                    ],
                )
                .unwrap();
        }
        transaction
            .execute(
                "INSERT INTO vcs_repo_activity_heads (repo_id, next_repo_seq, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (repo_id) DO UPDATE SET
                     next_repo_seq = excluded.next_repo_seq,
                     updated_at = excluded.updated_at",
                rusqlite::params![repo_id, repo_seq + 1, created_at],
            )
            .unwrap();
        transaction.commit().unwrap();
        drop(connection);

        assert_eq!(
            repository.storage_report().await.unwrap(),
            StorageReport {
                referenced_objects: baseline.referenced_objects + 1,
                referenced_bytes: baseline.referenced_bytes + 10,
                unreachable_objects: 0,
                unreachable_bytes: 0,
            }
        );
        repository
            .verify_blob(&BlobId::parse(format!("sha256:{digest}")).unwrap(), 10)
            .unwrap();
    }
}
