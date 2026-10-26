use super::*;
#[cfg(feature = "sqlite")]
use synck_vcs::storage::StorageBackend;
#[cfg(feature = "sqlite")]
use synck_vcs::types::{
    COMMIT_FORMAT_VERSION, CreateItem, EventProposal, canonical_commit_bytes_for_hash,
};

fn repo_id() -> RepoId {
    RepoId::parse("01890c6c-1111-7777-8888-aaaaaaaaaaaa").unwrap()
}

fn checkpoint(sequence: i64, commit_id: Option<CommitId>) -> ReplicaCheckpoint {
    ReplicaCheckpoint {
        protocol_version: PROTOCOL_VERSION,
        core_replay_version: CORE_REPLAY_VERSION,
        state_format_version: STATE_FORMAT_VERSION,
        repo_id: repo_id(),
        terminal_repo_seq: sequence,
        terminal_commit_id: commit_id,
        state_root: empty_state_root(&repo_id()),
    }
}

#[test]
fn empty_checkpoint_and_page_are_exact() {
    let checkpoint = checkpoint(0, None);
    checkpoint.validate().unwrap();
    let page = ReplicaCommitPage {
        protocol_version: PROTOCOL_VERSION,
        repo_id: repo_id(),
        checkpoint_etag: checkpoint.etag().unwrap(),
        groups: Vec::new(),
        last_scanned_repo_seq: 0,
        done: true,
    };
    validate_page(&page, &checkpoint, 0, 0).unwrap();
    let encoded = serde_json::to_vec(&page).unwrap();
    assert_eq!(
        decode_and_validate_page(&encoded, &checkpoint, 0, 0).unwrap(),
        page
    );
}

#[test]
fn checkpoint_rejects_inconsistent_empty_identity() {
    let invalid = checkpoint(
        0,
        Some(CommitId::parse(format!("sha256:{}", "00".repeat(32))).unwrap()),
    );
    assert!(invalid.validate().is_err());
}

#[test]
fn page_rejects_false_done_and_binding() {
    let mut checkpoint = checkpoint(0, None);
    checkpoint.terminal_repo_seq = 4;
    checkpoint.terminal_commit_id =
        Some(CommitId::parse(format!("sha256:{}", "11".repeat(32))).unwrap());
    let mut page = ReplicaCommitPage {
        protocol_version: PROTOCOL_VERSION,
        repo_id: repo_id(),
        checkpoint_etag: checkpoint.etag().unwrap(),
        groups: Vec::new(),
        last_scanned_repo_seq: 1,
        done: false,
    };
    assert!(validate_page(&page, &checkpoint, 1, 4).is_err());
    page.last_scanned_repo_seq = 4;
    page.done = true;
    page.checkpoint_etag.push('x');
    assert!(validate_page(&page, &checkpoint, 4, 4).is_err());
}

#[test]
fn decoder_rejects_unknown_fields_and_oversized_body() {
    let checkpoint = checkpoint(0, None);
    let unknown = format!(
        "{{\"protocol_version\":1,\"repo_id\":\"{}\",\"checkpoint_etag\":{},\"groups\":[],\"last_scanned_repo_seq\":0,\"done\":true,\"extra\":1}}",
        repo_id(),
        serde_json::to_string(&checkpoint.etag().unwrap()).unwrap(),
    );
    assert!(decode_and_validate_page(unknown.as_bytes(), &checkpoint, 0, 0).is_err());
    assert!(
        decode_and_validate_page(
            &vec![b' '; MAX_PAGE_UNCOMPRESSED_BYTES + 1],
            &checkpoint,
            0,
            0,
        )
        .is_err()
    );
}

#[cfg(feature = "sqlite")]
fn one_create_group(sequence: i64) -> ReplicaCommitGroup {
    let item_id = ItemId::new();
    let proposal = EventProposal {
        item_id: Some(item_id.clone()),
        event_type: "FILE::CREATE".to_string(),
        set_metadata: None,
        set_content: None,
        create_item: Some(CreateItem {
            item_type: "FILE".to_string(),
            parent_folder_id: None,
            name: format!("a-{sequence}.txt"),
        }),
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: None,
    };
    let payload = canonical_event_proposal_bytes(&proposal).unwrap();
    let payload_digest = compute_sha256_digest_bytes(&payload);
    let events_digest_bytes: [u8; 32] = Sha256::digest(payload_digest).into();
    let events_digest =
        Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(events_digest_bytes)).unwrap();
    let commit_id = compute_commit_id(
        &canonical_commit_bytes_for_hash(
            &"actor".to_string(),
            Some("create"),
            None,
            1,
            &events_digest,
        )
        .unwrap(),
    );
    let created_at = Utc::now();
    ReplicaCommitGroup {
        commit: ReplicaCommit {
            commit_pk: CommitPk::new(),
            commit_id,
            parent_commit_id: None,
            repo_seq: sequence,
            operation_id: Some(format!("op-{sequence}")),
            format_version: COMMIT_FORMAT_VERSION,
            import_job_id: None,
            author_id: "actor".to_string(),
            committer_id: "assistant:test".to_string(),
            message: Some("create".to_string()),
            occurred_at: None,
            created_at,
            event_count: 1,
            events_digest,
        },
        events: vec![ReplicaEvent::Item {
            commit_event_idx: 0,
            item_id,
            item_seq: 1,
            event_type: "FILE::CREATE".to_string(),
            payload_base64: base64::engine::general_purpose::STANDARD.encode(payload),
            event_payload_digest: Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
                payload_digest,
            ))
            .unwrap(),
            created_at,
            parent_folder_id: None,
            parent_folder_id_before: None,
        }],
    }
}

#[cfg(feature = "sqlite")]
#[test]
fn ref_commit_format_requires_a_ref_event() {
    let mut group = one_create_group(1);
    group.commit.format_version = REF_COMMIT_FORMAT_VERSION;
    let error = validate_group(&group).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ref commit contains no ref event")
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn replica_group_rejects_item_event_after_ref_event() {
    let mut group = one_create_group(1);
    let item_id = match &group.events[0] {
        ReplicaEvent::Item { item_id, .. } => item_id.clone(),
        ReplicaEvent::Ref { .. } => unreachable!(),
    };
    let ref_proposal = RefEventProposal {
        owner_item_id: item_id.clone(),
        event_type: "FILE_VERSION::CREATE".to_string(),
        set_metadata: Some(serde_json::json!({"file_item_id": item_id.as_str()})),
        set_content: None,
        base_ref_seq: 0,
        initial_ref_seq: None,
    };
    let ref_payload = canonical_ref_event_proposal_bytes(&ref_proposal).unwrap();
    let ref_digest = compute_sha256_digest_bytes(&ref_payload);
    let mut item_event = group.events.remove(0);
    let item_digest = match &mut item_event {
        ReplicaEvent::Item {
            commit_event_idx,
            event_payload_digest,
            ..
        } => {
            *commit_event_idx = 1;
            event_payload_digest.clone()
        }
        ReplicaEvent::Ref { .. } => unreachable!(),
    };
    group.events = vec![
        ReplicaEvent::Ref {
            commit_event_idx: 0,
            owner_item_id: item_id,
            ref_seq: 1,
            event_type: ref_proposal.event_type,
            payload_base64: base64::engine::general_purpose::STANDARD.encode(ref_payload),
            event_payload_digest: Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
                ref_digest,
            ))
            .unwrap(),
            created_at: group.commit.created_at,
        },
        item_event,
    ];
    let mut hasher = Sha256::new();
    hasher.update([COMMIT_EVENT_FAMILY_REF]);
    hasher.update(ref_digest);
    hasher.update([COMMIT_EVENT_FAMILY_ITEM]);
    hasher.update(synck_vcs::types::sha256_digest_to_digest_bytes(
        &item_digest,
    ));
    let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
        hasher.finalize().into(),
    ))
    .unwrap();
    group.commit.format_version = REF_COMMIT_FORMAT_VERSION;
    group.commit.event_count = 2;
    group.commit.events_digest = events_digest.clone();
    group.commit.commit_id = compute_commit_id(
        &canonical_stored_commit_bytes_for_hash(
            REF_COMMIT_FORMAT_VERSION,
            &group.commit.author_id,
            group.commit.message.as_deref(),
            None,
            None,
            2,
            &events_digest,
        )
        .unwrap(),
    );

    let error = validate_group(&group).unwrap_err();
    assert!(error.to_string().contains("item event follows a ref event"));
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_install_is_atomic_idempotent_and_projects_history() {
    use synck_vcs::state::StateIndexStore;

    let directory = tempfile::tempdir().unwrap();
    let repo_id = repo_id();
    let replica = crate::sqlite::SqliteReplica::initialize(
        directory.path().join("replica.sqlite3"),
        repo_id.clone(),
    )
    .await
    .unwrap();
    let group = one_create_group(1);
    validate_group(&group).unwrap();
    assert!(replica.install_group(&group).await.unwrap());
    assert!(!replica.install_group(&group).await.unwrap());
    replica.project_through(1).await.unwrap();
    let mut state = replica
        .storage()
        .state_index_store(repo_id.clone())
        .unwrap();
    let head = state.projection_head().await.unwrap().unwrap();
    let checkpoint = ReplicaCheckpoint {
        protocol_version: PROTOCOL_VERSION,
        core_replay_version: CORE_REPLAY_VERSION,
        state_format_version: STATE_FORMAT_VERSION,
        repo_id: repo_id.clone(),
        terminal_repo_seq: 1,
        terminal_commit_id: Some(group.commit.commit_id.clone()),
        state_root: Sha256Digest::parse(head.state_root.to_string()).unwrap(),
    };
    replica.verify_history().await.unwrap();
    replica.verify_checkpoint(&checkpoint).await.unwrap();

    let draft_pk = CommitPk::new();
    let mut transaction = replica.storage().begin_txn().await.unwrap();
    transaction
        .start_commit(
            &repo_id,
            &draft_pk,
            Some(2),
            None,
            None,
            COMMIT_FORMAT_VERSION,
            None,
            "actor",
            "actor",
            None,
            None,
            Utc::now(),
        )
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    assert!(matches!(
        replica.verify_history().await,
        Err(ReplicaError::Corrupt(_))
    ));
    replica
        .storage()
        .open_writable()
        .unwrap()
        .execute(
            "DELETE FROM vcs_commits WHERE repo_id=?1 AND commit_pk=?2",
            [repo_id.as_str(), draft_pk.as_str()],
        )
        .unwrap();

    let mut wrong_terminal = checkpoint.clone();
    wrong_terminal.terminal_commit_id =
        Some(CommitId::parse(format!("sha256:{}", "ab".repeat(32))).unwrap());
    assert!(matches!(
        replica.verify_checkpoint(&wrong_terminal).await,
        Err(ReplicaError::Malformed(_))
    ));

    replica
        .storage()
        .open_writable()
        .unwrap()
        .execute(
            "UPDATE vcs_commits SET message='corrupt' WHERE repo_id=?1",
            [repo_id.as_str()],
        )
        .unwrap();
    assert!(matches!(
        replica.verify_history().await,
        Err(ReplicaError::Corrupt(_))
    ));
    assert!(matches!(
        replica.install_group(&group).await,
        Err(ReplicaError::Corrupt(_))
    ));
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_checkpoint_rejects_later_sealed_history() {
    use synck_vcs::state::StateIndexStore;

    let directory = tempfile::tempdir().unwrap();
    let repo_id = repo_id();
    let replica = crate::sqlite::SqliteReplica::initialize(
        directory.path().join("replica.sqlite3"),
        repo_id.clone(),
    )
    .await
    .unwrap();
    let terminal = one_create_group(1);
    assert!(replica.install_group(&terminal).await.unwrap());
    replica.project_through(1).await.unwrap();
    let mut state = replica
        .storage()
        .state_index_store(repo_id.clone())
        .unwrap();
    let head = state.projection_head().await.unwrap().unwrap();
    let checkpoint = ReplicaCheckpoint {
        protocol_version: PROTOCOL_VERSION,
        core_replay_version: CORE_REPLAY_VERSION,
        state_format_version: STATE_FORMAT_VERSION,
        repo_id,
        terminal_repo_seq: 1,
        terminal_commit_id: Some(terminal.commit.commit_id.clone()),
        state_root: Sha256Digest::parse(head.state_root.to_string()).unwrap(),
    };
    assert!(replica.install_group(&one_create_group(2)).await.unwrap());
    replica.verify_history().await.unwrap();
    assert!(matches!(
        replica.verify_checkpoint(&checkpoint).await,
        Err(ReplicaError::Corrupt(_))
    ));
}
