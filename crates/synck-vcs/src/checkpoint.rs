//! Repository publication fingerprints. Commit acceptance is independent of them.
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::types::{CommitId, commit_id_to_digest_bytes};

/// The predecessor of the first checkpoint, not a stored checkpoint itself.
pub const GENESIS: [u8; 32] = [0; 32];

/// Hash a published position using fixed-width, big-endian framing.
///
/// Sequence numbers must be positive and strictly increasing. The caller owns
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
