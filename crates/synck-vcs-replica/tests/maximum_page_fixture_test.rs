use std::time::Instant;

use base64::Engine as _;
use chrono::Utc;
use sha2::{Digest, Sha256};
use synck_vcs::config::{
    SYNCKHUB_MAX_AUTHOR_ID_BYTES, SYNCKHUB_MAX_COMMIT_MESSAGE_BYTES,
    SYNCKHUB_MAX_COMMIT_PAYLOAD_BYTES, SYNCKHUB_MAX_EVENTS_PER_COMMIT,
    SYNCKHUB_MAX_OPERATION_ID_BYTES,
};
use synck_vcs::types::{
    COMMIT_FORMAT_VERSION, CommitPk, CreateItem, EventProposal, ItemId, RepoId, Sha256Digest,
    canonical_commit_bytes_for_hash, canonical_event_proposal_bytes, compute_commit_id,
    compute_sha256_digest_bytes, sha256_prefixed_hex_from_digest_bytes,
};
use synck_vcs_replica::{
    MAX_PAGE_UNCOMPRESSED_BYTES, ReplicaCommit, ReplicaCommitGroup, ReplicaEvent, validate_group,
};

#[test]
fn theoretical_maximum_server_commit_fits_one_page() {
    let started = Instant::now();
    let created_at = Utc::now();
    let target_payload_bytes = SYNCKHUB_MAX_COMMIT_PAYLOAD_BYTES / SYNCKHUB_MAX_EVENTS_PER_COMMIT;
    let mut events = Vec::with_capacity(SYNCKHUB_MAX_EVENTS_PER_COMMIT);
    let mut events_hasher = Sha256::new();
    let mut total_payload_bytes = 0usize;

    for index in 0..SYNCKHUB_MAX_EVENTS_PER_COMMIT {
        let item_id = ItemId::new();
        let mut proposal = EventProposal {
            item_id: Some(item_id.clone()),
            event_type: "FILE::CREATE".to_string(),
            set_metadata: Some(serde_json::json!({"pad": ""})),
            set_content: None,
            create_item: Some(CreateItem {
                item_type: "FILE".to_string(),
                parent_folder_id: None,
                name: format!("fixture-{index}.bin"),
            }),
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: None,
        };
        let base = canonical_event_proposal_bytes(&proposal).unwrap();
        let pad = "x".repeat(target_payload_bytes.saturating_sub(base.len()));
        proposal.set_metadata = Some(serde_json::json!({"pad": pad}));
        let payload = canonical_event_proposal_bytes(&proposal).unwrap();
        assert_eq!(payload.len(), target_payload_bytes);
        total_payload_bytes += payload.len();
        let digest = compute_sha256_digest_bytes(&payload);
        events_hasher.update(digest);
        events.push(ReplicaEvent::Item {
            commit_event_idx: i32::try_from(index).unwrap(),
            item_id,
            item_seq: 1,
            event_type: "FILE::CREATE".to_string(),
            payload_base64: base64::engine::general_purpose::STANDARD.encode(payload),
            event_payload_digest: Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
                digest,
            ))
            .unwrap(),
            created_at,
            parent_folder_id: None,
            parent_folder_id_before: None,
        });
    }
    assert!(total_payload_bytes <= SYNCKHUB_MAX_COMMIT_PAYLOAD_BYTES);
    assert!(SYNCKHUB_MAX_COMMIT_PAYLOAD_BYTES - total_payload_bytes < 10_000);

    let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
        events_hasher.finalize().into(),
    ))
    .unwrap();
    let author_id = "a".repeat(SYNCKHUB_MAX_AUTHOR_ID_BYTES);
    let message = "m".repeat(SYNCKHUB_MAX_COMMIT_MESSAGE_BYTES);
    let commit_id = compute_commit_id(
        &canonical_commit_bytes_for_hash(
            &author_id,
            Some(&message),
            None,
            events.len(),
            &events_digest,
        )
        .unwrap(),
    );
    let group = ReplicaCommitGroup {
        commit: ReplicaCommit {
            commit_pk: CommitPk::new(),
            commit_id,
            parent_commit_id: None,
            repo_seq: 1,
            operation_id: Some("o".repeat(SYNCKHUB_MAX_OPERATION_ID_BYTES)),
            format_version: COMMIT_FORMAT_VERSION,
            import_job_id: None,
            author_id: author_id.clone(),
            committer_id: author_id,
            message: Some(message),
            occurred_at: None,
            created_at,
            event_count: i32::try_from(events.len()).unwrap(),
            events_digest,
        },
        events,
    };
    validate_group(&group).unwrap();
    let bytes = serde_json::to_vec(&group).unwrap();
    assert!(bytes.len() < MAX_PAGE_UNCOMPRESSED_BYTES);
    eprintln!(
        "maximum replica fixture: canonical_payload_bytes={total_payload_bytes} encoded_group_bytes={} validate_encode_ms={}",
        bytes.len(),
        started.elapsed().as_millis()
    );

    // Keep the repository type linked into this contract fixture: repository
    // IDs add no variable-length input to the encoded maximum.
    let _ = RepoId::new();
}
