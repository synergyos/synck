use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::Path;

use cap_fs_ext::{FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::fs::OpenOptions;
use sha2::{Digest, Sha256};
use synck_vcs::types::ObjectId;

use crate::chunk_store::StorageMode;
use crate::error::{IoContext, LocalError, Result};
use crate::ooxml::{OoxmlKind, chunk_region_lengths};
use crate::repository::{
    Repository, is_cap_link_or_reparse, is_link_or_reparse, open_regular_file_nofollow,
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
    ) -> Result<ObjectId> {
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
    ) -> Result<ObjectId> {
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
    ) -> Result<ObjectId> {
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

        let regions = if let Some(kind) = OoxmlKind::from_path(relative) {
            chunk_region_lengths(source.try_clone().at("open", display)?.into_std(), expected_size, kind)
        } else { None };
        let mut source = source;
        std::io::Seek::rewind(&mut source).at("rewind capture source", display)?;
        let store = self.object_store();
        let captured = crate::raw_object::capture(source, expected_size, regions,
            &self.metadata_path().join("objects"),
            (self.storage_mode() == StorageMode::Chunked).then_some(&store), checkpoint_chunk_store)?;
        let after = root.symlink_metadata(relative).at("inspect", display)?;
        if !after.file_type().is_file() || is_cap_link_or_reparse(&after)
            || identity != metadata_identity(&after, display)?
            || captured.plain_digest != expected_digest || captured.size != expected_size
        {
            return Err(LocalError::Invalid(format!("file changed while being captured: {display}")));
        }
        self.record_plain_digest(&captured.id, &captured.plain_digest)?;
        Ok(captured.id)
    }

    pub(crate) fn verify_blob(&self, blob_id: &ObjectId, size: u64) -> Result<()> {
        self.copy_blob(blob_id, size, &mut std::io::sink())
    }

    pub(crate) fn copy_blob<W: Write>(&self, blob_id: &ObjectId, size: u64, target: &mut W) -> Result<()> {
        let actual = self.portable_view()?.copy_blob(blob_id, target)?;
        if actual != size { return Err(LocalError::Integrity("decoded object size mismatch".into())); }
        Ok(())
    }
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

pub(crate) struct PlainDigest(pub(crate) Sha256);
impl Default for PlainDigest { fn default() -> Self { Self(Sha256::new()) } }
impl Write for PlainDigest {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.update(bytes); Ok(bytes.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
impl PlainDigest { pub(crate) fn finish(self) -> String { hex::encode(self.0.finalize()) } }

impl Repository {
    pub(crate) fn record_plain_digest(&self, id: &ObjectId, digest: &str) -> Result<()> {
        let connection = crate::local_database::open_writable(self.storage())?;
        connection.execute("INSERT INTO local_object_digests(repo_id,object_id,plain_sha256) VALUES(?1,?2,?3) ON CONFLICT(repo_id,object_id) DO UPDATE SET plain_sha256=excluded.plain_sha256",
            rusqlite::params![self.config().repo_id().as_str(), id.as_str(), digest])?;
        Ok(())
    }
    pub(crate) fn cached_plain_digests(&self) -> Result<std::collections::HashMap<ObjectId, String>> {
        let connection = crate::local_database::open_readonly(self.storage())?;
        let mut statement = connection.prepare("SELECT object_id,plain_sha256 FROM local_object_digests WHERE repo_id=?1")?;
        let rows = statement.query_map([self.config().repo_id().as_str()], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?;
        let mut digests = std::collections::HashMap::new();
        for row in rows {
            let (id,digest) = row?;
            let id = ObjectId::parse(id).map_err(|e| LocalError::Integrity(e.to_string()))?;
            ObjectId::parse(format!("sha256:{digest}")).map_err(|e| LocalError::Integrity(e.to_string()))?;
            digests.insert(id,digest);
        }
        Ok(digests)
    }
    pub(crate) fn plain_digest(&self, id: &ObjectId) -> Result<String> {
        use rusqlite::OptionalExtension;
        let connection = crate::local_database::open_readonly(self.storage())?;
        let cached: Option<String> = connection.query_row("SELECT plain_sha256 FROM local_object_digests WHERE repo_id=?1 AND object_id=?2",
            rusqlite::params![self.config().repo_id().as_str(),id.as_str()], |row| row.get(0)).optional()?;
        if let Some(digest) = cached { return Ok(digest); }
        drop(connection);
        let mut digest = PlainDigest::default();
        self.portable_view()?.copy_blob(id,&mut digest)?;
        let digest = digest.finish();
        self.record_plain_digest(id,&digest)?;
        Ok(digest)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use uuid::Uuid;
    use super::*;
    use fastcdc::v2020::{FastCDC, Normalization};
    use synck_vcs::types::CommitProposal;

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

            let id = repository
                .install_object(
                    Path::new(name),
                    name,
                    &hex::encode(digest),
                    bytes.len() as u64,
                )
                .unwrap();

            assert_eq!(mapping_digests(&repository, &id.as_bytes()), expected, "{name}");
            differs_from_generic |= expected != generic;
            repository
                .verify_blob(
                    &id,
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
            let tree = repository.committed_tree().await.unwrap();
            for name in ["empty.docx", "empty.xlsx", "empty.pptx"] {
                let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("assets/templates")
                    .join(name);
                let bytes = fs::read(&source).unwrap();
                let digest = tree[name].blob_id.as_ref().unwrap().as_bytes();
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
        let first = repository
            .install_object(
                Path::new("first.zip"),
                "first.zip",
                &hex::encode(office_digest),
                office.len() as u64,
            )
            .unwrap();
        let generic_mapping = mapping_digests(&repository, &first.as_bytes());
        assert_eq!(
            generic_mapping,
            chunk_digests(&office, &[office.len() as u64])
        );

        fs::write(root.join("later.docx"), &office).unwrap();
        let second = repository
            .install_object(
                Path::new("later.docx"),
                "later.docx",
                &hex::encode(office_digest),
                office.len() as u64,
            )
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            mapping_digests(&repository, &first.as_bytes()),
            generic_mapping
        );

        let invalid = (0..(3 * MAX_CHUNK + 91))
            .map(|index| ((index.wrapping_mul(31) + index / 97) & 0xff) as u8)
            .collect::<Vec<_>>();
        let invalid_digest: [u8; 32] = Sha256::digest(&invalid).into();
        fs::write(root.join("invalid.docx"), &invalid).unwrap();
        let invalid_id = repository
            .install_object(
                Path::new("invalid.docx"),
                "invalid.docx",
                &hex::encode(invalid_digest),
                invalid.len() as u64,
            )
            .unwrap();
        assert_eq!(
            mapping_digests(&repository, &invalid_id.as_bytes()),
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


}
