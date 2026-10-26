use std::{fs, path::Path, sync::Arc};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synck_items::{
    chat_organization::{self, ChatAclMode, ChatOrganization},
    native_calendars::{CalendarEvent, EventTime},
    reader::NativeState,
};
use synck_local::portable::{
    BundleSnapshot, PortableRepository, import_repository, materialize_repository,
};
use synck_vcs::{
    config::CoreConfig,
    repo::Repo,
    storage::{Storage, StorageBackend},
    types::{CommitEventRow, CommitProposal, EventProposal, ItemCursor, ItemId, RepoId},
};
use synck_vcs_sqlite::SqliteStorage;

fn create(item: &ItemId, kind: &str, name: &str) -> EventProposal {
    serde_json::from_value(
        json!({"item_id":item,"event_type":format!("{kind}::CREATE"),
        "create_item":{"item_type":kind,"parent_folder_id":null,"name":name}}),
    )
    .unwrap()
}

fn organization_event(repo: &RepoId, state: &ChatOrganization, base: Option<i64>) -> EventProposal {
    let item = chat_organization::item_id(repo, &state.chat_item_id).unwrap();
    let mut event = if base.is_none() {
        create(
            &item,
            "CHAT_ORGANIZATION",
            &chat_organization::internal_name(&state.chat_item_id),
        )
    } else {
        serde_json::from_value(json!({"item_id":item,
            "event_type":"CHAT_ORGANIZATION::SET_METADATA","base_seq":base}))
        .unwrap()
    };
    event.set_metadata = Some(serde_json::to_value(state).unwrap());
    chat_organization::validate_event(&event).unwrap();
    event
}

async fn initialize(root: &Path) -> (SqliteStorage, Repo, RepoId) {
    let metadata = root.join(".synck");
    fs::create_dir_all(metadata.join("objects")).unwrap();
    let repo = RepoId::new();
    fs::write(
        metadata.join("config.json"),
        serde_json::to_vec(&json!({"format_version":6,"repo_id":repo,"authority":"FOLLOWER"}))
            .unwrap(),
    )
    .unwrap();
    fs::write(metadata.join("lock"), []).unwrap();
    let storage = SqliteStorage::new(metadata.join("repository.sqlite3"));
    storage.init_schema().await.unwrap();
    let core = Repo::new(
        Storage::new(Arc::new(storage.clone())),
        CoreConfig::default(),
    );
    (storage, core, repo)
}

async fn append(storage: &SqliteStorage, core: &Repo, repo: &RepoId, events: Vec<EventProposal>) {
    core.append(
        repo.clone(),
        CommitProposal {
            author_id: "alice".into(),
            committer_id: "alice".into(),
            message: None,
            occurred_at: None,
            operation_id: Some(ItemId::new().to_string()),
            events,
        },
    )
    .await
    .unwrap();
    core.sequence_repo_activity(repo.clone(), 100)
        .await
        .unwrap();
    storage.project_imported_history(repo).await.unwrap();
}

// A fixture producer for the public materializer. These tests have no objects or
// attached refs; hosted export uses its production PostgreSQL producer.
async fn export_fixture(storage: &SqliteStorage, repo: &RepoId, transfer: &Path) -> BundleSnapshot {
    fs::create_dir_all(transfer.join("objects")).unwrap();
    let mut records = vec![
        json!({"type":"synck_vcs_bundle_header","bundle_format_version":4,
        "repo_id":repo,"exported_at":"2026-10-26T12:00:00Z",
        "schema":{"subsystem":"synck_vcs_sqlite","schema_id":"core-v5"},
        "commit_format_version":5}),
    ];
    let commits = storage
        .list_commits_by_repo_seq_readonly(repo, 1, None, 100)
        .await
        .unwrap();
    let terminal = commits.last().unwrap().repo_seq.unwrap();
    for commit in commits {
        let mut record = serde_json::to_value(&commit).unwrap();
        let object = record.as_object_mut().unwrap();
        object.remove("repo_id");
        object.insert("type".into(), json!("commit"));
        let operation_id: Option<String> = storage.open_readonly().unwrap().query_row(
            "SELECT operation_id FROM vcs_commits WHERE repo_id=?1 AND lower(hex(commit_id))=?2",
            [repo.as_str(),&commit.commit_id.as_str()[7..]],|row|row.get(0)).unwrap();
        object.insert("operation_id".into(), json!(operation_id));
        object.entry("import_job_id").or_insert(Value::Null);
        records.push(record);
        for event in storage
            .list_commit_events_readonly(repo, &commit.commit_id, commit.event_count as usize + 1)
            .await
            .unwrap()
        {
            let CommitEventRow::Item(event) = event else {
                panic!("fixture has no refs")
            };
            let mut record = serde_json::to_value(&event).unwrap();
            let object = record.as_object_mut().unwrap();
            object.remove("repo_id");
            object.remove("payload");
            object.insert("type".into(), json!("event"));
            object.insert(
                "payload_json".into(),
                json!(String::from_utf8(event.payload).unwrap()),
            );
            records.push(record);
        }
    }
    let mut cursor = None;
    loop {
        let items = storage
            .list_items_readonly(repo, cursor.as_ref(), None, 512)
            .await
            .unwrap();
        if items.is_empty() {
            break;
        }
        for item in &items {
            let mut record = serde_json::to_value(item).unwrap();
            record.as_object_mut().unwrap().remove("repo_id");
            record["type"] = json!("item");
            records.push(record);
        }
        let item = items.last().unwrap();
        cursor = Some(ItemCursor {
            created_at: item.created_at,
            item_id: item.item_id.clone(),
        });
    }
    let bytes = records
        .into_iter()
        .map(|record| serde_json::to_string(&record).unwrap() + "\n")
        .collect::<String>()
        .into_bytes();
    fs::write(transfer.join("history.ndjson"), &bytes).unwrap();
    BundleSnapshot {
        repo_id: repo.clone(),
        terminal_repo_seq: terminal,
        history_sha256: format!("sha256:{}", hex::encode(Sha256::digest(&bytes))),
        history_bytes: bytes.len() as u64,
    }
}

async fn roundtrip(storage: &SqliteStorage, repo: &RepoId, root: &Path) -> PortableRepository {
    let transfer = root.join("transfer");
    let snapshot = export_fixture(storage, repo, &transfer).await;
    let materialized = root.join("materialized");
    materialize_repository(
        &transfer.join("history.ndjson"),
        &transfer.join("objects"),
        &snapshot,
        &materialized,
    )
    .await
    .unwrap();
    let archive = root.join("archive");
    PortableRepository::open(&materialized)
        .unwrap()
        .archive(&archive)
        .await
        .unwrap();
    let imported = root.join("imported");
    import_repository(&archive, &imported).await.unwrap();
    fs::remove_dir_all(root.join("source")).unwrap();
    fs::remove_dir_all(transfer).unwrap();
    fs::remove_dir_all(materialized).unwrap();
    fs::remove_dir_all(archive).unwrap();
    PortableRepository::open(&imported).unwrap()
}

async fn preserve_fixture(repository: &PortableRepository, label: &str, ids: Value) {
    let Some(destination) = std::env::var_os("SYNCK_PORTABLE_WORKSPACE_FIXTURES") else {
        return;
    };
    let destination = std::path::PathBuf::from(destination);
    fs::create_dir_all(&destination).unwrap();
    repository.archive(&destination.join(label)).await.unwrap();
    fs::write(
        destination.join(format!("{label}.json")),
        serde_json::to_vec_pretty(&ids).unwrap(),
    )
    .unwrap();
    println!("{}", json!({"fixture":destination.join(label),"ids":ids}));
}

#[tokio::test]
async fn chat_organization_history_survives_export_and_copy_without_changing_chat_heads() {
    let root = tempfile::tempdir().unwrap();
    let (storage, core, repo) = initialize(&root.path().join("source")).await;
    let a = ItemId::new();
    let b = ItemId::new();
    let chats = [ItemId::new(), ItemId::new(), ItemId::new()];
    let mut events = vec![
        create(&a, "FOLDER", "a-workspace"),
        create(&b, "FOLDER", "b-workspace"),
    ];
    events.extend(
        chats
            .iter()
            .enumerate()
            .map(|(i, id)| create(id, "CHAT", &format!("z-chat-{i}"))),
    );
    // More than one historical child page of hidden internal owners.
    let browse_chats = (0..257).map(|_| ItemId::new()).collect::<Vec<_>>();
    events.extend(
        browse_chats
            .iter()
            .map(|id| create(id, "CHAT", &format!("z-{id}"))),
    );
    append(&storage, &core, &repo, events).await;
    append(
        &storage,
        &core,
        &repo,
        vec![
            serde_json::from_value(json!({"item_id":chats[1],"event_type":"CHAT::FEDERATE",
            "base_seq":1,"set_is_federated":true,"set_federation_stream_id":"shared-chat-stream"}))
            .unwrap(),
            serde_json::from_value(json!({"item_id":chats[2],"event_type":"CHAT::TOMBSTONE",
            "base_seq":1,"set_tombstoned":true}))
            .unwrap(),
        ],
    )
    .await;
    let states = chats
        .iter()
        .chain(&browse_chats)
        .map(|chat| ChatOrganization {
            chat_item_id: chat.clone(),
            workspace_item_id: Some(a.clone()),
            item_acl_mode: ChatAclMode::ItemOnly,
            revision: 7,
        })
        .collect::<Vec<_>>();
    append(
        &storage,
        &core,
        &repo,
        states
            .iter()
            .map(|state| organization_event(&repo, state, None))
            .collect(),
    )
    .await;
    let mut changed = states[0].clone();
    changed.workspace_item_id = Some(b.clone());
    changed.item_acl_mode = ChatAclMode::Combine;
    changed.revision = 8;
    append(
        &storage,
        &core,
        &repo,
        vec![organization_event(&repo, &changed, Some(1))],
    )
    .await;
    let mut cleared = changed.clone();
    cleared.workspace_item_id = None;
    cleared.item_acl_mode = ChatAclMode::ItemOnly;
    cleared.revision = 9;
    append(
        &storage,
        &core,
        &repo,
        vec![organization_event(&repo, &cleared, Some(2))],
    )
    .await;
    let repository = roundtrip(&storage, &repo, root.path()).await;
    repository.verify().await.unwrap();
    for (through, expected) in [
        (1, None),
        (2, None),
        (3, Some(states[0].clone())),
        (4, Some(changed)),
        (5, Some(cleared.clone())),
    ] {
        let item = repository
            .show_item(&chats[0], Some(through))
            .await
            .unwrap()
            .item;
        assert_eq!(item.chat.unwrap().organization, expected);
        assert_eq!(item.projection.latest_item_seq, 1);
        assert_eq!(item.latest_ref_seq, 0);
    }
    assert_eq!(
        repository
            .show_item(&chats[0], None)
            .await
            .unwrap()
            .item
            .chat
            .unwrap()
            .organization,
        Some(cleared)
    );
    for (i, chat) in chats.iter().enumerate().skip(1) {
        let item = repository.show_item(chat, None).await.unwrap().item;
        assert_eq!(item.chat.unwrap().organization, Some(states[i].clone()));
        assert_eq!(item.projection.latest_item_seq, 2);
        assert_eq!(item.latest_ref_seq, 0);
    }
    let owner = chat_organization::item_id(&repo, &chats[0]).unwrap();
    assert!(matches!(
        repository
            .show_item(&owner, Some(3))
            .await
            .unwrap()
            .item
            .native,
        NativeState::ChatOrganization(_)
    ));
    // Hidden owners at the start of the namespace must not produce empty pages
    // or hide visible entries across the fixed-size historical child pages.
    for historical in [false, true] {
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let page = if historical {
                repository
                    .list_historical_items(5, None, cursor.as_ref(), 1)
                    .await
                    .unwrap()
            } else {
                repository
                    .list_items(None, cursor.as_ref(), 1)
                    .await
                    .unwrap()
            };
            assert!(
                page.items
                    .iter()
                    .all(|item| item.item_type != "CHAT_ORGANIZATION")
            );
            ids.extend(page.items.into_iter().map(|item| item.item_id));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(ids.len(), 2 + 2 + browse_chats.len()); // Tombstoned CHAT is not live.
        assert_eq!(
            ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
            ids.len()
        );
    }
    preserve_fixture(
        &repository,
        "chat",
        json!({"repo_id":repo,"chat_item_id":chats[0],
        "federated_chat_item_id":chats[1],"tombstoned_chat_item_id":chats[2],
        "organization_owner_item_id":owner,"workspace_a":a,"workspace_b":b,
        "before_baseline":2,"baseline":3,"relinked":4,"cleared":5}),
    )
    .await;
}

#[tokio::test]
async fn calendar_workspace_link_is_canonical_through_export_copy_and_historical_reads() {
    let root = tempfile::tempdir().unwrap();
    let (storage, core, repo) = initialize(&root.path().join("source")).await;
    let a = ItemId::new();
    let b = ItemId::new();
    let calendar = ItemId::new();
    let event = ItemId::new();
    let mut first = create(
        &calendar,
        "CALENDAR",
        &synck_items::native_calendars::history::internal_name("CALENDAR", &calendar),
    );
    first.set_metadata =
        Some(json!({"title":"Calendar","description":null,"color":"#336699","archived":false}));
    let mut data = CalendarEvent::new(
        calendar.clone(),
        "Meeting".into(),
        EventTime::Timed {
            start: "2026-10-26T12:00:00Z".parse().unwrap(),
            end: "2026-10-26T13:00:00Z".parse().unwrap(),
            time_zone: "UTC".into(),
        },
        "alice".into(),
    )
    .unwrap();
    data.case_item_id = Some(a.clone());
    let mut creation = create(
        &event,
        "CALENDAR_EVENT",
        &synck_items::native_calendars::history::internal_name("CALENDAR_EVENT", &event),
    );
    creation.set_metadata = Some(serde_json::to_value(&data).unwrap());
    append(
        &storage,
        &core,
        &repo,
        vec![
            create(&a, "FOLDER", "a-workspace"),
            create(&b, "FOLDER", "b-workspace"),
            first,
            creation,
        ],
    )
    .await;
    for (base, case) in [(1, Some(b.clone())), (2, None)] {
        data.case_item_id = case;
        append(
            &storage,
            &core,
            &repo,
            vec![
                serde_json::from_value(json!({"item_id":event,
            "event_type":"CALENDAR_EVENT::SET_METADATA","base_seq":base,"set_metadata":data}))
                .unwrap(),
            ],
        )
        .await;
    }
    let repository = roundtrip(&storage, &repo, root.path()).await;
    for (through, expected) in [
        (Some(1), Some(a.clone())),
        (Some(2), Some(b.clone())),
        (Some(3), None),
        (None, None),
    ] {
        let item = repository.show_item(&event, through).await.unwrap().item;
        let NativeState::CalendarEvent(data) = item.native else {
            panic!("expected native calendar event")
        };
        assert_eq!(data.case_item_id, expected);
        assert_eq!(data.calendar_item_id, calendar);
        assert_eq!(item.projection.item_id, event);
    }
    repository.verify().await.unwrap();
    preserve_fixture(
        &repository,
        "calendar",
        json!({"repo_id":repo,"event_item_id":event,
        "calendar_item_id":calendar,"workspace_a":a,"workspace_b":b,
        "assigned":1,"relinked":2,"cleared":3}),
    )
    .await;
}
