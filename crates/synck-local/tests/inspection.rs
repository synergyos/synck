use std::fs;

use synck_local::{
    CommitOutcome, HistoricalItemKind, Repository, TrackedItemKind, WorkingTreeEntryKind,
    cleanup_historical_cache, init,
};

#[tokio::test]
async fn working_tree_reports_live_paths_and_untracked_entries() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("folder")).unwrap();
    fs::write(root.join("folder/file.txt"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    fs::create_dir(root.join("moved")).unwrap();
    fs::rename(root.join("folder/file.txt"), root.join("moved/file.txt")).unwrap();
    fs::write(root.join("new.txt"), b"new").unwrap();

    let entries = repository.working_tree().await.unwrap();
    let moved = entries
        .iter()
        .find(|entry| entry.path == "moved/file.txt")
        .unwrap();
    assert_eq!(moved.kind, WorkingTreeEntryKind::File);
    assert!(moved.item_id.is_some());
    assert!(entries.iter().all(|entry| entry.path != "folder/file.txt"));
    assert!(entries.iter().any(|entry| {
        entry.path == "new.txt"
            && entry.kind == WorkingTreeEntryKind::File
            && entry.item_id.is_none()
    }));
    assert!(entries.iter().any(|entry| {
        entry.path == "moved"
            && entry.kind == WorkingTreeEntryKind::Folder
            && entry.item_id.is_none()
    }));
}

#[tokio::test]
async fn working_tree_edits_are_uncommitted_and_reject_stale_entries() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("folder")).unwrap();
    fs::write(root.join("folder/file.txt"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    let initial = repository.working_tree().await.unwrap();
    let folder = initial.iter().find(|entry| entry.path == "folder").unwrap();
    let file = initial
        .iter()
        .find(|entry| entry.path == "folder/file.txt")
        .unwrap();
    let drafts = repository
        .create_working_folder(None, "drafts")
        .await
        .unwrap();
    let note = repository
        .create_working_file(Some(&drafts), "note.txt")
        .await
        .unwrap();
    let renamed = repository
        .rename_working_entry(&note, "renamed.txt")
        .await
        .unwrap();
    let drafts = repository
        .working_tree()
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.path == "drafts")
        .unwrap();
    let moved = repository
        .move_working_entry(file, Some(&drafts))
        .await
        .unwrap();
    assert_eq!(renamed.path, "drafts/renamed.txt");
    assert_eq!(moved.path, "drafts/file.txt");
    assert!(!repository.status().await.unwrap().is_empty());

    let error = repository
        .rename_working_entry(file, "stale.txt")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("refresh before continuing"));

    repository.delete_working_entry(&renamed).await.unwrap();
    assert!(!root.join("drafts/renamed.txt").exists());
    assert!(!root.join(".synck/working-tree-edit.json").exists());
    assert!(!root.join(".synck/working-tree-edit-stage").exists());
    let drafts = repository
        .working_tree()
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.path == "drafts")
        .unwrap();
    fs::write(root.join("drafts/late.txt"), b"late").unwrap();
    let error = repository.delete_working_entry(&drafts).await.unwrap_err();
    assert!(error.to_string().contains("refresh before continuing"));
    assert!(root.join("drafts/late.txt").exists());
    assert!(folder.item_id.is_some());
}

#[tokio::test]
async fn working_tree_imports_regular_files_without_overwriting() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    let external = temporary.path().join("external");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&external).unwrap();
    fs::write(external.join("note.txt"), b"imported").unwrap();
    init(&root, "author").await.unwrap();
    let repository = Repository::discover(&root).await.unwrap();

    let imported = repository
        .import_working_files(None, &[external.join("note.txt")])
        .await
        .unwrap();
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].path, "note.txt");
    assert_eq!(fs::read(root.join("note.txt")).unwrap(), b"imported");

    fs::write(external.join("note.txt"), b"replacement").unwrap();
    assert!(
        repository
            .import_working_files(None, &[external.join("note.txt")])
            .await
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );
    assert_eq!(fs::read(root.join("note.txt")).unwrap(), b"imported");
}

#[tokio::test]
async fn working_tree_shows_ignored_entries_and_fences_recursive_delete() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    init(root, "author").await.unwrap();
    fs::write(root.join(".synckignore"), b"ignored/\n").unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("ignore local output").await.unwrap();

    fs::create_dir(root.join("ignored")).unwrap();
    fs::write(root.join("ignored/first.txt"), b"first").unwrap();
    let snapshot = repository.inspect_working_tree().await.unwrap();
    assert!(snapshot.changes.is_empty());
    let ignored = snapshot
        .entries
        .into_iter()
        .find(|entry| entry.path == "ignored")
        .unwrap();
    fs::write(root.join("ignored/late.txt"), b"late").unwrap();

    let error = repository.delete_working_entry(&ignored).await.unwrap_err();
    assert!(error.to_string().contains("refresh before continuing"));
    assert_eq!(fs::read(root.join("ignored/first.txt")).unwrap(), b"first");
    assert_eq!(fs::read(root.join("ignored/late.txt")).unwrap(), b"late");
}

#[tokio::test]
async fn working_tree_import_rejects_canonical_duplicate_names() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let upper = first.join("Caf\u{e9}.txt");
    let lower = second.join("caf\u{e9}.txt");
    let decomposed = second.join("cafe\u{301}.txt");
    fs::write(&upper, b"first").unwrap();
    fs::write(&lower, b"second").unwrap();
    fs::write(&decomposed, b"third").unwrap();
    init(&root, "author").await.unwrap();
    let repository = Repository::discover(&root).await.unwrap();

    let error = repository
        .import_working_files(None, &[upper, lower])
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "selected files contain duplicate names");
    // Names must already be NFC; a decomposed spelling is not portable.
    let error = repository
        .import_working_files(None, &[decomposed])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("is not portable"), "{error}");
    assert!(!root.join("Caf\u{e9}.txt").exists());
    assert!(!root.join("caf\u{e9}.txt").exists());
    assert!(!root.join("cafe\u{301}.txt").exists());
}

#[tokio::test]
async fn inspection_reports_repository_and_tracked_items() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("work");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("folder")).unwrap();
    fs::write(root.join("folder/file.txt"), b"content").unwrap();

    let expected_repo_id = init(&root, "author@example.test").await.unwrap();
    let repository = Repository::discover(&root.join("folder")).await.unwrap();
    repository.commit("initial").await.unwrap();

    let info = repository.info();
    assert_eq!(info.root, root.canonicalize().unwrap());
    assert_eq!(info.format_version, 7);
    assert_eq!(info.repo_id, expected_repo_id.as_str());
    assert_eq!(info.author, "author@example.test");

    let items = repository.tracked_items().await.unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].path, ".synckignore");
    assert_eq!(items[0].kind, TrackedItemKind::File);
    assert_eq!(items[0].item_seq, 1);
    assert_eq!(items[0].content_item_seq, Some(1));
    assert_eq!(items[1].path, "folder");
    assert_eq!(items[1].kind, TrackedItemKind::Folder);
    assert_eq!(items[1].item_seq, 1);
    assert_eq!(items[1].content_item_seq, None);
    assert_eq!(items[1].size, None);
    assert_eq!(items[2].path, "folder/file.txt");
    assert_eq!(items[2].kind, TrackedItemKind::File);
    assert_eq!(items[2].item_seq, 1);
    assert_eq!(items[2].content_item_seq, Some(1));
    assert_eq!(items[2].size, Some(7));

    let initial_modified_at = items[2].modified_at;
    fs::write(root.join("folder/file.txt"), b"updated").unwrap();
    repository.commit("update file").await.unwrap();
    let modified = repository.tracked_items().await.unwrap();
    assert_eq!(modified[2].item_seq, 2);
    assert_eq!(modified[2].content_item_seq, Some(2));
    assert_eq!(modified[2].size, Some(7));
    assert!(modified[2].modified_at >= initial_modified_at);

    fs::rename(
        root.join("folder/file.txt"),
        root.join("folder/renamed.txt"),
    )
    .unwrap();
    repository.commit("rename file").await.unwrap();
    let renamed = repository.tracked_items().await.unwrap();
    assert_eq!(renamed[2].path, "folder/renamed.txt");
    assert_eq!(renamed[2].item_seq, 3);
    assert_eq!(renamed[2].content_item_seq, Some(2));
    assert!(renamed[2].modified_at >= modified[2].modified_at);

    fs::remove_file(root.join("folder/renamed.txt")).unwrap();
    repository.commit("remove file").await.unwrap();
    let remaining = repository.tracked_items().await.unwrap();
    assert_eq!(remaining.len(), 2);
    assert_eq!(remaining[0].path, ".synckignore");
    assert_eq!(remaining[1].path, "folder");
}

#[tokio::test]
async fn author_updates_affect_only_future_commits() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("file.txt"), b"one").unwrap();
    init(root, "first author").await.unwrap();
    let mut repository = Repository::discover(root).await.unwrap();
    repository.commit("first").await.unwrap();

    repository.set_author("second author").unwrap();
    assert_eq!(repository.info().author, "second author");
    fs::write(root.join("file.txt"), b"two").unwrap();
    repository.commit("second").await.unwrap();
    drop(repository);

    let mut repository = Repository::discover(root).await.unwrap();
    assert_eq!(repository.info().author, "second author");
    let log = repository.log(None).await.unwrap();
    assert_eq!(log[0].author, "second author");
    assert_eq!(log[1].author, "first author");

    assert!(repository.set_author("").is_err());
    assert_eq!(repository.info().author, "second author");
    drop(repository);
    assert_eq!(
        Repository::discover(root).await.unwrap().info().author,
        "second author"
    );
}

#[tokio::test]
async fn log_pages_are_newest_first_and_use_exclusive_cursors() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("file.txt"), b"one").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();

    for (index, (message, contents)) in [
        ("one", b"one".as_slice()),
        ("two", b"two".as_slice()),
        ("three", b"three".as_slice()),
    ]
    .into_iter()
    .enumerate()
    {
        if index == 1 {
            let connection =
                rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
            connection
                .execute("UPDATE vcs_repo_activity_heads SET next_repo_seq = 3", [])
                .unwrap();
        }
        fs::write(root.join("file.txt"), contents).unwrap();
        let CommitOutcome::Committed(_) = repository.commit(message).await.unwrap() else {
            panic!("test commit was unexpectedly clean");
        };
    }

    let first = repository.log_page(None, 2).await.unwrap();
    assert_eq!(
        first
            .entries
            .iter()
            .map(|entry| (entry.repo_seq, entry.message.as_deref()))
            .collect::<Vec<_>>(),
        vec![(4, Some("three")), (3, Some("two"))]
    );
    assert_eq!(first.next_cursor, Some(3));

    let second = repository.log_page(first.next_cursor, 2).await.unwrap();
    assert_eq!(second.entries.len(), 2);
    assert_eq!(second.entries[0].repo_seq, 2);
    assert_eq!(second.entries[0].message.as_deref(), Some("one"));
    assert_eq!(second.entries[1].repo_seq, 1);
    assert_eq!(
        second.entries[1].message.as_deref(),
        Some("Initialize Synck repository")
    );
    assert_eq!(second.next_cursor, None);
    assert_eq!(
        first
            .entries
            .iter()
            .chain(&second.entries)
            .map(|entry| entry.repo_seq)
            .collect::<Vec<_>>(),
        vec![4, 3, 2, 1]
    );

    assert!(repository.log_page(None, 0).await.is_err());
    assert!(repository.log_page(None, 1_001).await.is_err());
}

#[tokio::test]
async fn historical_view_is_pinned_and_pages_root_children() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    for number in 0..257 {
        fs::create_dir(root.join(format!("folder-{number:03}"))).unwrap();
    }
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    let initial = match repository.commit("initial").await.unwrap() {
        CommitOutcome::Committed(commit) => commit,
        CommitOutcome::Clean => panic!("initial tree was clean"),
    };
    let view = repository.open_historical_view(&initial).await.unwrap();
    assert_eq!(view.repo_seq, 2);
    assert_eq!(view.commit_id, initial);
    assert!(view.state_root.starts_with("sha256:"));

    fs::create_dir(root.join("folder-new")).unwrap();
    repository.commit("later").await.unwrap();

    let first = repository
        .historical_children(&view, None, None)
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 256);
    assert_eq!(
        first
            .entries
            .iter()
            .filter(|item| item.kind == HistoricalItemKind::File)
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        [".synckignore"]
    );
    let latest_view = repository.open_historical_view("HEAD").await.unwrap();
    assert!(
        repository
            .historical_children(&latest_view, None, first.next_cursor.as_ref())
            .await
            .unwrap_err()
            .to_string()
            .contains("another state root")
    );
    let folder = first
        .entries
        .iter()
        .find(|item| item.kind == HistoricalItemKind::Folder)
        .unwrap()
        .item_id
        .clone();
    assert!(
        repository
            .historical_children(&view, Some(&folder), first.next_cursor.as_ref())
            .await
            .unwrap_err()
            .to_string()
            .contains("another parent")
    );
    let second = repository
        .historical_children(&view, None, first.next_cursor.as_ref())
        .await
        .unwrap();
    assert_eq!(second.entries.len(), 2);
    assert!(second.next_cursor.is_none());
    assert_eq!(
        first
            .entries
            .iter()
            .chain(&second.entries)
            .filter(|item| item.name == "folder-new")
            .count(),
        0
    );

    let other_root = tempfile::tempdir().unwrap();
    init(other_root.path(), "other").await.unwrap();
    let other = Repository::discover(other_root.path()).await.unwrap();
    assert!(
        other
            .historical_children(&view, None, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("another repository")
    );
}

#[tokio::test]
async fn historical_pages_count_only_visible_files() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("!before-sidecars.txt"), b"content").unwrap();
    for number in 0..512 {
        fs::write(root.join(format!("file-{number:03}.txt")), b"content").unwrap();
    }
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    let revision = match repository.commit("initial").await.unwrap() {
        CommitOutcome::Committed(commit) => commit,
        CommitOutcome::Clean => panic!("initial tree was clean"),
    };
    let view = repository.open_historical_view(&revision).await.unwrap();

    let first = repository
        .historical_children(&view, None, None)
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 256);
    assert!(first.next_cursor.is_some());
    assert!(
        first
            .entries
            .iter()
            .all(|item| item.kind == HistoricalItemKind::File)
    );

    let second = repository
        .historical_children(&view, None, first.next_cursor.as_ref())
        .await
        .unwrap();
    assert_eq!(second.entries.len(), 256);
    assert!(second.next_cursor.is_some());

    let third = repository
        .historical_children(&view, None, second.next_cursor.as_ref())
        .await
        .unwrap();
    assert_eq!(third.entries.len(), 2);
    assert!(third.next_cursor.is_none());

    let mut names = first
        .entries
        .into_iter()
        .chain(second.entries)
        .chain(third.entries)
        .map(|item| item.name)
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names.len(), 514);
    assert_eq!(
        names.first().map(String::as_str),
        Some("!before-sidecars.txt")
    );
    assert_eq!(names.last().map(String::as_str), Some("file-511.txt"));
}

#[tokio::test]
async fn historical_page_crosses_the_reserved_interval_at_an_exact_boundary() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    for number in 0..256 {
        fs::write(root.join(format!("!before-{number:03}.txt")), b"content").unwrap();
    }
    fs::write(root.join("z-after.txt"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    let revision = match repository.commit("initial").await.unwrap() {
        CommitOutcome::Committed(commit) => commit,
        CommitOutcome::Clean => panic!("initial tree was clean"),
    };
    let view = repository.open_historical_view(&revision).await.unwrap();

    let first = repository
        .historical_children(&view, None, None)
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 256);
    assert!(first.next_cursor.is_some());
    assert!(
        first
            .entries
            .iter()
            .all(|item| item.name.starts_with("!before-"))
    );

    let second = repository
        .historical_children(&view, None, first.next_cursor.as_ref())
        .await
        .unwrap();
    assert_eq!(
        second
            .entries
            .iter()
            .map(|item| item.name.as_str())
            .collect::<Vec<_>>(),
        [".synckignore", "z-after.txt"]
    );
    assert!(second.next_cursor.is_none());
}

#[tokio::test]
async fn repository_discovery_rejects_an_old_format() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    init(root, "author").await.unwrap();
    let config_path = root.join(".synck/config.json");
    let config = fs::read_to_string(&config_path).unwrap();
    let old = config.replace("\"format_version\":7", "\"format_version\":2");
    assert_ne!(old, config);
    fs::write(&config_path, old).unwrap();

    let error = match Repository::discover(root).await {
        Ok(_) => panic!("repository discovery accepted format 2"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("format-7"));
}

#[tokio::test]
async fn historical_file_publication_is_exact_and_manifest_owned() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("repository");
    let cache = temporary.path().join("cache");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("Folder")).unwrap();
    fs::write(root.join("Folder/document.bin"), b"old bytes").unwrap();
    init(&root, "author").await.unwrap();
    let repository = Repository::discover(&root).await.unwrap();
    let initial = match repository.commit("initial").await.unwrap() {
        CommitOutcome::Committed(commit) => commit,
        CommitOutcome::Clean => panic!("initial tree was clean"),
    };
    let view = repository.open_historical_view(&initial).await.unwrap();
    fs::write(root.join("Folder/document.bin"), b"new bytes").unwrap();
    repository.commit("update").await.unwrap();

    let root_page = repository
        .historical_children(&view, None, None)
        .await
        .unwrap();
    let folder = root_page
        .entries
        .iter()
        .find(|item| item.name == "Folder")
        .unwrap();
    let folder_page = repository
        .historical_children(&view, Some(&folder.item_id), None)
        .await
        .unwrap();
    let file = folder_page
        .entries
        .iter()
        .find(|item| item.name == "document.bin")
        .unwrap();
    assert_eq!(file.kind, HistoricalItemKind::File);

    let mut publication = repository
        .materialize_historical_file(&view, &file.item_id, &cache)
        .await
        .unwrap();
    assert_eq!(fs::read(&publication.path).unwrap(), b"old bytes");
    assert!(publication.cleanup().unwrap());
    assert!(!publication.path.exists());

    fs::create_dir(cache.join("not-owned")).unwrap();
    #[cfg(unix)]
    {
        let linked_target = temporary.path().join("linked-target");
        fs::create_dir(&linked_target).unwrap();
        std::os::unix::fs::symlink(&linked_target, cache.join("linked-publication")).unwrap();
    }
    let mut publication = repository
        .materialize_historical_file(&view, &file.item_id, &cache)
        .await
        .unwrap();
    assert_eq!(cleanup_historical_cache(&cache).unwrap(), 0);
    assert!(publication.path.exists());
    assert!(publication.cleanup().unwrap());
    assert!(!publication.path.exists());
    assert!(cache.join("not-owned").is_dir());
    #[cfg(unix)]
    assert!(cache.join("linked-publication").is_symlink());
}
