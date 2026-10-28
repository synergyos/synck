use std::io::Cursor;
use std::sync::Arc;

use rusqlite::params;
use serde_json::{Value, json};
use synck_vcs::bundle::{
    BUNDLE_FORMAT_VERSION, BundleCommitRecord, BundleEventRecord, BundleFederationDomainRecord,
    BundleHeaderRecord, BundleItemRecord, BundleRefEventRecord, BundleSchema, ImportFailure,
    SCHEMA_META_SUBSYSTEM_VCS,
};
use synck_vcs::config::CoreConfig;
use synck_vcs::repo::Repo;
use synck_vcs::state::StateIndexStore;
use synck_vcs::storage::{Storage, StorageBackend};
use synck_vcs::types::{
    COMMIT_FORMAT_VERSION, CommitEventRow, CommitProposal, CreateItem, EventContent, EventProposal,
    ItemId, RefEventProposal, RepoId,
};
use synck_vcs_sqlite::{SqliteStorage, format_timestamp};

fn proposal(events: Vec<EventProposal>, operation_id: &str) -> CommitProposal {
    CommitProposal {
        author_id: "original-author".into(),
        committer_id: "distinct-recorder".into(),
        message: Some(operation_id.into()),
        occurred_at: None,
        operation_id: Some(operation_id.into()),
        events,
    }
}

fn create(kind: &str) -> EventProposal {
    serde_json::from_value(json!({
        "item_id": ItemId::new(), "event_type": format!("{kind}::CREATE"),
        "create_item": CreateItem { item_type: kind.into(), parent_folder_id: None, name: kind.into() }
    })).unwrap()
}

async fn fixture() -> (tempfile::TempDir, SqliteStorage, RepoId, Vec<Value>) {
    let directory = tempfile::tempdir().unwrap();
    let source = SqliteStorage::new(directory.path().join("source.sqlite"));
    source.init_schema().await.unwrap();
    let repo_id = RepoId::new();
    let core = Repo::new(
        Storage::new(Arc::new(source.clone())),
        CoreConfig::default(),
    );
    core.append(
        repo_id.clone(),
        proposal(
            [
                "FOLDER",
                "FILE",
                "CHAT",
                "EMAIL",
                "TASK",
                "DATABASE",
                "TABLE",
                "CALENDAR",
                "CALENDAR_EVENT",
                "TIME_RECORD",
                "CALL",
                "POST",
            ]
            .into_iter()
            .map(create)
            .collect(),
            "create-mixed-items",
        ),
    )
    .await
    .unwrap();
    core.sequence_repo_activity(repo_id.clone(), 10)
        .await
        .unwrap();
    let items = source
        .list_items_readonly(&repo_id, None, None, 100)
        .await
        .unwrap();
    let chat = items.iter().find(|item| item.item_type == "CHAT").unwrap();
    let file = items.iter().find(|item| item.item_type == "FILE").unwrap();
    let change: EventProposal = serde_json::from_value(json!({
        "item_id":chat.item_id,"event_type":"CHAT::MESSAGE::CREATE","base_seq":1,
        "set_content":EventContent::Text{text:"legacy inline body".into()},
    }))
    .unwrap();
    core.append_with_refs(
        repo_id.clone(),
        proposal(vec![change], "mixed-item-and-ref"),
        vec![RefEventProposal {
            owner_item_id: file.item_id.clone(),
            event_type: "FILE_VERSION::CREATE".into(),
            set_metadata: Some(json!({"version_id":ItemId::new()})),
            set_content: None,
            base_ref_seq: 0,
            initial_ref_seq: None,
        }],
    )
    .await
    .unwrap();
    // Publication permits strictly increasing positions with gaps.
    let connection = source.open_writable().unwrap();
    connection
        .execute(
            "UPDATE vcs_repo_activity_heads SET next_repo_seq=7 WHERE repo_id=?1",
            [repo_id.as_str()],
        )
        .unwrap();
    core.sequence_repo_activity(repo_id.clone(), 10)
        .await
        .unwrap();
    source.verify_checkpoints(&repo_id).unwrap();
    connection.execute("INSERT INTO vcs_federation_domains VALUES (?1,'preserved-domain',37,'2026-01-01T00:00:00Z','2026-01-02T00:00:00Z')",[repo_id.as_str()]).unwrap();
    let records = export_records(&source, &repo_id).await;
    (directory, source, repo_id, records)
}

// Serialize the existing public wire DTOs from independent source storage.
async fn export_records(source: &SqliteStorage, repo_id: &RepoId) -> Vec<Value> {
    let mut records = vec![json!(BundleHeaderRecord {
        r#type: "synck_vcs_bundle_header".into(),
        bundle_format_version: BUNDLE_FORMAT_VERSION,
        repo_id: repo_id.to_string(),
        history_generation: source
            .open_readonly()
            .unwrap()
            .query_row(
                "SELECT generation FROM vcs_history_generations WHERE repo_id=?1",
                [repo_id.as_str()],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        exported_at: "2026-01-02T00:00:00Z".into(),
        schema: BundleSchema {
            subsystem: SCHEMA_META_SUBSYSTEM_VCS.into(),
            schema_id: "source-pg-physical-schema-provenance".into()
        },
        commit_format_version: COMMIT_FORMAT_VERSION,
    })];
    let mut previous = synck_vcs::checkpoint::GENESIS;
    for commit in source
        .list_commits_by_repo_seq_readonly(repo_id, 1, None, 100)
        .await
        .unwrap()
    {
        let sequence = commit.repo_seq.unwrap();
        let operation_id: Option<String> = source
            .open_readonly()
            .unwrap()
            .query_row(
                "SELECT operation_id FROM vcs_commits WHERE repo_id=?1 AND repo_seq=?2",
                params![repo_id.as_str(), sequence],
                |row| row.get(0),
            )
            .unwrap();
        previous = synck_vcs::checkpoint::digest(
            &previous,
            sequence as u64,
            &commit.commit_id,
            commit.created_at,
        )
        .unwrap();
        records.push(json!(BundleCommitRecord {
            r#type: "commit".into(),
            commit_id: commit.commit_id.to_string(),
            checkpoint: synck_vcs::types::sha256_digest_from_digest_bytes(&previous).unwrap(),
            repo_seq: sequence,
            operation_id,
            import_job_id: commit.import_job_id,
            format_version: commit.format_version,
            author_id: commit.author_id,
            committer_id: commit.committer_id,
            message: commit.message,
            occurred_at: commit.occurred_at.map(format_timestamp),
            created_at: format_timestamp(commit.created_at),
            event_count: commit.event_count,
            events_digest: commit.events_digest.to_string(),
        }));
        for event in source
            .list_commit_events_readonly(repo_id, &commit.commit_id, 100)
            .await
            .unwrap()
        {
            records.push(match event {
                CommitEventRow::Item(event) => json!(BundleEventRecord {
                    r#type: "event".into(),
                    commit_id: event.commit_id.to_string(),
                    repo_seq: sequence,
                    commit_event_idx: event.commit_event_idx,
                    item_id: event.item_id.to_string(),
                    item_seq: event.item_seq,
                    event_type: event.event_type,
                    payload_json: String::from_utf8(event.payload).unwrap(),
                    event_payload_digest: event.event_payload_digest.to_string(),
                    created_at: format_timestamp(event.created_at),
                    parent_folder_id: event.parent_folder_id.map(|id| id.to_string()),
                    parent_folder_id_before: event.parent_folder_id_before.map(|id| id.to_string()),
                }),
                CommitEventRow::Ref(event) => json!(BundleRefEventRecord {
                    r#type: "ref_event".into(),
                    commit_id: event.commit_id.to_string(),
                    repo_seq: sequence,
                    commit_event_idx: event.commit_event_idx,
                    owner_item_id: event.owner_item_id.to_string(),
                    ref_seq: event.ref_seq,
                    event_type: event.event_type,
                    payload_json: String::from_utf8(event.payload).unwrap(),
                    event_payload_digest: event.event_payload_digest.to_string(),
                    created_at: format_timestamp(event.created_at),
                }),
            });
        }
    }
    for item in source
        .list_items_readonly(repo_id, None, None, 100)
        .await
        .unwrap()
    {
        records.push(json!(BundleItemRecord {
            r#type: "item".into(),
            item_id: item.item_id.to_string(),
            item_type: item.item_type,
            parent_folder_id: item.parent_folder_id.map(|id| id.to_string()),
            name: item.name,
            name_key: item.name_key,
            head_seq: item.head_seq,
            ref_head_seq: item.ref_head_seq,
            lifecycle_state: serde_json::to_value(item.lifecycle_state)
                .unwrap()
                .as_str()
                .unwrap()
                .into(),
            sequencing_mode: serde_json::to_value(item.sequencing_mode)
                .unwrap()
                .as_str()
                .unwrap()
                .into(),
            created_at: format_timestamp(item.created_at),
            created_by: item.created_by,
            federation_stream_id: item.federation_stream_id,
        }));
    }
    records.push(json!(BundleFederationDomainRecord {
        r#type: "federation_domain".into(),
        domain_id: "preserved-domain".into(),
        last_domain_seq: 37,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-02T00:00:00Z".into(),
    }));
    records
}

fn input(records: &[Value]) -> Cursor<Vec<u8>> {
    Cursor::new(
        records
            .iter()
            .map(|record| serde_json::to_string(record).unwrap() + "\n")
            .collect::<String>()
            .into_bytes(),
    )
}

#[tokio::test]
async fn mixed_history_import_preserves_ids_ref_heads_gaps_and_next_append() {
    let (directory, source, repo_id, records) = fixture().await;
    let target = SqliteStorage::new(directory.path().join("target.sqlite"));
    let report = target.import_bundle(&mut input(&records)).await.unwrap();
    assert_eq!(
        (report.commits, report.items, report.terminal_repo_seq),
        (2, 12, 7)
    );
    assert_eq!(report.repo_id, repo_id);
    assert_eq!(report.source_schema.subsystem, SCHEMA_META_SUBSYSTEM_VCS);
    assert_eq!(
        report.source_schema.schema_id,
        "source-pg-physical-schema-provenance"
    );
    target.verify_checkpoints(&repo_id).unwrap();
    let original = export_records(&source, &repo_id).await;
    let imported = export_records(&target, &repo_id).await;
    assert_eq!(original, imported);
    let connection = target.open_readonly().unwrap();
    let next: i64 = connection
        .query_row(
            "SELECT next_repo_seq FROM vcs_repo_activity_heads WHERE repo_id=?1",
            [repo_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(next, 8);
    let ref_head: i64 = connection
        .query_row(
            "SELECT ref_head_seq FROM vcs_items WHERE repo_id=?1 AND item_type='FILE'",
            [repo_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ref_head, 1);
    let domain: (i64, String) = connection
        .query_row(
            "SELECT last_domain_seq,created_at FROM vcs_federation_domains WHERE repo_id=?1",
            [repo_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(domain, (37, "2026-01-01T00:00:00.000000000Z".into()));
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        6
    );
    let mut state = target.state_index_store(repo_id.clone()).unwrap();
    assert_eq!(
        state
            .projection_head()
            .await
            .unwrap()
            .unwrap()
            .applied_repo_seq,
        7
    );
    drop(state);
    let items = target
        .list_items_readonly(&repo_id, None, None, 100)
        .await
        .unwrap();
    let chat = items.iter().find(|item| item.item_type == "CHAT").unwrap();
    let file = items.iter().find(|item| item.item_type == "FILE").unwrap();
    let rename:EventProposal=serde_json::from_value(json!({"item_id":chat.item_id,"event_type":"CHAT::RENAME","base_seq":2,"set_name":"continued-chat"})).unwrap();
    let core = Repo::new(
        Storage::new(Arc::new(target.clone())),
        CoreConfig::default(),
    );
    core.append_with_refs(
        repo_id.clone(),
        proposal(vec![rename], "first-local-append"),
        vec![RefEventProposal {
            owner_item_id: file.item_id.clone(),
            event_type: "FILE_VERSION::PROMOTE".into(),
            set_metadata: None,
            set_content: None,
            base_ref_seq: 1,
            initial_ref_seq: None,
        }],
    )
    .await
    .unwrap();
    core.sequence_repo_activity(repo_id.clone(), 10)
        .await
        .unwrap();
    assert_eq!(
        target
            .list_commits_by_repo_seq_readonly(&repo_id, 8, None, 10)
            .await
            .unwrap()[0]
            .repo_seq,
        Some(8)
    );
    assert_eq!(
        target
            .get_item_readonly(&repo_id, &file.item_id)
            .await
            .unwrap()
            .unwrap()
            .ref_head_seq,
        2
    );
    target.project_imported_history(&repo_id).await.unwrap();
    let mut state = target.state_index_store(repo_id.clone()).unwrap();
    assert_eq!(
        state
            .projection_head()
            .await
            .unwrap()
            .unwrap()
            .applied_repo_seq,
        8
    );
}

#[tokio::test]
async fn invalid_or_incomplete_bundle_rolls_back_and_allows_clean_retry() {
    let (directory, _source, repo_id, records) = fixture().await;
    let mut cases = Vec::new();
    for field in ["checkpoint", "created_at", "committer_id", "occurred_at"] {
        let mut changed = records.clone();
        changed
            .iter_mut()
            .find(|record| record["type"] == "commit")
            .unwrap()[field] = match field {
            "checkpoint" => json!(format!("sha256:{}", "0".repeat(64))),
            "committer_id" => json!("tampered-recorder"),
            _ => json!("2026-10-28T00:00:00Z"),
        };
        cases.push(changed);
    }
    let mut corrupt = records.clone();
    corrupt
        .iter_mut()
        .find(|record| record["type"] == "ref_event")
        .unwrap()["event_payload_digest"] = json!(format!("sha256:{}", "0".repeat(64)));
    cases.push(corrupt);
    let mut missing = records.clone();
    let last_event = missing
        .iter()
        .position(|record| record["type"] == "ref_event")
        .unwrap();
    missing.remove(last_event);
    cases.push(missing);
    cases.push(records[..last_event].to_vec());
    let mut no_items = records.clone();
    no_items.retain(|record| record["type"] != "item");
    cases.push(no_items);
    let mut bad_ref_head = records.clone();
    bad_ref_head
        .iter_mut()
        .find(|record| record["type"] == "item" && record["item_type"] == "FILE")
        .unwrap()["ref_head_seq"] = json!(0);
    cases.push(bad_ref_head);
    for (index, invalid) in cases.into_iter().enumerate() {
        let target = SqliteStorage::new(directory.path().join(format!("invalid-{index}.sqlite")));
        assert!(matches!(
            target.import_bundle(&mut input(&invalid)).await,
            Err(ImportFailure::BeforeCommit(_))
        ));
        let connection = target.open_readonly().unwrap();
        for table in [
            "vcs_commits",
            "vcs_checkpoints",
            "vcs_history_generations",
            "vcs_events",
            "vcs_ref_events",
            "vcs_items",
            "vcs_federation_domains",
            "vcs_repo_activity_heads",
            "vcs_state_projection_heads",
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "rollback left {table} rows in case {index}");
        }
        drop(connection);
        target.import_bundle(&mut input(&records)).await.unwrap();
        assert!(
            matches!(target.import_bundle(&mut input(&records)).await,Err(ImportFailure::BeforeCommit(message)) if message.contains("not empty"))
        );
        assert_eq!(
            target
                .list_commits_by_repo_seq_readonly(&repo_id, 1, None, 100)
                .await
                .unwrap()
                .len(),
            2
        );
    }
}
