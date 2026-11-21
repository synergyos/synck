use crate::{
    Cursor, HistoryPage, HistoryPosition, MAX_PAGE_RECORDS, PendingCommit, ProtocolError,
    PublishedHeader, digest_id, invalid,
};
use synck_vcs::types::{
    CommitEventRow, CommitRow, EventsDigest, RepoId, canonical_commit_bytes_for_hash,
    compute_commit_id, decode_event_proposal, decode_ref_event_proposal,
    sha256_digest_to_digest_bytes,
};

/// The running event summary of one commit.
#[derive(Clone, Default)]
struct Events {
    digest: EventsDigest,
    next: usize,
    refs_started: bool,
}
impl Events {
    fn push(
        &mut self,
        event: &CommitEventRow,
        commit: &CommitRow,
        repo_id: &RepoId,
    ) -> Result<(), ProtocolError> {
        if event.commit_id() != &commit.commit_id
            || event.repo_seq() != commit.repo_seq
            || usize::try_from(event.commit_event_idx()).ok() != Some(self.next)
        {
            return Err(invalid("event commit, sequence or index mismatch"));
        }
        // Decoding is verification: an envelope that decodes is the one
        // encoding of its event.
        let payload = match event {
            CommitEventRow::Item(row) => {
                if self.refs_started
                    || &row.repo_id != repo_id
                    || row.created_at != commit.created_at
                    || row.item_seq < 1
                {
                    return Err(invalid("invalid item event placement"));
                }
                let proposal = decode_event_proposal(&row.payload)?;
                if proposal.item_id.as_ref() != Some(&row.item_id)
                    || proposal.event_type != row.event_type
                {
                    return Err(invalid("item event does not match its envelope"));
                }
                &row.payload
            }
            CommitEventRow::Ref(row) => {
                self.refs_started = true;
                if &row.repo_id != repo_id || row.created_at != commit.created_at || row.ref_seq < 1
                {
                    return Err(invalid("invalid ref event placement"));
                }
                let proposal = decode_ref_event_proposal(&row.payload)?;
                if proposal.owner_item_id != row.owner_item_id
                    || proposal.event_type != row.event_type
                {
                    return Err(invalid("ref event does not match its envelope"));
                }
                &row.payload
            }
        };
        self.digest.push(payload);
        self.next += 1;
        if self.next > commit.event_count as usize {
            return Err(invalid("too many commit events"));
        }
        Ok(())
    }
    /// The commit ID is the hash of the header its fields and events make.
    fn finish(&self, commit: &CommitRow) -> Result<(), ProtocolError> {
        if self.next != commit.event_count as usize {
            return Err(invalid("commit event count mismatch"));
        }
        let header = canonical_commit_bytes_for_hash(
            &commit.author_id,
            &commit.committer_id,
            commit.occurred_at,
            commit.message.as_deref(),
            self.next,
            &self.digest.clone().finish(commit.repo_seq),
        )?;
        if compute_commit_id(&header) != commit.commit_id {
            return Err(invalid("commit ID mismatch"));
        }
        Ok(())
    }
}

/// A header extends `after` by one, and its checkpoint follows from it.
fn header(header: &PublishedHeader, after: &Cursor) -> Result<(), ProtocolError> {
    let commit = &header.commit;
    if commit.repo_id != after.repository.repo_id {
        return Err(ProtocolError::Generation);
    }
    let sequence = commit
        .repo_seq
        .filter(|seq| *seq > 0)
        .ok_or_else(|| invalid("published commit requires positive repo_seq"))?
        as u64;
    if after.repo_seq.checked_add(1) != Some(sequence) || commit.event_count < 0 {
        return Err(invalid("invalid published commit position or count"));
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
        // Every fragment counts for at least one, so this also bounds header count.
        let mut remaining = MAX_PAGE_RECORDS;
        for fragment in &page.fragments {
            if fragment.events.is_empty() && fragment.header.commit.event_count != 0 {
                return Err(invalid("empty commit fragment"));
            }
            remaining = remaining
                .checked_sub(fragment.events.len().max(1))
                .ok_or(ProtocolError::Limit("history page events"))?;
        }
        if page.fragments.is_empty() {
            if page.has_more || self.pending.is_some() {
                return Err(invalid("invalid empty history page"));
            }
            return Ok(None);
        }
        let repo_id = self.position.completed.repository.repo_id.clone();
        let mut next = self.clone();
        let mut completed_any = false;
        for (index, fragment) in page.fragments.iter().enumerate() {
            let published = &fragment.header;
            match &next.pending {
                Some(previous) if previous != published => {
                    return Err(invalid("commit header changed during continuation"));
                }
                Some(_) => {}
                None => header(published, &next.position.completed)?,
            }
            for event in &fragment.events {
                next.events.push(event, &published.commit, &repo_id)?;
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
