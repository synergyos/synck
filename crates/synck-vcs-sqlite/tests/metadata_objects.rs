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

#[tokio::test]
async fn accepted_event_keeps_only_metadata_id_and_reads_without_payload() {
    use std::sync::Arc;
    use synck_vcs::config::CoreConfig;
    use synck_vcs::storage::Storage;
    use synck_vcs::types::{CommitProposal, EventProposal, ItemId, canonical_event_proposal_bytes};
    let directory = tempfile::tempdir().unwrap();
    let storage = SqliteStorage::new(directory.path().join("history.sqlite3"));
    storage.init_schema().await.unwrap();
    let repo = RepoId::new();
    let item = ItemId::new();
    let event = EventProposal::from_fields(serde_json::json!({
        "item_id": item, "event_type":"CHAT::CREATE",
        "create_item":{"item_type":"CHAT"},
        "set_metadata":{"private_note":"erasable value"},
    }))
    .unwrap();
    let id = event.set_metadata.as_ref().unwrap().id();
    let expected = canonical_event_proposal_bytes(&event).unwrap();
    assert!(!String::from_utf8_lossy(&expected).contains("erasable value"));
    assert!(!String::from_utf8_lossy(&expected).contains("set_metadata"));
    let core = synck_vcs::repo::Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    core.append(
        repo.clone(),
        CommitProposal {
            author_id: "writer".into(),
            committer_id: "writer".into(),
            message: None,
            occurred_at: None,
            operation_id: Some("metadata-write".into()),
            events: vec![event],
        },
    )
    .await
    .unwrap();
    core.sequence_repo_activity(repo.clone(), 10).await.unwrap();
    let rows = storage
        .list_events_for_item_readonly(&repo, &item, 1, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].payload, expected);
    assert_eq!(
        rows[0]
            .proposal()
            .unwrap()
            .set_metadata
            .unwrap()
            .value()
            .unwrap()["private_note"],
        "erasable value"
    );
    storage
        .open_writable()
        .unwrap()
        .execute(
            "DELETE FROM vcs_metadata_objects WHERE repo_id=?1 AND object_id=?2",
            rusqlite::params![repo.as_str(), id.as_bytes().as_slice()],
        )
        .unwrap();
    storage.verify_checkpoints(&repo).unwrap();
    let rows = storage
        .list_events_for_item_readonly(&repo, &item, 1, 10)
        .await
        .unwrap();
    let event = rows[0].proposal().unwrap();
    assert_eq!(event.set_metadata.as_ref().unwrap().id(), id);
    assert!(event.set_metadata.as_ref().unwrap().value().is_none());
    assert_eq!(canonical_event_proposal_bytes(&event).unwrap(), expected);
}
