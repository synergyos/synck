use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};
use synck_local::{ChangeKind, CommitOutcome, HiddenFilesMode, LocalError, Repository, init};

#[cfg(target_os = "macos")]
fn set_native_hidden(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let result = unsafe { libc::chflags(path.as_ptr(), libc::UF_HIDDEN) };
    assert_eq!(
        result,
        0,
        "chflags failed: {}",
        std::io::Error::last_os_error()
    );
}

#[cfg(windows)]
fn add_windows_attribute(path: &Path, attribute: u32) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, INVALID_FILE_ATTRIBUTES, SetFileAttributesW,
    };

    let path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let attributes = unsafe { GetFileAttributesW(path.as_ptr()) };
    assert_ne!(attributes, INVALID_FILE_ATTRIBUTES);
    let result = unsafe { SetFileAttributesW(path.as_ptr(), attributes | attribute) };
    assert_ne!(
        result,
        0,
        "SetFileAttributesW failed: {}",
        std::io::Error::last_os_error()
    );
}

#[cfg(windows)]
fn set_native_hidden(path: &Path) {
    add_windows_attribute(
        path,
        windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN,
    );
}

#[cfg(windows)]
fn set_native_system(path: &Path) {
    add_windows_attribute(
        path,
        windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_SYSTEM,
    );
}

#[cfg(windows)]
fn create_junction(link: &Path, target: &Path) {
    let output = std::process::Command::new("cmd")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mklink failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_junctions_are_rejected_or_removed_without_traversal() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    let external = temporary.path().join("external");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&external).unwrap();
    fs::write(external.join("sentinel"), b"outside").unwrap();
    init(&root, "author").await.unwrap();

    let working_junction = root.join("linked");
    create_junction(&working_junction, &external);
    let repository = Repository::discover(&root).await.unwrap();
    let error = repository.status().await.unwrap_err();
    assert!(error.to_string().contains("unsupported filesystem entry"));
    drop(repository);
    fs::remove_dir(&working_junction).unwrap();

    let temporary_junction = root.join(".synck").join("tmp").join("linked");
    create_junction(&temporary_junction, &external);
    drop(Repository::discover(&root).await.unwrap());
    assert!(!temporary_junction.exists());
    assert_eq!(fs::read(external.join("sentinel")).unwrap(), b"outside");
}

#[tokio::test]
async fn visible_root_names_cannot_use_the_version_sidecar_namespace() {
    let temporary = tempfile::tempdir().unwrap();
    init(temporary.path(), "author").await.unwrap();
    fs::write(
        temporary
            .path()
            .join(".__synck_file_version_set_not-a-sidecar"),
        b"content",
    )
    .unwrap();
    let repository = Repository::discover(temporary.path()).await.unwrap();
    let error = repository.status().await.unwrap_err();
    assert!(error.to_string().contains("reserved internal root name"));
}

#[tokio::test]
async fn explicit_move_rejects_a_reserved_root_name_before_filesystem_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.txt"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let reserved = ".__synck_file_version_set_moved-document";

    let error = repository
        .move_path(
            root,
            Path::new("document.txt"),
            Path::new(reserved),
            "move document",
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("reserved internal root name"));
    assert!(root.join("document.txt").is_file());
    assert!(!root.join(reserved).exists());
    assert_eq!(repository.log(None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn repository_discovery_rejects_format_one() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    init(root, "author").await.unwrap();
    let config_path = root.join(".synck/config.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["format_version"] = serde_json::json!(1);
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();

    let error = match Repository::discover(root).await {
        Ok(_) => panic!("repository discovery accepted format 1"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("format-6"));
}

#[tokio::test]
async fn restore_worktree_discards_the_complete_structural_delta_without_history() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("Folder")).unwrap();
    fs::write(root.join("Folder/document.bin"), b"before").unwrap();
    fs::write(root.join("deleted.bin"), b"deleted").unwrap();
    fs::write(root.join("kind"), b"file").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let head = repository.log(Some(1)).await.unwrap()[0].commit_id.clone();

    fs::write(root.join("Folder/document.bin"), b"after").unwrap();
    fs::rename(root.join("Folder"), root.join("Moved")).unwrap();
    fs::remove_file(root.join("deleted.bin")).unwrap();
    fs::remove_file(root.join("kind")).unwrap();
    fs::create_dir(root.join("kind")).unwrap();
    fs::write(root.join("kind/child.bin"), b"child").unwrap();
    fs::write(root.join("added.bin"), b"added").unwrap();

    assert!(repository.restore_worktree().await.unwrap());
    assert_eq!(
        fs::read(root.join("Folder/document.bin")).unwrap(),
        b"before"
    );
    assert_eq!(fs::read(root.join("deleted.bin")).unwrap(), b"deleted");
    assert_eq!(fs::read(root.join("kind")).unwrap(), b"file");
    assert!(!root.join("Moved").exists());
    assert!(!root.join("added.bin").exists());
    assert!(repository.status().await.unwrap().is_empty());
    assert_eq!(repository.log(Some(1)).await.unwrap()[0].commit_id, head);
    assert!(!root.join(".synck/worktree-restore.json").exists());
    assert!(!root.join(".synck/worktree-restore-stage").exists());
    assert!(!repository.restore_worktree().await.unwrap());
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn restore_worktree_supports_empty_history_and_preserves_ignored_entries() {
    let empty = tempfile::tempdir().unwrap();
    init(empty.path(), "author").await.unwrap();
    let repository = Repository::discover(empty.path()).await.unwrap();
    fs::write(empty.path().join("uncommitted.bin"), b"uncommitted").unwrap();
    assert!(repository.restore_worktree().await.unwrap());
    assert!(!empty.path().join("uncommitted.bin").exists());
    assert_eq!(repository.log(None).await.unwrap().len(), 1);
    assert!(repository.status().await.unwrap().is_empty());

    let ignored = tempfile::tempdir().unwrap();
    let root = ignored.path();
    fs::write(root.join(".synckignore"), b"ignored/\n").unwrap();
    fs::write(root.join("tracked.bin"), b"before").unwrap();
    fs::create_dir(root.join("ignored")).unwrap();
    fs::write(root.join("ignored/private.bin"), b"private").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("tracked.bin"), b"after").unwrap();
    fs::write(root.join("added.bin"), b"added").unwrap();
    assert!(repository.restore_worktree().await.unwrap());
    assert_eq!(fs::read(root.join("tracked.bin")).unwrap(), b"before");
    assert_eq!(
        fs::read(root.join("ignored/private.bin")).unwrap(),
        b"private"
    );
    assert!(!root.join("added.bin").exists());
    assert!(repository.status().await.unwrap().is_empty());
}

#[tokio::test]
async fn arbitrary_revert_preserves_independent_later_history() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"first").unwrap();
    fs::write(root.join("other.bin"), b"other-0").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("document.bin"), b"second").unwrap();
    let target = match repository.commit("edit document").await.unwrap() {
        CommitOutcome::Committed(commit) => commit,
        CommitOutcome::Clean => panic!("document edit was clean"),
    };
    repository
        .move_path(
            root,
            Path::new("document.bin"),
            Path::new("renamed.bin"),
            "move document",
        )
        .await
        .unwrap();
    fs::write(root.join("other.bin"), b"other-1").unwrap();
    repository.commit("edit other").await.unwrap();

    repository.revert(&target, "revert old edit").await.unwrap();
    assert!(!root.join("document.bin").exists());
    assert_eq!(fs::read(root.join("renamed.bin")).unwrap(), b"first");
    assert_eq!(fs::read(root.join("other.bin")).unwrap(), b"other-1");
    assert_eq!(repository.log(None).await.unwrap().len(), 6);
    assert!(repository.status().await.unwrap().is_empty());
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn arbitrary_revert_rejects_overlapping_later_content() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("document.bin"), b"second").unwrap();
    let target = match repository.commit("second").await.unwrap() {
        CommitOutcome::Committed(commit) => commit,
        CommitOutcome::Clean => panic!("second version was clean"),
    };
    fs::write(root.join("document.bin"), b"third").unwrap();
    repository.commit("third").await.unwrap();

    let error = repository
        .revert(&target, "must conflict")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("later history overlaps item"));
    assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"third");
    assert_eq!(repository.log(None).await.unwrap().len(), 4);
    assert!(!root.join(".synck/revert.json").exists());
    assert!(!root.join(".synck/revert-stage").exists());
    assert!(repository.status().await.unwrap().is_empty());
}

#[tokio::test]
async fn historical_tree_export_reconstructs_repository_and_folder_snapshots() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("work");
    let output = temporary.path().join("output");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&output).unwrap();
    fs::create_dir_all(root.join("Folder/empty")).unwrap();
    fs::write(root.join("Folder/document.bin"), b"first").unwrap();
    fs::write(root.join("removed.bin"), b"removed").unwrap();
    init(&root, "author").await.unwrap();
    let repository = Repository::discover(&root).await.unwrap();
    let initial = match repository.commit("initial").await.unwrap() {
        CommitOutcome::Committed(commit) => commit,
        CommitOutcome::Clean => panic!("initial tree was clean"),
    };
    repository
        .move_path(
            &root,
            Path::new("Folder"),
            Path::new("Renamed"),
            "rename folder",
        )
        .await
        .unwrap();
    fs::write(root.join("Renamed/document.bin"), b"second").unwrap();
    fs::remove_file(root.join("removed.bin")).unwrap();
    fs::write(root.join("added.bin"), b"added").unwrap();
    let latest = match repository.commit("later changes").await.unwrap() {
        CommitOutcome::Committed(commit) => commit,
        CommitOutcome::Clean => panic!("later tree was clean"),
    };

    let initial_tree = output.join("initial-tree");
    repository
        .export_tree(&root, &initial, None, &initial_tree)
        .await
        .unwrap();
    assert_eq!(
        fs::read(initial_tree.join("Folder/document.bin")).unwrap(),
        b"first"
    );
    assert!(initial_tree.join("Folder/empty").is_dir());
    assert_eq!(
        fs::read(initial_tree.join("removed.bin")).unwrap(),
        b"removed"
    );
    assert!(!initial_tree.join("Renamed").exists());
    assert!(!initial_tree.join(".synck").exists());

    let initial_folder = output.join("initial-folder");
    repository
        .export_tree(&root, &initial, Some(Path::new("Folder")), &initial_folder)
        .await
        .unwrap();
    assert_eq!(
        fs::read(initial_folder.join("document.bin")).unwrap(),
        b"first"
    );
    assert!(initial_folder.join("empty").is_dir());

    let latest_folder = output.join("latest-folder");
    repository
        .export_tree(&root, &latest, Some(Path::new("Renamed")), &latest_folder)
        .await
        .unwrap();
    assert_eq!(
        fs::read(latest_folder.join("document.bin")).unwrap(),
        b"second"
    );
    assert!(latest_folder.join("empty").is_dir());
    assert_eq!(repository.log(None).await.unwrap().len(), 4);
    assert!(repository.status().await.unwrap().is_empty());
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn historical_folder_export_does_not_read_unselected_file_content() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("work");
    let output = temporary.path().join("output");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&output).unwrap();
    fs::create_dir_all(root.join("Selected/Nested")).unwrap();
    fs::create_dir(root.join("Outside")).unwrap();
    fs::write(root.join("Selected/Nested/document.bin"), b"selected").unwrap();
    for number in 0..257 {
        fs::write(
            root.join(format!("Selected/page-{number:03}.txt")),
            number.to_string(),
        )
        .unwrap();
    }
    fs::write(root.join("Outside/unread.bin"), b"outside").unwrap();
    init(&root, "author").await.unwrap();
    let repository = Repository::discover(&root).await.unwrap();
    let commit = match repository.commit("initial").await.unwrap() {
        CommitOutcome::Committed(commit) => commit,
        CommitOutcome::Clean => panic!("initial tree was clean"),
    };
    repository
        .open_historical_view(commit.as_str())
        .await
        .unwrap();

    let connection = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE vcs_events SET payload = x'7b7d'
             WHERE item_id = (
                 SELECT item_id FROM vcs_items
                 WHERE repo_id = vcs_events.repo_id AND name = 'unread.bin'
             ) AND event_type = 'FILE::SET_CONTENT'",
            [],
        )
        .unwrap();
    drop(connection);

    let selected = output.join("selected");
    repository
        .export_tree(
            &root,
            commit.as_str(),
            Some(Path::new("Selected")),
            &selected,
        )
        .await
        .unwrap();
    assert_eq!(
        fs::read(selected.join("Nested/document.bin")).unwrap(),
        b"selected"
    );
    assert_eq!(fs::read(selected.join("page-256.txt")).unwrap(), b"256");
    assert!(!selected.join("Outside").exists());

    let error = repository
        .export_tree(&root, commit.as_str(), None, &output.join("complete"))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("historical content event digest disagrees"),
        "{error}"
    );
    assert!(!output.join("complete").exists());
}

fn install_unreachable_object(root: &Path, bytes: &[u8]) -> String {
    let digest = hex::encode(Sha256::digest(bytes));
    let digest_bytes = hex::decode(&digest).unwrap();
    let connection = rusqlite::Connection::open(root.join(".synck/object-store.sqlite3")).unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys = ON; BEGIN IMMEDIATE")
        .unwrap();
    connection
        .execute(
            "INSERT OR IGNORE INTO chunks(digest, raw_size, codec, payload)
             VALUES (?1, ?2, 'RAW', ?3)",
            rusqlite::params![digest_bytes, bytes.len() as i64, bytes],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO blobs(digest, size_bytes, complete) VALUES (?1, ?2, 1)",
            rusqlite::params![hex::decode(&digest).unwrap(), bytes.len() as i64],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO blob_chunks(blob_digest, ordinal, chunk_digest) VALUES (?1, 0, ?1)",
            [hex::decode(&digest).unwrap()],
        )
        .unwrap();
    connection.execute_batch("COMMIT").unwrap();
    digest
}

fn corrupt_blob(root: &Path, digest: &str) -> (Vec<u8>, Vec<u8>) {
    let connection = rusqlite::Connection::open(root.join(".synck/object-store.sqlite3")).unwrap();
    let (chunk_digest, mut payload): (Vec<u8>, Vec<u8>) = connection
        .query_row(
            "SELECT c.digest, c.payload
             FROM blob_chunks bc JOIN chunks c ON c.digest = bc.chunk_digest
             WHERE bc.blob_digest = ?1 ORDER BY bc.ordinal LIMIT 1",
            [hex::decode(digest).unwrap()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let saved = payload.clone();
    payload[0] ^= 0x5a;
    connection
        .execute(
            "UPDATE chunks SET payload = ?1 WHERE digest = ?2",
            rusqlite::params![payload, chunk_digest],
        )
        .unwrap();
    (chunk_digest, saved)
}

fn restore_chunk(root: &Path, chunk_digest: &[u8], payload: &[u8]) {
    rusqlite::Connection::open(root.join(".synck/object-store.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE chunks SET payload = ?1 WHERE digest = ?2",
            rusqlite::params![payload, chunk_digest],
        )
        .unwrap();
}

fn delete_unreachable_blob(root: &Path, digest: &str) {
    let connection = rusqlite::Connection::open(root.join(".synck/object-store.sqlite3")).unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys = ON; BEGIN IMMEDIATE")
        .unwrap();
    connection
        .execute(
            "DELETE FROM blobs WHERE digest = ?1",
            [hex::decode(digest).unwrap()],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM chunks WHERE NOT EXISTS (
                 SELECT 1 FROM blob_chunks WHERE chunk_digest = chunks.digest
             )",
            [],
        )
        .unwrap();
    connection.execute_batch("COMMIT").unwrap();
}

fn remove_object_database_family(root: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let path = root.join(format!(".synck/object-store.sqlite3{suffix}"));
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cannot remove {}: {error}", path.display()),
        }
    }
}

fn make_whole_file_repository(root: &Path, _bytes: &[u8], remove_object_database: bool) {
    let repository = synck_local::portable::PortableRepository::open(root).unwrap();
    let connection = rusqlite::Connection::open(root.join(".synck/object-store.sqlite3")).unwrap();
    let mut statement = connection
        .prepare("SELECT lower(hex(digest)) FROM blobs WHERE complete=1")
        .unwrap();
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap();
    for name in names {
        let name = name.unwrap();
        let mut file = fs::File::create(root.join(".synck/objects").join(&name)).unwrap();
        repository
            .copy_blob(
                &synck_vcs::types::ObjectId::parse(format!("sha256:{name}")).unwrap(),
                &mut file,
            )
            .unwrap();
    }
    drop(statement);
    drop(connection);
    drop(repository);
    let path = root.join(".synck/config.json");
    let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["storage_mode"] = serde_json::json!("WHOLE_FILE");
    fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    if remove_object_database {
        remove_object_database_family(root);
    }
}

async fn initialize_migration_repository(root: &Path, bytes: &[u8]) {
    fs::write(root.join("file.bin"), bytes).unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    drop(repository);
    make_whole_file_repository(root, bytes, false);
}

#[tokio::test]
async fn fresh_storage_is_chunked_deduplicated_and_compressed() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let bytes = vec![b'a'; 512 * 1024];
    fs::write(root.join("one.bin"), &bytes).unwrap();
    fs::write(root.join("two.bin"), &bytes).unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("duplicate files").await.unwrap();

    assert!(
        fs::read_dir(root.join(".synck/objects"))
            .unwrap()
            .next()
            .is_none()
    );
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".synck/config.json")).unwrap()).unwrap();
    assert_eq!(config["storage_mode"], "CHUNKED");
    let connection = rusqlite::Connection::open(root.join(".synck/object-store.sqlite3")).unwrap();
    let blobs: i64 = connection
        .query_row("SELECT count(*) FROM blobs WHERE complete = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    let zstd_chunks: i64 = connection
        .query_row(
            "SELECT count(*) FROM chunks WHERE codec = 'ZSTD'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mappings: i64 = connection
        .query_row("SELECT count(*) FROM blob_chunks", [], |row| row.get(0))
        .unwrap();
    let chunks: i64 = connection
        .query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(blobs, 2);
    assert!(zstd_chunks > 0);
    assert!(chunks < mappings);
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn chunking_reuses_content_after_an_early_insertion() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut original = Vec::with_capacity(2 * 1024 * 1024);
    for _ in 0..original.capacity() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        original.push(state as u8);
    }
    fs::write(root.join("document.bin"), &original).unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    let first_digest = Sha256::digest(&original).to_vec();
    let mut edited = original.clone();
    edited.splice(96 * 1024..96 * 1024, [0x5a; 4096]);
    fs::write(root.join("document.bin"), &edited).unwrap();
    repository.commit("insert content").await.unwrap();
    let second_digest = Sha256::digest(&edited).to_vec();

    let connection = rusqlite::Connection::open(root.join(".synck/object-store.sqlite3")).unwrap();
    let chunk_digests = |blob_digest: &[u8]| {
        connection
            .prepare(
                "SELECT chunk_digest FROM blob_chunks
                 WHERE blob_digest = ?1 ORDER BY ordinal",
            )
            .unwrap()
            .query_map([blob_digest], |row| row.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()
            .unwrap()
    };
    let first_chunks = chunk_digests(&first_digest);
    let second_chunks = chunk_digests(&second_digest);
    let reused = first_chunks.intersection(&second_chunks).count();
    assert!(first_chunks.len() > 4);
    assert!(
        reused > first_chunks.len() / 2,
        "only {reused} chunks reused"
    );
    drop(connection);
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn storage_optimize_migrates_whole_files_and_removes_them_after_switch() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let bytes = b"whole-file object bytes";
    fs::write(root.join("file.bin"), bytes).unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    drop(repository);
    make_whole_file_repository(root, bytes, true);

    let mut repository = Repository::discover(root).await.unwrap();
    assert!(repository.optimize_storage().await.unwrap());
    assert!(!repository.optimize_storage().await.unwrap());
    assert!(root.join(".synck/object-store.sqlite3").is_file());
    assert!(
        fs::read_dir(root.join(".synck/objects"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(repository.fsck().await.unwrap().is_empty());
    assert_eq!(fs::read(root.join("file.bin")).unwrap(), bytes);
}

#[tokio::test]
async fn storage_optimize_never_replaces_an_invalid_final_database() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let bytes = b"whole-file object bytes";
    fs::write(root.join("file.bin"), bytes).unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    drop(repository);
    make_whole_file_repository(root, bytes, true);
    let object_database = root.join(".synck/object-store.sqlite3");
    fs::write(&object_database, b"not a SQLite database").unwrap();

    let mut repository = Repository::discover(root).await.unwrap();
    assert!(repository.optimize_storage().await.is_err());
    assert_eq!(fs::read(object_database).unwrap(), b"not a SQLite database");
}

#[tokio::test]
async fn storage_optimize_resumes_each_durable_database_state() {
    let temporary = tempfile::tempdir().unwrap();
    let bytes = b"restartable whole-file object";

    let empty = temporary.path().join("empty");
    fs::create_dir(&empty).unwrap();
    initialize_migration_repository(&empty, bytes).await;
    let connection = rusqlite::Connection::open(empty.join(".synck/object-store.sqlite3")).unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys = ON; DELETE FROM blobs; DELETE FROM chunks;")
        .unwrap();
    drop(connection);
    let mut repository = Repository::discover(&empty).await.unwrap();
    assert!(repository.optimize_storage().await.unwrap());
    assert!(repository.fsck().await.unwrap().is_empty());
    drop(repository);

    let incomplete = temporary.path().join("incomplete");
    fs::create_dir(&incomplete).unwrap();
    initialize_migration_repository(&incomplete, bytes).await;
    let connection =
        rusqlite::Connection::open(incomplete.join(".synck/object-store.sqlite3")).unwrap();
    connection
        .execute("UPDATE blobs SET complete = 0", [])
        .unwrap();
    drop(connection);
    let mut repository = Repository::discover(&incomplete).await.unwrap();
    assert!(repository.optimize_storage().await.unwrap());
    assert!(repository.fsck().await.unwrap().is_empty());
    drop(repository);

    let imported = temporary.path().join("imported");
    fs::create_dir(&imported).unwrap();
    initialize_migration_repository(&imported, bytes).await;
    let mut repository = Repository::discover(&imported).await.unwrap();
    assert!(repository.optimize_storage().await.unwrap());
    assert!(repository.fsck().await.unwrap().is_empty());
    drop(repository);

    let switched = temporary.path().join("switched");
    fs::create_dir(&switched).unwrap();
    initialize_migration_repository(&switched, bytes).await;
    let path = switched.join(".synck/config.json");
    let mut config: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    config["storage_mode"] = serde_json::json!("CHUNKED");
    fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut repository = Repository::discover(&switched).await.unwrap();
    assert!(!repository.optimize_storage().await.unwrap());
    assert!(
        fs::read_dir(switched.join(".synck/objects"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn whole_file_mode_never_uses_office_chunking() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    init(root, "author").await.unwrap();
    let ignore = fs::read(root.join(".synckignore")).unwrap();
    make_whole_file_repository(root, &ignore, true);

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/templates/empty.docx");
    let bytes = fs::read(fixture).unwrap();
    fs::write(root.join("document.docx"), &bytes).unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("Office document").await.unwrap();

    let digest = hex::encode(Sha256::digest(&bytes));
    assert!(root.join(".synck/objects").join(&digest).is_file());
    assert!(!root.join(".synck/object-store.sqlite3").exists());
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn export_copies_current_and_historical_binary_revisions_exactly() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let binary = [0_u8, 0xff, 0x80, b'\n'];
    fs::write(root.join("document.bin"), binary).unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("binary").await.unwrap();
    let historical_seq = repository
        .versions(root, Path::new("document.bin"))
        .await
        .unwrap()[0]
        .item_seq;
    fs::write(root.join("document.bin"), []).unwrap();
    repository.commit("empty").await.unwrap();
    let commit_count = repository.log(None).await.unwrap().len();
    fs::write(root.join("document.bin"), b"dirty working bytes").unwrap();
    let status_before = repository.status().await.unwrap();
    let output = tempfile::tempdir().unwrap();

    repository
        .export(
            root,
            Path::new("document.bin"),
            &output.path().join("current.bin"),
            None,
        )
        .await
        .unwrap();
    repository
        .export(
            root,
            Path::new("document.bin"),
            &output.path().join("historical.bin"),
            Some(historical_seq),
        )
        .await
        .unwrap();

    assert!(
        fs::read(output.path().join("current.bin"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fs::read(output.path().join("historical.bin")).unwrap(),
        binary
    );
    let status_after = repository.status().await.unwrap();
    assert_eq!(status_after.len(), status_before.len());
    assert_eq!(status_after[0].kind, status_before[0].kind);
    assert_eq!(status_after[0].path, status_before[0].path);
    assert_eq!(repository.log(None).await.unwrap().len(), commit_count);
}

#[tokio::test]
async fn export_inside_the_worktree_is_an_ordinary_untracked_file() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("workspace")).unwrap();
    fs::write(root.join("document.bin"), b"content").unwrap();
    fs::write(root.join(".synckignore"), b"workspace/ignored-copy.bin\n").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let commit_count = repository.log(None).await.unwrap().len();
    let caller = root.join("workspace");

    repository
        .export(
            &caller,
            Path::new("../document.bin"),
            Path::new("copy.bin"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(fs::read(caller.join("copy.bin")).unwrap(), b"content");
    let status = repository.status().await.unwrap();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].kind, ChangeKind::Add);
    assert_eq!(status[0].path, "workspace/copy.bin");
    fs::remove_file(caller.join("copy.bin")).unwrap();

    repository
        .export(
            &caller,
            Path::new("../document.bin"),
            Path::new("ignored-copy.bin"),
            None,
        )
        .await
        .unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    assert_eq!(repository.log(None).await.unwrap().len(), commit_count);
}

#[tokio::test]
async fn export_rejects_invalid_sources_destinations_and_corrupt_objects() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("folder")).unwrap();
    fs::write(root.join("document.bin"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let output = tempfile::tempdir().unwrap();

    let error = repository
        .export(
            root,
            Path::new("document.bin"),
            &output.path().join("unknown.bin"),
            Some(999),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("does not exist"));
    let error = repository
        .export(
            root,
            Path::new("folder"),
            &output.path().join("folder.bin"),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not a tracked live file"));
    let existing = output.path().join("existing.bin");
    fs::write(&existing, b"keep").unwrap();
    let error = repository
        .export(root, Path::new("document.bin"), &existing, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("destination already exists"));
    assert_eq!(fs::read(&existing).unwrap(), b"keep");
    let error = repository
        .export(
            root,
            Path::new("document.bin"),
            &root.join(".synck/exported.bin"),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outside .synck"));
    let missing_parent = output.path().join("missing/exported.bin");
    let error = repository
        .export(root, Path::new("document.bin"), &missing_parent, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("inspect export parent"));
    assert!(!missing_parent.exists());

    let version = repository
        .versions(root, Path::new("document.bin"))
        .await
        .unwrap()
        .remove(0);
    let digest = version.blob_id.strip_prefix("sha256:").unwrap();
    corrupt_blob(root, digest);
    let corrupt_destination = output.path().join("corrupt.bin");
    let error = repository
        .export(root, Path::new("document.bin"), &corrupt_destination, None)
        .await
        .unwrap_err();
    assert!(matches!(error, LocalError::Integrity(_)), "{error:?}");
    assert!(!corrupt_destination.exists());
    assert!(fs::read_dir(output.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".synck-tmp-")
    }));
    assert_eq!(repository.log(None).await.unwrap().len(), 2);
}

#[cfg(unix)]
#[tokio::test]
async fn export_rejects_link_destinations_and_metadata_aliases() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let output = tempfile::tempdir().unwrap();
    let real_parent = output.path().join("real");
    fs::create_dir(&real_parent).unwrap();
    symlink(&real_parent, output.path().join("linked-parent")).unwrap();

    let error = repository
        .export(
            root,
            Path::new("document.bin"),
            &output.path().join("linked-parent/export.bin"),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not a real directory"));

    let linked_destination = output.path().join("linked-destination.bin");
    symlink("missing", &linked_destination).unwrap();
    let error = repository
        .export(root, Path::new("document.bin"), &linked_destination, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("destination already exists"));

    symlink(root.join(".synck"), root.join("metadata-alias")).unwrap();
    let error = repository
        .export(
            root,
            Path::new("document.bin"),
            &root.join("metadata-alias/objects/export.bin"),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outside .synck"));
}

#[tokio::test]
async fn revert_head_restores_file_content_without_removing_history() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("document.bin"), b"second").unwrap();
    repository.commit("edit").await.unwrap();

    let revert = repository.revert("HEAD", "revert edit").await.unwrap();

    assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"first");
    assert!(repository.status().await.unwrap().is_empty());
    let log = repository.log(None).await.unwrap();
    assert_eq!(log.len(), 4);
    assert_eq!(log[0].commit_id, revert);
    assert_eq!(log[0].message.as_deref(), Some("revert edit"));
    assert_eq!(log[1].message.as_deref(), Some("edit"));
    assert!(repository.fsck().await.unwrap().is_empty());

    repository
        .revert("HEAD", "revert the revert")
        .await
        .unwrap();
    assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"second");
    assert!(repository.status().await.unwrap().is_empty());
    assert_eq!(repository.log(None).await.unwrap().len(), 5);
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn revert_head_rejects_a_corrupt_prior_object_before_append() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let first = repository
        .versions(root, Path::new("document.bin"))
        .await
        .unwrap()
        .remove(0)
        .blob_id;
    fs::write(root.join("document.bin"), b"second").unwrap();
    repository.commit("edit").await.unwrap();

    let digest = first.strip_prefix("sha256:").unwrap();
    corrupt_blob(root, digest);
    let error = repository
        .revert("HEAD", "must not commit")
        .await
        .unwrap_err();
    assert!(matches!(error, LocalError::Integrity(_)), "{error:?}");
    assert_eq!(repository.log(None).await.unwrap().len(), 3);
    assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"second");
    assert!(!root.join(".synck/revert.json").exists());
    assert!(!root.join(".synck/revert-stage").exists());
}

#[tokio::test]
async fn revert_head_rejects_an_ignored_target_collision_before_append() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"tracked").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join(".synckignore"), b"document.bin\n").unwrap();
    repository.commit("ignore document path").await.unwrap();
    fs::remove_file(root.join("document.bin")).unwrap();
    repository.commit("delete document").await.unwrap();
    fs::write(root.join("document.bin"), b"ignored collision").unwrap();
    assert!(repository.status().await.unwrap().is_empty());

    let commit_count = repository.log(None).await.unwrap().len();
    let error = repository
        .revert("HEAD", "restore document")
        .await
        .unwrap_err();

    assert!(error.to_string().contains("revert target conflicts"));
    assert_eq!(repository.log(None).await.unwrap().len(), commit_count);
    assert_eq!(
        fs::read(root.join("document.bin")).unwrap(),
        b"ignored collision"
    );
    assert!(!root.join(".synck/revert.json").exists());
    assert!(!root.join(".synck/revert-stage").exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn revert_head_rejects_an_ignored_portable_name_collision_before_append() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"tracked").unwrap();
    fs::write(root.join(".synckignore"), b"DOCUMENT.BIN\n").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::remove_file(root.join("document.bin")).unwrap();
    repository.commit("delete document").await.unwrap();
    fs::write(root.join("DOCUMENT.BIN"), b"ignored collision").unwrap();
    assert!(repository.status().await.unwrap().is_empty());

    let commit_count = repository.log(None).await.unwrap().len();
    let error = repository
        .revert("HEAD", "restore document")
        .await
        .unwrap_err();

    assert!(error.to_string().contains("revert target conflicts"));
    assert_eq!(repository.log(None).await.unwrap().len(), commit_count);
    assert_eq!(
        fs::read(root.join("DOCUMENT.BIN")).unwrap(),
        b"ignored collision"
    );
    assert!(!root.join(".synck/revert.json").exists());
}

#[tokio::test]
async fn revert_head_restores_folder_move_and_descendant_edit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("drafts")).unwrap();
    fs::write(root.join("drafts/report.docx"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    fs::rename(root.join("drafts"), root.join("final")).unwrap();
    fs::write(root.join("final/report.docx"), b"second").unwrap();
    repository.commit("move and edit").await.unwrap();
    repository.revert("HEAD", "put draft back").await.unwrap();

    assert!(!root.join("final").exists());
    assert_eq!(fs::read(root.join("drafts/report.docx")).unwrap(), b"first");
    assert!(repository.status().await.unwrap().is_empty());
    let history = repository
        .history(root, Path::new("drafts/report.docx"))
        .await
        .unwrap();
    assert_eq!(history.len(), 3);
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn revert_head_restores_a_file_move_and_edit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("archive")).unwrap();
    fs::write(root.join("draft.docx"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    fs::rename(root.join("draft.docx"), root.join("archive/final.docx")).unwrap();
    fs::write(root.join("archive/final.docx"), b"second").unwrap();
    repository.commit("move and edit file").await.unwrap();
    repository.revert("HEAD", "restore draft").await.unwrap();

    assert_eq!(fs::read(root.join("draft.docx")).unwrap(), b"first");
    assert!(!root.join("archive/final.docx").exists());
    assert_eq!(
        repository
            .history(root, Path::new("draft.docx"))
            .await
            .unwrap()
            .len(),
        3
    );
    assert!(repository.status().await.unwrap().is_empty());
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn revert_head_restores_a_folder_move_without_child_events() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("drafts")).unwrap();
    fs::write(root.join("drafts/report.docx"), b"unchanged").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    fs::rename(root.join("drafts"), root.join("final")).unwrap();
    repository.commit("move folder").await.unwrap();
    repository.revert("HEAD", "put folder back").await.unwrap();

    assert!(!root.join("final").exists());
    assert_eq!(
        fs::read(root.join("drafts/report.docx")).unwrap(),
        b"unchanged"
    );
    assert!(repository.status().await.unwrap().is_empty());
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn revert_head_handles_recursive_and_same_path_delete_and_add() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("documents")).unwrap();
    fs::write(root.join("documents/report"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    fs::remove_dir_all(root.join("documents")).unwrap();
    repository.commit("delete documents").await.unwrap();
    repository
        .revert("HEAD", "restore documents")
        .await
        .unwrap();
    assert_eq!(fs::read(root.join("documents/report")).unwrap(), b"content");

    fs::remove_file(root.join("documents/report")).unwrap();
    fs::create_dir(root.join("documents/report")).unwrap();
    fs::write(root.join("documents/report/child"), b"child").unwrap();
    repository.commit("replace file with folder").await.unwrap();
    repository.revert("HEAD", "restore file").await.unwrap();
    assert_eq!(fs::read(root.join("documents/report")).unwrap(), b"content");
    assert!(repository.status().await.unwrap().is_empty());
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn revert_head_stages_path_swaps_without_overwrite() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("left.bin"), b"left").unwrap();
    fs::write(root.join("right.bin"), b"right").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    fs::rename(root.join("left.bin"), root.join("swap.tmp")).unwrap();
    fs::rename(root.join("right.bin"), root.join("left.bin")).unwrap();
    fs::rename(root.join("swap.tmp"), root.join("right.bin")).unwrap();
    repository.commit("swap documents").await.unwrap();
    repository.revert("HEAD", "unswap documents").await.unwrap();

    assert_eq!(fs::read(root.join("left.bin")).unwrap(), b"left");
    assert_eq!(fs::read(root.join("right.bin")).unwrap(), b"right");
    assert!(repository.status().await.unwrap().is_empty());
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn local_repository_round_trip() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("work");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("folder")).unwrap();
    fs::write(root.join("folder/file.txt"), b"first").unwrap();

    let repo_id = init(&root, "author@example.test").await.unwrap();
    assert_eq!(
        uuid::Uuid::parse_str(repo_id.as_str())
            .unwrap()
            .get_version_num(),
        7
    );
    let repository = Repository::discover(&root).await.unwrap();
    let status = repository.status().await.unwrap();
    assert_eq!(
        status
            .iter()
            .map(|change| (change.kind, change.path.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (ChangeKind::Add, "folder"),
            (ChangeKind::Add, "folder/file.txt")
        ]
    );
    assert!(matches!(
        repository.commit("initial").await.unwrap(),
        CommitOutcome::Committed(_)
    ));
    assert!(repository.status().await.unwrap().is_empty());
    assert!(matches!(
        repository.commit("clean").await.unwrap(),
        CommitOutcome::Clean
    ));

    fs::write(root.join("folder/file.txt"), b"second").unwrap();
    assert_eq!(
        repository.status().await.unwrap()[0].kind,
        ChangeKind::Modify
    );
    repository.commit("second").await.unwrap();
    let versions = repository
        .versions(&root, Path::new("folder/file.txt"))
        .await
        .unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].item_seq, 2);
    assert_eq!(versions[1].item_seq, 3);

    repository
        .restore(&root, Path::new("folder/file.txt"), Some(2))
        .await
        .unwrap();
    assert_eq!(fs::read(root.join("folder/file.txt")).unwrap(), b"first");
    assert_eq!(
        repository.status().await.unwrap()[0].kind,
        ChangeKind::Modify
    );

    let log = repository.log(None).await.unwrap();
    assert_eq!(log.len(), 3);
    assert_eq!(log[0].message.as_deref(), Some("second"));
    let findings = repository.fsck().await.unwrap();
    assert!(findings.is_empty(), "{findings:#?}");

    fs::write(root.join("folder/file.txt"), b"uncommitted-only").unwrap();
    let uncommitted_digest = hex::encode(Sha256::digest(b"uncommitted-only"));
    let archive = temporary.path().join("archive");
    repository.archive(&archive).await.unwrap();
    let archived = synck_local::portable::PortableRepository::open(&archive).unwrap();
    archived.verify().await.unwrap();
    for bytes in [b"first".as_slice(), b"second".as_slice()] {
        let id = synck_vcs::types::ObjectId::parse(format!(
            "sha256:{}",
            hex::encode(Sha256::digest(bytes))
        ))
        .unwrap();
        let mut actual = Vec::new();
        archived.copy_blob(&id, &mut actual).unwrap();
        assert_eq!(actual, bytes);
    }
    let absent = synck_vcs::types::ObjectId::parse(format!("sha256:{uncommitted_digest}")).unwrap();
    assert!(archived.copy_blob(&absent, &mut Vec::new()).is_err());
}

#[tokio::test]
async fn show_and_history_follow_item_identity_across_folder_moves() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("documents")).unwrap();
    fs::write(root.join("documents/draft.txt"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();

    let CommitOutcome::Committed(initial) = repository.commit("initial").await.unwrap() else {
        panic!("initial commit was unexpectedly clean");
    };
    fs::write(root.join("documents/draft.txt"), b"second").unwrap();
    let CommitOutcome::Committed(modified) = repository.commit("edit").await.unwrap() else {
        panic!("edit commit was unexpectedly clean");
    };
    fs::rename(
        root.join("documents/draft.txt"),
        root.join("documents/final.txt"),
    )
    .unwrap();
    let CommitOutcome::Committed(renamed) = repository.commit("rename document").await.unwrap()
    else {
        panic!("rename commit was unexpectedly clean");
    };
    fs::rename(root.join("documents"), root.join("archive")).unwrap();
    let CommitOutcome::Committed(moved_folder) = repository.commit("archive folder").await.unwrap()
    else {
        panic!("folder move commit was unexpectedly clean");
    };

    let initial_view = repository.show(&initial).await.unwrap();
    assert_eq!(initial_view.repo_seq, 2);
    assert_eq!(initial_view.message.as_deref(), Some("initial"));
    assert_eq!(
        initial_view
            .changes
            .iter()
            .map(|change| (change.kind, change.path.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (ChangeKind::Add, "documents"),
            (ChangeKind::Add, "documents/draft.txt"),
        ]
    );
    assert_eq!(
        repository.show(&modified).await.unwrap().changes[0].kind,
        ChangeKind::Modify
    );
    let rename_view = repository.show(&renamed).await.unwrap();
    assert_eq!(rename_view.changes.len(), 1);
    assert_eq!(rename_view.changes[0].kind, ChangeKind::Rename);
    assert_eq!(
        rename_view.changes[0].source_path.as_deref(),
        Some("documents/draft.txt")
    );
    assert_eq!(rename_view.changes[0].path, "documents/final.txt");
    let folder_view = repository.show(&moved_folder).await.unwrap();
    assert_eq!(folder_view.changes.len(), 1);
    assert_eq!(folder_view.changes[0].path, "archive");

    let history = repository
        .history(root, Path::new("archive/final.txt"))
        .await
        .unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(
        history
            .iter()
            .map(|commit| commit.changes[0].kind)
            .collect::<Vec<_>>(),
        vec![ChangeKind::Add, ChangeKind::Modify, ChangeKind::Rename,]
    );
    assert!(
        history
            .iter()
            .all(|commit| commit.commit_id != moved_folder)
    );

    assert!(repository.show("not-a-commit").await.is_err());
    let missing = format!("sha256:{}", "0".repeat(64));
    assert!(
        repository
            .show(&missing)
            .await
            .unwrap_err()
            .to_string()
            .contains("does not exist")
    );
    assert!(
        repository
            .history(root, Path::new("documents/draft.txt"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn show_uses_commit_boundary_paths_for_nested_moves() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir_all(root.join("source/child")).unwrap();
    fs::write(root.join("source/child/document.txt"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    fs::rename(root.join("source"), root.join("a-archive")).unwrap();
    fs::rename(
        root.join("a-archive/child/document.txt"),
        root.join("z-final.txt"),
    )
    .unwrap();
    let CommitOutcome::Committed(commit_id) = repository.commit("reorganize").await.unwrap() else {
        panic!("reorganization commit was unexpectedly clean");
    };

    let shown = repository.show(&commit_id).await.unwrap();
    let file_move = shown
        .changes
        .iter()
        .find(|change| change.path == "z-final.txt")
        .unwrap();
    assert_eq!(file_move.kind, ChangeKind::MoveRename);
    assert_eq!(
        file_move.source_path.as_deref(),
        Some("source/child/document.txt")
    );
    let history = repository
        .history(root, Path::new("z-final.txt"))
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[1].changes[0].source_path.as_deref(),
        Some("source/child/document.txt")
    );
    assert_eq!(history[1].changes[0].path, "z-final.txt");
}

#[tokio::test]
async fn deleted_and_recover_preserve_file_identity_and_history() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("documents")).unwrap();
    fs::write(root.join("documents/report.pdf"), b"committed bytes").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    let database = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    let item_id: String = database
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'report.pdf'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(database);

    fs::remove_file(root.join("documents/report.pdf")).unwrap();
    let CommitOutcome::Committed(deletion_commit) =
        repository.commit("delete report").await.unwrap()
    else {
        panic!("deletion commit was unexpectedly clean");
    };
    let deleted = repository.deleted().await.unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].item_id, item_id);
    assert_eq!(deleted[0].item_type, "FILE");
    assert_eq!(deleted[0].path, "documents/report.pdf");
    assert_eq!(deleted[0].commit_id, deletion_commit);

    let recovery_commit = repository
        .recover_item(&item_id, "recover report")
        .await
        .unwrap();
    assert_eq!(
        fs::read(root.join("documents/report.pdf")).unwrap(),
        b"committed bytes"
    );
    assert!(repository.deleted().await.unwrap().is_empty());
    assert!(repository.status().await.unwrap().is_empty());

    let history = repository
        .history(root, Path::new("documents/report.pdf"))
        .await
        .unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(
        history
            .iter()
            .map(|commit| commit.changes[0].kind)
            .collect::<Vec<_>>(),
        vec![ChangeKind::Add, ChangeKind::Delete, ChangeKind::Add]
    );
    assert_eq!(history[2].commit_id, recovery_commit);

    let database = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    let (stored_id, lifecycle): (String, String) = database
        .query_row(
            "SELECT item_id, lifecycle_state FROM vcs_items WHERE name = 'report.pdf'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored_id, item_id);
    assert_eq!(lifecycle, "LIVE");
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn folder_recovery_restores_its_deletion_group_only() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir_all(root.join("case/evidence")).unwrap();
    fs::write(root.join("case/old.txt"), b"old").unwrap();
    fs::write(root.join("case/current.txt"), b"current").unwrap();
    fs::write(root.join("case/evidence/photo.jpg"), b"photo").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    let database = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    let folder_id: String = database
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'case'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let current_id: String = database
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'current.txt'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let old_id: String = database
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'old.txt'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(database);

    fs::remove_file(root.join("case/old.txt")).unwrap();
    repository.commit("delete old draft").await.unwrap();
    fs::remove_dir_all(root.join("case")).unwrap();
    repository.commit("delete case").await.unwrap();

    let error = repository
        .recover_item(&current_id, "recover current")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("recover the parent instead"));

    repository
        .recover_item(&folder_id, "recover case")
        .await
        .unwrap();
    assert_eq!(fs::read(root.join("case/current.txt")).unwrap(), b"current");
    assert_eq!(
        fs::read(root.join("case/evidence/photo.jpg")).unwrap(),
        b"photo"
    );
    assert!(!root.join("case/old.txt").exists());
    let deleted = repository.deleted().await.unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].item_id, old_id);
    assert_eq!(deleted[0].path, "case/old.txt");
    assert!(repository.status().await.unwrap().is_empty());
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn recover_refuses_an_ignored_filesystem_collision_before_append() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"tracked").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join(".synckignore"), b"document.bin\n").unwrap();
    repository
        .commit("ignore untracked document path")
        .await
        .unwrap();

    let database = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    let item_id: String = database
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'document.bin'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(database);
    fs::remove_file(root.join("document.bin")).unwrap();
    repository.commit("delete document").await.unwrap();
    fs::write(root.join("document.bin"), b"untracked collision").unwrap();
    assert!(repository.status().await.unwrap().is_empty());

    let before = repository.log(None).await.unwrap().len();
    let error = repository
        .recover_item(&item_id, "recover document")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("recovery target already exists"));
    assert_eq!(repository.log(None).await.unwrap().len(), before);
    assert_eq!(
        fs::read(root.join("document.bin")).unwrap(),
        b"untracked collision"
    );
    assert!(!root.join(".synck/recover.json").exists());
}

#[tokio::test]
async fn recover_refuses_a_live_name_conflict_without_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("report.docx"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("first report").await.unwrap();
    let database = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    let first_id: String = database
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'report.docx'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(database);

    fs::remove_file(root.join("report.docx")).unwrap();
    repository.commit("delete first report").await.unwrap();
    fs::write(root.join("report.docx"), b"replacement").unwrap();
    repository.commit("replacement report").await.unwrap();
    let before = repository.log(None).await.unwrap().len();

    let error = repository
        .recover_item(&first_id, "recover first report")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("conflicts with item"));
    assert_eq!(repository.log(None).await.unwrap().len(), before);
    assert_eq!(fs::read(root.join("report.docx")).unwrap(), b"replacement");
    assert!(!root.join(".synck/recover.json").exists());
}

#[tokio::test]
async fn recover_verifies_objects_before_accepting_a_commit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"recoverable").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let version = repository
        .versions(root, Path::new("document.bin"))
        .await
        .unwrap()
        .pop()
        .unwrap();
    let database = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    let item_id: String = database
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'document.bin'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(database);
    fs::remove_file(root.join("document.bin")).unwrap();
    repository.commit("delete document").await.unwrap();

    let digest = version.blob_id.strip_prefix("sha256:").unwrap();
    let (chunk_digest, saved) = corrupt_blob(root, digest);
    let before = repository.log(None).await.unwrap().len();
    let error = repository
        .recover_item(&item_id, "recover document")
        .await
        .unwrap_err();
    assert!(matches!(error, LocalError::Integrity(_)), "{error:?}");
    assert_eq!(repository.log(None).await.unwrap().len(), before);
    assert!(!root.join(".synck/recover.json").exists());
    assert!(!root.join("document.bin").exists());
    restore_chunk(root, &chunk_digest, &saved);
    repository
        .recover_item(&item_id, "recover document")
        .await
        .unwrap();
    assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"recoverable");
    assert!(!root.join(".synck/recover.json").exists());
    assert_eq!(
        repository
            .log(None)
            .await
            .unwrap()
            .iter()
            .filter(|commit| commit.message.as_deref() == Some("recover document"))
            .count(),
        1
    );
    assert!(repository.status().await.unwrap().is_empty());
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn binary_and_empty_versions_restore_exact_bytes() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("binary");
    let binary = [0_u8, 0xff, 0x80, b'\n'];
    fs::write(&path, binary).unwrap();
    init(temporary.path(), "author").await.unwrap();
    let repository = Repository::discover(temporary.path()).await.unwrap();
    repository.commit("binary").await.unwrap();
    fs::write(&path, []).unwrap();
    repository.commit("empty").await.unwrap();

    repository
        .restore(temporary.path(), Path::new("binary"), Some(2))
        .await
        .unwrap();
    assert_eq!(fs::read(&path).unwrap(), binary);
    repository
        .restore(temporary.path(), Path::new("binary"), None)
        .await
        .unwrap();
    assert!(fs::read(&path).unwrap().is_empty());
}

#[tokio::test]
async fn gc_aborts_before_deletion_when_a_referenced_object_is_corrupt() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("file"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    install_unreachable_object(root, b"unreachable");
    let digest = hex::encode(Sha256::digest(b"content"));
    corrupt_blob(root, &digest);

    let error = repository.gc().await.unwrap_err();
    assert!(error.to_string().contains("chunk digest mismatch"));
    let connection = rusqlite::Connection::open(root.join(".synck/object-store.sqlite3")).unwrap();
    let present: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM blobs WHERE digest=?1)",
            [Sha256::digest(b"unreachable").to_vec()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(present);
}

#[tokio::test]
async fn gc_aborts_before_deletion_for_a_malformed_object_entry() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    install_unreachable_object(root, b"unreachable");
    fs::write(root.join(".synck/objects/not-an-object"), b"invalid").unwrap();

    let error = repository.gc().await.unwrap_err();
    assert!(error.to_string().contains("noncanonical object filename"));
    fs::remove_file(root.join(".synck/objects/not-an-object")).unwrap();
    assert_eq!(
        repository
            .storage_report()
            .await
            .unwrap()
            .unreachable_objects,
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn gc_aborts_before_deletion_for_an_object_store_symlink() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    install_unreachable_object(root, b"unreachable");
    let objects = root.join(".synck/objects");
    let prefix = (0_u8..=u8::MAX)
        .map(|value| format!("{value:02x}"))
        .map(|name| objects.join(name))
        .find(|path| !path.exists())
        .unwrap();
    symlink(root.join(".synck/tmp"), &prefix).unwrap();

    let error = repository.gc().await.unwrap_err();
    assert!(error.to_string().contains("linked portable entry"));
    fs::remove_file(prefix).unwrap();
    assert_eq!(
        repository
            .storage_report()
            .await
            .unwrap()
            .unreachable_objects,
        1
    );
}

#[tokio::test]
async fn repository_open_rejects_a_regular_orphan_object_database_sidecar() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    init(root, "author").await.unwrap();
    let database = root.join(".synck/repository.local.sqlite3");
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute("UPDATE local_repository_settings SET hidden_files = 'IGNORE'", [])
        .unwrap();
    drop(connection);
    remove_object_database_family(root);
    let sidecar = root.join(".synck/object-store.sqlite3-shm");
    fs::write(&sidecar, b"sentinel").unwrap();

    let error = match Repository::discover(root).await {
        Ok(_) => panic!("repository unexpectedly accepted an orphan sidecar"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("sidecar is invalid"), "{error}");
    assert_eq!(fs::read(sidecar).unwrap(), b"sentinel");
}

#[cfg(unix)]
#[tokio::test]
async fn repository_open_rejects_a_linked_object_database_sidecar() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    init(root, "author").await.unwrap();
    for suffix in ["-wal", "-shm"] {
        let sidecar = root.join(format!(".synck/object-store.sqlite3{suffix}"));
        match fs::remove_file(&sidecar) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cannot remove {}: {error}", sidecar.display()),
        }
    }
    let external = root.join("external");
    fs::write(&external, b"sentinel").unwrap();
    symlink(&external, root.join(".synck/object-store.sqlite3-wal")).unwrap();

    let error = match Repository::discover(root).await {
        Ok(_) => panic!("repository unexpectedly accepted a linked sidecar"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("sidecar is invalid"), "{error}");
    assert_eq!(fs::read(external).unwrap(), b"sentinel");
}

#[tokio::test]
async fn gc_converges_after_a_subset_was_already_deleted() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    let removed_before_rerun = install_unreachable_object(root, b"first orphan");
    install_unreachable_object(root, b"second orphan");
    delete_unreachable_blob(root, &removed_before_rerun);

    let removed = repository.gc().await.unwrap();
    assert_eq!(removed.removed_objects, 1);
    assert_eq!(removed.removed_bytes, 13);
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
async fn filesystem_folder_move_preserves_descendant_identity_after_atomic_save() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir_all(root.join("source/nested")).unwrap();
    fs::create_dir(root.join("target")).unwrap();
    fs::write(root.join("source/nested/document.txt"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let database = root.join(".synck/repository.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    let folder_id: String = connection
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'source'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let file_id: String = connection
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'document.txt'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);

    fs::rename(root.join("source"), root.join("target/renamed")).unwrap();
    fs::remove_file(root.join("target/renamed/nested/document.txt")).unwrap();
    fs::write(root.join("target/renamed/nested/document.txt"), b"second").unwrap();
    let status = repository.status().await.unwrap();
    assert_eq!(status.len(), 2);
    assert!(status.iter().any(|change| {
        change.kind == ChangeKind::MoveRename
            && change.source_path.as_deref() == Some("source")
            && change.path == "target/renamed"
    }));
    assert!(status.iter().any(|change| {
        change.kind == ChangeKind::Modify
            && change.source_path.as_deref() == Some("source/nested/document.txt")
            && change.path == "target/renamed/nested/document.txt"
    }));

    repository.commit("organize and edit").await.unwrap();
    assert!(repository.status().await.unwrap().is_empty());
    let connection = rusqlite::Connection::open(database).unwrap();
    let moved_folder_id: String = connection
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'renamed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let moved_file_id: String = connection
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'document.txt'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(moved_folder_id, folder_id);
    assert_eq!(moved_file_id, file_id);
    assert_eq!(
        repository
            .versions(root, Path::new("target/renamed/nested/document.txt"))
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn filesystem_move_into_vacated_tracked_path_preserves_source_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("source"), b"source bytes").unwrap();
    fs::write(root.join("destination"), b"destination bytes").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let database = root.join(".synck/repository.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    let source_id: String = connection
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'source'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);

    fs::remove_file(root.join("destination")).unwrap();
    fs::rename(root.join("source"), root.join("destination")).unwrap();
    let status = repository.status().await.unwrap();
    assert_eq!(status.len(), 2);
    assert!(status.iter().any(|change| {
        change.kind == ChangeKind::Rename
            && change.source_path.as_deref() == Some("source")
            && change.path == "destination"
    }));
    assert!(
        status
            .iter()
            .any(|change| { change.kind == ChangeKind::Delete && change.path == "destination" })
    );

    repository.commit("replace destination").await.unwrap();
    let connection = rusqlite::Connection::open(database).unwrap();
    let destination_id: String = connection
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'destination' AND lifecycle_state = 'LIVE'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(destination_id, source_id);
    assert_eq!(fs::read(root.join("destination")).unwrap(), b"source bytes");
    assert!(repository.status().await.unwrap().is_empty());
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn explicit_moves_preserve_file_and_folder_identity() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir_all(root.join("source/nested")).unwrap();
    fs::create_dir(root.join("target")).unwrap();
    fs::write(root.join("source/nested/document.txt"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("source/nested/document.txt"), b"second").unwrap();
    repository.commit("second version").await.unwrap();

    let database = root.join(".synck/repository.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    let source_folder_id: String = connection
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'source'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let file_id: String = connection
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'document.txt'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);

    repository
        .move_path(
            root,
            Path::new("source"),
            Path::new("target/renamed"),
            "move folder",
        )
        .await
        .unwrap();
    assert!(!root.join("source").exists());
    assert_eq!(
        fs::read(root.join("target/renamed/nested/document.txt")).unwrap(),
        b"second"
    );
    assert!(repository.status().await.unwrap().is_empty());

    repository
        .move_path(
            root,
            Path::new("target/renamed/nested/document.txt"),
            Path::new("document-final.txt"),
            "move document",
        )
        .await
        .unwrap();
    assert_eq!(
        fs::read(root.join("document-final.txt")).unwrap(),
        b"second"
    );
    fs::write(root.join("document-final.txt"), b"third").unwrap();
    repository.commit("edit after move").await.unwrap();
    let versions = repository
        .versions(root, Path::new("document-final.txt"))
        .await
        .unwrap();
    assert_eq!(versions.len(), 3);

    let connection = rusqlite::Connection::open(database).unwrap();
    let moved_folder_id: String = connection
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'renamed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let moved_file_id: String = connection
        .query_row(
            "SELECT item_id FROM vcs_items WHERE name = 'document-final.txt'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(moved_folder_id, source_folder_id);
    assert_eq!(moved_file_id, file_id);
    assert!(!root.join(".synck/move.json").exists());
    let findings = repository.fsck().await.unwrap();
    assert!(findings.is_empty(), "{findings:#?}");
}

#[tokio::test]
async fn explicit_move_refuses_a_dirty_working_tree_without_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document"), b"content").unwrap();
    fs::write(root.join("other"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("other"), b"dirty").unwrap();

    let error = repository
        .move_path(
            root,
            Path::new("document"),
            Path::new("renamed"),
            "must fail",
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("requires a clean working tree"));
    assert!(root.join("document").is_file());
    assert!(!root.join("renamed").exists());
    assert!(!root.join(".synck/move.json").exists());
    assert_eq!(repository.log(None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn explicit_move_refuses_overwrite_and_descendant_targets() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir_all(root.join("folder/child")).unwrap();
    fs::write(root.join("folder/document"), b"content").unwrap();
    fs::write(root.join("occupied"), b"occupied").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    let error = repository
        .move_path(
            root,
            Path::new("folder/document"),
            Path::new("occupied"),
            "must not overwrite",
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("destination already exists"));
    let error = repository
        .move_path(
            root,
            Path::new("folder"),
            Path::new("folder/child/nested"),
            "must not cycle",
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("below itself"));
    assert!(root.join("folder/document").is_file());
    assert_eq!(fs::read(root.join("occupied")).unwrap(), b"occupied");
    assert!(!root.join(".synck/move.json").exists());
    assert_eq!(repository.log(None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn repository_discovery_completes_an_interrupted_move() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("before"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let connection = rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    let (item_id, head_seq): (String, i64) = connection
        .query_row(
            "SELECT item_id, head_seq FROM vcs_items WHERE name = 'before'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    drop(connection);
    let intent = serde_json::json!({
        "version": 2,
        "operation_id": uuid::Uuid::new_v4().to_string(),
        "source": "before",
        "destination": "after",
        "item_id": item_id,
        "item_type": "FILE",
        "base_seq": head_seq,
        "result_head_seq": head_seq + 1,
        "new_parent_id": null,
        "new_name": "after",
        "message": "interrupted move"
    });
    fs::write(
        root.join(".synck/move.json"),
        serde_json::to_vec_pretty(&intent).unwrap(),
    )
    .unwrap();
    fs::rename(root.join("before"), root.join("after")).unwrap();
    drop(repository);

    let recovered = Repository::discover(root).await.unwrap();
    assert!(recovered.status().await.unwrap().is_empty());
    assert_eq!(recovered.log(None).await.unwrap().len(), 3);
    assert_eq!(
        recovered.log(None).await.unwrap()[0].message.as_deref(),
        Some("interrupted move")
    );
    assert!(!root.join(".synck/move.json").exists());
    let versions = recovered.versions(root, Path::new("after")).await.unwrap();
    assert_eq!(versions.len(), 1);
    let findings = recovered.fsck().await.unwrap();
    assert!(findings.is_empty(), "{findings:#?}");
    drop(recovered);

    fs::write(
        root.join(".synck/move.json"),
        serde_json::to_vec_pretty(&intent).unwrap(),
    )
    .unwrap();
    let finalized = Repository::discover(root).await.unwrap();
    assert_eq!(finalized.log(None).await.unwrap().len(), 3);
    assert!(finalized.status().await.unwrap().is_empty());
    assert!(!root.join(".synck/move.json").exists());
}

#[tokio::test]
async fn malformed_move_intent_fails_closed() {
    let temporary = tempfile::tempdir().unwrap();
    init(temporary.path(), "author").await.unwrap();
    fs::write(temporary.path().join(".synck/move.json"), b"{}").unwrap();
    let error = match Repository::discover(temporary.path()).await {
        Ok(_) => panic!("repository with malformed move intent was accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("invalid move intent"));
}

#[tokio::test]
async fn malformed_or_orphaned_revert_state_fails_closed() {
    let malformed = tempfile::tempdir().unwrap();
    init(malformed.path(), "author").await.unwrap();
    fs::write(malformed.path().join(".synck/revert.json"), b"{}").unwrap();
    let error = match Repository::discover(malformed.path()).await {
        Ok(_) => panic!("repository with malformed revert intent was accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("invalid revert intent"));

    let orphaned = tempfile::tempdir().unwrap();
    init(orphaned.path(), "author").await.unwrap();
    fs::create_dir(orphaned.path().join(".synck/revert-stage")).unwrap();
    let error = match Repository::discover(orphaned.path()).await {
        Ok(_) => panic!("repository with orphaned revert staging was accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("orphaned revert staging"));

    let simultaneous = tempfile::tempdir().unwrap();
    init(simultaneous.path(), "author").await.unwrap();
    fs::write(simultaneous.path().join(".synck/move.json"), b"{}").unwrap();
    fs::write(simultaneous.path().join(".synck/revert.json"), b"{}").unwrap();
    let error = match Repository::discover(simultaneous.path()).await {
        Ok(_) => panic!("repository with simultaneous intents was accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("simultaneous operation intents"));
}

#[tokio::test]
async fn repository_config_rejects_unknown_fields() {
    let temporary = tempfile::tempdir().unwrap();
    init(temporary.path(), "author").await.unwrap();
    let config_path = temporary.path().join(".synck/config.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["unexpected"] = serde_json::Value::Bool(true);
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();

    let error = match Repository::discover(temporary.path()).await {
        Ok(_) => panic!("repository with an unknown config field was accepted"),
        Err(error) => error,
    };
    assert!(matches!(error, LocalError::ConfigJson(_)));
}

#[tokio::test]
async fn repository_open_rejects_local_schema_drift() {
    let temporary = tempfile::tempdir().unwrap();
    init(temporary.path(), "author").await.unwrap();
    let database = temporary.path().join(".synck/repository.local.sqlite3");
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE local_file_heads RENAME TO local_file_heads_old;
             CREATE TABLE local_file_heads (
                 repo_id TEXT NOT NULL,
                 item_id TEXT NOT NULL,
                 item_seq INTEGER NOT NULL CHECK (item_seq > 0),
                 blob_id TEXT NOT NULL,
                 size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
                 PRIMARY KEY (repo_id, item_id)
             );
             DROP TABLE local_file_heads_old;",
        )
        .unwrap();
    drop(connection);

    let error = match Repository::discover(temporary.path()).await {
        Ok(_) => panic!("repository open accepted a damaged local schema"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("local SQLite schema definition"));
}

#[tokio::test]
async fn repository_open_rejects_unknown_local_schema_objects() {
    let temporary = tempfile::tempdir().unwrap();
    init(temporary.path(), "author").await.unwrap();
    let database = temporary.path().join(".synck/repository.local.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch("CREATE VIEW sqliteXforeign AS SELECT 'value' AS value;")
        .unwrap();
    drop(connection);

    let error = match Repository::discover(temporary.path()).await {
        Ok(_) => panic!("repository open accepted an unknown schema object"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("unknown objects"), "{error}");

    let connection = rusqlite::Connection::open(database).unwrap();
    let version: i64 = connection
        .query_row(
            "SELECT version FROM local_schema_meta WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let storage_tables: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE type = 'table' AND name = 'local_repository_settings'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let foreign_views: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE type = 'view' AND name = 'sqliteXforeign'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, 11);
    assert_eq!(storage_tables, 1);
    assert_eq!(foreign_views, 1);
}

#[tokio::test]
async fn repository_open_rejects_all_old_local_schema_versions_without_migration() {
    for version in 1..=10 {
        let temporary = tempfile::tempdir().unwrap();
        init(temporary.path(), "author").await.unwrap();
        let database = temporary.path().join(".synck/repository.local.sqlite3");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "UPDATE local_schema_meta SET version = ?1 WHERE singleton = 1",
                [version],
            )
            .unwrap();
        drop(connection);

        let error = match Repository::discover(temporary.path()).await {
            Ok(_) => panic!("repository discovery accepted local schema version {version}"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("unsupported local schema version"),
            "version {version}: {error}"
        );
        let connection = rusqlite::Connection::open(database).unwrap();
        let stored: i64 = connection
            .query_row(
                "SELECT version FROM local_schema_meta WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, version);
    }
}

#[tokio::test]
async fn worktree_cache_schema_rejects_incomplete_file_fingerprints() {
    let temporary = tempfile::tempdir().unwrap();
    init(temporary.path(), "author").await.unwrap();
    let connection =
        rusqlite::Connection::open(temporary.path().join(".synck/repository.local.sqlite3")).unwrap();
    let repo_id: String = connection
        .query_row("SELECT repo_id FROM local_repository_settings", [], |row| {
            row.get(0)
        })
        .unwrap();
    let result = connection.execute(
        "INSERT INTO local_worktree_entries(
             repo_id, path, node_kind, device_id, file_id, size_bytes,
             mtime_seconds, mtime_nanos, change_seconds, change_nanos,
             observed_blob_id, stat_valid
         ) VALUES (?1, 'file', 'FILE', ?2, ?3, 1, 0, 0, 0, NULL, ?4, 1)",
        rusqlite::params![
            repo_id,
            1_u64.to_be_bytes().as_slice(),
            2_u64.to_be_bytes().as_slice(),
            format!("sha256:{}", "0".repeat(64)),
        ],
    );
    assert!(result.is_err());

    let result = connection.execute(
        "INSERT INTO local_worktree_entries(
             repo_id, path, node_kind, device_id, file_id, birth_seconds
         ) VALUES (?1, 'folder', 'FOLDER', ?2, ?3, 1)",
        rusqlite::params![
            repo_id,
            1_u64.to_be_bytes().as_slice(),
            2_u64.to_be_bytes().as_slice(),
        ],
    );
    assert!(result.is_err());

    let result = connection.execute(
        "INSERT INTO local_worktree_scan_state(
             repo_id, ignore_blob_id, monitor_instance_id, monitor_sequence
         ) VALUES (?1, NULL, ?2, NULL)",
        rusqlite::params![repo_id, uuid::Uuid::new_v4().to_string()],
    );
    assert!(result.is_err());
}

#[tokio::test]
async fn ignore_rules_do_not_hide_tracked_files() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("ignored")).unwrap();
    fs::write(root.join("ignored/tracked.txt"), b"tracked").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    fs::write(root.join(".synckignore"), b"ignored/\n").unwrap();
    fs::write(root.join("ignored/untracked.txt"), b"untracked").unwrap();
    fs::remove_file(root.join("ignored/tracked.txt")).unwrap();
    let status = repository.status().await.unwrap();
    assert!(status.iter().any(|change| {
        change.kind == ChangeKind::Delete && change.path == "ignored/tracked.txt"
    }));
    assert!(
        !status
            .iter()
            .any(|change| change.path == "ignored/untracked.txt")
    );
}

#[tokio::test]
async fn dot_hidden_entries_require_opt_in_but_remain_tracked_after_opt_out() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join(".notes"), b"one").unwrap();
    init(root, "author").await.unwrap();
    let mut repository = Repository::discover(root).await.unwrap();
    assert!(
        repository
            .status()
            .await
            .unwrap()
            .iter()
            .all(|change| change.path != ".notes")
    );

    repository
        .set_hidden_files_mode(HiddenFilesMode::Track)
        .unwrap();
    assert!(
        repository
            .status()
            .await
            .unwrap()
            .iter()
            .any(|change| { change.kind == ChangeKind::Add && change.path == ".notes" })
    );
    repository.commit("track hidden file").await.unwrap();

    repository
        .set_hidden_files_mode(HiddenFilesMode::Ignore)
        .unwrap();
    fs::write(root.join(".notes"), b"two").unwrap();
    assert!(
        repository
            .status()
            .await
            .unwrap()
            .iter()
            .any(|change| { change.kind == ChangeKind::Modify && change.path == ".notes" })
    );
}

#[cfg(any(target_os = "macos", windows))]
#[tokio::test]
async fn native_hidden_entries_follow_repository_policy_but_tracked_files_remain_tracked() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("visible.txt"), b"visible").unwrap();
    fs::write(root.join("native-hidden.txt"), b"hidden").unwrap();
    fs::create_dir(root.join("native-hidden-directory")).unwrap();
    fs::write(
        root.join("native-hidden-directory/child.txt"),
        b"hidden child",
    )
    .unwrap();
    fs::create_dir(root.join("later-hidden-directory")).unwrap();
    fs::write(
        root.join("later-hidden-directory/child.txt"),
        b"later hidden child",
    )
    .unwrap();
    set_native_hidden(&root.join("native-hidden.txt"));
    set_native_hidden(&root.join("native-hidden-directory"));

    init(root, "author").await.unwrap();
    let mut repository = Repository::discover(root).await.unwrap();
    let status = repository.status().await.unwrap();
    assert!(status.iter().any(|change| change.path == "visible.txt"));
    assert!(
        status
            .iter()
            .any(|change| change.path == "later-hidden-directory/child.txt")
    );
    assert!(
        status
            .iter()
            .all(|change| !change.path.starts_with("native-hidden"))
    );

    set_native_hidden(&root.join("later-hidden-directory"));
    let status = repository.status().await.unwrap();
    assert!(
        status
            .iter()
            .all(|change| !change.path.starts_with("later-hidden-directory"))
    );

    let inspection = repository.inspect_working_tree().await.unwrap();
    assert!(
        inspection
            .entries
            .iter()
            .any(|entry| entry.path == "native-hidden.txt")
    );
    assert!(
        inspection
            .entries
            .iter()
            .any(|entry| entry.path == "native-hidden-directory/child.txt")
    );
    assert!(
        inspection
            .changes
            .iter()
            .all(|change| !change.path.starts_with("native-hidden"))
    );

    fs::write(root.join(".synckignore"), b"!native-hidden.txt\n").unwrap();
    let status = repository.status().await.unwrap();
    assert!(
        status
            .iter()
            .all(|change| { change.path != "native-hidden.txt" })
    );
    repository
        .set_hidden_files_mode(HiddenFilesMode::Track)
        .unwrap();
    let status = repository.status().await.unwrap();
    assert!(
        status
            .iter()
            .any(|change| change.kind == ChangeKind::Add && change.path == "native-hidden.txt")
    );
    repository.commit("track whitelisted file").await.unwrap();

    repository
        .set_hidden_files_mode(HiddenFilesMode::Ignore)
        .unwrap();

    fs::write(root.join(".synckignore"), b"").unwrap();
    fs::write(root.join("native-hidden.txt"), b"changed").unwrap();
    let status = repository.status().await.unwrap();
    assert!(
        status.iter().any(|change| {
            change.kind == ChangeKind::Modify && change.path == "native-hidden.txt"
        })
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_system_entries_are_untracked() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("system-metadata.bin"), b"system").unwrap();
    set_native_system(&root.join("system-metadata.bin"));
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();

    assert!(repository.status().await.unwrap().is_empty());
    assert!(
        repository
            .inspect_working_tree()
            .await
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.path == "system-metadata.bin")
    );
}

#[tokio::test]
async fn repository_lock_fails_without_waiting() {
    let temporary = tempfile::tempdir().unwrap();
    init(temporary.path(), "author").await.unwrap();
    let first = Repository::discover(temporary.path()).await.unwrap();
    let error = match Repository::discover(temporary.path()).await {
        Ok(_) => panic!("second repository open unexpectedly acquired the lock"),
        Err(error) => error,
    };
    assert!(matches!(error, LocalError::Busy));
    drop(first);
    Repository::discover(temporary.path()).await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn scanner_rejects_discoverable_symlink() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    init(temporary.path(), "author").await.unwrap();
    symlink("outside", temporary.path().join("link")).unwrap();
    let repository = Repository::discover(temporary.path()).await.unwrap();
    let error = repository.status().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unsupported filesystem entry: link")
    );
}

#[tokio::test]
async fn same_path_folder_delete_and_file_add_are_atomic() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("node")).unwrap();
    fs::write(root.join("node/child"), b"child").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("tree").await.unwrap();

    // Create the replacement before deleting the tracked tree so Linux cannot
    // recycle an inode and make two distinct items share an identity.
    fs::write(root.join("replacement"), b"file").unwrap();
    fs::remove_dir_all(root.join("node")).unwrap();
    fs::rename(root.join("replacement"), root.join("node")).unwrap();
    let status = repository.status().await.unwrap();
    assert_eq!(
        status
            .iter()
            .map(|change| (change.kind, change.path.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (ChangeKind::Delete, "node"),
            (ChangeKind::Add, "node"),
            (ChangeKind::Delete, "node/child"),
        ]
    );
    let CommitOutcome::Committed(commit_id) =
        repository.commit("replace folder with file").await.unwrap()
    else {
        panic!("same-path delete and add commit was unexpectedly clean");
    };
    let shown = repository.show(&commit_id).await.unwrap();
    assert_eq!(
        shown
            .changes
            .iter()
            .map(|change| (change.kind, change.path.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (ChangeKind::Delete, "node"),
            (ChangeKind::Add, "node"),
            (ChangeKind::Delete, "node/child"),
        ]
    );
    assert!(repository.status().await.unwrap().is_empty());
    let findings = repository.fsck().await.unwrap();
    assert!(findings.is_empty(), "{findings:#?}");
}

#[tokio::test]
async fn fsck_reports_corrupt_object_bytes() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join("file"), b"content").unwrap();
    init(temporary.path(), "author").await.unwrap();
    let repository = Repository::discover(temporary.path()).await.unwrap();
    repository.commit("initial").await.unwrap();

    let digest = hex::encode(Sha256::digest(b"content"));
    corrupt_blob(temporary.path(), &digest);
    let findings = repository.fsck().await.unwrap();
    assert!(
        findings
            .iter()
            .any(|finding| finding.contains("chunk digest mismatch"))
    );
}

#[tokio::test]
async fn init_refuses_an_ancestor_repository() {
    let temporary = tempfile::tempdir().unwrap();
    init(temporary.path(), "author").await.unwrap();
    let child = temporary.path().join("child");
    fs::create_dir(&child).unwrap();
    let error = init(&child, "other").await.unwrap_err();
    assert!(error.to_string().contains("inside an existing repository"));
}

#[tokio::test]
async fn missing_projection_rebuilds_from_authoritative_history() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join("file"), b"content").unwrap();
    init(temporary.path(), "author").await.unwrap();
    let repository = Repository::discover(temporary.path()).await.unwrap();
    repository.commit("initial").await.unwrap();

    let connection =
        rusqlite::Connection::open(temporary.path().join(".synck/repository.local.sqlite3")).unwrap();
    connection
        .execute_batch("DELETE FROM local_projection_state; DELETE FROM local_file_heads;")
        .unwrap();
    drop(connection);

    assert!(repository.status().await.unwrap().is_empty());
    let findings = repository.fsck().await.unwrap();
    assert!(findings.is_empty(), "{findings:#?}");
}

#[tokio::test]
async fn tracked_ignore_file_cannot_ignore_itself_after_commit() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join(".synckignore"), b"nothing\n").unwrap();
    init(temporary.path(), "author").await.unwrap();
    let repository = Repository::discover(temporary.path()).await.unwrap();
    repository.commit("track ignore file").await.unwrap();

    fs::write(temporary.path().join(".synckignore"), b".synckignore\n").unwrap();
    let status = repository.status().await.unwrap();
    assert!(
        status
            .iter()
            .any(|change| { change.kind == ChangeKind::Modify && change.path == ".synckignore" })
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn scanner_rejects_portable_name_collision() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join("Name"), b"one").unwrap();
    fs::write(temporary.path().join("name"), b"two").unwrap();
    let error = init(temporary.path(), "author").await.unwrap_err();
    assert!(error.to_string().contains("portable name collision"));
}

#[tokio::test]
async fn scanner_rejects_nested_repository_metadata() {
    let temporary = tempfile::tempdir().unwrap();
    fs::create_dir_all(temporary.path().join("child/.synck")).unwrap();
    let error = init(temporary.path(), "author").await.unwrap_err();
    assert!(error.to_string().contains("nested .synck"));
    assert!(!temporary.path().join(".synck").exists());
    assert!(!temporary.path().join(".synckignore").exists());
}

#[tokio::test]
async fn failed_init_preserves_a_preexisting_ignore_file() {
    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join(".synckignore"), b"*.private\n").unwrap();
    fs::create_dir_all(temporary.path().join("child/.synck")).unwrap();

    let error = init(temporary.path(), "author").await.unwrap_err();

    assert!(error.to_string().contains("nested .synck"));
    assert!(!temporary.path().join(".synck").exists());
    assert_eq!(
        fs::read(temporary.path().join(".synckignore")).unwrap(),
        b"*.private\n"
    );
}

#[tokio::test]
async fn shared_profile_rejects_a_case_variant_nested_control_component() {
    let temporary = tempfile::tempdir().unwrap();
    fs::create_dir(temporary.path().join("parent")).unwrap();
    fs::write(temporary.path().join(".synckignore"), b"").unwrap();
    init(temporary.path(), "author").await.unwrap();
    let mut repository = Repository::discover(temporary.path()).await.unwrap();
    repository.commit("baseline").await.unwrap();
    repository
        .set_hidden_files_mode(HiddenFilesMode::Track)
        .unwrap();
    fs::create_dir(temporary.path().join("parent/.SYNCK")).unwrap();

    let status = repository.status().await.unwrap();
    assert!(status.iter().any(|change| change.path == "parent/.SYNCK"));
    let accepted_before = repository.log(None).await.unwrap().len();
    let error = repository.commit("reserved path").await.unwrap_err();
    assert!(error.to_string().contains("RESERVED_WORKTREE_PATH"));
    assert_eq!(repository.log(None).await.unwrap().len(), accepted_before);
}

#[cfg(unix)]
#[tokio::test]
async fn ignored_untracked_symlink_is_not_visited_by_status_or_unrelated_restore() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    fs::write(temporary.path().join(".synckignore"), b"link\n").unwrap();
    fs::write(temporary.path().join("tracked.bin"), b"before").unwrap();
    init(temporary.path(), "author").await.unwrap();
    let repository = Repository::discover(temporary.path()).await.unwrap();
    repository.commit("initial").await.unwrap();
    symlink("missing", temporary.path().join("link")).unwrap();
    fs::write(temporary.path().join("tracked.bin"), b"after").unwrap();

    let status = repository.status().await.unwrap();
    assert!(status.iter().all(|change| change.path != "link"));
    assert!(repository.restore_worktree().await.unwrap());
    assert_eq!(
        fs::read(temporary.path().join("tracked.bin")).unwrap(),
        b"before"
    );
    assert_eq!(
        fs::read_link(temporary.path().join("link")).unwrap(),
        Path::new("missing")
    );
}

#[tokio::test]
async fn corrupt_nearer_metadata_shadows_an_ancestor_repository() {
    let temporary = tempfile::tempdir().unwrap();
    init(temporary.path(), "author").await.unwrap();
    let child = temporary.path().join("child");
    let descendant = child.join("descendant");
    fs::create_dir_all(&descendant).unwrap();
    fs::write(child.join(".synck"), b"not a repository").unwrap();
    let error = match Repository::discover(&descendant).await {
        Ok(_) => panic!("discovery incorrectly fell through to the ancestor repository"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("invalid repository metadata entry")
    );
}

#[tokio::test]
async fn scanner_cleans_regular_restore_temps_and_rejects_wrong_types() {
    let temporary = tempfile::tempdir().unwrap();
    init(temporary.path(), "author").await.unwrap();
    let repository = Repository::discover(temporary.path()).await.unwrap();
    let stale = temporary
        .path()
        .join(format!(".synck-tmp-{}", uuid::Uuid::new_v4()));
    fs::write(&stale, b"stale").unwrap();
    repository.status().await.unwrap();
    assert!(!stale.exists());

    let invalid = temporary
        .path()
        .join(format!(".synck-tmp-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&invalid).unwrap();
    let error = repository.status().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("reserved temporary entry is not a regular file")
    );
}

#[tokio::test]
async fn commit_never_reuses_a_corrupt_existing_object() {
    let temporary = tempfile::tempdir().unwrap();
    let file = temporary.path().join("file");
    fs::write(&file, b"first").unwrap();
    init(temporary.path(), "author").await.unwrap();
    let repository = Repository::discover(temporary.path()).await.unwrap();
    repository.commit("first").await.unwrap();
    fs::write(&file, b"second").unwrap();
    repository.commit("second").await.unwrap();

    let first_digest = hex::encode(Sha256::digest(b"first"));
    corrupt_blob(temporary.path(), &first_digest);
    fs::write(&file, b"first").unwrap();
    let error = repository.commit("must fail").await.unwrap_err();
    assert!(matches!(error, LocalError::Integrity(_)), "{error:?}");
    assert_eq!(repository.log(None).await.unwrap().len(), 3);
}

#[tokio::test]
async fn new_files_receive_hidden_initial_named_versions_in_the_same_commit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("one.txt"), b"one").unwrap();
    fs::write(root.join("two.txt"), b"two").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    let commit_id = match repository.commit("initial files").await.unwrap() {
        CommitOutcome::Committed(commit_id) => commit_id,
        CommitOutcome::Clean => panic!("initial files were not committed"),
    };

    assert_eq!(repository.log(None).await.unwrap().len(), 2);
    assert!(repository.status().await.unwrap().is_empty());
    for name in ["one.txt", "two.txt"] {
        let versions = repository
            .named_versions(root, Path::new(name), false)
            .await
            .unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].label.as_deref(), Some("0.01"));
        assert_eq!(versions[0].kind, "MINOR");
        assert_eq!(versions[0].source_item_seq, 2);
        assert_eq!(versions[0].lifecycle_state, "LIVE");
        assert_eq!(
            repository
                .history(root, Path::new(name))
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            repository.history(root, Path::new(name)).await.unwrap()[0].commit_id,
            commit_id
        );
    }
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn named_version_lifecycle_is_append_only_and_restore_is_a_worktree_change() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.txt"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("document.txt"), b"second").unwrap();
    repository.commit("second revision").await.unwrap();

    repository
        .create_named_version(
            root,
            Path::new("document.txt"),
            None,
            "MAJOR",
            Some("sent to client"),
            "name delivered revision",
        )
        .await
        .unwrap();
    let versions = repository
        .named_versions(root, Path::new("document.txt"), false)
        .await
        .unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].label.as_deref(), Some("0.01"));
    assert_eq!(versions[1].label.as_deref(), Some("1.00"));
    assert_eq!(versions[1].description.as_deref(), Some("sent to client"));
    let version_id = versions[1].version_id.clone();

    repository
        .set_named_version_description(
            root,
            Path::new("document.txt"),
            &version_id,
            "corrected context",
            "correct version context",
        )
        .await
        .unwrap();
    repository
        .set_named_version_kind(
            root,
            Path::new("document.txt"),
            &version_id,
            "MINOR",
            "demote checkpoint",
        )
        .await
        .unwrap();
    let versions = repository
        .named_versions(root, Path::new("document.txt"), false)
        .await
        .unwrap();
    assert_eq!(versions[1].label.as_deref(), Some("0.02"));
    assert_eq!(
        versions[1].description.as_deref(),
        Some("corrected context")
    );

    let initial_id = versions[0].version_id.clone();
    repository
        .restore_named_version(root, Path::new("document.txt"), &initial_id)
        .await
        .unwrap();
    assert_eq!(fs::read(root.join("document.txt")).unwrap(), b"first");
    assert_eq!(repository.status().await.unwrap().len(), 1);
    assert!(repository.restore_worktree().await.unwrap());

    let tombstone_commit = repository
        .tombstone_named_version(
            root,
            Path::new("document.txt"),
            &version_id,
            "remove obsolete checkpoint",
        )
        .await
        .unwrap();
    assert_eq!(
        repository
            .named_versions(root, Path::new("document.txt"), false)
            .await
            .unwrap()
            .len(),
        1
    );
    let all = repository
        .named_versions(root, Path::new("document.txt"), true)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].lifecycle_state, "TOMBSTONED");
    assert_eq!(all[1].label, None);
    let error = repository
        .revert(&tombstone_commit, "cannot reverse permanent label deletion")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("requires a filesystem-only commit"),
        "{error}"
    );
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn root_init_creates_complete_chunked_checkout_without_ignoring_ordinary_folders() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    init(root, "author").await.unwrap();
    let portable = synck_local::portable::PortableRepository::open(root).unwrap();
    assert_eq!(portable.storage_mode(), synck_local::StorageMode::Chunked);
    assert_eq!(
        portable.authority(),
        synck_local::portable::Authority::IndependentLocal
    );
    assert!(portable.checkout_paths().is_some());
    assert_eq!(portable.verify().await.unwrap().commits, 1);
    drop(portable);
    for name in ["target", "dist", "build"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("notes.txt"), b"project notes").unwrap();
    }
    fs::create_dir(root.join("node_modules")).unwrap();
    fs::write(root.join("node_modules/cache"), b"ignored").unwrap();
    fs::write(root.join(".hidden"), b"ignored").unwrap();
    let repository = Repository::discover(root).await.unwrap();
    assert_eq!(repository.hidden_files_mode(), HiddenFilesMode::Ignore);
    let changes = repository.status().await.unwrap();
    assert_eq!(changes.len(), 6);
    for name in ["target", "dist", "build"] {
        assert!(
            changes
                .iter()
                .any(|change| change.path == format!("{name}/notes.txt"))
        );
    }
    repository.commit("project folders").await.unwrap();
    assert!(repository.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn atomic_commit_exceeds_old_event_cap_and_includes_a_rename() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    synck_local::init(&root, "tester").await.unwrap();
    fs::write(root.join("before.txt"), b"retained identity").unwrap();
    let repository = Repository::discover(&root).await.unwrap();
    repository.commit("before").await.unwrap();
    fs::rename(root.join("before.txt"), root.join("after.txt")).unwrap();
    for index in 0..3334 {
        fs::write(root.join(format!("file-{index:04}.txt")), b"data").unwrap();
    }
    repository.commit("one atomic large change").await.unwrap();
    let commits = repository.log(None).await.unwrap();
    assert_eq!(commits.len(), 3);
    assert!(commits[0].event_count > 10_000);
    assert!(repository.status().await.unwrap().is_empty());
    repository.verify_complete().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn init_reports_all_invalid_names_with_noncolliding_fixes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join(".synckignore"), b"ignored:entry\n").unwrap();
    for name in ["bad:name", "bad?name", "bad_name", "ignored:entry"] {
        fs::write(root.join(name), name.as_bytes()).unwrap();
    }
    fs::create_dir(root.join("folder?")).unwrap();
    fs::write(root.join("folder?/NUL.txt"), b"keep").unwrap();
    let error = init(root, "author").await.unwrap_err().to_string();
    for diagnostic in [
        "4 non-portable name(s)",
        "\"bad:name\" -> \"bad_name (2)\"",
        "\"bad?name\" -> \"bad_name (3)\"",
        "\"folder?\" -> \"folder_\"",
        "\"folder?/NUL.txt\" -> \"folder_/_NUL.txt\"",
    ] {
        assert!(error.contains(diagnostic), "missing {diagnostic}: {error}");
    }
    assert!(!error.contains("ignored:entry"));
    assert!(!root.join(".synck").exists());
    assert_eq!(fs::read(root.join("bad:name")).unwrap(), b"bad:name");
    assert_eq!(fs::read(root.join("folder?/NUL.txt")).unwrap(), b"keep");
}

#[cfg(unix)]
#[tokio::test]
async fn normalization_only_name_repair_preserves_bytes_and_is_idempotent() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    init(root, "author").await.unwrap();
    fs::write(root.join("e\u{301}.txt"), b"normalization").unwrap();
    let repository = Repository::discover(root).await.unwrap();
    let repairs = repository.fix_names().await.unwrap();
    assert_eq!(repairs.len(), 1);
    assert_eq!(repairs[0].to, "é.txt");
    assert_eq!(fs::read(root.join("é.txt")).unwrap(), b"normalization");
    assert!(repository.fix_names().await.unwrap().is_empty());
}
