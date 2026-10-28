use synck_vcs::metadata::Metadata;
use synck_vcs::storage::StorageBackend;
use synck_vcs::types::{ObjectId, RepoId};
use synck_vcs_sqlite::SqliteStorage;

#[tokio::test]
async fn metadata_roundtrips_and_missing_differs_from_corrupt() {
    let directory = tempfile::tempdir().unwrap();
    let storage = SqliteStorage::new(directory.path().join("history.sqlite3"));
    storage.init_schema().await.unwrap();
    let repo = RepoId::new();
    let metadata = Metadata::new(serde_json::json!({"subject":"hello"})).unwrap();
    storage.put_metadata(&repo, &metadata).unwrap();
    storage.put_metadata(&repo, &metadata).unwrap();
    let copy = storage
        .get_metadata(&repo, &metadata.id())
        .unwrap()
        .unwrap();
    assert_eq!(copy.payload(), metadata.payload());
    assert_eq!(copy.value(), metadata.value());
    assert_eq!(storage.verify_metadata_objects(&repo).unwrap(), 1);
    assert!(
        storage
            .get_metadata(&repo, &ObjectId::from_digest([0; 32]))
            .unwrap()
            .is_none()
    );
    assert!(
        storage
            .get_metadata(&RepoId::new(), &metadata.id())
            .unwrap()
            .is_none()
    );
    let mut changed = metadata.payload().to_vec();
    *changed.last_mut().unwrap() ^= 1;
    storage
        .open_writable()
        .unwrap()
        .execute(
            "UPDATE vcs_metadata_objects SET payload=?1 WHERE repo_id=?2 AND object_id=?3",
            rusqlite::params![changed, repo.as_str(), metadata.id().as_bytes().as_slice()],
        )
        .unwrap();
    assert!(storage.get_metadata(&repo, &metadata.id()).is_err());
    assert!(storage.verify_metadata_objects(&repo).is_err());
    assert!(storage.put_metadata(&repo, &metadata).is_err());
}
