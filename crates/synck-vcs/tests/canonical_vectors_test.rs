use sha2::{Digest, Sha256};
use synck_vcs::metadata::Metadata;
use synck_vcs::types::{
    EventProposal, ObjectId, Sha256Digest, canonical_commit_bytes_for_hash,
    canonical_event_proposal_bytes, compute_commit_id,
};

#[test]
fn external_event_and_commit_vectors() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/events.json")).unwrap();
    for vector in vectors.as_array().unwrap() {
        let mut event: EventProposal = serde_json::from_value(vector["event"].clone()).unwrap();
        let bytes = canonical_event_proposal_bytes(&event).unwrap();
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            vector["canonical_event"]
        );
        let event_hash = Sha256::digest(&bytes);
        assert_eq!(
            format!("sha256:{}", hex::encode(event_hash)),
            vector["event_digest"]
        );
        let digest = Sha256Digest::parse(format!(
            "sha256:{}",
            hex::encode(Sha256::digest([&[0u8][..], event_hash.as_slice()].concat()))
        ))
        .unwrap();
        assert_eq!(digest.as_str(), vector["events_digest"]);
        let header = &vector["commit"];
        let commit_bytes = canonical_commit_bytes_for_hash(
            &header["author_id"].as_str().unwrap().to_owned(),
            &header["committer_id"].as_str().unwrap().to_owned(),
            None,
            header["message"].as_str(),
            None,
            1,
            &digest,
        )
        .unwrap();
        assert_eq!(
            std::str::from_utf8(&commit_bytes).unwrap(),
            vector["canonical_commit"]
        );
        assert_eq!(
            compute_commit_id(&commit_bytes).as_str(),
            vector["commit_id"]
        );
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
