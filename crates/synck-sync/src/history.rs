use std::io::Write;

use sha2::{Digest, Sha256};
use synck_vcs::types::{
    COMMIT_EVENT_FAMILY_ITEM, COMMIT_EVENT_FAMILY_REF, CommitEventRow, EventProposal,
    RefEventProposal, canonical_event_proposal_bytes, canonical_ref_event_proposal_bytes,
    canonical_stored_commit_bytes_for_hash, compute_commit_id, sha256_digest_to_digest_bytes,
};

use crate::{
    Cursor, HistoryPage, MAX_EVENTS, MAX_HISTORY_BYTES, MAX_HISTORY_COMMITS, ProtocolError,
    PublishedCommit, RepositoryIdentity, digest_id, invalid,
};

/// Verify canonical event/commit identity and links. The caller must additionally
/// replay events against its previously verified item state before installing a
/// projection. This function deliberately does not trust row parent caches.
pub fn verify_published_commit(
    published: &PublishedCommit,
    repository: &RepositoryIdentity,
) -> Result<(), ProtocolError> {
    repository.validate()?;
    let commit = &published.commit;
    if commit.repo_id != repository.repo_id {
        return Err(ProtocolError::Generation);
    }
    if commit.repo_seq.is_none_or(|sequence| sequence < 1) {
        return Err(invalid("published commit requires positive repo_seq"));
    }
    if published.events.is_empty() || published.events.len() > MAX_EVENTS {
        return Err(ProtocolError::Limit("event count"));
    }
    if usize::try_from(commit.event_count).ok() != Some(published.events.len()) {
        return Err(invalid("commit event count mismatch"));
    }
    let mut digest = Sha256::new();
    let mut refs_started = false;
    for (index, event) in published.events.iter().enumerate() {
        if event.commit_id() != &commit.commit_id
            || event.repo_seq() != commit.repo_seq
            || usize::try_from(event.commit_event_idx()).ok() != Some(index)
        {
            return Err(invalid("event commit, sequence or index mismatch"));
        }
        let (family, payload, stored_digest) = match event {
            CommitEventRow::Item(row) => {
                if row.payload.len() > crate::MAX_EVENT_BYTES {
                    return Err(ProtocolError::Limit("event bytes"));
                }
                if refs_started
                    || row.repo_id != repository.repo_id
                    || row.created_at != commit.created_at
                    || row.item_seq < 1
                {
                    return Err(invalid("invalid item event placement"));
                }
                let proposal: EventProposal = serde_json::from_slice(&row.payload)
                    .map_err(|error| invalid(error.to_string()))?;
                if proposal.item_id.as_ref() != Some(&row.item_id)
                    || proposal.event_type != row.event_type
                    || canonical_event_proposal_bytes(&proposal)? != row.payload
                {
                    return Err(invalid("noncanonical or mismatched item event"));
                }
                (
                    COMMIT_EVENT_FAMILY_ITEM,
                    &row.payload,
                    &row.event_payload_digest,
                )
            }
            CommitEventRow::Ref(row) => {
                if row.payload.len() > crate::MAX_EVENT_BYTES {
                    return Err(ProtocolError::Limit("event bytes"));
                }
                refs_started = true;
                if row.repo_id != repository.repo_id
                    || row.created_at != commit.created_at
                    || row.ref_seq < 1
                {
                    return Err(invalid("invalid ref event placement"));
                }
                let proposal: RefEventProposal = serde_json::from_slice(&row.payload)
                    .map_err(|error| invalid(error.to_string()))?;
                if proposal.owner_item_id != row.owner_item_id
                    || proposal.event_type != row.event_type
                    || canonical_ref_event_proposal_bytes(&proposal)? != row.payload
                {
                    return Err(invalid("noncanonical or mismatched ref event"));
                }
                (
                    COMMIT_EVENT_FAMILY_REF,
                    &row.payload,
                    &row.event_payload_digest,
                )
            }
        };
        let actual_digest = digest_id(Sha256::digest(payload).into());
        if stored_digest != &actual_digest {
            return Err(invalid("event payload digest mismatch"));
        }
        digest.update([family]);
        digest.update(sha256_digest_to_digest_bytes(&actual_digest));
    }
    if digest_id(digest.finalize().into()) != commit.events_digest {
        return Err(invalid("commit event digest mismatch"));
    }
    let commit_bytes = canonical_stored_commit_bytes_for_hash(
        commit.format_version,
        &commit.author_id,
        &commit.committer_id,
        commit.occurred_at,
        commit.message.as_deref(),
        commit.import_job_id.as_deref(),
        published.events.len(),
        &commit.events_digest,
    )?;
    if compute_commit_id(&commit_bytes) != commit.commit_id {
        return Err(invalid("commit ID mismatch"));
    }
    Ok(())
}

/// Check a bounded page from the exact previously durable cursor. No state is
/// changed on failure. Persist events, projections and the returned cursor in
/// one local transaction after semantic replay succeeds.
pub fn verify_history_page(
    page: &HistoryPage,
    expected_after: &Cursor,
) -> Result<Cursor, ProtocolError> {
    expected_after.validate()?;
    page.after.validate()?;
    page.next.validate()?;
    if page.after.repository != expected_after.repository
        || page.next.repository != expected_after.repository
    {
        return Err(ProtocolError::Generation);
    }
    if &page.after != expected_after {
        return Err(ProtocolError::Checkpoint);
    }
    if page.commits.len() > MAX_HISTORY_COMMITS || (page.commits.is_empty() && page.has_more) {
        return Err(ProtocolError::Limit(
            "history commit count or empty continuation",
        ));
    }
    encoded_size(page, MAX_HISTORY_BYTES)?;
    let mut cursor = expected_after.clone();
    for published in &page.commits {
        verify_published_commit(published, &cursor.repository)?;
        let sequence = published.commit.repo_seq.expect("verified publication") as u64;
        if sequence <= cursor.repo_seq {
            return Err(invalid("history positions must increase"));
        }
        let checkpoint = synck_vcs::checkpoint::digest(
            &sha256_digest_to_digest_bytes(&cursor.checkpoint),
            sequence,
            &published.commit.commit_id,
            published.commit.created_at,
        )?;
        if digest_id(checkpoint) != published.checkpoint {
            return Err(ProtocolError::Checkpoint);
        }
        cursor.repo_seq = sequence;
        cursor.checkpoint.clone_from(&published.checkpoint);
    }
    if page.next != cursor {
        return Err(ProtocolError::Checkpoint);
    }
    Ok(cursor)
}

/// Count serialized bytes without allocating another page-sized buffer. HTTP
/// consumers must also cap the response body before deserialization.
fn encoded_size(value: &impl serde::Serialize, limit: usize) -> Result<usize, ProtocolError> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(bytes.len())
                .filter(|length| *length <= self.limit)
                .ok_or_else(|| std::io::Error::other("history page byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| ProtocolError::Limit("history bytes"))?;
    Ok(counter.bytes)
}
