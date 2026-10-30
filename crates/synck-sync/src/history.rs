use crate::{
    Cursor, HistoryPage, HistoryPosition, MAX_PAGE_RECORDS, PendingCommit, ProtocolError,
    PublishedCommit, PublishedHeader, RepositoryIdentity, check_page_size, digest_id, invalid,
};
use sha2::{Digest, Sha256};
use synck_vcs::types::{
    COMMIT_EVENT_FAMILY_ITEM, COMMIT_EVENT_FAMILY_REF, CommitEventRow, CommitRow, EventProposal,
    RefEventProposal, canonical_event_proposal_bytes, canonical_ref_event_proposal_bytes,
    canonical_stored_commit_bytes_for_hash, compute_commit_id, sha256_digest_to_digest_bytes,
};

#[derive(Clone, Default)]
struct Events {
    digest: Sha256,
    next: usize,
    refs_started: bool,
}
impl Events {
    fn push(
        &mut self,
        event: &CommitEventRow,
        commit: &CommitRow,
        repository: &RepositoryIdentity,
    ) -> Result<(), ProtocolError> {
        let index = self.next;
        if event.commit_id() != &commit.commit_id
            || event.repo_seq() != commit.repo_seq
            || usize::try_from(event.commit_event_idx()).ok() != Some(index)
        {
            return Err(invalid("event commit, sequence or index mismatch"));
        }
        let (family, payload, stored_digest) = match event {
            CommitEventRow::Item(row) => {
                if row.payload.len() > synck_vcs::limits::MAX_EVENT_PAYLOAD_BYTES {
                    return Err(ProtocolError::Limit("event bytes"));
                }
                if self.refs_started
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
                if row.payload.len() > synck_vcs::limits::MAX_EVENT_PAYLOAD_BYTES {
                    return Err(ProtocolError::Limit("event bytes"));
                }
                self.refs_started = true;
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
        self.digest.update([family]);
        self.digest
            .update(sha256_digest_to_digest_bytes(&actual_digest));
        self.next += 1;
        if self.next > commit.event_count as usize {
            return Err(invalid("too many commit events"));
        }
        Ok(())
    }
    fn finish(&self, commit: &CommitRow) -> Result<(), ProtocolError> {
        if self.next != commit.event_count as usize
            || digest_id(self.digest.clone().finalize().into()) != commit.events_digest
        {
            return Err(invalid("commit event count or digest mismatch"));
        }
        Ok(())
    }
}

fn header(header: &PublishedHeader, after: &Cursor) -> Result<(), ProtocolError> {
    after.validate()?;
    let commit = &header.commit;
    if commit.repo_id != after.repository.repo_id {
        return Err(ProtocolError::Generation);
    }
    let sequence = commit
        .repo_seq
        .filter(|seq| *seq > 0)
        .ok_or_else(|| invalid("published commit requires positive repo_seq"))?
        as u64;
    if after.repo_seq.checked_add(1) != Some(sequence) || commit.event_count < 1 {
        return Err(invalid("invalid published commit position or count"));
    }
    let bytes = canonical_stored_commit_bytes_for_hash(
        commit.format_version,
        &commit.author_id,
        &commit.committer_id,
        commit.occurred_at,
        commit.message.as_deref(),
        commit.event_count as usize,
        &commit.events_digest,
    )?;
    if compute_commit_id(&bytes) != commit.commit_id {
        return Err(invalid("commit ID mismatch"));
    }
    let checkpoint = synck_vcs::checkpoint::digest(
        &sha256_digest_to_digest_bytes(&after.checkpoint),
        sequence,
        &commit.commit_id,
        commit.created_at,
    )?;
    if digest_id(checkpoint) != header.checkpoint {
        return Err(ProtocolError::Checkpoint);
    }
    Ok(())
}

/// Verify full event content, without checking continuity from a prior cursor.
/// Admission policy never restricts verification of already accepted history.
pub fn verify_published_commit(
    published: &PublishedCommit,
    repository: &RepositoryIdentity,
) -> Result<(), ProtocolError> {
    repository.validate()?;
    let commit = &published.commit;
    if commit.repo_id != repository.repo_id {
        return Err(ProtocolError::Generation);
    }
    if commit.repo_seq.is_none_or(|n| n < 1) || commit.event_count < 1 {
        return Err(invalid("invalid published commit"));
    }
    let bytes = canonical_stored_commit_bytes_for_hash(
        commit.format_version,
        &commit.author_id,
        &commit.committer_id,
        commit.occurred_at,
        commit.message.as_deref(),
        commit.event_count as usize,
        &commit.events_digest,
    )?;
    if compute_commit_id(&bytes) != commit.commit_id {
        return Err(invalid("commit ID mismatch"));
    }
    let mut events = Events::default();
    for event in &published.events {
        events.push(event, commit, repository)?;
    }
    events.finish(commit)
}

/// Stream one bounded page at a time. Only `Some(cursor)` from push authorizes
/// installation through the last complete commit, after Core semantic replay
/// succeeds. A trailing incomplete fragment remains private. Invalid pages leave
/// all state unchanged. The caller durably stages events and installs complete
/// history/projections/cursor in one transaction. On restart replay the partial
/// staging from its last complete cursor; never deserialize an untrusted hash
/// accumulator as verified state.
#[derive(Clone)]
pub struct HistoryVerifier {
    position: HistoryPosition,
    pending: Option<PublishedHeader>,
    events: Events,
}
impl HistoryVerifier {
    pub fn new(completed: Cursor) -> Result<Self, ProtocolError> {
        completed.validate()?;
        Ok(Self {
            position: HistoryPosition {
                completed,
                pending: None,
            },
            pending: None,
            events: Events::default(),
        })
    }
    pub fn position(&self) -> &HistoryPosition {
        &self.position
    }
    pub fn push(&mut self, page: &HistoryPage) -> Result<Option<Cursor>, ProtocolError> {
        if page.after != self.position {
            return Err(ProtocolError::Checkpoint);
        }
        // Every fragment is nonempty, so this also bounds header count.
        let mut remaining = MAX_PAGE_RECORDS;
        for fragment in &page.fragments {
            if fragment.events.is_empty() {
                return Err(invalid("empty commit fragment"));
            }
            remaining = remaining
                .checked_sub(fragment.events.len())
                .ok_or(ProtocolError::Limit("history page events"))?;
        }
        check_page_size(page)?;
        if page.fragments.is_empty() {
            if page.has_more || self.pending.is_some() {
                return Err(invalid("invalid empty history page"));
            }
            return Ok(None);
        }
        let mut next = self.clone();
        let mut completed_any = false;
        for (index, fragment) in page.fragments.iter().enumerate() {
            let published = &fragment.header;
            if let Some(previous) = &next.pending {
                if serde_json::to_value(previous).map_err(|e| invalid(e.to_string()))?
                    != serde_json::to_value(published).map_err(|e| invalid(e.to_string()))?
                {
                    return Err(invalid("commit header changed during continuation"));
                }
            } else {
                header(published, &next.position.completed)?;
            }
            for event in &fragment.events {
                next.events.push(
                    event,
                    &published.commit,
                    &next.position.completed.repository,
                )?;
            }
            if next.events.next == published.commit.event_count as usize {
                next.events.finish(&published.commit)?;
                next.position.completed.repo_seq =
                    published.commit.repo_seq.expect("checked publication") as u64;
                next.position.completed.checkpoint = published.checkpoint.clone();
                next.position.pending = None;
                next.pending = None;
                next.events = Events::default();
                completed_any = true;
            } else {
                if index + 1 != page.fragments.len() || !page.has_more {
                    return Err(invalid(
                        "incomplete commit must end a page with continuation",
                    ));
                }
                next.pending = Some(published.clone());
                next.position.pending = Some(PendingCommit {
                    commit_id: published.commit.commit_id.clone(),
                    next_event_index: next.events.next as u64,
                });
            }
        }
        let completed = completed_any.then(|| next.position.completed.clone());
        *self = next;
        Ok(completed)
    }
}
