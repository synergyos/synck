use chrono::{TimeZone, Utc};
use synck_sync::*;
use synck_vcs::types::*;
use uuid::Uuid;

fn repository() -> RepositoryIdentity {
    RepositoryIdentity {
        repo_id: RepoId::parse("019a2900-0000-7000-8000-000000000001").unwrap(),
        generation: Uuid::parse_str("019a2900-0000-7000-8000-000000000002").unwrap(),
    }
}
fn genesis_value() -> [u8; 32] {
    let repository = repository();
    synck_vcs::checkpoint::genesis(&repository.repo_id, &repository.generation.to_string(), false)
        .unwrap()
}
fn now() -> Timestamp {
    Utc.timestamp_opt(1_793_145_600, 123_456_789).unwrap()
}
fn digest(bytes: [u8; 32]) -> Sha256Digest {
    Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(bytes)).unwrap()
}
fn vector() -> serde_json::Value {
    serde_json::from_str(include_str!("../../../docs/spec/vectors/sync.json")).unwrap()
}
fn request() -> SubmitRequest {
    serde_json::from_value(vector()["request"].clone()).unwrap()
}
fn records() -> Vec<ProposalRecord> {
    serde_json::from_value(vector()["records"].clone()).unwrap()
}
fn verifier(request: &SubmitRequest) -> ProposalVerifier {
    ProposalVerifier::new(
        request.repository.clone(),
        request.operation_id.clone(),
        now() + chrono::Duration::hours(1),
    )
    .unwrap()
}
fn upload(request: &SubmitRequest, records: &[ProposalRecord], size: usize) -> ProposalVerifier {
    let mut state = verifier(request);
    for records in records.chunks(size) {
        let page = ProposalPage {
            repository: request.repository.clone(),
            operation_id: request.operation_id.clone(),
            offset: state.next_offset(),
            records: records.to_vec(),
        };
        state.push(&page, now()).unwrap();
    }
    state
}
fn published(request: &SubmitRequest, records: &[ProposalRecord]) -> PublishedCommit {
    let validated = validate_submit(request).unwrap();
    let events = records
        .iter()
        .enumerate()
        .filter_map(|(i, record)| match record {
            ProposalRecord::Item(event) => {
                let payload = canonical_event_proposal_bytes(event).unwrap();
                Some(CommitEventRow::Item(EventRow {
                    repo_id: request.repository.repo_id.clone(),
                    item_id: event.item_id.clone().unwrap(),
                    item_seq: 1,
                    event_type: event.event_type.clone(),
                    commit_id: validated.commit_id.clone(),
                    commit_event_idx: i as i32,
                    repo_seq: Some(1),
                    event_payload_digest: digest(compute_sha256_digest_bytes(&payload)),
                    payload,
                    metadata: None,
                    created_at: now(),
                    parent_folder_id: None,
                    parent_folder_id_before: None,
                }))
            }
            ProposalRecord::Ref(event) => {
                let payload = canonical_ref_event_proposal_bytes(event).unwrap();
                Some(CommitEventRow::Ref(RefEventRow {
                    repo_id: request.repository.repo_id.clone(),
                    owner_item_id: event.owner_item_id.clone(),
                    ref_seq: 1,
                    event_type: event.event_type.clone(),
                    commit_id: validated.commit_id.clone(),
                    commit_event_idx: i as i32,
                    repo_seq: Some(1),
                    event_payload_digest: digest(compute_sha256_digest_bytes(&payload)),
                    payload,
                    metadata: None,
                    created_at: now(),
                }))
            }
            ProposalRecord::Guard(_) => None,
        })
        .collect();
    let checkpoint = digest(
        synck_vcs::checkpoint::digest(
            &genesis_value(),
            1,
            &validated.commit_id,
            now(),
        )
        .unwrap(),
    );
    PublishedCommit {
        commit: CommitRow {
            repo_id: request.repository.repo_id.clone(),
            commit_id: validated.commit_id,
            repo_seq: Some(1),
            format_version: COMMIT_FORMAT_VERSION,
            author_id: request.metadata.author_id.clone(),
            committer_id: request.metadata.committer_id.clone(),
            message: request.metadata.message.clone(),
            occurred_at: request.metadata.occurred_at,
            created_at: now(),
            event_count: request.event_count as i32,
            events_digest: request.events_digest.clone(),
        },
        events,
        checkpoint,
    }
}
fn page(
    published: &PublishedCommit,
    after: HistoryPosition,
    events: Vec<CommitEventRow>,
    has_more: bool,
) -> HistoryPage {
    HistoryPage {
        after,
        fragments: vec![HistoryFragment {
            header: PublishedHeader {
                commit: published.commit.clone(),
                checkpoint: published.checkpoint.clone(),
            },
            events,
        }],
        has_more,
    }
}

#[test]
fn independent_wire_vector_and_page_boundaries_preserve_identity() {
    let v = vector();
    let request = request();
    let validated = upload(&request, &records(), 1)
        .finish(&request, now())
        .unwrap();
    assert_eq!(
        validated.commit_id.as_str(),
        v["commit_id"].as_str().unwrap()
    );
    assert_eq!(
        validated.events_digest.as_str(),
        v["events_digest"].as_str().unwrap()
    );
    assert_eq!(
        validated.request_digest.as_str(),
        v["request_digest"].as_str().unwrap()
    );
    if let ProposalRecord::Item(event) = &records()[0] {
        assert_eq!(
            canonical_event_proposal_bytes(event).unwrap(),
            v["canonical_event_utf8"].as_str().unwrap().as_bytes()
        );
    } else {
        panic!();
    }
}

#[test]
fn large_import_vector_pages_more_than_ten_thousand_events_atomically() {
    let v: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/sync-large.json")).unwrap();
    let request: SubmitRequest = serde_json::from_value(v["request"].clone()).unwrap();
    let records: Vec<ProposalRecord> = (0..10_001)
        .map(|i| {
            ProposalRecord::Item(
                serde_json::from_value(serde_json::json!({
        "item_id":format!("019a2900-0000-7000-8000-{:012x}",i+100),"event_type":"FOLDER::CREATE",
        "create_item":{"item_type":"FOLDER","name":format!("folder-{i:05}")}}))
                .unwrap(),
            )
        })
        .collect();
    for size in [MAX_PAGE_RECORDS, 317] {
        let state = upload(&request, &records, size);
        let actual = state.finish(&request, now()).unwrap();
        assert_eq!(actual.commit_id.as_str(), v["commit_id"].as_str().unwrap());
        assert_eq!(
            actual.events_digest.as_str(),
            v["events_digest"].as_str().unwrap()
        );
        assert_eq!(
            actual.request_digest.as_str(),
            v["request_digest"].as_str().unwrap()
        );
    }
    let published = published(&request, &records);
    verify_published_commit(&published, &repository()).unwrap();
    let genesis = Cursor::genesis(repository(), false).unwrap();
    let mut state = HistoryVerifier::new(genesis.clone()).unwrap();
    let mut installed = 0;
    for (i, events) in published.events.chunks(317).enumerate() {
        let last = (i + 1) * 317 >= published.events.len();
        let p = page(&published, state.position().clone(), events.to_vec(), !last);
        let completed = state.push(&p).unwrap();
        if last {
            assert_eq!(completed.as_ref().unwrap().checkpoint, published.checkpoint);
            installed += 1;
        } else {
            assert!(completed.is_none());
            assert_eq!(state.position().completed, genesis);
            // Restart: reconstruct the private partial verifier from staged pages.
            if i == 2 {
                let mut resumed = HistoryVerifier::new(genesis.clone()).unwrap();
                for earlier in published.events[..(i + 1) * 317].chunks(317) {
                    let p = page(
                        &published,
                        resumed.position().clone(),
                        earlier.to_vec(),
                        true,
                    );
                    assert!(resumed.push(&p).unwrap().is_none());
                }
                assert_eq!(resumed.position(), state.position());
                state = resumed;
            }
        }
    }
    assert_eq!(installed, 1);
    assert!(state.position().pending.is_none());
}

#[test]
fn staged_upload_rejects_gaps_tampering_and_expiry_without_acceptance() {
    let request = request();
    let mut state = verifier(&request);
    let mut p = ProposalPage {
        repository: repository(),
        operation_id: request.operation_id.clone(),
        offset: 1,
        records: records(),
    };
    assert!(state.push(&p, now()).is_err());
    assert_eq!(state.next_offset(), 0);
    p.offset = 0;
    p.records.extend(records());
    if let ProposalRecord::Item(event) = &mut p.records[1] {
        event.item_id = None;
    }
    assert!(state.push(&p, now()).is_err());
    assert_eq!(state.next_offset(), 0);
    p.records = records();
    state.push(&p, now()).unwrap();
    assert!(state.push(&p, now()).is_err()); // Durable server handles exact-page retries before calling push.
    let mut bad = request.clone();
    bad.event_count += 1;
    assert!(state.finish(&bad, now()).is_err());
    bad = request.clone();
    bad.events_digest = digest([9; 32]);
    assert!(state.finish(&bad, now()).is_err());
    state.finish(&request, now()).unwrap();
    let expired = now() + chrono::Duration::hours(1);
    assert!(matches!(
        state.finish(&request, expired),
        Err(ProtocolError::UploadExpired)
    ));
    p.offset = state.next_offset();
    assert!(matches!(
        state.push(&p, expired),
        Err(ProtocolError::UploadExpired)
    ));
    // Reupload after expired staging is discarded preserves content identity.
    assert_eq!(
        upload(&request, &records(), 1)
            .finish(&request, now())
            .unwrap()
            .commit_id,
        validate_submit(&request).unwrap().commit_id
    );
}

#[test]
fn refs_and_guards_span_pages_and_bind_the_request() {
    let initial = request();
    let mut rec = records();
    rec.push(ProposalRecord::Ref(serde_json::from_value(serde_json::json!({
        "owner_item_id":"019a2900-0000-7000-8000-000000000003","event_type":"FILE_VERSION::CREATE","base_ref_seq":0})).unwrap()));
    for i in 0..1100 {
        rec.push(ProposalRecord::Guard(ItemHead {
            item_id: ItemId::parse(format!("019a2900-0000-7000-8000-{:012x}", i + 100)).unwrap(),
            head_seq: 1,
        }));
    }
    let state = upload(&initial, &rec, 256);
    let request = state.request(initial.metadata.clone()).unwrap();
    assert_eq!(request.event_count, 2);
    assert_eq!(request.guard_count, 1100);
    state.finish(&request, now()).unwrap();
    let no_guards = upload(&initial, &rec[..2], 1)
        .request(initial.metadata.clone())
        .unwrap();
    assert_eq!(
        validate_submit(&request).unwrap().commit_id,
        validate_submit(&no_guards).unwrap().commit_id
    );
    assert_ne!(
        validate_submit(&request).unwrap().request_digest,
        validate_submit(&no_guards).unwrap().request_digest
    );
    let mut state = verifier(&initial);
    rec.swap(0, 1);
    assert!(
        state
            .push(
                &ProposalPage {
                    repository: repository(),
                    operation_id: initial.operation_id,
                    offset: 0,
                    records: rec[..2].to_vec()
                },
                now()
            )
            .is_err()
    );
}

#[test]
fn history_tampering_replay_and_partial_terminal_pages_leave_cursor_unchanged() {
    let request = request();
    let published = published(&request, &records());
    let mut state = HistoryVerifier::new(Cursor::genesis(repository(), false).unwrap()).unwrap();
    let original = state.position().clone();
    let valid = page(
        &published,
        original.clone(),
        published.events.clone(),
        false,
    );
    let mut bad = valid.clone();
    bad.fragments[0].header.commit.created_at += chrono::Duration::seconds(1);
    assert!(state.push(&bad).is_err());
    assert_eq!(state.position(), &original);
    bad = valid.clone();
    if let CommitEventRow::Item(row) = &mut bad.fragments[0].events[0] {
        row.payload.push(b' ');
    }
    assert!(state.push(&bad).is_err());
    assert_eq!(state.position(), &original);
    bad = valid.clone();
    bad.fragments[0].events.clear();
    assert!(state.push(&bad).is_err());
    bad = valid.clone();
    bad.after.completed.repository.generation = Uuid::now_v7();
    assert!(state.push(&bad).is_err());
    assert!(state.push(&valid).unwrap().is_some());
    assert!(state.push(&valid).is_err());
    let terminal = HistoryPage {
        after: state.position().clone(),
        fragments: vec![],
        has_more: false,
    };
    assert!(state.push(&terminal).unwrap().is_none());
    let mut bad = terminal;
    bad.has_more = true;
    assert!(state.push(&bad).is_err());
}

#[test]
fn receipts_bind_generation_operation_guards_and_metadata() {
    let request = request();
    let result = validate_submit(&request).unwrap();
    let receipt = Receipt {
        operation_id: request.operation_id.clone(),
        request_digest: result.request_digest,
        commit_id: result.commit_id,
        accepted_at: now(),
        published: None,
    };
    receipt.validate_for(&request).unwrap();
    for changed in [
        {
            let mut r = request.clone();
            r.operation_id = "other".into();
            r
        },
        {
            let mut r = request.clone();
            r.repository.generation = Uuid::now_v7();
            r
        },
        {
            let mut r = request.clone();
            r.guards_digest = digest([2; 32]);
            r
        },
        {
            let mut r = request.clone();
            r.metadata.message = Some("other".into());
            r
        },
    ] {
        assert!(receipt.validate_for(&changed).is_err());
    }
}

#[test]
fn server_policy_is_nonretryable_and_does_not_restrict_history() {
    let mut request = request();
    let rec = records();
    let published = published(&request, &rec);
    let policy = ServerPolicy {
        max_events_per_commit: Some(0),
        max_object_bytes: Some(20),
        quota_bytes: Some(100),
    };
    let violations = policy.publication_violations([&published.commit, &published.commit]);
    assert_eq!(violations.len(), 2);
    assert_eq!(violations[0].commit_id, published.commit.commit_id);
    let response = SubmitResult::PolicyLimit {
        violation: policy.check_commit(1).unwrap_err(),
    };
    assert_eq!(
        serde_json::to_value(response).unwrap()["status"],
        "POLICY_LIMIT"
    );
    assert!(policy.check_object(21).is_err());
    assert!(policy.check_quota(90, 11).is_err());
    verify_published_commit(&published, &repository()).unwrap();
    let unrestricted = ServerPolicy::default();
    assert!(unrestricted.check_commit(2_000_000).is_ok());
    assert!(unrestricted.check_object(u64::MAX).is_ok());
    assert!(
        unrestricted
            .publication_violations([&published.commit])
            .is_empty()
    );
    request.metadata.message = Some("x".repeat(synck_vcs::limits::MAX_MESSAGE_BYTES));
    request.operation_id = "é".repeat(synck_vcs::limits::MAX_OPERATION_ID_BYTES / 2);
    validate_submit(&request).unwrap();
    request.operation_id.push('x');
    assert!(validate_submit(&request).is_err());
}

#[test]
fn transfer_bounds_and_unknown_fields_are_rejected() {
    let request = request();
    let mut state = verifier(&request);
    let oversized = ProposalPage {
        repository: repository(),
        operation_id: request.operation_id.clone(),
        offset: 0,
        records: vec![records()[0].clone(); MAX_PAGE_RECORDS + 1],
    };
    assert!(matches!(
        state.push(&oversized, now()),
        Err(ProtocolError::Limit(_))
    ));
    let mut wire = serde_json::to_value(&request).unwrap();
    wire["extension"] = true.into();
    assert!(serde_json::from_value::<SubmitRequest>(wire).is_err());
    let caps = Capabilities { encrypted: false,
        protocol_version: PROTOCOL_VERSION,
        repository: repository(),
        latest: Cursor::genesis(repository(), false).unwrap(),
        policy: ServerPolicy::default(),
    };
    caps.validate().unwrap();
}

#[test]
fn maximum_format_event_fits_both_transfers_without_relaxing_page_bounds() {
    let initial = request();
    let mut record = records().remove(0);
    if let ProposalRecord::Item(event) = &mut record {
        event.set_federation_stream_id = Some(String::new());
        let overhead = canonical_event_proposal_bytes(event).unwrap().len();
        event.set_federation_stream_id =
            Some("X".repeat(synck_vcs::limits::MAX_EVENT_PAYLOAD_BYTES - overhead));
        assert_eq!(
            canonical_event_proposal_bytes(event).unwrap().len(),
            synck_vcs::limits::MAX_EVENT_PAYLOAD_BYTES
        );
    }
    let state = upload(&initial, &[record.clone()], 1);
    let request = state.request(initial.metadata.clone()).unwrap();
    state.finish(&request, now()).unwrap();
    let published = published(&request, &[record.clone()]);
    let mut history = HistoryVerifier::new(Cursor::genesis(repository(), false).unwrap()).unwrap();
    assert!(
        history
            .push(&page(
                &published,
                history.position().clone(),
                published.events.clone(),
                false
            ))
            .unwrap()
            .is_some()
    );
    let mut history = HistoryVerifier::new(Cursor::genesis(repository(), false).unwrap()).unwrap();
    let oversized = HistoryPage {
        after: history.position().clone(),
        fragments: vec![
            HistoryFragment {
                header: PublishedHeader {
                    commit: published.commit.clone(),
                    checkpoint: published.checkpoint.clone()
                },
                events: published.events.clone(),
            };
            3
        ],
        has_more: false,
    };
    assert!(matches!(
        history.push(&oversized),
        Err(ProtocolError::Limit("page bytes"))
    ));
    assert_eq!(history.position().completed, Cursor::genesis(repository(), false).unwrap());
    let mut state = verifier(&initial);
    assert!(matches!(
        state.push(
            &ProposalPage {
                repository: repository(),
                operation_id: initial.operation_id,
                offset: 0,
                records: vec![record; 9]
            },
            now()
        ),
        Err(ProtocolError::Limit("page bytes"))
    ));
    assert_eq!(state.next_offset(), 0);
}

#[test]
fn history_continuation_keeps_partial_item_and_ref_events_private() {
    let initial = request();
    let mut records = records();
    records.push(ProposalRecord::Ref(
        serde_json::from_value(serde_json::json!({
            "owner_item_id":"019a2900-0000-7000-8000-000000000003",
            "event_type":"FILE_VERSION::CREATE","base_ref_seq":0
        }))
        .unwrap(),
    ));
    let request = upload(&initial, &records, 1)
        .request(initial.metadata)
        .unwrap();
    let published = published(&request, &records);
    let genesis = Cursor::genesis(repository(), false).unwrap();
    let mut state = HistoryVerifier::new(genesis.clone()).unwrap();
    let mut first = page(
        &published,
        state.position().clone(),
        published.events[..1].to_vec(),
        false,
    );
    assert!(state.push(&first).is_err());
    assert_eq!(state.position().completed, genesis);
    assert!(state.position().pending.is_none());
    first.has_more = true;
    assert!(state.push(&first).unwrap().is_none());
    assert_eq!(state.position().completed, genesis);
    let pending = state.position().clone();
    let last = page(
        &published,
        pending.clone(),
        published.events[1..].to_vec(),
        false,
    );
    let mut changed = last.clone();
    changed.fragments[0].header.commit.message = Some("different".into());
    assert!(state.push(&changed).is_err());
    assert_eq!(state.position(), &pending);
    changed = last.clone();
    if let CommitEventRow::Ref(row) = &mut changed.fragments[0].events[0] {
        row.payload.push(b' ');
    }
    assert!(state.push(&changed).is_err());
    assert_eq!(state.position(), &pending);
    let completed = state.push(&last).unwrap().unwrap();
    assert_eq!(completed.repo_seq, 1);
    assert_eq!(completed.checkpoint, published.checkpoint);
    assert!(state.position().pending.is_none());
}

fn history_vector(name: &str) -> (serde_json::Value, Vec<PublishedCommit>) {
    let vector: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/sync-history.json")).unwrap();
    assert_eq!(
        serde_json::to_value(repository()).unwrap(),
        vector["repository"]
    );
    assert_eq!(
        now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        vector["accepted_at"]
    );
    let case = vector[name].clone();
    let mut previous = genesis_value();
    let mut ordinal = 0;
    let commits = case["event_counts"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, count)| {
            let mut initial = request();
            initial.metadata = serde_json::from_value(vector["metadata"].clone()).unwrap();
            initial.metadata.message = Some(format!("commit-{index}"));
            let records: Vec<_> = (0..count.as_u64().unwrap())
                .map(|_| {
                    let event = serde_json::from_value(serde_json::json!({
                        "item_id": format!("019a2900-0000-7000-8000-{:012x}", ordinal + 100),
                        "event_type": "FOLDER::CREATE",
                        "create_item": {"item_type":"FOLDER", "name":format!("folder-{ordinal:05}")}
                    }))
                    .unwrap();
                    ordinal += 1;
                    ProposalRecord::Item(event)
                })
                .collect();
            let request = upload(&initial, &records, MAX_PAGE_RECORDS)
                .request(initial.metadata)
                .unwrap();
            let mut commit = published(&request, &records);
            let sequence = index as i64 + 1;
            commit.commit.repo_seq = Some(sequence);
            for event in &mut commit.events {
                match event {
                    CommitEventRow::Item(row) => row.repo_seq = Some(sequence),
                    CommitEventRow::Ref(_) => unreachable!(),
                }
            }
            previous = synck_vcs::checkpoint::digest(
                &previous,
                sequence as u64,
                &commit.commit.commit_id,
                now(),
            )
            .unwrap();
            commit.checkpoint = digest(previous);
            let expected = &case["commits"][index];
            assert_eq!(commit.commit.commit_id.as_str(), expected["commit_id"]);
            assert_eq!(
                commit.commit.events_digest.as_str(),
                expected["events_digest"]
            );
            assert_eq!(commit.checkpoint.as_str(), expected["checkpoint"]);
            commit
        })
        .collect();
    (case, commits)
}

fn vector_history_page(
    spec: &serde_json::Value,
    commits: &[PublishedCommit],
    after: HistoryPosition,
    has_more: bool,
) -> HistoryPage {
    HistoryPage {
        after,
        fragments: spec["fragments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|range| {
                let commit = &commits[range["commit_index"].as_u64().unwrap() as usize];
                let start = range["start_event"].as_u64().unwrap() as usize;
                let end = range["end_event"].as_u64().unwrap() as usize;
                HistoryFragment {
                    header: PublishedHeader {
                        commit: commit.commit.clone(),
                        checkpoint: commit.checkpoint.clone(),
                    },
                    events: commit.events[start..end].to_vec(),
                }
            })
            .collect(),
        has_more,
    }
}

#[test]
fn many_small_commits_share_one_page_and_roll_back_on_late_corruption() {
    let (vector, commits) = history_vector("many_small_commits");
    let mut state = HistoryVerifier::new(Cursor::genesis(repository(), false).unwrap()).unwrap();
    let original = state.position().clone();
    let page = vector_history_page(&vector["pages"][0], &commits, original.clone(), false);
    assert_eq!(page.fragments.len(), 128);
    assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_PAGE_BYTES);
    let mut bad = page.clone();
    bad.fragments.last_mut().unwrap().header.checkpoint = digest([9; 32]);
    assert!(state.push(&bad).is_err());
    assert_eq!(state.position(), &original);
    // Limits count all fragments, not just the largest commit in the page.
    bad = page.clone();
    bad.fragments
        .resize(MAX_PAGE_RECORDS + 1, page.fragments[0].clone());
    assert!(matches!(
        state.push(&bad),
        Err(ProtocolError::Limit("history page events"))
    ));
    assert_eq!(state.position(), &original);
    bad = page.clone();
    bad.fragments[1].events.clear();
    assert!(state.push(&bad).is_err());
    bad = page.clone();
    bad.fragments.swap(0, 1);
    assert!(state.push(&bad).is_err());
    assert_eq!(state.position(), &original);
    let completed = state.push(&page).unwrap().unwrap();
    assert_eq!(completed.repo_seq, 128);
    assert_eq!(completed.checkpoint, commits.last().unwrap().checkpoint);
    assert!(state.position().pending.is_none());
}

#[test]
fn history_page_can_start_and_end_mid_commit_with_complete_commits_between() {
    let (vector, commits) = history_vector("starts_and_ends_mid_commit");
    let mut state = HistoryVerifier::new(Cursor::genesis(repository(), false).unwrap()).unwrap();
    let pages = vector["pages"].as_array().unwrap();
    for (index, spec) in pages.iter().enumerate() {
        let page = vector_history_page(
            spec,
            &commits,
            state.position().clone(),
            index + 1 < pages.len(),
        );
        if index == 1 {
            let pending = state.position().clone();
            for variant in 0..4 {
                let mut bad = page.clone();
                match variant {
                    0 => bad.has_more = false,
                    1 => bad.fragments[1].header.commit.message = Some("changed".into()),
                    2 => {
                        bad.fragments[2].events.pop();
                    }
                    3 => bad.fragments[0].header.commit.created_at += chrono::Duration::seconds(1),
                    _ => unreachable!(),
                }
                assert!(state.push(&bad).is_err());
                assert_eq!(state.position(), &pending);
            }
        }
        let completed = state.push(&page).unwrap();
        let sequence = spec["completed_repo_seq"].as_u64().unwrap();
        assert_eq!(state.position().completed.repo_seq, sequence);
        if sequence == 0 {
            assert!(completed.is_none());
        } else {
            let completed = completed.unwrap();
            assert_eq!(completed.repo_seq, sequence);
            assert_eq!(
                completed.checkpoint,
                commits[sequence as usize - 1].checkpoint
            );
        }
        assert_eq!(
            state
                .position()
                .pending
                .as_ref()
                .map(|p| p.next_event_index),
            spec["pending_event_index"].as_u64()
        );
        if index == 1 {
            // Installing through commit 3 leaves only commit 4's prefix private.
            // A restart replays that prefix from the last completed cursor.
            let mut resumed = HistoryVerifier::new(state.position().completed.clone()).unwrap();
            let prefix = HistoryPage {
                after: resumed.position().clone(),
                fragments: vec![page.fragments.last().unwrap().clone()],
                has_more: true,
            };
            assert!(resumed.push(&prefix).unwrap().is_none());
            assert_eq!(resumed.position(), state.position());
            state = resumed;
        }
    }
}

#[test]
fn history_rejects_a_sequence_gap_even_with_a_matching_checkpoint() {
    let (vector, commits) = history_vector("many_small_commits");
    let mut state = HistoryVerifier::new(Cursor::genesis(repository(), false).unwrap()).unwrap();
    let original = state.position().clone();
    let mut page = vector_history_page(&vector["pages"][0], &commits, original.clone(), false);
    page.fragments.truncate(2);
    let previous = sha256_digest_to_digest_bytes(&page.fragments[0].header.checkpoint);
    let fragment = &mut page.fragments[1];
    fragment.header.commit.repo_seq = Some(3);
    if let CommitEventRow::Item(row) = &mut fragment.events[0] {
        row.repo_seq = Some(3);
    }
    fragment.header.checkpoint = digest(
        synck_vcs::checkpoint::digest(
            &previous,
            3,
            &fragment.header.commit.commit_id,
            fragment.header.commit.created_at,
        )
        .unwrap(),
    );
    assert!(state.push(&page).is_err());
    assert_eq!(state.position(), &original);
}
