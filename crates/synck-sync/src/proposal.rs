use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};
use synck_vcs::types::{
    CommitId, EventContent, ItemId, ObjectId, Sha256Digest, canonical_commit_bytes_for_hash,
    canonical_json_bytes, compute_commit_id,
};

use crate::{
    MAX_EVENTS, MAX_HEAD_GUARDS, MAX_OPERATION_ID_BYTES, MAX_PROPOSAL_BYTES, ProtocolError,
    SubmitRequest, digest_id, invalid,
};

/// Stable identities for a request, before authority-assigned placement or time.
/// All object IDs include metadata and every content role, including unknown
/// application roles. The caller must prove durability in the authorized scope.
pub struct ValidatedProposal {
    pub request_digest: Sha256Digest,
    pub commit_id: CommitId,
    pub events_digest: Sha256Digest,
    pub required_objects: BTreeSet<ObjectId>,
}

/// Check bounded, deterministic wire shape. Persisted-head checks, structural
/// invariants, authorization and durable receipt storage remain transactional
/// authority responsibilities. Never treat this result as accepted state.
pub fn validate_submit(request: &SubmitRequest) -> Result<ValidatedProposal, ProtocolError> {
    request.repository.validate()?;
    let operation_id = request
        .proposal
        .operation_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid("operation_id is required"))?;
    if operation_id.len() > MAX_OPERATION_ID_BYTES
        || !operation_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    {
        return Err(invalid(
            "operation_id must be 1..128 ASCII letters, digits, '-', '_', '.' or ':'",
        ));
    }
    for identity in [&request.proposal.author_id, &request.proposal.committer_id] {
        if identity.is_empty() || identity.len() > 256 {
            return Err(invalid("author and committer IDs must be 1..256 bytes"));
        }
    }
    if request
        .proposal
        .message
        .as_ref()
        .is_some_and(|message| message.len() > 4096)
    {
        return Err(ProtocolError::Limit("commit message"));
    }
    let event_count = request
        .proposal
        .events
        .len()
        .checked_add(request.ref_events.len())
        .ok_or(ProtocolError::Limit("event count"))?;
    if event_count == 0 || event_count > MAX_EVENTS {
        return Err(ProtocolError::Limit("event count"));
    }
    if request.head_guards.len() > MAX_HEAD_GUARDS {
        return Err(ProtocolError::Limit("head guards"));
    }
    let mut previous_guard = None;
    for guard in &request.head_guards {
        if guard.head_seq < 1 || previous_guard.is_some_and(|previous| previous >= &guard.item_id) {
            return Err(invalid(
                "head guards must be unique, sorted by item ID and positive",
            ));
        }
        previous_guard = Some(&guard.item_id);
    }
    let mut bases: BTreeMap<&ItemId, Option<i64>> = BTreeMap::new();
    let mut ref_bases = BTreeMap::new();
    let mut required_objects = BTreeSet::new();
    for event in &request.proposal.events {
        let item_id = event
            .item_id
            .as_ref()
            .ok_or_else(|| invalid("every event must supply a stable item_id"))?;
        if event.event_type.is_empty() || event.event_type.len() > 256 {
            return Err(invalid("event type must be 1..256 bytes"));
        }
        if event.create_item.is_some() && (event.base_seq.is_some() || bases.contains_key(item_id))
        {
            return Err(invalid(
                "creation must be the first event for an item and have no base",
            ));
        }
        if let Some(base) = event.base_seq {
            if base < 1 {
                return Err(invalid("existing item base must be positive"));
            }
        } else if event.create_item.is_none() && !bases.contains_key(item_id) {
            return Err(invalid("existing item events require base_seq"));
        }
        match bases.insert(item_id, event.base_seq) {
            Some(base) if base != event.base_seq => return Err(invalid("item bases disagree")),
            _ => {}
        }
        if let Some(metadata) = &event.set_metadata {
            required_objects.insert(metadata.id());
        }
        collect_content(&event.set_content, &mut required_objects);
    }
    for event in &request.ref_events {
        if event.event_type.is_empty() || event.event_type.len() > 256 || event.base_ref_seq < 0 {
            return Err(invalid("invalid ref event type or base"));
        }
        match ref_bases.insert(&event.owner_item_id, event.base_ref_seq) {
            Some(base) if base != event.base_ref_seq => return Err(invalid("ref bases disagree")),
            _ => {}
        }
        if let Some(metadata) = &event.set_metadata {
            required_objects.insert(metadata.id());
        }
        collect_content(&event.set_content, &mut required_objects);
    }
    let config = synck_vcs::config::CoreConfig {
        max_events_per_commit: Some(MAX_EVENTS),
        max_event_payload_bytes: Some(crate::MAX_EVENT_BYTES),
        max_commit_payload_bytes: Some(MAX_PROPOSAL_BYTES),
        ..Default::default()
    };
    let payloads =
        synck_vcs::validate_payload_limits(&config, &request.proposal.events, &request.ref_events)?;
    let mut digest = Sha256::new();
    for (index, payload) in payloads.iter().enumerate() {
        let family = if index < request.proposal.events.len() {
            synck_vcs::types::COMMIT_EVENT_FAMILY_ITEM
        } else {
            synck_vcs::types::COMMIT_EVENT_FAMILY_REF
        };
        digest.update([family]);
        digest.update(Sha256::digest(payload));
    }
    let events_digest = digest_id(digest.finalize().into());
    let commit_id = compute_commit_id(&canonical_commit_bytes_for_hash(
        &request.proposal.author_id,
        &request.proposal.committer_id,
        request.proposal.occurred_at,
        request.proposal.message.as_deref(),
        None,
        event_count,
        &events_digest,
    )?);
    let value = serde_json::to_value(request).map_err(|error| invalid(error.to_string()))?;
    let request_bytes = canonical_json_bytes(&value)?;
    if request_bytes.len() > MAX_PROPOSAL_BYTES {
        return Err(ProtocolError::Limit("proposal bytes"));
    }
    let request_digest = digest_id(Sha256::digest(&request_bytes).into());
    Ok(ValidatedProposal {
        request_digest,
        commit_id,
        events_digest,
        required_objects,
    })
}

fn collect_content(content: &Option<EventContent>, objects: &mut BTreeSet<ObjectId>) {
    if let Some(content) = content {
        objects.extend(content.values().cloned());
    }
}
