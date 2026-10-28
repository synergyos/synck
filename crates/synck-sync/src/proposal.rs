use crate::{
    CommitMetadata, MAX_PAGE_RECORDS, ProposalPage, ProposalRecord, ProtocolError,
    RepositoryIdentity, SubmitRequest, check_page_size, digest_id, invalid,
};
use sha2::{Digest, Sha256};
use synck_vcs::types::{
    CommitId, ItemId, Sha256Digest, Timestamp, canonical_commit_bytes_for_hash,
    canonical_event_proposal_bytes, canonical_json_bytes, canonical_ref_event_proposal_bytes,
    compute_commit_id,
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
    let commit_id = compute_commit_id(&canonical_commit_bytes_for_hash(
        &metadata.author_id,
        &metadata.committer_id,
        metadata.occurred_at,
        metadata.message.as_deref(),
        metadata.import_job_id.as_deref(),
        request.event_count as usize,
        &request.events_digest,
    )?);
    let value = serde_json::to_value(request).map_err(|e| invalid(e.to_string()))?;
    let request_digest = digest_id(Sha256::digest(canonical_json_bytes(&value)?).into());
    Ok(ValidatedProposal {
        request_digest,
        commit_id,
        events_digest: request.events_digest.clone(),
    })
}

/// Constant-sized streaming summary (apart from one previous guard ID). Pages
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
    events_hasher: Sha256,
    guards_hasher: Sha256,
    refs_started: bool,
    previous_guard: Option<ItemId>,
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
            events_hasher: Sha256::new(),
            guards_hasher: Sha256::new(),
            refs_started: false,
            previous_guard: None,
        })
    }
    pub fn next_offset(&self) -> u64 {
        self.offset
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
        check_page_size(page)?;
        let mut next = self.clone();
        for record in &page.records {
            match record {
                ProposalRecord::Item(event) => {
                    if next.refs_started || next.previous_guard.is_some() {
                        return Err(invalid("item follows ref or guard"));
                    }
                    if event.item_id.is_none()
                        || event.event_type.trim().is_empty()
                        || event.base_seq.is_some_and(|seq| seq < 1)
                        || (event.create_item.is_some() && event.base_seq.is_some())
                    {
                        return Err(invalid("invalid item proposal shape"));
                    }
                    next.event(0, canonical_event_proposal_bytes(event)?)?;
                }
                ProposalRecord::Ref(event) => {
                    if next.previous_guard.is_some() {
                        return Err(invalid("ref follows guard"));
                    }
                    if event.event_type.trim().is_empty() || event.base_ref_seq < 0 {
                        return Err(invalid("invalid ref proposal shape"));
                    }
                    next.refs_started = true;
                    next.event(1, canonical_ref_event_proposal_bytes(event)?)?;
                }
                ProposalRecord::Guard(guard) => {
                    if guard.head_seq < 1
                        || next
                            .previous_guard
                            .as_ref()
                            .is_some_and(|id| id >= &guard.item_id)
                    {
                        return Err(invalid("head guards must be unique, sorted and positive"));
                    }
                    let value = serde_json::to_value(guard).map_err(|e| invalid(e.to_string()))?;
                    next.guards_hasher
                        .update(Sha256::digest(canonical_json_bytes(&value)?));
                    next.guards = next
                        .guards
                        .checked_add(1)
                        .ok_or_else(|| invalid("guard count overflow"))?;
                    next.previous_guard = Some(guard.item_id.clone());
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
    fn event(&mut self, family: u8, payload: Vec<u8>) -> Result<(), ProtocolError> {
        self.events = self
            .events
            .checked_add(1)
            .filter(|n| *n <= i32::MAX as u64)
            .ok_or_else(|| invalid("event count does not fit the canonical representation"))?;
        self.events_hasher.update([family]);
        self.events_hasher.update(Sha256::digest(payload));
        Ok(())
    }
    /// Build the final request from the exact uploaded stream. Guards bind the
    /// request digest but do not alter canonical commit identity.
    pub fn request(&self, metadata: CommitMetadata) -> Result<SubmitRequest, ProtocolError> {
        let request = SubmitRequest {
            repository: self.repository.clone(),
            operation_id: self.operation_id.clone(),
            metadata,
            event_count: self.events,
            events_digest: digest_id(self.events_hasher.clone().finalize().into()),
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
