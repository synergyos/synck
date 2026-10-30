//! Backend-independent semantic verification of VCS history and materialized items.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use crate::envelope::{CommitEnvelopeValidator, EnvelopeItemState, EnvelopeRefState};
pub use crate::envelope::{VerifyCommit, VerifyEvent, VerifyRefEvent};
use crate::naming::compute_name_key;
use crate::transition::{ItemState, apply_event as apply_item_event, check_source_revision};
use crate::content::EventContent;
use crate::types::{
    EventProposal, ItemId, LifecycleState, SequencingMode, canonical_json_bytes,
    compute_sha256_digest_bytes, sha256_prefixed_hex_from_digest_bytes,
};

#[derive(Debug)]
pub struct VerifyItem {
    pub item_id: ItemId,
    pub item_type: String,
    pub parent_folder_id: Option<ItemId>,
    pub name: Option<String>,
    pub name_key: Option<String>,
    pub record_key: Option<String>,
    pub link_folder_id: Option<ItemId>,
    pub head_seq: i64,
    pub ref_head_seq: i64,
    pub lifecycle_state: LifecycleState,
    pub sequencing_mode: SequencingMode,
    pub federation_stream_id: Option<String>,
}

#[derive(Clone)]
struct ReplayedItem {
    state: ItemState,
    head_seq: i64,
    content: Option<EventContent>,
}

pub struct SemanticVerifier {
    previous_repo_seq: Option<i64>,
    last_item_seq: HashMap<ItemId, i64>,
    last_ref_seq: HashMap<ItemId, i64>,
    tombstoned: HashSet<ItemId>,
    replayed_items: HashMap<ItemId, ReplayedItem>,
    encrypted: bool,
    live_names: HashMap<(Option<ItemId>, String), ItemId>,
    live_keys: HashMap<(String, String), ItemId>,
    linked_records: HashMap<ItemId, HashSet<ItemId>>,
    federation_streams: HashMap<String, ItemId>,
}

impl SemanticVerifier {
    pub fn new(encrypted: bool) -> Self {
        Self {
            previous_repo_seq: None,
            last_item_seq: HashMap::new(),
            last_ref_seq: HashMap::new(),
            tombstoned: HashSet::new(),
            replayed_items: HashMap::new(),
            encrypted,
            live_names: HashMap::new(),
            live_keys: HashMap::new(),
            linked_records: HashMap::new(),
            federation_streams: HashMap::new(),
        }
    }

    pub fn verify_commit(&mut self, commit: &VerifyCommit, events: &[VerifyEvent]) -> Vec<String> {
        self.verify_commit_events(commit, events.iter().map(VerifyCommitEvent::Item))
    }

    pub fn verify_commit_events<'a>(
        &mut self,
        commit: &VerifyCommit,
        events: impl IntoIterator<Item = VerifyCommitEvent<'a>>,
    ) -> Vec<String> {
        let mut heads = BTreeMap::new();
        let mut ref_heads = BTreeMap::new();
        let label = commit
            .repo_seq
            .map(|seq| format!("repo sequence {seq}"))
            .unwrap_or_else(|| "unsequenced commit".to_string());
        let mut replay_findings = Vec::new();
        if self.encrypted && commit.message.is_some() {
            replay_findings.push(format!("{label}: an encrypted repository has no messages"));
        }
        let mut touched: HashMap<ItemId, Option<ItemState>> = HashMap::new();
        let mut validator = CommitEnvelopeValidator::begin(commit, &mut self.previous_repo_seq);
        for event in events {
            match event {
                VerifyCommitEvent::Item(event) => {
                    let sequencing = self.replayed_items.get(&event.item_id).map(|item| {
                        (
                            item.state.sequencing_mode,
                            item.state.federation_stream_id.clone(),
                        )
                    });
                    let item = heads.entry(event.item_id.clone()).or_insert_with(|| {
                        EnvelopeItemState::from_head(
                            self.last_item_seq.get(&event.item_id).copied(),
                            self.tombstoned.contains(&event.item_id),
                            sequencing.as_ref().map(|sequencing| sequencing.0),
                            sequencing.and_then(|sequencing| sequencing.1),
                        )
                    });
                    if let Some(proposal) = validator.validate_event(event, item) {
                        let context = format!("{label} event {}", event.commit_event_idx);
                        if let Some(source) = proposal.source_revision()
                            && let Err(error) = check_source_revision(
                                &event.item_id,
                                source,
                                self.last_item_seq.get(&source.item_id).copied(),
                            )
                        {
                            replay_findings.push(format!("{context}: {error}"));
                        }
                        touched.entry(event.item_id.clone()).or_insert_with(|| {
                            let replayed = self.replayed_items.get(&event.item_id);
                            replayed.map(|item| item.state.clone())
                        });
                        self.replay_event(event, &proposal, &context, &mut replay_findings);
                    }
                }
                VerifyCommitEvent::Ref(event) => {
                    match self.replayed_items.get(&event.owner_item_id) {
                        None => replay_findings.push(format!(
                            "{label} ref event {}: owner item is absent",
                            event.commit_event_idx
                        )),
                        Some(owner)
                            if owner.state.lifecycle_state == LifecycleState::Tombstoned =>
                        {
                            replay_findings.push(format!(
                                "{label} ref event {}: owner item is tombstoned",
                                event.commit_event_idx
                            ));
                        }
                        Some(_) => {}
                    }
                    let owner = ref_heads
                        .entry(event.owner_item_id.clone())
                        .or_insert_with(|| EnvelopeRefState {
                            head_seq: self
                                .last_ref_seq
                                .get(&event.owner_item_id)
                                .copied()
                                .unwrap_or(0),
                            ..EnvelopeRefState::default()
                        });
                    validator.validate_ref_event(event, owner);
                }
            }
        }
        let mut findings = validator.finish();
        findings.append(&mut replay_findings);
        self.check_commit_end(&label, &touched, &mut findings);
        for (item_id, item) in heads {
            if let Some(head_seq) = item.head_seq {
                self.last_item_seq.insert(item_id.clone(), head_seq);
            }
            if item.tombstoned {
                self.tombstoned.insert(item_id);
            }
        }
        for (owner_item_id, owner) in ref_heads {
            self.last_ref_seq.insert(owner_item_id, owner.head_seq);
        }
        findings
    }

    pub fn finish(self, items: &[VerifyItem]) -> Vec<String> {
        let mut findings = Vec::new();
        self.finish_with(items, |finding| findings.push(finding));
        findings
    }

    /// Finish verification against the exact item state reconstructed from
    /// the verified history itself.
    ///
    /// This is for history-only replicas that deliberately do not maintain
    /// the ordinary mutable `vcs_items` projection. It still checks global
    /// live-name uniqueness, parent validity, and folder acyclicity.
    pub fn finish_replayed(self) -> Vec<String> {
        let items = self
            .replayed_items
            .iter()
            .map(|(item_id, replayed)| VerifyItem {
                item_id: item_id.clone(),
                item_type: replayed.state.item_type.clone(),
                parent_folder_id: replayed.state.parent_folder_id.clone(),
                name: replayed.state.name.clone(),
                name_key: replayed.state.name_key.clone(),
                record_key: replayed.state.record_key.clone(),
                link_folder_id: replayed.state.link_folder_id.clone(),
                head_seq: replayed.head_seq,
                ref_head_seq: self.last_ref_seq.get(item_id).copied().unwrap_or(0),
                lifecycle_state: replayed.state.lifecycle_state,
                sequencing_mode: replayed.state.sequencing_mode,
                federation_stream_id: replayed.state.federation_stream_id.clone(),
            })
            .collect::<Vec<_>>();
        self.finish(&items)
    }

    pub fn finish_with(self, items: &[VerifyItem], mut push: impl FnMut(String)) {
        let materialized: HashMap<&ItemId, &VerifyItem> =
            items.iter().map(|item| (&item.item_id, item)).collect();
        for item_id in self.replayed_items.keys() {
            if !materialized.contains_key(item_id) {
                push(format!(
                    "item {item_id}: event history has no materialized item"
                ));
            }
        }
        for owner_item_id in self.last_ref_seq.keys() {
            if !materialized.contains_key(owner_item_id) {
                push(format!(
                    "item {owner_item_id}: ref history has no materialized owner"
                ));
            }
        }
        for item in items {
            let Some(replayed) = self.replayed_items.get(&item.item_id) else {
                push(format!(
                    "item {}: materialized item has no event history",
                    item.item_id
                ));
                continue;
            };
            if item.item_type != replayed.state.item_type
                || item.parent_folder_id != replayed.state.parent_folder_id
                || item.name != replayed.state.name
                || item.name_key != replayed.state.name_key
                || item.record_key != replayed.state.record_key
                || item.link_folder_id != replayed.state.link_folder_id
                || item.head_seq != replayed.head_seq
                || item.ref_head_seq != self.last_ref_seq.get(&item.item_id).copied().unwrap_or(0)
                || item.lifecycle_state != replayed.state.lifecycle_state
                || item.sequencing_mode != replayed.state.sequencing_mode
                || item.federation_stream_id != replayed.state.federation_stream_id
            {
                push(format!(
                    "item {}: materialized state does not match event history",
                    item.item_id
                ));
            }
            if self.last_item_seq.get(&item.item_id) != Some(&item.head_seq) {
                push(format!("item {}: head sequence mismatch", item.item_id));
            }
            if self.last_ref_seq.get(&item.item_id).copied().unwrap_or(0) != item.ref_head_seq {
                push(format!("item {}: ref head sequence mismatch", item.item_id));
            }
            match item.name.as_deref().map(compute_name_key) {
                None if item.name_key.is_none() => {}
                Some(Ok(key)) if Some(&key) == item.name_key.as_ref() => {}
                None | Some(Ok(_)) => push(format!("item {}: name key mismatch", item.item_id)),
                Some(Err(error)) => push(format!(
                    "item {}: invalid name {:?}: {error}",
                    item.item_id, item.name
                )),
            }
            match (item.sequencing_mode, item.federation_stream_id.as_deref()) {
                (SequencingMode::Local, None) => {}
                (SequencingMode::Federated, Some(value)) if !value.is_empty() => {}
                _ => push(format!(
                    "item {}: sequencing mode and federation ID disagree",
                    item.item_id
                )),
            }
        }

        let mut live_names = HashSet::new();
        for item in items {
            if item.lifecycle_state == LifecycleState::Live
                && let Some(name_key) = &item.name_key
                && !live_names.insert((item.parent_folder_id.clone(), name_key.clone()))
            {
                push(format!(
                    "item {}: duplicate live portable name",
                    item.item_id
                ));
            }
            if let Some(parent_id) = item.parent_folder_id.as_ref() {
                match materialized.get(parent_id) {
                    None => push(format!(
                        "item {}: parent {parent_id} is absent",
                        item.item_id
                    )),
                    Some(parent) if parent.item_type != "FOLDER" => push(format!(
                        "item {}: parent {parent_id} is not a folder",
                        item.item_id
                    )),
                    Some(_) => {}
                }
            }
        }
        verify_acyclic(items, &materialized, &mut push);
    }

    /// The digest of the structural state after the commits verified so far.
    pub fn state_digest(&self) -> Result<String, crate::errors::CoreError> {
        let mut item_ids: Vec<&ItemId> = self.replayed_items.keys().collect();
        item_ids.sort_by_key(|item_id| item_id.as_str());
        let mut data = b"synck-state-v1\n".to_vec();
        for item_id in item_ids {
            let item = &self.replayed_items[item_id];
            let state = &item.state;
            let content = item.content.as_ref().map_or_else(
                || json!({}),
                |content| serde_json::to_value(content.roles()).expect("role map is JSON"),
            );
            let value: Value = json!({
                "item_id": item_id.as_str(),
                "item_type": state.item_type,
                "parent_folder_id": state.parent_folder_id.as_ref().map(ItemId::as_str),
                "name": state.name,
                "name_key": state.name_key,
                "record_key": state.record_key,
                "link_folder_id": state.link_folder_id.as_ref().map(ItemId::as_str),
                "content": content,
                "head_seq": item.head_seq,
                "ref_head_seq": self.last_ref_seq.get(item_id).copied().unwrap_or(0),
                "lifecycle_state": state.lifecycle_state,
                "sequencing_mode": state.sequencing_mode,
                "federation_stream_id": state.federation_stream_id,
            });
            data.extend_from_slice(&canonical_json_bytes(&value)?);
            data.push(b'\n');
        }
        Ok(sha256_prefixed_hex_from_digest_bytes(
            compute_sha256_digest_bytes(&data),
        ))
    }

    /// Apply the rules on a commit's final state. `touched` maps each item the
    /// commit changed to its state before the commit, so atomic swaps pass.
    fn check_commit_end(
        &mut self,
        label: &str,
        touched: &HashMap<ItemId, Option<ItemState>>,
        findings: &mut Vec<String>,
    ) {
        for before in touched.values().flatten() {
            if before.lifecycle_state == LifecycleState::Live {
                if let Some(name_key) = &before.name_key {
                    self.live_names
                        .remove(&(before.parent_folder_id.clone(), name_key.clone()));
                }
                if let Some(key) = &before.record_key {
                    self.live_keys
                        .remove(&(before.item_type.clone(), key.clone()));
                }
            }
        }
        for before in touched.values().flatten() {
            if let Some(stream) = &before.federation_stream_id {
                self.federation_streams.remove(stream);
            }
        }
        for (item_id, before) in touched {
            if let Some(folder) = before.as_ref().and_then(|state| state.link_folder_id.as_ref())
                && let Some(records) = self.linked_records.get_mut(folder)
            {
                records.remove(item_id);
            }
        }
        for (item_id, before) in touched {
            let Some(item) = self.replayed_items.get(item_id) else {
                continue;
            };
            let state = item.state.clone();
            let mut report = |rule: &str| findings.push(format!("{label}: item {item_id}: {rule}"));
            if state.lifecycle_state == LifecycleState::Live {
                if let Some(name_key) = &state.name_key {
                    let slot = (state.parent_folder_id.clone(), name_key.clone());
                    if self.live_names.insert(slot, item_id.clone()).is_some() {
                        report("duplicate live name");
                    }
                }
                if let Some(key) = &state.record_key {
                    let slot = (state.item_type.clone(), key.clone());
                    if self.live_keys.insert(slot, item_id.clone()).is_some() {
                        report("duplicate live record key");
                    }
                }
            }
            if let Some(stream) = &state.federation_stream_id
                && self.federation_streams.insert(stream.clone(), item_id.clone()).is_some()
            {
                report("duplicate federation stream");
            }
            if let Some(parent) = &state.parent_folder_id {
                match self.replayed_items.get(parent) {
                    Some(parent) if parent.state.item_type == "FOLDER" => {}
                    _ => report("parent is not a folder in this repository"),
                }
                let mut seen = HashSet::from([item_id]);
                let mut current = Some(parent);
                while let Some(ancestor) = current {
                    if !seen.insert(ancestor) {
                        report("folder cycle");
                        break;
                    }
                    current = self
                        .replayed_items
                        .get(ancestor)
                        .and_then(|item| item.state.parent_folder_id.as_ref());
                }
            }
            let Some(target) = &state.link_folder_id else {
                continue;
            };
            if state.lifecycle_state == LifecycleState::Tombstoned {
                continue;
            }
            self.linked_records
                .entry(target.clone())
                .or_default()
                .insert(item_id.clone());
            if state.sequencing_mode == SequencingMode::Federated {
                report("a federated item has a link");
            }
            let unchanged = before
                .as_ref()
                .is_some_and(|before| before.link_folder_id.as_ref() == Some(target));
            match self.replayed_items.get(target) {
                Some(folder) if folder.state.item_type == "FOLDER" => {
                    match folder.state.lifecycle_state {
                        LifecycleState::Live => {}
                        LifecycleState::Trash if unchanged => {}
                        LifecycleState::Trash => report("link set to a folder in the trash"),
                        LifecycleState::Tombstoned => report("link to a tombstoned folder"),
                    }
                }
                _ => report("link target is not a folder in this repository"),
            }
        }
        for item_id in touched.keys() {
            let Some(item) = self.replayed_items.get(item_id) else {
                continue;
            };
            if item.state.lifecycle_state == LifecycleState::Tombstoned
                && self
                    .linked_records
                    .get(item_id)
                    .is_some_and(|records| !records.is_empty())
            {
                findings.push(format!(
                    "{label}: item {item_id}: tombstoned with records still linked"
                ));
            }
        }
    }

    fn replay_event(
        &mut self,
        event: &VerifyEvent,
        proposal: &EventProposal,
        context: &str,
        findings: &mut Vec<String>,
    ) {
        let existed = self.replayed_items.contains_key(&event.item_id);
        if !existed {
            let Some(create) = proposal.create_item.as_ref() else {
                return;
            };
            let state = match ItemState::from_create(create, SequencingMode::Local, None) {
                Ok(state) => state,
                Err(error) => {
                    findings.push(format!("{context}: invalid created name: {error}"));
                    return;
                }
            };
            self.replayed_items
                .insert(
                    event.item_id.clone(),
                    ReplayedItem {
                        state,
                        head_seq: 0,
                        content: None,
                    },
                );
        }
        let Some(item) = self.replayed_items.get_mut(&event.item_id) else {
            return;
        };
        let transition = match apply_item_event(&mut item.state, proposal) {
            Ok(transition) => transition,
            Err(error) => {
                findings.push(format!("{context}: {error}"));
                return;
            }
        };
        if event.parent_folder_id_before != transition.parent_folder_id_before {
            findings.push(format!("{context}: previous parent mismatch"));
        }
        if event.parent_folder_id != transition.parent_folder_id_after {
            findings.push(format!("{context}: resulting parent mismatch"));
        }
        item.head_seq = event.item_seq;
        if proposal.replaces_content() {
            item.content.clone_from(&proposal.set_content);
        }
    }
}

#[derive(Clone, Copy)]
pub enum VerifyCommitEvent<'a> {
    Item(&'a VerifyEvent),
    Ref(&'a VerifyRefEvent),
}

fn verify_acyclic(
    items: &[VerifyItem],
    by_id: &HashMap<&ItemId, &VerifyItem>,
    push: &mut impl FnMut(String),
) {
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Mark {
        Unvisited,
        Visiting,
        Done,
    }
    let mut marks: HashMap<ItemId, Mark> = items
        .iter()
        .map(|item| (item.item_id.clone(), Mark::Unvisited))
        .collect();
    for start in items {
        if marks.get(&start.item_id) != Some(&Mark::Unvisited) {
            continue;
        }
        let mut path = Vec::new();
        let mut current = start.item_id.clone();
        loop {
            match marks.get(&current).copied().unwrap_or(Mark::Done) {
                Mark::Done => break,
                Mark::Visiting => {
                    push(format!("item {current}: folder cycle detected"));
                    break;
                }
                Mark::Unvisited => {
                    marks.insert(current.clone(), Mark::Visiting);
                    path.push(current.clone());
                    let Some(parent) = by_id
                        .get(&current)
                        .and_then(|item| item.parent_folder_id.clone())
                    else {
                        break;
                    };
                    current = parent;
                }
            }
        }
        for item_id in path {
            marks.insert(item_id, Mark::Done);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        COMMIT_EVENT_FAMILY_ITEM, COMMIT_EVENT_FAMILY_REF, COMMIT_FORMAT_VERSION, CreateItem,
        RefEventProposal, Sha256Digest, canonical_commit_bytes_for_hash,
        canonical_event_proposal_bytes, canonical_ref_event_proposal_bytes,
        canonical_stored_commit_bytes_for_hash, compute_commit_id, compute_sha256_digest_bytes,
        sha256_prefixed_hex_from_digest_bytes,
    };
    use sha2::{Digest, Sha256};

    fn valid_history() -> (VerifyCommit, Vec<VerifyEvent>, VerifyItem) {
        let item_id = ItemId::new();
        let proposal = EventProposal { set_link: None, merged_from: None,
            item_id: Some(item_id.clone()),
            event_type: "FILE::CREATE".to_string(),
            set_metadata: None,
            set_content: Some(crate::content::EventContent::bytes(crate::types::ObjectId::from_digest([7; 32]))),
            create_item: Some(CreateItem { key: None, link: None, derived_from: None,
                item_type: "FILE".to_string(),
                parent_folder_id: None,
                name: Some("file.txt".to_string()),
            }),
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: None,
        };
        let payload = canonical_event_proposal_bytes(&proposal).unwrap();
        let payload_digest = compute_sha256_digest_bytes(&payload);
        let events_digest_bytes =
            compute_sha256_digest_bytes(&[&[0u8][..], &payload_digest[..]].concat());
        let events_digest =
            Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(events_digest_bytes))
                .unwrap();
        let author = "author".to_string();
        let commit_bytes = canonical_commit_bytes_for_hash(
            &author,
            &author,
            None,
            Some("message"),
            1,
            &events_digest,
        )
        .unwrap();
        let commit = VerifyCommit {
            commit_id: Some(compute_commit_id(&commit_bytes)),
            repo_seq: Some(1),
            format_version: COMMIT_FORMAT_VERSION,
            committer_id: author.clone(),
            occurred_at: None,
            author_id: author,
            message: Some("message".to_string()),
            event_count: Some(1),
            events_digest: Some(events_digest),
        };
        let event = VerifyEvent {
            commit_event_idx: 0,
            item_id: item_id.clone(),
            item_seq: 1,
            event_type: "FILE::CREATE".to_string(),
            payload,
            event_payload_digest: payload_digest.to_vec(),
            parent_folder_id: None,
            parent_folder_id_before: None,
        };
        let item = VerifyItem { link_folder_id: None, record_key: None,
            item_id,
            item_type: "FILE".to_string(),
            parent_folder_id: None,
            name: Some("file.txt".to_string()),
            name_key: Some("file.txt".to_string()),
            head_seq: 1,
            ref_head_seq: 0,
            lifecycle_state: LifecycleState::Live,
            sequencing_mode: SequencingMode::Local,
            federation_stream_id: None,
        };
        (commit, vec![event], item)
    }

    #[test]
    fn valid_history_and_materialization_pass() {
        let (commit, events, item) = valid_history();
        let mut verifier = SemanticVerifier::new(false);
        assert!(verifier.verify_commit(&commit, &events).is_empty());
        assert!(verifier.finish(&[item]).is_empty());
    }

    #[test]
    fn payload_corruption_is_reported() {
        let (commit, mut events, item) = valid_history();
        events[0].event_payload_digest[0] ^= 0xff;
        let mut verifier = SemanticVerifier::new(false);
        let findings = verifier.verify_commit(&commit, &events);
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("payload digest mismatch"))
        );
        assert!(verifier.finish(&[item]).is_empty());
    }

    #[test]
    fn new_item_base_sequence_is_rejected() {
        let (commit, mut events, item) = valid_history();
        let mut proposal: EventProposal = serde_json::from_slice(&events[0].payload).unwrap();
        proposal.base_seq = Some(0);
        events[0].payload = canonical_event_proposal_bytes(&proposal).unwrap();
        events[0].event_payload_digest = compute_sha256_digest_bytes(&events[0].payload).to_vec();

        let mut verifier = SemanticVerifier::new(false);
        let findings = verifier.verify_commit(&commit, &events);
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("base sequence is Some(0), expected None"))
        );
        assert!(verifier.finish(&[item]).is_empty());
    }

    fn ref_commit(
        repo_seq: i64,
        item_event: Option<&VerifyEvent>,
        ref_event: &VerifyRefEvent,
    ) -> VerifyCommit {
        let mut hasher = Sha256::new();
        let event_count = if let Some(event) = item_event {
            hasher.update([COMMIT_EVENT_FAMILY_ITEM]);
            hasher.update(&event.event_payload_digest);
            2
        } else {
            1
        };
        hasher.update([COMMIT_EVENT_FAMILY_REF]);
        hasher.update(&ref_event.event_payload_digest);
        let digest: [u8; 32] = hasher.finalize().into();
        let events_digest =
            Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(digest)).unwrap();
        let author = "author".to_string();
        let commit_bytes = canonical_stored_commit_bytes_for_hash(
            COMMIT_FORMAT_VERSION,
            &author,
            &author,
            None,
            None,
            event_count,
            &events_digest,
        )
        .unwrap();
        VerifyCommit {
            commit_id: Some(compute_commit_id(&commit_bytes)),
            repo_seq: Some(repo_seq),
            format_version: COMMIT_FORMAT_VERSION,
            committer_id: author.clone(),
            occurred_at: None,
            author_id: author,
            message: None,
            event_count: Some(event_count as i32),
            events_digest: Some(events_digest),
        }
    }

    fn verify_ref_event(owner_item_id: ItemId, commit_event_idx: i32) -> VerifyRefEvent {
        let proposal = RefEventProposal {
            owner_item_id: owner_item_id.clone(),
            event_type: "FILE_VERSION::CREATE".to_string(),
            set_metadata: Some(crate::metadata::MetadataRef::new(serde_json::json!({"file_item_id": owner_item_id.as_str()})).unwrap()),
            set_content: None,
            base_ref_seq: 0,
        };
        let payload = canonical_ref_event_proposal_bytes(&proposal).unwrap();
        let payload_digest = compute_sha256_digest_bytes(&payload);
        VerifyRefEvent {
            commit_event_idx,
            owner_item_id,
            ref_seq: 1,
            event_type: proposal.event_type,
            payload,
            event_payload_digest: payload_digest.to_vec(),
        }
    }

    #[test]
    fn ref_before_owner_creation_is_rejected_at_event_position() {
        let item_id = ItemId::new();
        let ref_event = verify_ref_event(item_id, 0);
        let commit = ref_commit(1, None, &ref_event);
        let mut verifier = SemanticVerifier::new(false);
        let findings = verifier.verify_commit_events(&commit, [VerifyCommitEvent::Ref(&ref_event)]);
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("owner item is absent"))
        );
    }

    #[test]
    fn ref_after_owner_tombstone_is_rejected_at_event_position() {
        let (create_commit, create_events, _) = valid_history();
        let item_id = create_events[0].item_id.clone();
        let tombstone = EventProposal { set_link: None, merged_from: None,
            item_id: Some(item_id.clone()),
            event_type: "FILE::TOMBSTONE".to_string(),
            set_metadata: None,
            set_content: None,
            create_item: None,
            set_parent: None,
            set_name: None,
            set_tombstoned: Some(true),
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: Some(1),
        };
        let payload = canonical_event_proposal_bytes(&tombstone).unwrap();
        let payload_digest = compute_sha256_digest_bytes(&payload);
        let item_event = VerifyEvent {
            commit_event_idx: 0,
            item_id: item_id.clone(),
            item_seq: 2,
            event_type: tombstone.event_type,
            payload,
            event_payload_digest: payload_digest.to_vec(),
            parent_folder_id: None,
            parent_folder_id_before: None,
        };
        let ref_event = verify_ref_event(item_id, 1);
        let commit = ref_commit(2, Some(&item_event), &ref_event);
        let mut verifier = SemanticVerifier::new(false);
        assert!(
            verifier
                .verify_commit(&create_commit, &create_events)
                .is_empty()
        );
        let findings = verifier.verify_commit_events(
            &commit,
            [
                VerifyCommitEvent::Item(&item_event),
                VerifyCommitEvent::Ref(&ref_event),
            ],
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("owner item is tombstoned"))
        );
    }

    #[test]
    fn item_event_after_ref_event_is_rejected() {
        let (create_commit, create_events, _) = valid_history();
        let item_id = create_events[0].item_id.clone();
        let mut verifier = SemanticVerifier::new(false);
        assert!(
            verifier
                .verify_commit(&create_commit, &create_events)
                .is_empty()
        );

        let mut ref_event = verify_ref_event(item_id.clone(), 0);
        let item_proposal = EventProposal { set_link: None, merged_from: None,
            item_id: Some(item_id.clone()),
            event_type: "FILE::SET_METADATA".to_string(),
            set_metadata: Some(crate::metadata::MetadataRef::new(serde_json::json!({"key": "value"})).unwrap()),
            set_content: None,
            create_item: None,
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: Some(1),
        };
        let item_payload = canonical_event_proposal_bytes(&item_proposal).unwrap();
        let item_digest = compute_sha256_digest_bytes(&item_payload);
        let item_event = VerifyEvent {
            commit_event_idx: 1,
            item_id,
            item_seq: 2,
            event_type: item_proposal.event_type,
            payload: item_payload,
            event_payload_digest: item_digest.to_vec(),
            parent_folder_id: None,
            parent_folder_id_before: None,
        };
        ref_event.commit_event_idx = 0;

        let mut hasher = Sha256::new();
        hasher.update([COMMIT_EVENT_FAMILY_REF]);
        hasher.update(&ref_event.event_payload_digest);
        hasher.update([COMMIT_EVENT_FAMILY_ITEM]);
        hasher.update(&item_event.event_payload_digest);
        let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
            hasher.finalize().into(),
        ))
        .unwrap();
        let author = "author".to_string();
        let commit_bytes = canonical_stored_commit_bytes_for_hash(
            COMMIT_FORMAT_VERSION,
            &author,
            &author,
            None,
            None,
            2,
            &events_digest,
        )
        .unwrap();
        let commit = VerifyCommit {
            commit_id: Some(compute_commit_id(&commit_bytes)),
            repo_seq: Some(2),
            format_version: COMMIT_FORMAT_VERSION,
            committer_id: author.clone(),
            occurred_at: None,
            author_id: author,
            message: None,
            event_count: Some(2),
            events_digest: Some(events_digest),
        };
        let findings = verifier.verify_commit_events(
            &commit,
            [
                VerifyCommitEvent::Ref(&ref_event),
                VerifyCommitEvent::Item(&item_event),
            ],
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("item event follows a ref event"))
        );
    }

    #[test]
    fn the_same_commit_format_accepts_item_only_history() {
        let (_, events, _) = valid_history();
        let event = &events[0];
        let mut hasher = Sha256::new();
        hasher.update([COMMIT_EVENT_FAMILY_ITEM]);
        hasher.update(&event.event_payload_digest);
        let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
            hasher.finalize().into(),
        ))
        .unwrap();
        let author = "author".to_string();
        let commit_bytes = canonical_stored_commit_bytes_for_hash(
            COMMIT_FORMAT_VERSION,
            &author,
            &author,
            None,
            None,
            1,
            &events_digest,
        )
        .unwrap();
        let commit = VerifyCommit {
            commit_id: Some(compute_commit_id(&commit_bytes)),
            repo_seq: Some(1),
            format_version: COMMIT_FORMAT_VERSION,
            committer_id: author.clone(),
            occurred_at: None,
            author_id: author,
            message: None,
            event_count: Some(1),
            events_digest: Some(events_digest),
        };
        let mut verifier = SemanticVerifier::new(false);
        let findings = verifier.verify_commit(&commit, &events);
        assert!(findings.is_empty(), "{findings:?}");
        let mut changed = commit.clone();
        changed.committer_id = "another committer".into();
        let findings = SemanticVerifier::new(false).verify_commit(&changed, &events);
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("commit ID mismatch"))
        );
        changed = commit;
        changed.occurred_at = Some("2026-10-28T12:00:00Z".parse().unwrap());
        let findings = SemanticVerifier::new(false).verify_commit(&changed, &events);
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("commit ID mismatch"))
        );
    }
}
