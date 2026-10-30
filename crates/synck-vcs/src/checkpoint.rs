//! Repository publication fingerprints. Commit acceptance is independent of them.
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::errors::CoreError;
use crate::types::{CommitId, RepoId, commit_id_to_digest_bytes};

/// The repository format hashed into the genesis value.
pub const REPOSITORY_FORMAT: u32 = 7;

/// The predecessor of the first checkpoint, not a stored checkpoint itself.
/// It names one history of one repository, so no chain validates anywhere else.
pub fn genesis(
    repo_id: &RepoId,
    generation: &str,
    encrypted: bool,
) -> Result<[u8; 32], CoreError> {
    let raw = |value: &str| {
        uuid::Uuid::parse_str(value)
            .map(uuid::Uuid::into_bytes)
            .map_err(|_| CoreError::InvalidProposal("invalid genesis identity".into()))
    };
    let mut hash = Sha256::new();
    hash.update(b"synck-genesis-v1");
    hash.update(raw(repo_id.as_str())?);
    hash.update(raw(generation)?);
    hash.update(REPOSITORY_FORMAT.to_be_bytes());
    hash.update([u8::from(encrypted)]);
    Ok(hash.finalize().into())
}

/// Hash a published position using fixed-width, big-endian framing.
///
/// Sequence numbers start at one and must be consecutive. The caller owns
/// those ordering checks and the atomic publication of position and checkpoint.
pub fn digest(
    previous: &[u8; 32],
    repo_seq: u64,
    commit_id: &CommitId,
    accepted_at: DateTime<Utc>,
) -> Result<[u8; 32], crate::errors::CoreError> {
    if repo_seq == 0 || accepted_at.timestamp_subsec_nanos() >= 1_000_000_000 {
        return Err(crate::errors::CoreError::InvalidProposal(
            "invalid checkpoint position or timestamp".into(),
        ));
    }
    let mut hash = Sha256::new();
    hash.update(b"synck-checkpoint-v1");
    hash.update(previous);
    hash.update(repo_seq.to_be_bytes());
    hash.update(commit_id_to_digest_bytes(commit_id));
    hash.update(accepted_at.timestamp().to_be_bytes());
    hash.update(accepted_at.timestamp_subsec_nanos().to_be_bytes());
    Ok(hash.finalize().into())
}
