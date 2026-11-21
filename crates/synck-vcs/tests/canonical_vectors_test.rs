use sha2::{Digest, Sha256};
use synck_vcs::metadata::Metadata;
use synck_vcs::types::{
    EventProposal, EventsDigest, ObjectId, RefEventProposal, canonical_commit_bytes_for_hash,
    canonical_event_proposal_bytes, canonical_ref_event_proposal_bytes, compute_commit_id,
    decode_event_proposal, decode_ref_event_proposal,
};

/// Every rejected envelope is refused by both decoders.
#[test]
fn rejected_event_envelopes() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/events.json")).unwrap();
    for vector in vectors["invalid"].as_array().unwrap() {
        let bytes = hex::decode(vector["envelope_hex"].as_str().unwrap()).unwrap();
        let name = vector["name"].as_str().unwrap();
        assert!(decode_event_proposal(&bytes).is_err(), "{name}");
        assert!(decode_ref_event_proposal(&bytes).is_err(), "{name}");
    }
}

#[test]
fn external_event_and_commit_vectors() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/events.json")).unwrap();
    for vector in vectors["valid"].as_array().unwrap() {
        let name = vector["name"].as_str().unwrap();
        let is_ref = vector["event"].get("owner_item_id").is_some();
        let mut event = None;
        let bytes = if is_ref {
            let proposal: RefEventProposal =
                serde_json::from_value(vector["event"].clone()).unwrap();
            let bytes = canonical_ref_event_proposal_bytes(&proposal).unwrap();
            let decoded = decode_ref_event_proposal(&bytes).unwrap();
            assert_eq!(canonical_ref_event_proposal_bytes(&decoded).unwrap(), bytes);
            bytes
        } else {
            let proposal: EventProposal = serde_json::from_value(vector["event"].clone()).unwrap();
            let bytes = canonical_event_proposal_bytes(&proposal).unwrap();
            let decoded = decode_event_proposal(&bytes).unwrap();
            assert_eq!(canonical_event_proposal_bytes(&decoded).unwrap(), bytes);
            event = Some(proposal);
            bytes
        };
        assert_eq!(hex::encode(&bytes), vector["envelope_hex"], "{name}");
        let event_hash = Sha256::digest(&bytes);
        assert_eq!(
            format!("sha256:{}", hex::encode(event_hash)),
            vector["event_digest"]
        );
        let mut digest = EventsDigest::default();
        digest.push(&bytes);
        let digest = digest.finish(None);
        assert_eq!(digest.as_str(), vector["events_digest"]);
        let header = &vector["commit"];
        let commit_bytes = canonical_commit_bytes_for_hash(
            &header["author_id"].as_str().unwrap().to_owned(),
            &header["committer_id"].as_str().unwrap().to_owned(),
            None,
            header["message"].as_str(),
            1,
            &digest,
        )
        .unwrap();
        assert_eq!(hex::encode(&commit_bytes), vector["header_hex"], "{name}");
        assert_eq!(
            compute_commit_id(&commit_bytes).as_str(),
            vector["commit_id"]
        );
        let Some(mut event) = event else {
            continue;
        };
        if let Some(objects) = vector.get("objects").and_then(|objects| objects.as_array()) {
            for object in objects {
                let id = ObjectId::parse(object["id"].as_str().unwrap()).unwrap();
                let payload = synck_vcs::object::Object::verify(
                    &id,
                    hex::decode(object["payload_hex"].as_str().unwrap()).unwrap(),
                )
                .unwrap();
                event
                    .set_content
                    .as_mut()
                    .unwrap()
                    .resolve(payload)
                    .unwrap();
            }
            assert_eq!(canonical_event_proposal_bytes(&event).unwrap(), bytes);
        }
        if let Some(metadata) = vector.get("metadata") {
            let id = ObjectId::parse(metadata["id"].as_str().unwrap()).unwrap();
            let payload = Metadata::verify(
                &id,
                hex::decode(metadata["payload_hex"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(payload.value(), &metadata["value"]);
            event
                .set_metadata
                .as_mut()
                .unwrap()
                .resolve(payload)
                .unwrap();
            assert_eq!(
                canonical_event_proposal_bytes(&event).unwrap(),
                bytes,
                "hydrating metadata must not change the envelope"
            );
        }
    }
}
