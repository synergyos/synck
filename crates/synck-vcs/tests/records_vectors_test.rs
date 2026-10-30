use std::collections::HashMap;

use sha2::{Digest, Sha256};
use synck_vcs::types::{
    COMMIT_FORMAT_VERSION, ItemId, Sha256Digest, canonical_json_bytes,
    canonical_stored_commit_bytes_for_hash, compute_commit_id, sha256_prefixed_hex_from_digest_bytes,
};
use synck_vcs::verify::{
    SemanticVerifier, VerifyCommit, VerifyCommitEvent, VerifyEvent, VerifyRefEvent,
};

/// Replay each commit of `records.json`: core validity needs no family code.
#[test]
fn external_record_vectors() {
    let vectors: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/records.json")).unwrap();
    for vector in vectors {
        let name = vector["name"].as_str().unwrap();
        let mut verifier = SemanticVerifier::new(false);
        let mut item_seq: HashMap<ItemId, i64> = HashMap::new();
        let mut ref_seq: HashMap<ItemId, i64> = HashMap::new();
        let mut parents: HashMap<ItemId, Option<ItemId>> = HashMap::new();
        for (index, commit) in vector["commits"].as_array().unwrap().iter().enumerate() {
            let mut hasher = Sha256::new();
            let mut position = 0;
            let mut events = Vec::new();
            for event in commit["events"].as_array().unwrap() {
                let item_id = ItemId::parse(event["item_id"].as_str().unwrap()).unwrap();
                let payload = canonical_json_bytes(event).unwrap();
                let digest = Sha256::digest(&payload);
                hasher.update([0]);
                hasher.update(digest);
                let seq = item_seq.entry(item_id.clone()).or_default();
                *seq += 1;
                let previous = parents.get(&item_id).cloned().flatten();
                let before = previous.clone().filter(|_| event.get("set_parent").is_some());
                let after = match (event.get("create_item"), event.get("set_parent")) {
                    (Some(create), _) => create["parent_folder_id"]
                        .as_str()
                        .map(|id| ItemId::parse(id).unwrap()),
                    (None, Some(parent)) => parent.as_str().map(|id| ItemId::parse(id).unwrap()),
                    (None, None) => previous,
                };
                parents.insert(item_id.clone(), after.clone());
                events.push(VerifyEvent {
                    commit_event_idx: position,
                    item_seq: *seq,
                    item_id,
                    event_type: event["event_type"].as_str().unwrap().to_string(),
                    payload,
                    event_payload_digest: digest.to_vec(),
                    parent_folder_id: after,
                    parent_folder_id_before: before,
                });
                position += 1;
            }
            let mut ref_events = Vec::new();
            for event in commit["ref_events"].as_array().into_iter().flatten() {
                let owner = ItemId::parse(event["owner_item_id"].as_str().unwrap()).unwrap();
                let payload = canonical_json_bytes(event).unwrap();
                let digest = Sha256::digest(&payload);
                hasher.update([1]);
                hasher.update(digest);
                let seq = ref_seq.entry(owner.clone()).or_default();
                *seq += 1;
                ref_events.push(VerifyRefEvent {
                    commit_event_idx: position,
                    owner_item_id: owner,
                    ref_seq: *seq,
                    event_type: event["event_type"].as_str().unwrap().to_string(),
                    payload,
                    event_payload_digest: digest.to_vec(),
                });
                position += 1;
            }
            let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
                hasher.finalize().into(),
            ))
            .unwrap();
            let author = "author".to_string();
            let message = commit["message"].as_str().map(str::to_string);
            let commit_id = canonical_stored_commit_bytes_for_hash(
                COMMIT_FORMAT_VERSION,
                &author,
                &author,
                None,
                message.as_deref(),
                position as usize,
                &events_digest,
            )
            .ok()
            .map(|bytes| compute_commit_id(&bytes));
            let header = VerifyCommit {
                commit_id,
                repo_seq: Some(index as i64 + 1),
                format_version: COMMIT_FORMAT_VERSION,
                author_id: author.clone(),
                committer_id: author,
                occurred_at: None,
                message,
                event_count: Some(position),
                events_digest: Some(events_digest),
            };
            let findings = verifier.verify_commit_events(
                &header,
                events
                    .iter()
                    .map(VerifyCommitEvent::Item)
                    .chain(ref_events.iter().map(VerifyCommitEvent::Ref)),
            );
            let valid = commit["valid"].as_bool().unwrap();
            assert_eq!(findings.is_empty(), valid, "{name} commit {index}: {findings:?}");
            if !valid {
                break;
            }
            assert_eq!(
                verifier.state_digest().unwrap(),
                commit["state_digest"].as_str().unwrap(),
                "{name} commit {index}"
            );
        }
    }
}
