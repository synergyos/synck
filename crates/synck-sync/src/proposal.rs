use crate::{
    CommitMetadata, MAX_PAGE_RECORDS, ProposalPage, ProposalRecord, ProtocolError,
    RepositoryIdentity, SubmitRequest, Wire, digest_id, invalid, wire,
};
use sha2::{Digest, Sha256};
use synck_vcs::types::{
    CommitId, EventsDigest, ItemId, Sha256Digest, Timestamp, canonical_event_proposal_bytes,
    canonical_ref_event_proposal_bytes, compute_commit_id,
};

#[derive(Debug)]
pub struct ValidatedProposal {
    pub request_digest: Sha256Digest,
    pub commit_id: CommitId,
    pub events_digest: Sha256Digest,
}

/// Validate the small final request and compute its identities. The authority
/// must also finish its staged ProposalVerifier and run Core acceptance checks.
pub fn validate_submit(request: &SubmitRequest) -> Result<ValidatedProposal, ProtocolError> {
    request.repository.validate()?;
    if request
        .repository_head
        .is_some_and(|head| head > i64::MAX as u64)
    {
        return Err(invalid(
            "repository head does not fit the canonical representation",
        ));
    }
    let metadata = &request.metadata;
    synck_vcs::limits::validate_commit_metadata(
        &metadata.author_id,
        &metadata.committer_id,
        metadata.message.as_deref(),
        Some(&request.operation_id),
    )?;
    // These are the format's existing signed event-index representation bounds,
    // not a configured admission or transfer limit.
    if request.event_count == 0 || request.event_count > i32::MAX as u64 {
        return Err(invalid(
            "event count does not fit the canonical representation",
        ));
    }
    let commit_id = compute_commit_id(&wire::submit_header(request)?);
    let request_digest = digest_id(Sha256::digest(wire::encode(request)?).into());
    Ok(ValidatedProposal {
        request_digest,
        commit_id,
        events_digest: request.events_digest.clone(),
    })
}

/// Constant-sized streaming summary. Pages
/// belong in durable staging owned by the authority, not in this verifier. After
/// restart replay those pages to reconstruct the summary before resuming.
/// Invalid pages leave the previous summary unchanged.
#[derive(Clone)]
pub struct ProposalVerifier {
    repository: RepositoryIdentity,
    operation_id: String,
    expires_at: Timestamp,
    offset: u64,
    events: u64,
    guards: u64,
    events_hasher: EventsDigest,
    guards_hasher: Sha256,
    refs_started: bool,
    previous_guard: Option<ItemId>,
    previous_subtree: Option<ItemId>,
    subtree: Option<SubtreeDigest>,
}

#[derive(Clone)]
struct SubtreeDigest {
    remaining: u32,
    previous_descendant: Option<ItemId>,
    hasher: Sha256,
}

impl ProposalVerifier {
    pub fn new(
        repository: RepositoryIdentity,
        operation_id: String,
        expires_at: Timestamp,
    ) -> Result<Self, ProtocolError> {
        repository.validate()?;
        synck_vcs::limits::validate_operation_id(&operation_id)?;
        if expires_at.timestamp_subsec_nanos() >= 1_000_000_000 {
            return Err(invalid("invalid upload expiry"));
        }
        Ok(Self {
            repository,
            operation_id,
            expires_at,
            offset: 0,
            events: 0,
            guards: 0,
            events_hasher: EventsDigest::default(),
            guards_hasher: Sha256::new(),
            refs_started: false,
            previous_guard: None,
            previous_subtree: None,
            subtree: None,
        })
    }
    pub fn next_offset(&self) -> u64 {
        self.offset
    }
    pub fn event_count(&self) -> u64 {
        self.events
    }
    fn live(&self, now: Timestamp) -> Result<(), ProtocolError> {
        if now >= self.expires_at {
            return Err(ProtocolError::UploadExpired);
        }
        Ok(())
    }
    pub fn push(&mut self, page: &ProposalPage, now: Timestamp) -> Result<(), ProtocolError> {
        self.live(now)?;
        if page.repository != self.repository {
            return Err(ProtocolError::Generation);
        }
        if page.operation_id != self.operation_id || page.offset != self.offset {
            return Err(invalid("upload operation or offset mismatch"));
        }
        if page.records.is_empty() || page.records.len() > MAX_PAGE_RECORDS {
            return Err(ProtocolError::Limit("proposal page records"));
        }
        let mut next = self.clone();
        let mut descendant_bytes = Vec::new();
        for record in &page.records {
            match record {
                ProposalRecord::Item(event) => {
                    if next.refs_started
                        || next.previous_guard.is_some()
                        || next.previous_subtree.is_some()
                    {
                        return Err(invalid("item follows ref or guard"));
                    }
                    next.event(&canonical_event_proposal_bytes(event)?)?;
                }
                ProposalRecord::Ref(event) => {
                    if next.previous_guard.is_some() || next.previous_subtree.is_some() {
                        return Err(invalid("ref follows guard"));
                    }
                    next.refs_started = true;
                    next.event(&canonical_ref_event_proposal_bytes(event)?)?;
                }
                ProposalRecord::Guard(guard) => {
                    if next.previous_subtree.is_some()
                        || guard.head_seq < 1
                        || next
                            .previous_guard
                            .as_ref()
                            .is_some_and(|id| id >= &guard.item_id)
                    {
                        return Err(invalid("head guards must be unique, sorted and positive"));
                    }
                    next.guard(Sha256::digest(wire::encode(record)?))?;
                    next.previous_guard = Some(guard.item_id.clone());
                }
                ProposalRecord::SubtreeGuardStart {
                    root_item_id,
                    require_empty_live_descendants,
                    descendant_count,
                } => {
                    if next.subtree.is_some()
                        || next
                            .previous_subtree
                            .as_ref()
                            .is_some_and(|id| id >= root_item_id)
                    {
                        return Err(invalid(
                            "subtree guards must be unique, sorted and complete",
                        ));
                    }
                    if *require_empty_live_descendants && *descendant_count != 0 {
                        return Err(invalid("empty-live subtree guard cannot list descendants"));
                    }
                    let mut hasher = Sha256::new();
                    hasher.update(wire::encode(record)?);
                    if *descendant_count == 0 {
                        next.guard(hasher.finalize())?;
                    } else {
                        next.subtree = Some(SubtreeDigest {
                            remaining: *descendant_count,
                            previous_descendant: None,
                            hasher,
                        });
                    }
                    next.previous_subtree = Some(root_item_id.clone());
                }
                ProposalRecord::SubtreeGuardDescendant(head) => {
                    let subtree = next
                        .subtree
                        .as_mut()
                        .ok_or_else(|| invalid("descendant without an open subtree guard"))?;
                    if next.previous_subtree.as_ref() == Some(&head.item_id)
                        || subtree
                            .previous_descendant
                            .as_ref()
                            .is_some_and(|id| id >= &head.item_id)
                    {
                        return Err(invalid(
                            "subtree descendants must be unique, sorted and exclude the root",
                        ));
                    }
                    descendant_bytes.clear();
                    record.put(&mut descendant_bytes)?;
                    // The tag marks a transport record. The logical guard hashes
                    // the original untagged UUID and sequence pair.
                    subtree.hasher.update(&descendant_bytes[1..]);
                    subtree.remaining -= 1;
                    if subtree.remaining == 0 {
                        let subtree = next.subtree.take().expect("open subtree guard");
                        next.guard(subtree.hasher.finalize())?;
                    } else {
                        subtree.previous_descendant = Some(head.item_id.clone());
                    }
                }
            }
        }
        next.offset = next
            .offset
            .checked_add(page.records.len() as u64)
            .ok_or_else(|| invalid("upload offset overflow"))?;
        *self = next;
        Ok(())
    }
    fn event(&mut self, envelope: &[u8]) -> Result<(), ProtocolError> {
        self.events = self
            .events
            .checked_add(1)
            .filter(|n| *n <= i32::MAX as u64)
            .ok_or_else(|| invalid("event count exceeds 2^31 - 1"))?;
        self.events_hasher.push(envelope);
        Ok(())
    }
    fn guard(&mut self, digest: sha2::digest::Output<Sha256>) -> Result<(), ProtocolError> {
        self.guards_hasher.update(digest);
        self.guards = self
            .guards
            .checked_add(1)
            .ok_or_else(|| invalid("guard count overflow"))?;
        Ok(())
    }
    /// Build the final request from the exact uploaded stream. Guards bind the
    /// request digest but do not alter canonical commit identity.
    pub fn request(&self, metadata: CommitMetadata) -> Result<SubmitRequest, ProtocolError> {
        if self.subtree.is_some() {
            return Err(invalid("incomplete subtree guard"));
        }
        let request = SubmitRequest {
            repository_head: None,
            repository: self.repository.clone(),
            operation_id: self.operation_id.clone(),
            metadata,
            event_count: self.events,
            events_digest: self.events_hasher.clone().finish(None),
            guard_count: self.guards,
            guards_digest: digest_id(self.guards_hasher.clone().finalize().into()),
        };
        validate_submit(&request)?;
        Ok(request)
    }
    pub fn finish(
        &self,
        request: &SubmitRequest,
        now: Timestamp,
    ) -> Result<ValidatedProposal, ProtocolError> {
        self.live(now)?;
        if request.repository != self.repository {
            return Err(ProtocolError::Generation);
        }
        let expected = self.request(request.metadata.clone())?;
        if request.operation_id != expected.operation_id
            || request.event_count != expected.event_count
            || request.events_digest != expected.events_digest
            || request.guard_count != expected.guard_count
            || request.guards_digest != expected.guards_digest
        {
            return Err(invalid(
                "submission does not match staged event and guard stream",
            ));
        }
        validate_submit(request)
    }
}
