use chrono::DateTime;
use serde::Deserialize;
use synck_vcs::{
    checkpoint,
    types::{CommitId, RepoId},
};

#[derive(Deserialize)]
struct Vectors {
    genesis: Vec<Genesis>,
    steps: Vec<Vector>,
}

#[derive(Deserialize)]
struct Genesis {
    repo_id: String,
    generation: String,
    encrypted: bool,
    value: String,
}

#[derive(Deserialize)]
struct Vector {
    valid: bool,
    previous: String,
    repo_seq: u64,
    commit_id: String,
    accepted_seconds: i64,
    accepted_nanos: u32,
    checkpoint: String,
}

#[test]
fn checkpoint_chain_matches_independent_vectors() {
    let vectors: Vectors =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/checkpoints.json")).unwrap();
    for vector in &vectors.genesis {
        let repo_id = RepoId::parse(&vector.repo_id).unwrap();
        let value = checkpoint::genesis(&repo_id, &vector.generation, vector.encrypted).unwrap();
        assert_eq!(hex::encode(value), vector.value);
    }
    let first = &vectors.genesis[0];
    let genesis = checkpoint::genesis(
        &RepoId::parse(&first.repo_id).unwrap(),
        &first.generation,
        first.encrypted,
    )
    .unwrap();
    let mut previous = genesis;
    let mut previous_sequence = None;
    for vector in vectors.steps {
        assert_eq!(hex::encode(previous), vector.previous);
        let sequence =
            synck_vcs::bundle::validate_import_repo_seq(previous_sequence, vector.repo_seq as i64);
        if !vector.valid {
            assert!(sequence.is_err());
            continue;
        }
        previous_sequence = Some(sequence.unwrap());
        let commit = CommitId::parse(vector.commit_id).unwrap();
        let time =
            DateTime::from_timestamp(vector.accepted_seconds, vector.accepted_nanos).unwrap();
        let digest = checkpoint::digest(&previous, vector.repo_seq, &commit, time).unwrap();
        assert_eq!(hex::encode(digest), vector.checkpoint);
        assert_ne!(
            digest,
            checkpoint::digest(&previous, vector.repo_seq + 1, &commit, time).unwrap()
        );
        let mut wrong = previous;
        wrong[0] ^= 1;
        assert_ne!(
            digest,
            checkpoint::digest(&wrong, vector.repo_seq, &commit, time).unwrap()
        );
        previous = digest;
    }
}
