use serde_json::Value;
use sha2::{Digest, Sha256};
use synck_vcs::types::{
    Sha256Digest, canonical_commit_bytes_for_hash, canonical_stored_commit_bytes_for_hash,
    compute_commit_id,
};

#[test]
fn commit_headers_and_tagged_events_match_independent_vectors() {
    let vectors: Vec<Value> =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/commits.json")).unwrap();
    for vector in vectors {
        let header = &vector["header"];
        let mut stream = Sha256::new();
        for event in vector["events"].as_array().unwrap() {
            let digest = Sha256::digest(event["payload"].as_str().unwrap().as_bytes());
            assert_eq!(format!("sha256:{}", hex::encode(digest)), event["digest"]);
            stream.update([event["family"].as_u64().unwrap() as u8]);
            stream.update(digest);
        }
        let digest =
            Sha256Digest::parse(format!("sha256:{}", hex::encode(stream.finalize()))).unwrap();
        assert_eq!(digest.as_str(), header["events_digest"]);
        let author = header["author_id"].as_str().unwrap().to_owned();
        let committer = header["committer_id"].as_str().unwrap().to_owned();
        let occurred = header["occurred_at"].as_str().map(|s| s.parse().unwrap());
        let bytes = canonical_commit_bytes_for_hash(
            &author,
            &committer,
            occurred,
            header["message"].as_str(),
            header["import_job_id"].as_str(),
            header["event_count"].as_u64().unwrap() as usize,
            &digest,
        )
        .unwrap();
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), vector["canonical"]);
        assert_eq!(compute_commit_id(&bytes).as_str(), vector["commit_id"]);
        for version in [0, 1, 2, 3, 4, 5, 7] {
            assert!(
                canonical_stored_commit_bytes_for_hash(
                    version, &author, &committer, occurred, None, None, 1, &digest
                )
                .is_err()
            );
        }
    }
}
