use serde_json::Value;
use synck_vcs::types::{
    EventProposal, EventsDigest, RefEventProposal, Sha256Digest, canonical_commit_bytes_for_hash,
    canonical_event_proposal_bytes, canonical_ref_event_proposal_bytes, compute_commit_id,
    decode_commit_header,
};

#[test]
fn commit_headers_and_events_match_independent_vectors() {
    let vectors: Vec<Value> =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/commits.json")).unwrap();
    for vector in vectors {
        let name = vector["name"].as_str().unwrap();
        let header = &vector["header"];
        let mut digest = EventsDigest::default();
        for event in vector["events"].as_array().unwrap() {
            let bytes = if event["event"].get("owner_item_id").is_some() {
                let proposal: RefEventProposal =
                    serde_json::from_value(event["event"].clone()).unwrap();
                canonical_ref_event_proposal_bytes(&proposal).unwrap()
            } else {
                let proposal: EventProposal =
                    serde_json::from_value(event["event"].clone()).unwrap();
                canonical_event_proposal_bytes(&proposal).unwrap()
            };
            assert_eq!(hex::encode(&bytes), event["envelope_hex"], "{name}");
            digest.push(&bytes);
        }
        let digest = digest.finish(None);
        assert_eq!(digest.as_str(), header["events_digest"], "{name}");
        let occurred = header["occurred_at"].as_str().map(|s| s.parse().unwrap());
        let bytes = canonical_commit_bytes_for_hash(
            &header["author_id"].as_str().unwrap().to_owned(),
            &header["committer_id"].as_str().unwrap().to_owned(),
            occurred,
            header["message"].as_str(),
            header["event_count"].as_u64().unwrap() as usize,
            &Sha256Digest::parse(header["events_digest"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(hex::encode(&bytes), vector["header_hex"], "{name}");
        assert_eq!(
            compute_commit_id(&bytes).as_str(),
            vector["commit_id"],
            "{name}"
        );
        let decoded = decode_commit_header(&bytes).unwrap();
        assert_eq!(decoded.events_digest, digest, "{name}");
    }
}
