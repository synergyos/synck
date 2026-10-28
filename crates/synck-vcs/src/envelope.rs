//! Streaming canonical accepted-commit envelope validation.
//!
//! The validator retains only commit-wide counters and digests. The caller
//! owns per-item state, so a projector can keep it in disk-backed staging
//! instead of buffering an arbitrarily large commit in memory.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::{
    COMMIT_EVENT_FAMILY_ITEM, COMMIT_EVENT_FAMILY_REF, COMMIT_FORMAT_VERSION, CommitId,
    EventProposal, ItemId, RefEventProposal, SequencingMode, Sha256Digest,
    canonical_event_proposal_bytes, canonical_ref_event_proposal_bytes,
    canonical_stored_commit_bytes_for_hash, compute_commit_id, compute_sha256_digest_bytes,
    sha256_prefixed_hex_from_digest_bytes,
};

#[derive(Clone, Debug)]
pub struct VerifyCommit {
    pub commit_id: Option<CommitId>,
    pub repo_seq: Option<i64>,
    pub format_version: i32,
    pub import_job_id: Option<String>,
    pub author_id: String,
    pub committer_id: String,
    pub occurred_at: Option<crate::types::Timestamp>,
    pub message: Option<String>,
    pub event_count: Option<i32>,
    pub events_digest: Option<Sha256Digest>,
}

#[derive(Debug)]
pub struct VerifyEvent {
    pub commit_event_idx: i32,
    pub item_id: ItemId,
    pub item_seq: i64,
    pub event_type: String,
    pub payload: Vec<u8>,
    pub event_payload_digest: Vec<u8>,
    pub parent_folder_id: Option<ItemId>,
    pub parent_folder_id_before: Option<ItemId>,
}

#[derive(Debug)]
pub struct VerifyRefEvent {
    pub commit_event_idx: i32,
    pub owner_item_id: ItemId,
    pub ref_seq: i64,
    pub event_type: String,
    pub payload: Vec<u8>,
    pub event_payload_digest: Vec<u8>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EnvelopeRefState {
    pub head_seq: i64,
    pub commit_base_seq: Option<i64>,
    pub commit_started: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EnvelopeItemState {
    pub head_seq: Option<i64>,
    pub tombstoned: bool,
    pub commit_base_seq: Option<i64>,
    pub commit_started: bool,
    pub federation_changed: bool,
    pub sequencing_mode: Option<SequencingMode>,
    pub federation_stream_id: Option<String>,
    pub created_in_commit: bool,
    pub bootstrap_rebind_seen: bool,
}

impl EnvelopeItemState {
    pub fn from_head(
        head_seq: Option<i64>,
        tombstoned: bool,
        sequencing_mode: Option<SequencingMode>,
        federation_stream_id: Option<String>,
    ) -> Self {
        Self {
            head_seq,
            tombstoned,
            commit_base_seq: None,
            commit_started: false,
            federation_changed: false,
            sequencing_mode,
            federation_stream_id,
            created_in_commit: false,
            bootstrap_rebind_seen: false,
        }
    }
}

pub struct CommitEnvelopeValidator {
    commit: VerifyCommit,
    label: String,
    next_event_index: usize,
    refs_started: bool,
    events_hasher: Sha256,
    findings: Vec<String>,
}

impl CommitEnvelopeValidator {
    pub fn begin(commit: &VerifyCommit, previous_repo_seq: &mut Option<i64>) -> Self {
        let mut findings = Vec::new();
        let label = commit
            .repo_seq
            .map(|seq| format!("repo sequence {seq}"))
            .unwrap_or_else(|| "unsequenced commit".to_string());
        match commit.repo_seq {
            Some(seq) => {
                match *previous_repo_seq {
                    None if seq != 1 => {
                        findings.push(format!("{label}: first repository sequence must be 1"));
                    }
                    Some(previous) if seq <= previous => findings.push(format!(
                        "{label}: repository sequence does not increase after {previous}"
                    )),
                    _ => {}
                }
                *previous_repo_seq = Some(seq);
            }
            None => findings.push(format!("{label}: repo_seq is absent")),
        }
        if !matches!(commit.format_version, COMMIT_FORMAT_VERSION) {
            findings.push(format!(
                "{label}: unsupported commit format {}",
                commit.format_version
            ));
        }
        if commit.commit_id.is_none() {
            findings.push(format!("{label}: commit ID is absent"));
        }
        if commit.events_digest.is_none() {
            findings.push(format!("{label}: events digest is absent"));
        }
        Self {
            commit: commit.clone(),
            label,
            next_event_index: 0,
            refs_started: false,
            events_hasher: Sha256::new(),
            findings,
        }
    }

    pub fn validate_event(
        &mut self,
        event: &VerifyEvent,
        item: &mut EnvelopeItemState,
    ) -> Option<EventProposal> {
        let context = format!("{} event {}", self.label, event.commit_event_idx);
        if self.refs_started {
            self.findings
                .push(format!("{context}: item event follows a ref event"));
        }
        if usize::try_from(event.commit_event_idx).ok() != Some(self.next_event_index) {
            self.findings
                .push(format!("{context}: event index is not contiguous"));
        }
        self.next_event_index = self.next_event_index.saturating_add(1);

        if item.tombstoned {
            self.findings
                .push(format!("{context}: event follows tombstone"));
        }
        let previous_item_seq = item.head_seq;
        if !item.commit_started {
            item.commit_base_seq = previous_item_seq;
            item.commit_started = true;
        }
        let expected_item_seq = previous_item_seq.unwrap_or(0).saturating_add(1);
        if event.item_seq != expected_item_seq {
            self.findings.push(format!(
                "{context}: item sequence is {}, expected {expected_item_seq}",
                event.item_seq
            ));
        }
        item.head_seq = Some(event.item_seq);

        let proposal: EventProposal = match serde_json::from_slice(&event.payload) {
            Ok(proposal) => proposal,
            Err(error) => {
                self.findings
                    .push(format!("{context}: invalid event payload: {error}"));
                self.hash_payload(
                    &event.payload,
                    &event.event_payload_digest,
                    COMMIT_EVENT_FAMILY_ITEM,
                    &context,
                );
                return None;
            }
        };
        if proposal.item_id.as_ref() != Some(&event.item_id) {
            self.findings
                .push(format!("{context}: payload item ID mismatch"));
        }
        if proposal.event_type != event.event_type {
            self.findings
                .push(format!("{context}: payload event type mismatch"));
        }
        if proposal.base_seq != item.commit_base_seq {
            self.findings.push(format!(
                "{context}: base sequence is {:?}, expected {:?}",
                proposal.base_seq, item.commit_base_seq
            ));
        }
        match canonical_event_proposal_bytes(&proposal) {
            Ok(canonical) if canonical == event.payload => {}
            Ok(_) => self
                .findings
                .push(format!("{context}: payload is not canonical")),
            Err(error) => self
                .findings
                .push(format!("{context}: event canonicalization failed: {error}")),
        }
        self.hash_payload(
            &event.payload,
            &event.event_payload_digest,
            COMMIT_EVENT_FAMILY_ITEM,
            &context,
        );

        if previous_item_seq.is_none() && proposal.create_item.is_none() {
            self.findings
                .push(format!("{context}: first event does not create the item"));
        }
        if previous_item_seq.is_some() && proposal.create_item.is_some() {
            self.findings
                .push(format!("{context}: item is created more than once"));
        }
        if proposal.create_item.is_some()
            && (proposal.set_parent.is_some()
                || proposal.set_name.is_some()
                || proposal.set_tombstoned.is_some()
                || proposal.set_in_trash.is_some())
        {
            self.findings.push(format!(
                "{context}: creation is combined with structural modifiers"
            ));
        }
        if proposal.create_item.is_some() {
            item.created_in_commit = true;
            item.sequencing_mode = Some(SequencingMode::Local);
            item.federation_stream_id = None;
            match (
                proposal.set_is_federated,
                proposal.set_federation_stream_id.as_deref(),
            ) {
                (None, None) => {}
                (Some(true), Some(stream)) if !stream.is_empty() => {
                    item.sequencing_mode = Some(SequencingMode::Federated);
                    item.federation_stream_id = Some(stream.to_string());
                }
                _ => self.findings.push(format!(
                    "{context}: creation has an invalid federation envelope"
                )),
            }
        } else {
            match proposal.set_is_federated {
                Some(true) => {
                    let stream = proposal
                        .set_federation_stream_id
                        .as_deref()
                        .filter(|stream| !stream.is_empty());
                    if stream.is_none() {
                        self.findings
                            .push(format!("{context}: federation transition has no stream ID"));
                    }
                    let redundant_bootstrap = item.created_in_commit
                        && !item.bootstrap_rebind_seen
                        && item.sequencing_mode == Some(SequencingMode::Federated)
                        && stream == item.federation_stream_id.as_deref();
                    if redundant_bootstrap {
                        item.bootstrap_rebind_seen = true;
                    } else if item.sequencing_mode == Some(SequencingMode::Local)
                        && !item.federation_changed
                    {
                        item.federation_changed = true;
                        item.sequencing_mode = Some(SequencingMode::Federated);
                        item.federation_stream_id = stream.map(str::to_string);
                    } else {
                        self.findings
                            .push(format!("{context}: illegal federation transition"));
                    }
                }
                Some(false) => {
                    if proposal.set_federation_stream_id.is_some() {
                        self.findings
                            .push(format!("{context}: unfederation also sets a stream ID"));
                    }
                    if item.sequencing_mode == Some(SequencingMode::Federated)
                        && !item.federation_changed
                    {
                        item.federation_changed = true;
                        item.sequencing_mode = Some(SequencingMode::Local);
                        item.federation_stream_id = None;
                    } else {
                        self.findings
                            .push(format!("{context}: illegal federation transition"));
                    }
                }
                None if proposal.set_federation_stream_id.is_some() => {
                    self.findings.push(format!(
                        "{context}: stream ID is set without a federation transition"
                    ));
                }
                None => {}
            }
        }
        if (proposal.set_is_federated.is_some() || proposal.set_federation_stream_id.is_some())
            && (proposal.set_parent.is_some()
                || proposal.set_name.is_some()
                || proposal.set_tombstoned.is_some()
                || proposal.set_in_trash.is_some())
        {
            self.findings.push(format!(
                "{context}: federation transition is combined with structural modifiers"
            ));
        }
        if proposal.set_tombstoned == Some(true) {
            item.tombstoned = true;
        }
        Some(proposal)
    }

    pub fn validate_ref_event(
        &mut self,
        event: &VerifyRefEvent,
        owner: &mut EnvelopeRefState,
    ) -> Option<RefEventProposal> {
        let context = format!("{} ref event {}", self.label, event.commit_event_idx);
        self.refs_started = true;
        if usize::try_from(event.commit_event_idx).ok() != Some(self.next_event_index) {
            self.findings
                .push(format!("{context}: event index is not contiguous"));
        }
        self.next_event_index = self.next_event_index.saturating_add(1);

        if !owner.commit_started {
            owner.commit_base_seq = Some(owner.head_seq);
            owner.commit_started = true;
        }
        let proposal: RefEventProposal = match serde_json::from_slice(&event.payload) {
            Ok(proposal) => proposal,
            Err(error) => {
                self.findings
                    .push(format!("{context}: invalid ref event payload: {error}"));
                self.hash_payload(
                    &event.payload,
                    &event.event_payload_digest,
                    COMMIT_EVENT_FAMILY_REF,
                    &context,
                );
                return None;
            }
        };
        let expected_ref_seq = owner.head_seq.saturating_add(1);
        let valid_legacy_baseline = owner.head_seq == 0
            && owner.commit_base_seq == Some(0)
            && proposal.event_type == "FILE_VERSION::LEGACY_BASELINE"
            && proposal.initial_ref_seq == Some(event.ref_seq)
            && event.ref_seq >= 1;
        if event.ref_seq != expected_ref_seq && !valid_legacy_baseline {
            self.findings.push(format!(
                "{context}: ref sequence is {}, expected {expected_ref_seq}",
                event.ref_seq
            ));
        }
        if proposal.initial_ref_seq.is_some() && !valid_legacy_baseline {
            self.findings
                .push(format!("{context}: invalid legacy baseline sequence"));
        }
        if proposal.event_type == "FILE_VERSION::LEGACY_BASELINE"
            && proposal.initial_ref_seq.is_none()
        {
            self.findings.push(format!(
                "{context}: legacy baseline has no initial sequence"
            ));
        }
        owner.head_seq = event.ref_seq;
        if proposal.owner_item_id != event.owner_item_id {
            self.findings
                .push(format!("{context}: payload owner item ID mismatch"));
        }
        if proposal.event_type != event.event_type {
            self.findings
                .push(format!("{context}: payload event type mismatch"));
        }
        if Some(proposal.base_ref_seq) != owner.commit_base_seq {
            self.findings.push(format!(
                "{context}: base ref sequence is {}, expected {:?}",
                proposal.base_ref_seq, owner.commit_base_seq
            ));
        }
        match canonical_ref_event_proposal_bytes(&proposal) {
            Ok(canonical) if canonical == event.payload => {}
            Ok(_) => self
                .findings
                .push(format!("{context}: payload is not canonical")),
            Err(error) => self.findings.push(format!(
                "{context}: ref event canonicalization failed: {error}"
            )),
        }
        self.hash_payload(
            &event.payload,
            &event.event_payload_digest,
            COMMIT_EVENT_FAMILY_REF,
            &context,
        );
        Some(proposal)
    }

    pub fn finish(mut self) -> Vec<String> {
        if self.next_event_index == 0 {
            self.findings
                .push(format!("{}: commit has no events", self.label));
        }
        if self
            .commit
            .event_count
            .and_then(|count| usize::try_from(count).ok())
            != Some(self.next_event_index)
        {
            self.findings
                .push(format!("{}: event count mismatch", self.label));
        }
        let digest_bytes: [u8; 32] = self.events_hasher.finalize().into();
        let events_digest =
            Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(digest_bytes))
                .expect("internal SHA-256 digest is valid");
        if self.commit.events_digest.as_ref() != Some(&events_digest) {
            self.findings
                .push(format!("{}: events digest mismatch", self.label));
        }
        match canonical_stored_commit_bytes_for_hash(
            self.commit.format_version,
            &self.commit.author_id,
            &self.commit.committer_id,
            self.commit.occurred_at,
            self.commit.message.as_deref(),
            self.commit.import_job_id.as_deref(),
            self.next_event_index,
            &events_digest,
        ) {
            Ok(bytes) => {
                if self.commit.commit_id.as_ref() != Some(&compute_commit_id(&bytes)) {
                    self.findings
                        .push(format!("{}: commit ID mismatch", self.label));
                }
            }
            Err(error) => self.findings.push(format!(
                "{}: commit canonicalization failed: {error}",
                self.label
            )),
        }
        self.findings
    }

    fn hash_payload(
        &mut self,
        payload: &[u8],
        stored_digest: &[u8],
        event_family: u8,
        context: &str,
    ) {
        let payload_digest = compute_sha256_digest_bytes(payload);
        if stored_digest != payload_digest {
            self.findings
                .push(format!("{context}: payload digest mismatch"));
        }
        self.events_hasher.update([event_family]);
        self.events_hasher.update(payload_digest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        CreateItem, canonical_commit_bytes_for_hash, canonical_event_proposal_bytes,
    };

    fn validate_proposals(
        proposals: Vec<EventProposal>,
        mut item: EnvelopeItemState,
    ) -> (Vec<String>, EnvelopeItemState) {
        let item_id = ItemId::new();
        let initial_head = item.head_seq.unwrap_or(0);
        let mut digest_input = Vec::new();
        let events: Vec<_> = proposals
            .into_iter()
            .enumerate()
            .map(|(index, mut proposal)| {
                proposal.item_id = Some(item_id.clone());
                let payload = canonical_event_proposal_bytes(&proposal).unwrap();
                let payload_digest = compute_sha256_digest_bytes(&payload);
                digest_input.push(COMMIT_EVENT_FAMILY_ITEM);
                digest_input.extend_from_slice(&payload_digest);
                VerifyEvent {
                    commit_event_idx: i32::try_from(index).unwrap(),
                    item_id: item_id.clone(),
                    item_seq: initial_head + i64::try_from(index).unwrap() + 1,
                    event_type: proposal.event_type,
                    payload,
                    event_payload_digest: payload_digest.to_vec(),
                    parent_folder_id: None,
                    parent_folder_id_before: None,
                }
            })
            .collect();
        let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
            compute_sha256_digest_bytes(&digest_input),
        ))
        .unwrap();
        let author = "author".to_string();
        let commit_bytes = canonical_commit_bytes_for_hash(
            &author,
            &author,
            None,
            None,
            None,
            events.len(),
            &events_digest,
        )
        .unwrap();
        let commit = VerifyCommit {
            commit_id: Some(compute_commit_id(&commit_bytes)),
            repo_seq: Some(1),
            format_version: COMMIT_FORMAT_VERSION,
            import_job_id: None,
            committer_id: author.clone(),
            occurred_at: None,
            author_id: author,
            message: None,
            event_count: Some(i32::try_from(events.len()).unwrap()),
            events_digest: Some(events_digest),
        };
        let mut previous = None;
        let mut validator = CommitEnvelopeValidator::begin(&commit, &mut previous);
        for event in &events {
            assert!(validator.validate_event(event, &mut item).is_some());
        }
        (validator.finish(), item)
    }

    fn validate_single(proposal: EventProposal) -> (Vec<String>, EnvelopeItemState) {
        validate_proposals(vec![proposal], EnvelopeItemState::default())
    }

    fn create_proposal() -> EventProposal {
        EventProposal {
            item_id: None,
            event_type: "FILE::CREATE".to_string(),
            set_metadata: None,
            set_content: None,
            create_item: Some(CreateItem {
                item_type: "FILE".to_string(),
                parent_folder_id: None,
                name: "file.txt".to_string(),
            }),
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: None,
        }
    }

    #[test]
    fn validator_consumes_events_without_retaining_proposals() {
        let (findings, item) = validate_single(create_proposal());
        assert!(findings.is_empty());
        assert_eq!(item.head_seq, Some(1));
    }

    #[test]
    fn validator_rejects_impossible_create_federation_envelopes() {
        let mut proposal = create_proposal();
        proposal.set_is_federated = Some(false);
        let (findings, _) = validate_single(proposal);
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("invalid federation envelope"))
        );

        let mut proposal = create_proposal();
        proposal.set_is_federated = Some(true);
        proposal.set_federation_stream_id = Some("stream".to_string());
        assert!(validate_single(proposal).0.is_empty());
    }

    #[test]
    fn validator_accepts_only_the_canonical_bootstrap_rebind() {
        let mut create = create_proposal();
        create.set_is_federated = Some(true);
        create.set_federation_stream_id = Some("stream".to_string());
        let mut rebind = create_proposal();
        rebind.create_item = None;
        rebind.event_type = "ITEM::FEDERATE".to_string();
        rebind.set_is_federated = Some(true);
        rebind.set_federation_stream_id = Some("stream".to_string());
        assert!(
            validate_proposals(
                vec![create.clone(), rebind.clone()],
                EnvelopeItemState::default()
            )
            .0
            .is_empty()
        );

        rebind.set_federation_stream_id = Some("other".to_string());
        assert!(
            validate_proposals(vec![create, rebind], EnvelopeItemState::default())
                .0
                .iter()
                .any(|finding| finding.contains("illegal federation transition"))
        );
    }
}
