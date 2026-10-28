use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synck_file_folder::{
    BlobRef, CurrentItem, ImmutableItemKind, Intent, IntentProposal, ItemBase, Location,
    ProfileLifecycleState, SemanticErrorCode, compute_operation_id, project_commit,
    project_proposal_events, validate_and_materialize, validate_projected_proposal,
};
use synck_vcs::types::{
    ObjectId, COMMIT_FORMAT_VERSION, CommitId, CommitRow, EventContent, EventProposal, EventRow,
    ItemId, RepoId, Sha256Digest, canonical_commit_bytes_for_hash, canonical_event_proposal_bytes,
    compute_commit_id, compute_sha256_digest_bytes, sha256_prefixed_hex_from_digest_bytes,
};

const INTENTS_FIXTURE: &str = include_str!("fixtures/v1/intents.json");
const EVENTS_FIXTURE: &str = include_str!("fixtures/v1/events.json");
const GROUPS_FIXTURE: &str = include_str!("fixtures/v1/change_groups.json");
const CONFORMANCE_FIXTURE: &str = include_str!("fixtures/v1/conformance.json");

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

fn item(number: u64) -> ItemId {
    ItemId::parse(format!("0198e901-d208-7670-ae29-{number:012x}")).unwrap()
}

fn repo_id() -> RepoId {
    RepoId::parse("0198e8fa-664e-7e80-b134-94ea7ecec437").unwrap()
}

fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

fn blob(byte: char, size: u64) -> BlobRef {
    BlobRef {
        blob_id: ObjectId::parse(digest(byte)).unwrap(),
        size,
    }
}

fn current(
    item_id: ItemId,
    item_type: &str,
    parent_item_id: Option<ItemId>,
    name: &str,
    head_seq: i64,
    lifecycle_state: ProfileLifecycleState,
    trash_commit_id: Option<CommitId>,
) -> CurrentItem {
    CurrentItem {
        item_id,
        item_type: item_type.to_string(),
        parent_item_id,
        name: name.to_string(),
        name_key: synck_vcs::naming::compute_name_key(name).unwrap(),
        head_seq,
        lifecycle_state,
        trash_commit_id,
        content: None,
    }
}

fn event(item_id: ItemId, event_type: &str, base_seq: Option<i64>) -> EventProposal {
    EventProposal {
        item_id: Some(item_id),
        event_type: event_type.to_string(),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq,
    }
}

#[test]
fn shared_conformance_corpus_locks_profile_outcomes() {
    let fixture: ConformanceFixture = serde_json::from_str(CONFORMANCE_FIXTURE).unwrap();
    assert_eq!(synck_file_folder::WORKTREE_CONTROL_NAME_KEY, ".synck");
    for case in fixture.cases {
        let current = case
            .current
            .into_iter()
            .map(|item| (item.item_id.clone(), item))
            .collect();
        match (
            validate_and_materialize(&fixture.repo_id, &case.proposal, &current),
            case.expected_error,
        ) {
            (Ok(materialized), None) => {
                assert!(!materialized.events.is_empty(), "{}", case.name);
                if case.name.contains("retains") {
                    assert!(
                        materialized.events.iter().all(|event| {
                            !matches!(
                                event.item_id.as_ref().map(ItemId::as_str),
                                Some(
                                    "0198e901-d208-7670-ae29-000000000022"
                                        | "0198e901-d208-7670-ae29-000000000032"
                                )
                            )
                        }),
                        "{}",
                        case.name
                    );
                }
            }
            (Err(error), Some(expected)) => {
                assert_eq!(error.code.as_str(), expected, "{}", case.name)
            }
            (Ok(_), Some(expected)) => panic!("{} accepted; expected {expected}", case.name),
            (Err(error), None) => panic!("{} failed: {error}", case.name),
        }
    }
}

#[test]
fn intent_fixture_decodes_strictly_and_requires_nullable_parent_fields() {
    let intents: Vec<Intent> = serde_json::from_str(INTENTS_FIXTURE).unwrap();
    assert_eq!(intents.len(), 6);
    assert!(serde_json::from_str::<Intent>(
        r#"{"kind":"create_folder","item_id":"0198e901-d208-7670-ae29-6834da5adfc1","location":{"name":"Folder"}}"#
    )
    .is_err());
    assert!(serde_json::from_str::<Intent>(
        r#"{"kind":"create_folder","item_id":"0198e901-d208-7670-ae29-6834da5adfc1","location":{"parent_item_id":null,"name":"Folder"},"extra":true}"#
    )
    .is_err());
    assert!(
        serde_json::from_str::<Intent>(
            r#"{"kind":"unknown","item_id":"0198e901-d208-7670-ae29-6834da5adfc1"}"#
        )
        .is_err()
    );
}

#[test]
fn top_level_permutations_have_identical_operation_ids_and_event_fields() {
    let folder_id = item(1);
    let file_id = item(2);
    let folder = Intent::CreateFolder {
        item_id: folder_id.clone(),
        location: Location {
            parent_item_id: None,
            name: "Folder".to_string(),
        },
    };
    let file = Intent::CreateFile {
        item_id: file_id,
        location: Location {
            parent_item_id: Some(folder_id),
            name: "a.txt".to_string(),
        },
        content: blob('0', 1),
    };
    let first = IntentProposal {
        message: Some("create".to_string()),
        intents: vec![folder.clone(), file.clone()],
    };
    let second = IntentProposal {
        message: Some("create".to_string()),
        intents: vec![file, folder],
    };
    let first_output = validate_and_materialize(&repo_id(), &first, &BTreeMap::new()).unwrap();
    let second_output = validate_and_materialize(&repo_id(), &second, &BTreeMap::new()).unwrap();
    assert_eq!(first_output.operation_id, second_output.operation_id);
    // Salts identify each materialization; the logical operation stays stable.
    let fields = |events: &[EventProposal]| -> Value {
        Value::Array(events.iter().map(|event| {
            let mut value = serde_json::to_value(event).unwrap();
            if let Some(metadata) = &event.set_metadata {
                value.as_object_mut().unwrap().remove("metadata_id");
                value["set_metadata"] = metadata.value().unwrap().clone();
            }
            value
        }).collect())
    };
    assert_eq!(fields(&first_output.events), fields(&second_output.events));
    let expected: Value = serde_json::from_str(EVENTS_FIXTURE).unwrap();
    assert_eq!(fields(&first_output.events), expected);
    assert_eq!(first_output.blobs, vec![blob('0', 1)]);
}

#[test]
fn operation_identity_distinguishes_absent_and_empty_messages() {
    let intent = Intent::CreateFolder {
        item_id: item(1),
        location: Location {
            parent_item_id: None,
            name: "Folder".to_string(),
        },
    };
    let absent = compute_operation_id(&repo_id(), None, std::slice::from_ref(&intent)).unwrap();
    let empty = compute_operation_id(&repo_id(), Some(""), &[intent]).unwrap();
    assert_ne!(absent, empty);

    assert_eq!(
        validate_and_materialize(
            &repo_id(),
            &IntentProposal {
                message: None,
                intents: Vec::new(),
            },
            &BTreeMap::new(),
        )
        .unwrap_err()
        .code,
        SemanticErrorCode::EmptyIntent
    );

    let oversized_message = IntentProposal {
        message: Some("x".repeat(synck_file_folder::MAX_MESSAGE_BYTES + 1)),
        intents: vec![Intent::CreateFolder {
            item_id: item(2),
            location: Location {
                parent_item_id: None,
                name: "Folder".to_string(),
            },
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &oversized_message, &BTreeMap::new())
            .unwrap_err()
            .code,
        SemanticErrorCode::MessageTooLarge
    );
}

#[test]
fn materializer_rejects_invalid_state_graphs_and_conflicting_inputs() {
    let folder_id = item(1);
    let file_id = item(2);
    let child_id = item(3);
    let mut rows = BTreeMap::new();
    rows.insert(
        folder_id.clone(),
        current(
            folder_id.clone(),
            "FOLDER",
            None,
            "Folder",
            4,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    rows.insert(
        file_id.clone(),
        current(
            file_id.clone(),
            "FILE",
            Some(folder_id.clone()),
            "file.txt",
            7,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    rows.insert(
        child_id.clone(),
        current(
            child_id.clone(),
            "FOLDER",
            Some(folder_id.clone()),
            "Child",
            2,
            ProfileLifecycleState::Live,
            None,
        ),
    );

    let update_file = |item_id, base_seq| IntentProposal {
        message: None,
        intents: vec![Intent::UpdateFile {
            item_id,
            base_seq,
            location: None,
            content: Some(blob('a', 1)),
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &update_file(item(99), 1), &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::ItemNotFound
    );
    assert_eq!(
        validate_and_materialize(&repo_id(), &update_file(file_id.clone(), 6), &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::BaseSeqMismatch
    );
    assert_eq!(
        validate_and_materialize(&repo_id(), &update_file(folder_id.clone(), 4), &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::ConflictingIntent
    );
    let mut tombstoned_rows = rows.clone();
    tombstoned_rows.get_mut(&file_id).unwrap().lifecycle_state = ProfileLifecycleState::Tombstoned;
    assert_eq!(
        validate_and_materialize(
            &repo_id(),
            &update_file(file_id.clone(), 7),
            &tombstoned_rows,
        )
        .unwrap_err()
        .code,
        SemanticErrorCode::ItemTombstoned
    );

    let create_under = |parent_item_id| IntentProposal {
        message: None,
        intents: vec![Intent::CreateFolder {
            item_id: item(10),
            location: Location {
                parent_item_id: Some(parent_item_id),
                name: "New".to_string(),
            },
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &create_under(item(98)), &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::ParentNotFound
    );
    assert_eq!(
        validate_and_materialize(&repo_id(), &create_under(file_id.clone()), &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::ParentNotFolder
    );
    let mut trashed_parent_rows = rows.clone();
    trashed_parent_rows
        .get_mut(&folder_id)
        .unwrap()
        .lifecycle_state = ProfileLifecycleState::Trash;
    assert_eq!(
        validate_and_materialize(
            &repo_id(),
            &create_under(folder_id.clone()),
            &trashed_parent_rows,
        )
        .unwrap_err()
        .code,
        SemanticErrorCode::ParentNotLive
    );

    let conflict = IntentProposal {
        message: None,
        intents: vec![Intent::CreateFolder {
            item_id: item(11),
            location: Location {
                parent_item_id: Some(folder_id.clone()),
                name: "FILE.TXT".to_string(),
            },
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &conflict, &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::NameConflict
    );

    let cycle = IntentProposal {
        message: None,
        intents: vec![Intent::UpdateFolder {
            item_id: folder_id.clone(),
            base_seq: 4,
            location: Location {
                parent_item_id: Some(child_id),
                name: "Folder".to_string(),
            },
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &cycle, &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::CycleDetected
    );

    let duplicate = IntentProposal {
        message: None,
        intents: vec![
            Intent::UpdateFile {
                item_id: file_id.clone(),
                base_seq: 7,
                location: None,
                content: Some(blob('a', 1)),
            },
            Intent::UpdateFile {
                item_id: file_id,
                base_seq: 7,
                location: None,
                content: Some(blob('b', 1)),
            },
        ],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &duplicate, &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::ConflictingIntent
    );

    let conflicting_blob_sizes = IntentProposal {
        message: None,
        intents: vec![
            Intent::CreateFile {
                item_id: item(20),
                location: Location {
                    parent_item_id: None,
                    name: "a".to_string(),
                },
                content: blob('c', 1),
            },
            Intent::CreateFile {
                item_id: item(21),
                location: Location {
                    parent_item_id: None,
                    name: "b".to_string(),
                },
                content: blob('c', 2),
            },
        ],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &conflicting_blob_sizes, &BTreeMap::new())
            .unwrap_err()
            .code,
        SemanticErrorCode::InvalidContent
    );
}

#[test]
fn updates_emit_only_changed_fields_in_fixed_order_and_reject_no_ops() {
    let file_id = item(1);
    let mut rows = BTreeMap::new();
    let mut file = current(
        file_id.clone(),
        "FILE",
        None,
        "old.txt",
        7,
        ProfileLifecycleState::Live,
        None,
    );
    file.content = Some(blob('0', 1));
    rows.insert(file_id.clone(), file);
    let proposal = IntentProposal {
        message: None,
        intents: vec![Intent::UpdateFile {
            item_id: file_id.clone(),
            base_seq: 7,
            location: Some(Location {
                parent_item_id: None,
                name: "new.txt".to_string(),
            }),
            content: Some(blob('1', 2)),
        }],
    };
    let output = validate_and_materialize(&repo_id(), &proposal, &rows).unwrap();
    assert_eq!(
        output
            .events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["FILE::SET_NAME", "FILE::SET_CONTENT"]
    );
    assert!(output.events.iter().all(|event| event.base_seq == Some(7)));

    let no_op = IntentProposal {
        message: None,
        intents: vec![Intent::UpdateFile {
            item_id: file_id,
            base_seq: 7,
            location: Some(Location {
                parent_item_id: None,
                name: "old.txt".to_string(),
            }),
            content: Some(blob('0', 1)),
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &no_op, &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::NoOp
    );
}

#[test]
fn lifecycle_guards_include_all_non_tombstoned_heads_and_preserve_provenance() {
    let root_id = item(1);
    let live_child_id = item(2);
    let independent_child_id = item(3);
    let root_trash = CommitId::parse(digest('a')).unwrap();
    let independent_trash = CommitId::parse(digest('b')).unwrap();
    let mut rows = BTreeMap::new();
    rows.insert(
        root_id.clone(),
        current(
            root_id.clone(),
            "FOLDER",
            None,
            "Folder",
            1,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    rows.insert(
        live_child_id.clone(),
        current(
            live_child_id.clone(),
            "FILE",
            Some(root_id.clone()),
            "live.txt",
            2,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    rows.insert(
        independent_child_id.clone(),
        current(
            independent_child_id.clone(),
            "FILE",
            Some(root_id.clone()),
            "independent.txt",
            3,
            ProfileLifecycleState::Trash,
            Some(independent_trash),
        ),
    );
    let trash = IntentProposal {
        message: None,
        intents: vec![Intent::Trash {
            root: ItemBase {
                item_id: root_id.clone(),
                base_seq: 1,
            },
            descendants: vec![ItemBase {
                item_id: live_child_id.clone(),
                base_seq: 2,
            }],
        }],
    };
    let output = validate_and_materialize(&repo_id(), &trash, &rows).unwrap();
    assert_eq!(output.subtree_guards.len(), 1);
    assert_eq!(
        output.subtree_guards[0].expected_non_tombstoned_descendants,
        vec![
            (live_child_id.clone(), 2),
            (independent_child_id.clone(), 3)
        ]
    );
    assert_eq!(
        output
            .events
            .iter()
            .map(|event| event.item_id.as_ref().unwrap())
            .collect::<Vec<_>>(),
        vec![&live_child_id, &root_id]
    );

    rows.get_mut(&root_id).unwrap().lifecycle_state = ProfileLifecycleState::Trash;
    rows.get_mut(&root_id).unwrap().trash_commit_id = Some(root_trash.clone());
    rows.get_mut(&live_child_id).unwrap().lifecycle_state = ProfileLifecycleState::Trash;
    rows.get_mut(&live_child_id).unwrap().trash_commit_id = Some(root_trash.clone());
    let wrong_provenance = IntentProposal {
        message: None,
        intents: vec![Intent::Restore {
            root: ItemBase {
                item_id: root_id.clone(),
                base_seq: 1,
            },
            trash_commit_id: CommitId::parse(digest('c')).unwrap(),
            descendants: vec![ItemBase {
                item_id: live_child_id.clone(),
                base_seq: 2,
            }],
            location: None,
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &wrong_provenance, &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::LifecycleMismatch
    );
    let mut wrong_descendants = vec![
        ItemBase {
            item_id: live_child_id.clone(),
            base_seq: 2,
        },
        ItemBase {
            item_id: independent_child_id.clone(),
            base_seq: 3,
        },
    ];
    wrong_descendants.sort_by(|left, right| left.item_id.cmp(&right.item_id));
    let wrong_descendant_provenance = IntentProposal {
        message: None,
        intents: vec![Intent::Restore {
            root: ItemBase {
                item_id: root_id.clone(),
                base_seq: 1,
            },
            trash_commit_id: root_trash.clone(),
            descendants: wrong_descendants,
            location: None,
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &wrong_descendant_provenance, &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::LifecycleMismatch
    );
    let restore = IntentProposal {
        message: None,
        intents: vec![Intent::Restore {
            root: ItemBase {
                item_id: root_id.clone(),
                base_seq: 1,
            },
            trash_commit_id: root_trash,
            descendants: vec![ItemBase {
                item_id: live_child_id.clone(),
                base_seq: 2,
            }],
            location: None,
        }],
    };
    let output = validate_and_materialize(&repo_id(), &restore, &rows).unwrap();
    assert_eq!(
        output
            .events
            .iter()
            .map(|event| event.item_id.as_ref().unwrap())
            .collect::<Vec<_>>(),
        vec![&root_id, &live_child_id]
    );
    assert!(
        output
            .events
            .iter()
            .all(|event| event.item_id.as_ref() != Some(&independent_child_id))
    );
}

#[test]
fn lifecycle_rejects_mismatch_and_overlap_without_an_aggregate_size_cap() {
    let root_id = item(1);
    let child_id = item(2);
    let mut rows = BTreeMap::new();
    rows.insert(
        root_id.clone(),
        current(
            root_id.clone(),
            "FOLDER",
            None,
            "root",
            1,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    rows.insert(
        child_id.clone(),
        current(
            child_id.clone(),
            "FOLDER",
            Some(root_id.clone()),
            "child",
            1,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    let incomplete = IntentProposal {
        message: None,
        intents: vec![Intent::Trash {
            root: ItemBase {
                item_id: root_id.clone(),
                base_seq: 1,
            },
            descendants: Vec::new(),
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &incomplete, &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::LifecycleMismatch
    );
    let orphan_id = item(3);
    let mut missing_parent_rows = rows.clone();
    missing_parent_rows.insert(
        orphan_id.clone(),
        current(
            orphan_id.clone(),
            "FILE",
            Some(item(99)),
            "orphan",
            1,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    let missing_parent = IntentProposal {
        message: None,
        intents: vec![Intent::Trash {
            root: ItemBase {
                item_id: root_id.clone(),
                base_seq: 1,
            },
            descendants: vec![ItemBase {
                item_id: orphan_id,
                base_seq: 1,
            }],
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &missing_parent, &missing_parent_rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::ParentNotFound
    );
    let overlap = IntentProposal {
        message: None,
        intents: vec![
            Intent::Trash {
                root: ItemBase {
                    item_id: root_id.clone(),
                    base_seq: 1,
                },
                descendants: vec![ItemBase {
                    item_id: child_id.clone(),
                    base_seq: 1,
                }],
            },
            Intent::Trash {
                root: ItemBase {
                    item_id: child_id,
                    base_seq: 1,
                },
                descendants: Vec::new(),
            },
        ],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &overlap, &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::ConflictingIntent
    );

    let overflow_root = item(10_100);
    let mut overflow_rows = BTreeMap::new();
    overflow_rows.insert(
        overflow_root.clone(),
        current(
            overflow_root.clone(),
            "FOLDER",
            None,
            "root",
            1,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    let mut descendants = Vec::new();
    for number in 100..10_099 {
        let child = item(number);
        overflow_rows.insert(
            child.clone(),
            current(
                child.clone(),
                "FILE",
                Some(overflow_root.clone()),
                &format!("child-{number}"),
                1,
                ProfileLifecycleState::Live,
                None,
            ),
        );
        descendants.push(ItemBase {
            item_id: child,
            base_seq: 1,
        });
    }
    descendants.sort_by(|left, right| left.item_id.cmp(&right.item_id));
    let exact = IntentProposal {
        message: None,
        intents: vec![Intent::Trash {
            root: ItemBase {
                item_id: overflow_root.clone(),
                base_seq: 1,
            },
            descendants: descendants.clone(),
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &exact, &overflow_rows)
            .unwrap()
            .events
            .len(),
        10_000
    );
    let second_root = item(10_101);
    let second_child = item(10_102);
    overflow_rows.insert(
        second_root.clone(),
        current(
            second_root.clone(),
            "FOLDER",
            None,
            "second-root",
            1,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    overflow_rows.insert(
        second_child.clone(),
        current(
            second_child.clone(),
            "FILE",
            Some(second_root.clone()),
            "second-child",
            1,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    let aggregate = IntentProposal {
        message: None,
        intents: vec![
            Intent::Trash {
                root: ItemBase {
                    item_id: overflow_root.clone(),
                    base_seq: 1,
                },
                descendants: descendants.clone(),
            },
            Intent::Trash {
                root: ItemBase {
                    item_id: second_root,
                    base_seq: 1,
                },
                descendants: vec![ItemBase {
                    item_id: second_child,
                    base_seq: 1,
                }],
            },
        ],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &aggregate, &overflow_rows)
            .unwrap().events.len(),
        10_002
    );
    let overflow_child = item(10_099);
    overflow_rows.insert(
        overflow_child.clone(),
        current(
            overflow_child.clone(),
            "FILE",
            Some(overflow_root.clone()),
            "overflow-child",
            1,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    descendants.push(ItemBase {
        item_id: overflow_child,
        base_seq: 1,
    });
    descendants.sort_by(|left, right| left.item_id.cmp(&right.item_id));
    let oversized = IntentProposal {
        message: None,
        intents: vec![Intent::Trash {
            root: ItemBase {
                item_id: overflow_root,
                base_seq: 1,
            },
            descendants,
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &oversized, &overflow_rows)
            .unwrap().events.len(),
        10_001
    );
}

#[test]
fn reserved_control_components_are_rejected_only_when_exposed() {
    let create = IntentProposal {
        message: None,
        intents: vec![Intent::CreateFolder {
            item_id: item(1),
            location: Location {
                parent_item_id: None,
                name: ".SYNCK".to_string(),
            },
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &create, &BTreeMap::new())
            .unwrap_err()
            .code,
        SemanticErrorCode::ReservedWorktreePath
    );

    let nested_parent = item(10);
    let nested = IntentProposal {
        message: None,
        intents: vec![
            Intent::CreateFolder {
                item_id: nested_parent.clone(),
                location: Location {
                    parent_item_id: None,
                    name: "safe".to_string(),
                },
            },
            Intent::CreateFolder {
                item_id: item(11),
                location: Location {
                    parent_item_id: Some(nested_parent),
                    name: ".Synck".to_string(),
                },
            },
        ],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &nested, &BTreeMap::new())
            .unwrap_err()
            .code,
        SemanticErrorCode::ReservedWorktreePath
    );

    let reserved_id = item(2);
    let file_id = item(3);
    let mut rows = BTreeMap::new();
    rows.insert(
        reserved_id.clone(),
        current(
            reserved_id.clone(),
            "FOLDER",
            None,
            ".synck",
            4,
            ProfileLifecycleState::Trash,
            Some(CommitId::parse(digest('a')).unwrap()),
        ),
    );
    rows.insert(
        file_id.clone(),
        current(
            file_id.clone(),
            "FILE",
            None,
            "file.txt",
            1,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    let relocate = IntentProposal {
        message: None,
        intents: vec![Intent::UpdateFile {
            item_id: file_id.clone(),
            base_seq: 1,
            location: Some(Location {
                parent_item_id: None,
                name: ".SyNcK".to_string(),
            }),
            content: None,
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &relocate, &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::ReservedWorktreePath
    );
    let unrelated = IntentProposal {
        message: None,
        intents: vec![Intent::UpdateFile {
            item_id: file_id,
            base_seq: 1,
            location: None,
            content: Some(blob('1', 1)),
        }],
    };
    validate_and_materialize(&repo_id(), &unrelated, &rows).unwrap();
    let restore = IntentProposal {
        message: None,
        intents: vec![Intent::Restore {
            root: ItemBase {
                item_id: reserved_id,
                base_seq: 4,
            },
            trash_commit_id: CommitId::parse(digest('a')).unwrap(),
            descendants: Vec::new(),
            location: None,
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &restore, &rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::ReservedWorktreePath
    );

    let hidden_parent_id = item(20);
    let relocated_root_id = item(21);
    let hidden_reserved_id = item(22);
    let mut hidden_rows = BTreeMap::new();
    hidden_rows.insert(
        hidden_parent_id.clone(),
        current(
            hidden_parent_id.clone(),
            "FOLDER",
            None,
            "hidden",
            1,
            ProfileLifecycleState::Trash,
            Some(CommitId::parse(digest('b')).unwrap()),
        ),
    );
    hidden_rows.insert(
        relocated_root_id.clone(),
        current(
            relocated_root_id.clone(),
            "FOLDER",
            Some(hidden_parent_id),
            "relocated",
            2,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    hidden_rows.insert(
        hidden_reserved_id.clone(),
        current(
            hidden_reserved_id,
            "FOLDER",
            Some(relocated_root_id.clone()),
            ".synck",
            3,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    let expose_descendant = IntentProposal {
        message: None,
        intents: vec![Intent::UpdateFolder {
            item_id: relocated_root_id,
            base_seq: 2,
            location: Location {
                parent_item_id: None,
                name: "relocated".to_string(),
            },
        }],
    };
    assert_eq!(
        validate_and_materialize(&repo_id(), &expose_descendant, &hidden_rows)
            .unwrap_err()
            .code,
        SemanticErrorCode::ReservedWorktreePath
    );
}

#[test]
fn projector_preserves_visible_changes_and_profile_heads_only() {
    let file_id = item(1);
    let record_id = item(2);
    let mut create = event(file_id.clone(), "FILE::CREATE", None);
    create.create_item = Some(synck_vcs::types::CreateItem {
        item_type: "FILE".to_string(),
        parent_folder_id: None,
        name: "file.txt".to_string(),
    });
    create.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"product": "preserved only in Core"})).unwrap());
    let mut omitted = event(file_id.clone(), "ITEM::DATA_LINK::PUT", None);
    omitted.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"record_id": "r"})).unwrap());
    let mut out_of_profile = event(record_id.clone(), "ITEM::DATA_LINK::PUT", Some(1));
    out_of_profile.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"record_id": "r"})).unwrap());
    let (commit, rows) = accepted_commit(vec![create, omitted, out_of_profile], Some("mixed"));
    let group = project_commit(
        &commit,
        &rows,
        &[ImmutableItemKind {
            item_id: record_id,
            item_type: "RECORD".to_string(),
        }],
    )
    .unwrap()
    .unwrap();
    assert_eq!(group.changes.len(), 1);
    assert!(group.changes[0].event.set_metadata.is_none());
    assert_eq!(group.item_heads.len(), 1);
    assert_eq!(group.item_heads[0].head_seq, 2);
    let mut fixture: Value = serde_json::from_str(GROUPS_FIXTURE).unwrap();
    fixture["mixed_group"]["commit_id"] = json!(commit.commit_id);
    assert_eq!(
        serde_json::to_value(&group).unwrap(),
        fixture["mixed_group"]
    );
}

#[test]
fn projector_accepts_hash_bound_import_identity() {
    let folder_id = item(41);
    let mut create = event(folder_id, "FOLDER::CREATE", None);
    create.create_item = Some(synck_vcs::types::CreateItem {
        item_type: "FOLDER".to_string(),
        parent_folder_id: None,
        name: "Imported".to_string(),
    });
    let (mut commit, mut rows) = accepted_commit(vec![create], Some("import"));
    let job_id = "550e8400-e29b-41d4-a716-446655440000";
    let bytes = canonical_commit_bytes_for_hash(
        &commit.author_id,
        &commit.committer_id,
        commit.occurred_at,
        commit.message.as_deref(),
        Some(job_id),
        rows.len(),
        &commit.events_digest,
    )
    .unwrap();
    let commit_id = compute_commit_id(&bytes);
    commit.format_version = COMMIT_FORMAT_VERSION;
    commit.import_job_id = Some(job_id.to_string());
    commit.commit_id = commit_id.clone();
    for row in &mut rows {
        row.commit_id = commit_id.clone();
    }

    let group = project_commit(&commit, &rows, &[]).unwrap().unwrap();
    assert_eq!(group.commit_id, commit_id);
    assert_eq!(group.changes.len(), 1);
}

#[test]
fn projector_emits_head_only_groups_omits_out_of_profile_commits_and_keeps_tombstones() {
    let file_id = item(1);
    let folder_id = item(2);
    let record_id = item(3);
    let mut file_metadata = event(file_id.clone(), "FILE::SET_METADATA", Some(4));
    file_metadata.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"label": "x"})).unwrap());
    let mut folder_link = event(folder_id.clone(), "ITEM::DATA_LINK::PUT", Some(7));
    folder_link.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"record_id": "r"})).unwrap());
    let (commit, rows) = accepted_commit(vec![file_metadata, folder_link], None);
    let group = project_commit(
        &commit,
        &rows,
        &[
            ImmutableItemKind {
                item_id: file_id.clone(),
                item_type: "FILE".to_string(),
            },
            ImmutableItemKind {
                item_id: folder_id,
                item_type: "FOLDER".to_string(),
            },
        ],
    )
    .unwrap()
    .unwrap();
    assert!(group.changes.is_empty());
    assert_eq!(group.item_heads.len(), 2);
    let mut fixture: Value = serde_json::from_str(GROUPS_FIXTURE).unwrap();
    fixture["head_only_group"]["commit_id"] = json!(commit.commit_id);
    assert_eq!(
        serde_json::to_value(&group).unwrap(),
        fixture["head_only_group"]
    );

    let mut record_event = event(record_id.clone(), "ITEM::DATA_LINK::PUT", Some(2));
    record_event.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"record_id": "r"})).unwrap());
    let (commit, rows) = accepted_commit(vec![record_event], None);
    assert!(
        project_commit(
            &commit,
            &rows,
            &[ImmutableItemKind {
                item_id: record_id,
                item_type: "RECORD".to_string(),
            }],
        )
        .unwrap()
        .is_none()
    );

    let mut tombstone = event(file_id.clone(), "FILE::TOMBSTONE", Some(9));
    tombstone.set_tombstoned = Some(true);
    let (commit, rows) = accepted_commit(vec![tombstone], None);
    let group = project_commit(
        &commit,
        &rows,
        &[ImmutableItemKind {
            item_id: file_id,
            item_type: "FILE".to_string(),
        }],
    )
    .unwrap()
    .unwrap();
    assert_eq!(group.changes.len(), 1);
    assert_eq!(group.changes[0].event.event_type, "FILE::TOMBSTONE");
}

#[test]
fn projector_omits_an_out_of_profile_item_created_and_mutated_in_one_commit() {
    let record_id = item(3);
    let mut create = event(record_id.clone(), "RECORD::CREATE", None);
    create.create_item = Some(synck_vcs::types::CreateItem {
        item_type: "RECORD".to_string(),
        parent_folder_id: None,
        name: "record".to_string(),
    });
    let mut mutation = event(record_id, "RECORD::SET_METADATA", None);
    mutation.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"field": "value"})).unwrap());
    let proposals = vec![create, mutation];
    validate_projected_proposal(None, &proposals, &[]).unwrap();
    let (commit, rows) = accepted_commit(proposals, None);
    assert!(project_commit(&commit, &rows, &[]).unwrap().is_none());
}

#[test]
fn existing_item_can_move_into_a_folder_created_by_the_same_proposal() {
    let folder_id = item(1);
    let file_id = item(2);
    let mut rows = BTreeMap::new();
    rows.insert(
        file_id.clone(),
        current(
            file_id.clone(),
            "FILE",
            None,
            "report.txt",
            4,
            ProfileLifecycleState::Live,
            None,
        ),
    );
    rows.get_mut(&file_id).unwrap().content = Some(blob('a', 7));
    let proposal = IntentProposal {
        message: Some("archive".to_string()),
        intents: vec![
            Intent::UpdateFile {
                item_id: file_id,
                base_seq: 4,
                location: Some(Location {
                    parent_item_id: Some(folder_id.clone()),
                    name: "report.txt".to_string(),
                }),
                content: None,
            },
            Intent::CreateFolder {
                item_id: folder_id,
                location: Location {
                    parent_item_id: None,
                    name: "archive".to_string(),
                },
            },
        ],
    };
    let materialized = validate_and_materialize(&repo_id(), &proposal, &rows).unwrap();
    assert_eq!(
        materialized
            .events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["FOLDER::CREATE", "FILE::SET_PARENT"]
    );
}

#[test]
fn projector_preserves_metadata_identity_and_rejects_unknown_keys() {
    let file_id = item(1);
    let mut content = event(file_id.clone(), "FILE::SET_CONTENT", Some(4));
    content.set_content = Some(EventContent::bytes(synck_vcs::types::ObjectId::parse(digest('a')).unwrap()));
    content.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({
        "content": {"size": 7},
        "upload_intent_id": "intent",
        "revision_origin": "LOCAL",
        "provenance": {
            "source": "DESKTOP",
            "committed_by": "actor",
            "human_editor_ids": ["human"],
            "primary_human_editor_id": "human"
        }
    })).unwrap());
    let kinds = [ImmutableItemKind {
        item_id: file_id.clone(),
        item_type: "FILE".to_string(),
    }];
    validate_projected_proposal(None, &[content.clone()], &kinds).unwrap();
    let projected = project_proposal_events(&[content.clone()], &kinds).unwrap();
    assert_eq!(
        projected[0].set_metadata.as_ref().unwrap().id(),
        content.set_metadata.as_ref().unwrap().id()
    );
    let (commit, rows) = accepted_commit(vec![content.clone()], None);
    let group = project_commit(&commit, &rows, &kinds).unwrap().unwrap();
    assert_eq!(
        group.changes[0].event.set_metadata.as_ref().unwrap().id(),
        content.set_metadata.as_ref().unwrap().id()
    );

    let mut metadata = content.set_metadata.as_ref().unwrap().value().unwrap().clone();
    metadata["unknown"] = Value::Bool(true);
    content.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(metadata).unwrap());
    assert_eq!(
        validate_projected_proposal(None, &[content], &kinds)
            .unwrap_err()
            .code,
        SemanticErrorCode::MalformedProfileEvent
    );
}

#[test]
fn projector_accepts_only_the_deployed_product_omission_shapes() {
    let file_id = item(1);
    let folder_id = item(2);
    let file_kind = [ImmutableItemKind {
        item_id: file_id.clone(),
        item_type: "FILE".to_string(),
    }];
    let folder_kind = [ImmutableItemKind {
        item_id: folder_id.clone(),
        item_type: "FOLDER".to_string(),
    }];

    for event_type in [
        "FILE::SET_METADATA",
        "FILE::REVISION_SUPPRESS",
        "FILE::REVISION_UNSUPPRESS",
        "FILE::VERSION::TOMBSTONE",
        "ITEM::DATA_LINK::PUT",
        "ITEM::DATA_LINK::DELETE",
        "ITEM::SOURCE_MODIFIED_AT::REPAIR",
    ] {
        let mut proposal = event(file_id.clone(), event_type, Some(3));
        proposal.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"contract": "product"})).unwrap());
        validate_projected_proposal(None, &[proposal], &file_kind).unwrap();
    }

    let mut version_create = event(file_id.clone(), "FILE::VERSION::CREATE", Some(3));
    version_create.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"content": {"size": 1}})).unwrap());
    version_create.set_content = Some(EventContent::bytes(synck_vcs::types::ObjectId::parse(digest('d')).unwrap()));
    validate_projected_proposal(None, &[version_create], &file_kind).unwrap();

    for with_content in [false, true] {
        let mut version_metadata = event(file_id.clone(), "FILE::VERSION::SET_METADATA", Some(3));
        version_metadata.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"item_seq": 1})).unwrap());
        if with_content {
            version_metadata.set_content = Some(EventContent::bytes(synck_vcs::types::ObjectId::parse(digest('e')).unwrap()));
        }
        validate_projected_proposal(None, &[version_metadata], &file_kind).unwrap();
    }

    for event_type in ["FOLDER::SET_METADATA", "FOLDER::WORKFLOW_RUNTIME"] {
        let mut proposal = event(folder_id.clone(), event_type, Some(5));
        proposal.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"contract": "product"})).unwrap());
        validate_projected_proposal(None, &[proposal], &folder_kind).unwrap();
    }

    for (is_federated, stream_id) in [(true, Some("stream")), (false, None)] {
        let mut proposal = event(file_id.clone(), "FILE::FEDERATE", Some(3));
        proposal.set_is_federated = Some(is_federated);
        proposal.set_federation_stream_id = stream_id.map(str::to_string);
        validate_projected_proposal(None, &[proposal], &file_kind).unwrap();
    }

    let mut malformed = event(file_id, "FILE::SET_METADATA", Some(3));
    malformed.set_metadata = Some(synck_vcs::metadata::MetadataRef::new(json!({"contract": "product"})).unwrap());
    malformed.set_parent = Some(None);
    assert_eq!(
        validate_projected_proposal(None, &[malformed], &file_kind)
            .unwrap_err()
            .code,
        SemanticErrorCode::MalformedProfileEvent
    );

    assert!(synck_vcs::metadata::MetadataRef::new(json!({
        "value": "x".repeat(synck_file_folder::MAX_EVENT_PAYLOAD_BYTES)
    })).is_err());
}

#[test]
fn projector_fails_closed_on_move_kind_maps_prefixes_and_malformed_shapes() {
    let file_id = item(1);
    let kinds = [ImmutableItemKind {
        item_id: file_id.clone(),
        item_type: "FILE".to_string(),
    }];
    let mut moved = event(file_id.clone(), "FILE::MOVE", Some(1));
    moved.set_name = Some("moved.txt".to_string());
    assert_eq!(
        validate_projected_proposal(None, &[moved], &kinds)
            .unwrap_err()
            .code,
        SemanticErrorCode::UnknownProfileEvent
    );

    let mut renamed = event(file_id.clone(), "FILE::SET_NAME", Some(1));
    renamed.set_name = Some("renamed.txt".to_string());
    assert_eq!(
        validate_projected_proposal(None, std::slice::from_ref(&renamed), &[])
            .unwrap_err()
            .code,
        SemanticErrorCode::MalformedProfileEvent
    );
    assert_eq!(
        validate_projected_proposal(
            None,
            std::slice::from_ref(&renamed),
            &[
                kinds[0].clone(),
                ImmutableItemKind {
                    item_id: item(2),
                    item_type: "FILE".to_string(),
                },
            ],
        )
        .unwrap_err()
        .code,
        SemanticErrorCode::MalformedProfileEvent
    );

    renamed.event_type = "FOLDER::SET_NAME".to_string();
    assert_eq!(
        validate_projected_proposal(None, std::slice::from_ref(&renamed), &kinds)
            .unwrap_err()
            .code,
        SemanticErrorCode::MalformedProfileEvent
    );
    renamed.event_type = "FILE::SET_NAME".to_string();
    renamed.set_in_trash = Some(true);
    assert_eq!(
        validate_projected_proposal(None, &[renamed], &kinds)
            .unwrap_err()
            .code,
        SemanticErrorCode::MalformedProfileEvent
    );
}

#[test]
fn projector_detects_noncanonical_payload_and_digest_or_envelope_corruption() {
    let file_id = item(1);
    let mut rename = event(file_id.clone(), "FILE::SET_NAME", Some(1));
    rename.set_name = Some("renamed.txt".to_string());
    let kinds = [ImmutableItemKind {
        item_id: file_id,
        item_type: "FILE".to_string(),
    }];
    let (commit, mut rows) = accepted_commit(vec![rename], None);
    rows[0].payload.push(b' ');
    assert_eq!(
        project_commit(&commit, &rows, &kinds).unwrap_err().code,
        SemanticErrorCode::MalformedProfileEvent
    );

    let mut rename = event(kinds[0].item_id.clone(), "FILE::SET_NAME", Some(4));
    rename.set_name = Some("renamed-again.txt".to_string());
    let (commit, mut rows) = accepted_commit(vec![rename], None);
    rows[0].item_seq -= 1;
    assert_eq!(
        project_commit(&commit, &rows, &kinds).unwrap_err().code,
        SemanticErrorCode::MalformedProfileEvent
    );

    let mut missing_base = event(kinds[0].item_id.clone(), "FILE::SET_NAME", None);
    missing_base.set_name = Some("missing-base.txt".to_string());
    assert_eq!(
        validate_projected_proposal(None, &[missing_base], &kinds)
            .unwrap_err()
            .code,
        SemanticErrorCode::MalformedProfileEvent
    );
}

fn accepted_commit(
    proposals: Vec<EventProposal>,
    message: Option<&str>,
) -> (CommitRow, Vec<EventRow>) {
    let repo_id = repo_id();
    let mut item_heads = BTreeMap::<ItemId, i64>::new();
    let mut digest_hasher = Sha256::new();
    let mut rows = Vec::new();
    for (index, proposal) in proposals.iter().enumerate() {
        let payload = canonical_event_proposal_bytes(proposal).unwrap();
        let digest = compute_sha256_digest_bytes(&payload);
        digest_hasher.update([synck_vcs::types::COMMIT_EVENT_FAMILY_ITEM]);
        digest_hasher.update(digest);
        let item_id = proposal.item_id.clone().unwrap();
        let item_seq = item_heads
            .entry(item_id.clone())
            .or_insert_with(|| proposal.base_seq.unwrap_or(0));
        *item_seq += 1;
        rows.push((index, item_id, *item_seq, proposal, payload, digest));
    }
    let events_digest_bytes: [u8; 32] = digest_hasher.finalize().into();
    let events_digest =
        Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(events_digest_bytes)).unwrap();
    let author = "author".to_string();
    let commit_bytes = canonical_commit_bytes_for_hash(
        &author,
        &"committer".to_string(),
        None,
        message,
        None,
        rows.len(),
        &events_digest,
    )
    .unwrap();
    let commit_id = compute_commit_id(&commit_bytes);
    let accepted_at = "2026-08-20T12:00:00Z".parse().unwrap();
    let event_rows = rows
        .into_iter()
        .map(
            |(index, item_id, item_seq, proposal, payload, digest)| EventRow {
                metadata: proposal.set_metadata.as_ref().and_then(|value| value.payload()).cloned(),
                repo_id: repo_id.clone(),
                item_id,
                item_seq,
                event_type: proposal.event_type.clone(),
                commit_id: commit_id.clone(),
                commit_event_idx: index as i32,
                repo_seq: Some(1),
                payload,
                event_payload_digest: Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
                    digest,
                ))
                .unwrap(),
                created_at: accepted_at,
                parent_folder_id: None,
                parent_folder_id_before: None,
            },
        )
        .collect();
    (
        CommitRow {
            repo_id,
            commit_id,
            repo_seq: Some(1),
            format_version: COMMIT_FORMAT_VERSION,
            import_job_id: None,
            author_id: author,
            committer_id: "committer".to_string(),
            message: message.map(str::to_string),
            occurred_at: None,
            created_at: accepted_at,
            event_count: proposals.len() as i32,
            events_digest,
        },
        event_rows,
    )
}
