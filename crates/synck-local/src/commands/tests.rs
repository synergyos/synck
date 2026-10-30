use std::{collections::BTreeMap, fs};

use cap_std::{ambient_authority, fs::Dir};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use synck_file_folder::{
    CurrentItem, ImmutableItemKind, IntentProposal, project_proposal_events,
    validate_and_materialize,
};
use synck_vcs::types::{
    COMMIT_FORMAT_VERSION, CommitId, CommitRow, EventProposal, EventRow, ItemId, RepoId,
    Sha256Digest, canonical_commit_bytes_for_hash, canonical_event_proposal_bytes,
    compute_commit_id, compute_sha256_digest_bytes, sha256_prefixed_hex_from_digest_bytes,
};

#[cfg(windows)]
use super::entry_matches_revert_state;
use super::{
    CommitOutcome,
    WORKTREE_RESTORE_INTENT_NAME, WORKTREE_RESTORE_INTENT_VERSION, WORKTREE_RESTORE_STAGE_NAME,
    WorktreeRestoreIntent, build_recover_intent, build_revert_intent,
    build_worktree_restore_items, materialize_events, proposal_item_kind_requirements,
    worktree_restore_manifest,
};
use crate::LocalError;
use crate::event_profile::classify_commit;
use crate::init;
use crate::repository::{Repository, windows_extended_length_path_units};

const CONFORMANCE_FIXTURE: &str =
    include_str!("../../../../docs/spec/vectors/file-folder/conformance.json");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConformanceFixture {
    repo_id: RepoId,
    cases: Vec<ConformanceCase>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConformanceCase {
    name: String,
    proposal: IntentProposal,
    current: Vec<CurrentItem>,
    expected_error: Option<String>,
}

// Each materialization salts its metadata, so compare decoded metadata
// values instead of metadata IDs.
fn canonical_events(events: &[EventProposal]) -> Vec<serde_json::Value> {
    events
        .iter()
        .map(|event| {
            let mut value: serde_json::Value =
                serde_json::from_slice(&canonical_event_proposal_bytes(event).unwrap())
                    .unwrap();
            if let Some(metadata) = event.set_metadata.as_ref().and_then(|m| m.value()) {
                let object = value.as_object_mut().unwrap();
                object.remove("metadata_id");
                object.insert("metadata".to_string(), metadata.clone());
            }
            value
        })
        .collect()
}

fn accepted_rows(
    repo_id: &RepoId,
    message: Option<&str>,
    events: &[EventProposal],
    current: &BTreeMap<ItemId, CurrentItem>,
) -> (CommitRow, Vec<EventRow>) {
    let created_at = "2026-08-20T12:00:00Z".parse().unwrap();
    let mut next_item_seq = BTreeMap::new();
    let mut rows = Vec::with_capacity(events.len());
    let mut aggregate_digest = Sha256::new();
    for (index, event) in events.iter().enumerate() {
        let item_id = event.item_id.clone().unwrap();
        let item_seq = next_item_seq.entry(item_id.clone()).or_insert_with(|| {
            event
                .create_item
                .as_ref()
                .map_or_else(|| current.get(&item_id).unwrap().head_seq + 1, |_| 1)
        });
        let payload = canonical_event_proposal_bytes(event).unwrap();
        let digest = compute_sha256_digest_bytes(&payload);
        aggregate_digest.update([synck_vcs::types::COMMIT_EVENT_FAMILY_ITEM]);
        aggregate_digest.update(digest);
        rows.push(EventRow {
            metadata: event.set_metadata.as_ref().and_then(|value| value.payload()).cloned(),
            repo_id: repo_id.clone(),
            item_id,
            item_seq: *item_seq,
            event_type: event.event_type.clone(),
            commit_id: CommitId::parse(format!("sha256:{}", "0".repeat(64))).unwrap(),
            commit_event_idx: index as i32,
            repo_seq: Some(1),
            payload,
            event_payload_digest: Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
                digest,
            ))
            .unwrap(),
            created_at,
            parent_folder_id: None,
            parent_folder_id_before: None,
        });
        *item_seq += 1;
    }
    let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
        aggregate_digest.finalize().into(),
    ))
    .unwrap();
    let commit_id = compute_commit_id(
        &canonical_commit_bytes_for_hash(
            &"test".to_string(),
            &"test".to_string(),
            None,
            message,
            events.len(),
            &events_digest,
        )
        .unwrap(),
    );
    for row in &mut rows {
        row.commit_id = commit_id.clone();
    }
    (
        CommitRow {
            repo_id: repo_id.clone(),
            commit_id,
            repo_seq: Some(1),
            format_version: COMMIT_FORMAT_VERSION,
            author_id: "test".to_string(),
            committer_id: "test".to_string(),
            message: message.map(str::to_string),
            occurred_at: None,
            created_at,
            event_count: events.len() as i32,
            events_digest,
        },
        rows,
    )
}

#[test]
fn local_adapter_matches_shared_conformance_corpus() {
    let fixture: ConformanceFixture = serde_json::from_str(CONFORMANCE_FIXTURE).unwrap();
    for case in fixture.cases {
        let current: BTreeMap<_, _> = case
            .current
            .into_iter()
            .map(|item| (item.item_id.clone(), item))
            .collect();
        let reference = validate_and_materialize(&fixture.repo_id, &case.proposal, &current);
        let local = materialize_events(
            &fixture.repo_id,
            case.proposal.message.as_deref(),
            case.proposal.intents.clone(),
            &current,
        );
        match (reference, local, case.expected_error) {
            (Ok(reference), Ok(local), None) => {
                let (target_ids, _) = proposal_item_kind_requirements(&local.events);
                let kinds = target_ids
                    .iter()
                    .map(|item_id| ImmutableItemKind {
                        item_id: item_id.clone(),
                        item_type: current[item_id].item_type.clone(),
                    })
                    .collect::<Vec<_>>();
                let projected = project_proposal_events(&local.events, &kinds).unwrap();
                assert_eq!(
                    canonical_events(&projected),
                    canonical_events(&reference.events),
                    "{}",
                    case.name
                );

                let (commit, rows) = accepted_rows(
                    &fixture.repo_id,
                    case.proposal.message.as_deref(),
                    &local.events,
                    &current,
                );
                let item_kinds = current
                    .iter()
                    .map(|(item_id, item)| (item_id.clone(), item.item_type.clone()))
                    .collect();
                let classified = classify_commit(&commit, &rows, &item_kinds).unwrap();
                let visible = classified
                    .into_iter()
                    .map(|event| event.proposal)
                    .collect::<Vec<_>>();
                assert_eq!(
                    canonical_events(&visible),
                    canonical_events(&reference.events),
                    "{}",
                    case.name
                );
            }
            (Err(reference), Err(LocalError::FileFolder(local)), Some(expected)) => {
                assert_eq!(reference.code, local.code, "{}", case.name);
                assert_eq!(local.code.as_str(), expected, "{}", case.name);
            }
            (reference, local, expected) => panic!(
                "{} diverged: reference={}, local={}, expected_error={expected:?}",
                case.name,
                reference
                    .as_ref()
                    .err()
                    .map_or("accepted", |error| error.code.as_str()),
                match &local {
                    Ok(_) => "accepted",
                    Err(LocalError::FileFolder(error)) => error.code.as_str(),
                    Err(_) => "non-semantic-error",
                }
            ),
        }
    }
}

#[test]
fn windows_absolute_paths_use_the_extended_length_prefix() {
    let input: Vec<u16> = r"C:\work\file".encode_utf16().collect();
    let expected: Vec<u16> = "\\\\?\\C:\\work\\file\0".encode_utf16().collect();
    assert_eq!(windows_extended_length_path_units(&input), expected);
}

#[cfg(windows)]
#[tokio::test]
async fn inverse_stage_rejects_a_windows_junction() {
    let temporary = tempfile::tempdir().unwrap();
    let target = temporary.path().join("target");
    let junction = temporary.path().join("junction");
    fs::create_dir(&target).unwrap();
    let output = std::process::Command::new("cmd")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mklink failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let state = RevertItemState {
        plain_digest: None,
        parent_id: None,
        name: "folder".to_string(),
        path: "folder".to_string(),
        live: true,
        head_seq: 1,
        blob_id: None,
        size: None,
    };
    let repo_root = tempfile::tempdir().unwrap();
    init(repo_root.path(), "author").await.unwrap();
    let repository = Repository::discover(repo_root.path()).await.unwrap();
    assert!(!entry_matches_revert_state(&repository, &junction, "FOLDER", &state).unwrap());
}

#[tokio::test]
async fn export_publication_never_replaces_an_intervening_destination() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"committed").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let (_, blob_id, size) = repository
        .resolve_file_revision(root, std::path::Path::new("document.bin"), None)
        .await
        .unwrap();
    let output = tempfile::tempdir().unwrap();
    let parent = Dir::open_ambient_dir(output.path(), ambient_authority()).unwrap();
    let destination = output.path().join("export.bin");
    let staged = repository
        .copy_object_to_temporary(&parent, output.path(), &destination, &blob_id, size)
        .unwrap();
    fs::write(&destination, b"intervening").unwrap();

    repository
        .publish_temporary_file(
            &parent,
            output.path(),
            &staged,
            std::ffi::OsStr::new("export.bin"),
            &destination,
            false,
        )
        .unwrap_err();

    assert_eq!(fs::read(&destination).unwrap(), b"intervening");
    assert!(!output.path().join(staged).exists());
}

#[tokio::test]
async fn discovery_materializes_an_accepted_recover_intent_once() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"recoverable").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    let item_id = repository
        .committed_tree()
        .await
        .unwrap()
        .get("document.bin")
        .unwrap()
        .item_id
        .clone();
    fs::remove_file(root.join("document.bin")).unwrap();
    repository.commit("delete document").await.unwrap();

    let replay = repository.replay_history(None).await.unwrap();
    let intent = build_recover_intent(&item_id, "recover document", &replay.nodes).unwrap();
    repository.validate_recover_objects(&intent).unwrap();
    repository.write_recover_intent(&intent).unwrap();
    repository.append_recover_intent(&intent).await.unwrap();
    repository.recover().await.unwrap();
    assert!(!root.join("document.bin").exists());
    assert!(root.join(".synck/recover.json").is_file());
    drop(repository);

    let recovered = Repository::discover(root).await.unwrap();
    assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"recoverable");
    assert!(!root.join(".synck/recover.json").exists());
    assert_eq!(
        recovered
            .log(None)
            .await
            .unwrap()
            .iter()
            .filter(|commit| commit.message.as_deref() == Some("recover document"))
            .count(),
        1
    );
    assert!(recovered.status().await.unwrap().is_empty());
}

#[tokio::test]
async fn discovery_completes_an_accepted_partially_staged_revert_once() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("document.bin"), b"second").unwrap();
    repository.commit("edit document").await.unwrap();

    let head = repository.log(Some(1)).await.unwrap().remove(0);
    let before = repository
        .replay_history(Some(head.repo_seq - 1))
        .await
        .unwrap();
    let after = repository
        .replay_history(Some(head.repo_seq))
        .await
        .unwrap();
    let intent = build_revert_intent(
        repository.config().repo_id(),
        (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
        (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
        "revert document",
        &before.nodes,
        &after.nodes,
        &after.nodes,
    )
    .unwrap();
    repository.validate_revert_objects(&intent).unwrap();
    repository.write_revert_intent(&intent).unwrap();
    repository.append_revert_intent(&intent).await.unwrap();
    repository.recover().await.unwrap();
    let stage = repository.ensure_revert_stage().unwrap();
    repository
        .stage_revert_item(&stage, &intent.items[0])
        .unwrap();
    assert!(!root.join("document.bin").exists());
    assert!(stage.join(intent.items[0].item_id.as_str()).is_file());
    drop(repository);

    let recovered = Repository::discover(root).await.unwrap();
    assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"first");
    assert!(!root.join(".synck/revert.json").exists());
    assert!(!root.join(".synck/revert-stage").exists());
    assert_eq!(
        recovered
            .log(None)
            .await
            .unwrap()
            .iter()
            .filter(|commit| commit.message.as_deref() == Some("revert document"))
            .count(),
        1
    );
    assert!(recovered.status().await.unwrap().is_empty());
    assert!(recovered.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn discovery_appends_a_persisted_unaccepted_revert_once() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("document.bin"), b"second").unwrap();
    repository.commit("edit document").await.unwrap();

    let head = repository.log(Some(1)).await.unwrap().remove(0);
    let before = repository
        .replay_history(Some(head.repo_seq - 1))
        .await
        .unwrap();
    let after = repository
        .replay_history(Some(head.repo_seq))
        .await
        .unwrap();
    let intent = build_revert_intent(
        repository.config().repo_id(),
        (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
        (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
        "revert document",
        &before.nodes,
        &after.nodes,
        &after.nodes,
    )
    .unwrap();
    repository.write_revert_intent(&intent).unwrap();
    drop(repository);

    let recovered = Repository::discover(root).await.unwrap();
    assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"first");
    assert_eq!(
        recovered
            .log(None)
            .await
            .unwrap()
            .iter()
            .filter(|commit| commit.message.as_deref() == Some("revert document"))
            .count(),
        1
    );
    assert!(recovered.status().await.unwrap().is_empty());
}

#[tokio::test]
async fn discovery_refuses_a_changed_preappend_revert_worktree() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"first").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("document.bin"), b"second").unwrap();
    repository.commit("edit document").await.unwrap();

    let head = repository.log(Some(1)).await.unwrap().remove(0);
    let before = repository
        .replay_history(Some(head.repo_seq - 1))
        .await
        .unwrap();
    let after = repository
        .replay_history(Some(head.repo_seq))
        .await
        .unwrap();
    let intent = build_revert_intent(
        repository.config().repo_id(),
        (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
        (&CommitId::parse(&head.commit_id).unwrap(), head.repo_seq),
        "revert document",
        &before.nodes,
        &after.nodes,
        &after.nodes,
    )
    .unwrap();
    repository.write_revert_intent(&intent).unwrap();
    drop(repository);
    fs::write(root.join("document.bin"), b"third").unwrap();

    let error = match Repository::discover(root).await {
        Ok(_) => panic!("changed pre-append revert unexpectedly recovered"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("working tree changed after the revert intent")
    );
}

#[tokio::test]
async fn discovery_clears_a_completed_worktree_restore_intent() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"before").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("document.bin"), b"after").unwrap();
    fs::write(root.join("added.bin"), b"added").unwrap();
    let (committed, working, _, working_ids) = repository.changes().await.unwrap();
    let head = repository.log(Some(1)).await.unwrap().remove(0);
    let intent = WorktreeRestoreIntent {
        version: WORKTREE_RESTORE_INTENT_VERSION,
        base_commit_id: Some(CommitId::parse(&head.commit_id).unwrap()),
        base_repo_seq: head.repo_seq,
        source_tree: worktree_restore_manifest(&working, &working_ids).unwrap(),
        items: build_worktree_restore_items(&committed, &working, &working_ids).unwrap(),
    };
    repository
        .write_json_intent(WORKTREE_RESTORE_INTENT_NAME, "worktree-restore", &intent)
        .unwrap();
    repository
        .materialize_inverse_items(&intent.items, WORKTREE_RESTORE_STAGE_NAME)
        .unwrap();
    repository
        .remove_inverse_stage(WORKTREE_RESTORE_STAGE_NAME)
        .unwrap();
    drop(repository);

    let recovered = Repository::discover(root).await.unwrap();
    assert_eq!(fs::read(root.join("document.bin")).unwrap(), b"before");
    assert!(!root.join("added.bin").exists());
    assert!(!root.join(".synck/worktree-restore.json").exists());
    assert!(recovered.status().await.unwrap().is_empty());
    assert!(recovered.fsck().await.unwrap().is_empty());
}

#[tokio::test]
async fn ordinary_commit_reconciles_once_and_publishes_only_affected_identities() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("changed.bin"), b"before").unwrap();
    fs::write(root.join("unrelated.bin"), b"unrelated").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    let historical_before = repository.state_index_catch_ups();
    let work_before = repository.commit_work_counters();
    fs::write(root.join("changed.bin"), b"after").unwrap();
    repository.commit("edit one file").await.unwrap();

    let work_after = repository.commit_work_counters();
    // The historical-state index is a disposable cache for historical
    // trees: commits and plain log never touch it.
    assert_eq!(repository.state_index_catch_ups() - historical_before, 0);
    assert_eq!(work_after.0 - work_before.0, 1);
    assert_eq!(work_after.1 - work_before.1, 1);
    assert_eq!(repository.log(Some(1)).await.unwrap().len(), 1);
    assert_eq!(repository.state_index_catch_ups() - historical_before, 0);
    assert!(repository.status().await.unwrap().is_empty());
}

#[tokio::test]
async fn commit_checkpoints_changed_chunked_objects_once() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    for index in 0..3 {
        fs::write(root.join(format!("document-{index}.bin")), b"before").unwrap();
    }
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();

    let before = repository.object_store().durability_checkpoint_count();
    for index in 0..3 {
        fs::write(root.join(format!("document-{index}.bin")), b"after").unwrap();
    }
    repository.commit("edit three files").await.unwrap();
    let after = repository.object_store().durability_checkpoint_count();

    assert_eq!(after - before, 1);
    assert!(repository.status().await.unwrap().is_empty());
}

#[tokio::test]
async fn accepted_commit_recovers_after_exact_identity_publication_fails() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"before").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    fs::write(root.join("document.bin"), b"after").unwrap();

    let database = root.join(".synck/repository.local.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_identity_update
             BEFORE UPDATE ON local_worktree_identities
             BEGIN
                 SELECT RAISE(ABORT, 'injected identity publication failure');
             END;",
        )
        .unwrap();
    drop(connection);

    let accepted = match repository.commit("accepted once").await.unwrap_err() {
        crate::LocalError::AcceptedNeedsRecovery { commit_id, .. } => commit_id,
        error => panic!("unexpected commit error: {error}"),
    };
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch("DROP TRIGGER reject_identity_update")
        .unwrap();
    drop(connection);
    drop(repository);

    let recovered = Repository::discover(root).await.unwrap();
    assert!(recovered.status().await.unwrap().is_empty());
    let commits = recovered.log(None).await.unwrap();
    assert_eq!(
        commits
            .iter()
            .filter(|commit| commit.message.as_deref() == Some("accepted once"))
            .count(),
        1
    );
    assert!(commits.iter().any(|commit| commit.commit_id == accepted));
}

async fn commit_captured_snapshot_then(repository: &Repository, mutate: impl FnOnce()) {
    let (committed, working, changes, working_ids) = repository.changes().await.unwrap();
    let objects = repository
        .capture_changed_files(&working, &changes, None)
        .unwrap();
    let built_intents =
        super::build_intents(&committed, &working, &changes, &working_ids, &objects, None)
            .unwrap();
    let mut intents = built_intents.intents;
    let current = repository
        .current_file_folder_context(&mut intents)
        .unwrap();
    let built = super::materialize_events(
        repository.config().repo_id(),
        Some("captured"),
        intents,
        &current,
    )
    .unwrap();
    repository
        .core()
        .append_file_folder_checked_with_refs(
            repository.config().repo_id().clone(),
            synck_vcs::types::CommitProposal {
                author_id: repository.config().author().to_string(),
                committer_id: repository.config().author().to_string(),
                message: Some("captured".to_string()),
                occurred_at: None,
                operation_id: Some(uuid::Uuid::new_v4().to_string()),
                events: built.events,
            },
            built.ref_events,
            &[],
            &built.subtree_guards,
        )
        .await
        .unwrap();
    repository.recover_current().await.unwrap();
    mutate();
    repository
        .publish_committed_identities(
            &committed,
            &working,
            &changes,
            &working_ids,
            &built_intents.created_ids,
        )
        .unwrap();
}

#[tokio::test]
async fn post_capture_filesystem_changes_remain_dirty() {
    for scenario in ["edit", "replace", "rename", "delete", "file-to-folder"] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        fs::write(root.join("document.bin"), b"before").unwrap();
        init(root, "author").await.unwrap();
        let repository = Repository::discover(root).await.unwrap();
        repository.commit("initial").await.unwrap();
        fs::write(root.join("document.bin"), b"captured").unwrap();

        commit_captured_snapshot_then(&repository, || match scenario {
            "edit" => fs::write(root.join("document.bin"), b"later").unwrap(),
            "replace" => {
                fs::write(root.join("replacement.bin"), b"replacement").unwrap();
                fs::remove_file(root.join("document.bin")).unwrap();
                fs::rename(root.join("replacement.bin"), root.join("document.bin")).unwrap();
            }
            "rename" => {
                fs::rename(root.join("document.bin"), root.join("renamed.bin")).unwrap()
            }
            "delete" => fs::remove_file(root.join("document.bin")).unwrap(),
            "file-to-folder" => {
                fs::remove_file(root.join("document.bin")).unwrap();
                fs::create_dir(root.join("document.bin")).unwrap();
            }
            _ => unreachable!(),
        })
        .await;

        let changes = repository.status().await.unwrap();
        if scenario == "file-to-folder" {
            assert_eq!(
                changes
                    .iter()
                    .filter(|change| change.path == "document.bin")
                    .map(|change| change.kind)
                    .collect::<Vec<_>>(),
                vec![crate::ChangeKind::Delete, crate::ChangeKind::Add],
                "{scenario} was hidden by exact identity publication: {changes:?}"
            );
        } else {
            let expected = match scenario {
                "edit" | "replace" => crate::ChangeKind::Modify,
                "rename" => crate::ChangeKind::Rename,
                "delete" => crate::ChangeKind::Delete,
                _ => unreachable!(),
            };
            assert!(
                changes.iter().any(|change| change.kind == expected),
                "{scenario} was hidden by exact identity publication: {changes:?}"
            );
        }
    }
}

#[tokio::test]
async fn detailed_log_uses_one_state_store_and_one_delta_per_commit() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"zero").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    for revision in 1..=2 {
        fs::write(root.join("document.bin"), revision.to_string()).unwrap();
        repository
            .commit(&format!("revision {revision}"))
            .await
            .unwrap();
    }

    let before = repository.historical_work_counts();
    let catch_ups_before = repository.state_index_catch_ups();
    assert_eq!(repository.detailed_log(Some(3)).await.unwrap().len(), 3);
    let after = repository.historical_work_counts();
    assert_eq!(after[0] - before[0], 1);
    assert_eq!(after[1] - before[1], 3);
    assert_eq!(repository.state_index_catch_ups() - catch_ups_before, 1);
}

async fn history_work_with_unrelated_folders(unrelated: usize) -> [usize; 6] {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"zero").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    for revision in 1..=2 {
        fs::write(root.join("document.bin"), revision.to_string()).unwrap();
        repository
            .commit(&format!("revision {revision}"))
            .await
            .unwrap();
    }
    for start in (0..unrelated).step_by(3_000) {
        for index in start..std::cmp::min(start + 3_000, unrelated) {
            fs::create_dir(root.join(format!("unrelated-{index:05}"))).unwrap();
        }
        repository.commit("unrelated folders").await.unwrap();
    }

    let before = repository.historical_work_counts();
    assert_eq!(
        repository
            .history(root, std::path::Path::new("document.bin"))
            .await
            .unwrap()
            .len(),
        3
    );
    let after = repository.historical_work_counts();
    std::array::from_fn(|index| after[index] - before[index])
}

#[tokio::test]
async fn item_history_work_ignores_ten_thousand_unrelated_items() {
    let empty = history_work_with_unrelated_folders(0).await;
    let large = history_work_with_unrelated_folders(10_000).await;
    assert_eq!(large, empty);
    assert_eq!(large[0], 1);
    assert_eq!(large[2], 1);
    assert_eq!(large[3], 3);
}

#[tokio::test]
async fn item_history_rejects_target_event_corruption_after_projection() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("document.bin"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    repository.commit("initial").await.unwrap();
    assert_eq!(
        repository
            .history(root, std::path::Path::new("document.bin"))
            .await
            .unwrap()
            .len(),
        1
    );
    let item_id = repository
        .committed_tree()
        .await
        .unwrap()
        .get("document.bin")
        .unwrap()
        .item_id
        .clone();
    let connection =
        rusqlite::Connection::open(root.join(".synck/repository.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE vcs_events SET payload = x'7b7d'
             WHERE repo_id = ?1 AND item_id = ?2 AND event_type = 'FILE::CREATE'",
            rusqlite::params![repository.config().repo_id().as_str(), item_id.as_str()],
        )
        .unwrap();
    drop(connection);

    let error = repository
        .history(root, std::path::Path::new("document.bin"))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("historical event payload digest disagrees"),
        "{error}"
    );
}

#[tokio::test]
async fn commit_history_carries_tree_types_and_event_display_subjects() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("Folder")).unwrap();
    fs::write(root.join("Folder/book.xlsx"), b"content").unwrap();
    init(root, "author").await.unwrap();
    let repository = Repository::discover(root).await.unwrap();
    let CommitOutcome::Committed(commit_id) =
        repository.commit("initial content").await.unwrap()
    else {
        panic!("initial content commit was unexpectedly clean");
    };

    let shown = repository.show_events(&commit_id).await.unwrap();
    assert!(
        shown
            .commit
            .changes
            .iter()
            .any(|change| { change.path == "Folder" && change.item_type == "FOLDER" })
    );
    assert!(
        shown
            .commit
            .changes
            .iter()
            .any(|change| { change.path == "Folder/book.xlsx" && change.item_type == "FILE" })
    );

    let file_events = shown
        .events
        .iter()
        .filter(|event| event.item_name == "book.xlsx")
        .collect::<Vec<_>>();
    assert_eq!(file_events.len(), 2);
    assert!(file_events.iter().all(|event| event.item_type == "FILE"));
    assert!(
        file_events
            .iter()
            .all(|event| { event.parent_folder_name.as_deref() == Some("Folder") })
    );
    assert!(
        file_events
            .iter()
            .any(|event| event.event_type == "FILE_VERSION::CREATE")
    );

    let first_page = repository
        .show_event_page(&commit_id, None, 2)
        .await
        .unwrap();
    assert_eq!(first_page.events.len(), 2);
    assert_eq!(first_page.total_event_count, shown.events.len());
    let cursor = first_page.next_after_commit_event_idx.unwrap();
    let second_page = repository
        .show_event_page(&commit_id, Some(cursor), 50)
        .await
        .unwrap();
    assert_eq!(second_page.next_after_commit_event_idx, None);
    let paged_event_indexes = first_page
        .events
        .iter()
        .chain(&second_page.events)
        .map(|event| event.commit_event_idx)
        .collect::<Vec<_>>();
    assert_eq!(
        paged_event_indexes,
        shown
            .events
            .iter()
            .map(|event| event.commit_event_idx)
            .collect::<Vec<_>>()
    );
}
