use synck_local::portable::{PortableRepository, ServedRepository};

#[tokio::test]
async fn served_repository_reports_published_head_while_the_repository_is_locked() {
    let parent = tempfile::tempdir().unwrap();
    let origin = parent.path().join("origin");
    let repo_id = synck_local::init(&origin, "owner").await.unwrap();

    let locked = PortableRepository::open(&origin).unwrap();
    let served = ServedRepository::open(&origin).unwrap();
    let capabilities = served.capabilities().unwrap();
    capabilities.validate().unwrap();
    assert_eq!(served.repository_id(), &repo_id);
    assert_eq!(capabilities.repository.repo_id, repo_id);
    assert_eq!(capabilities.latest.repo_seq, 1);
    assert!(!capabilities.encrypted);
    drop(locked);

    let clone = parent.path().join("clone");
    synck_local::clone_repository(&origin, &clone, None, &[], false)
        .await
        .unwrap();
    assert!(ServedRepository::open(&clone).is_err());
}
