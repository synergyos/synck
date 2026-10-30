//! The rules every write path shares, so append and ingest cannot drift apart.
//!
//! Paths differ only in who chooses item IDs and sequences. The shape of their
//! events, how events change items, what a commit's final state must satisfy and
//! the rows they write are this code.

use super::*;
use crate::transition::AppliedTransition;
use crate::types::{AuthorId, CommitterId};

/// What a path does with a create event that names no item.
#[derive(Clone, Copy)]
pub(super) enum MissingCreateId {
    Assign,
    Reject,
}

/// A commit's events resolved to their items.
pub(super) struct ResolvedEvents {
    pub event_item_ids: Vec<ItemId>,
    pub created_items: Vec<CreatedItemInfo>,
    pub create_item_ids: HashSet<ItemId>,
}

pub(super) fn resolve_events(
    events: &[EventProposal],
    missing: MissingCreateId,
) -> Result<ResolvedEvents, CoreError> {
    let mut resolved = ResolvedEvents {
        event_item_ids: Vec::with_capacity(events.len()),
        created_items: Vec::new(),
        create_item_ids: HashSet::new(),
    };
    for (idx, ev) in events.iter().enumerate() {
        if ev.create_item.is_none() {
            let item_id = ev
                .item_id
                .clone()
                .ok_or_else(|| CoreError::InvalidProposal("event missing item_id".to_string()))?;
            resolved.event_item_ids.push(item_id);
            continue;
        }
        if ev.modifies_item() || ev.changes_federation() {
            return Err(CoreError::InvalidProposal(
                "create_item events must not also use structural or federation modifiers"
                    .to_string(),
            ));
        }
        let item_id = match (&ev.item_id, missing) {
            (Some(item_id), _) => item_id.clone(),
            (None, MissingCreateId::Assign) => ItemId::new(),
            (None, MissingCreateId::Reject) => {
                return Err(CoreError::InvalidProposal(
                    "create_item requires explicit item_id for federated domains".to_string(),
                ));
            }
        };
        if !resolved.create_item_ids.insert(item_id.clone()) {
            return Err(CoreError::InvalidProposal(
                "duplicate create_item for item_id within a single commit".to_string(),
            ));
        }
        resolved.event_item_ids.push(item_id.clone());
        resolved.created_items.push(CreatedItemInfo { event_idx: idx, item_id });
    }
    for created in &resolved.created_items {
        let first_idx = resolved
            .event_item_ids
            .iter()
            .position(|id| *id == created.item_id)
            .expect("created item id must be present in event_item_ids");
        if first_idx != created.event_idx {
            return Err(CoreError::InvalidProposal(
                "create_item event must be the first event for that item within the commit"
                    .to_string(),
            ));
        }
    }
    Ok(resolved)
}

/// Events of a created item never have a `base_seq`. Events of an existing item
/// have one, unless the path fills it in under the item locks.
pub(super) fn check_base_seq_shape(
    events: &[EventProposal],
    resolved: &ResolvedEvents,
    mode: AppendBaseSeqMode,
) -> Result<(), CoreError> {
    for (ev, item_id) in events.iter().zip(&resolved.event_item_ids) {
        let message = if resolved.create_item_ids.contains(item_id) {
            ev.base_seq
                .is_some()
                .then_some("base_seq must be null for events targeting a newly created item")
        } else {
            match mode {
                AppendBaseSeqMode::RequirePresent => ev.base_seq.is_none().then_some(
                    "base_seq must be present for events targeting an existing item",
                ),
                AppendBaseSeqMode::MaterializeUnderItemLocks => ev.base_seq.is_some().then_some(
                    "base_seq must be null for append-mode commits targeting an existing item",
                ),
            }
        };
        if let Some(message) = message {
            return Err(CoreError::InvalidProposal(message.to_string()));
        }
    }
    Ok(())
}

/// All events of one item in a commit agree on its `base_seq`.
pub(super) fn observed_base_seqs(
    events: &[EventProposal],
    event_item_ids: &[ItemId],
) -> Result<HashMap<ItemId, Option<i64>>, CoreError> {
    let mut observed = HashMap::new();
    for (ev, item_id) in events.iter().zip(event_item_ids) {
        if *observed.entry(item_id.clone()).or_insert(ev.base_seq) != ev.base_seq {
            return Err(CoreError::InvalidProposal(
                "inconsistent base_seq for item within a single commit".to_string(),
            ));
        }
    }
    Ok(observed)
}

pub(super) fn check_base_seqs(
    repo_id: &RepoId,
    items: &HashMap<ItemId, ItemRow>,
    observed: &HashMap<ItemId, Option<i64>>,
    create_item_ids: &HashSet<ItemId>,
) -> Result<(), CoreError> {
    for (item_id, base_seq) in observed {
        if create_item_ids.contains(item_id) {
            continue;
        }
        let row = items.get(item_id).ok_or_else(|| CoreError::ItemNotFound {
            repo_id: repo_id.clone(),
            item_id: item_id.clone(),
        })?;
        let found = base_seq.expect("existing-item base_seq presence was validated");
        if found != row.head_seq {
            return Err(CoreError::BaseSeqMismatch {
                repo_id: repo_id.clone(),
                item_id: item_id.clone(),
                expected: row.head_seq,
                found,
            });
        }
    }
    Ok(())
}

/// Ref events of one owner agree on its base.
pub(super) fn observe_ref_bases(
    ref_events: &[RefEventProposal],
) -> Result<HashMap<ItemId, i64>, CoreError> {
    let mut observed = HashMap::new();
    for event in ref_events {
        if event.event_type.trim().is_empty() {
            return Err(CoreError::InvalidProposal(
                "ref event_type must not be empty".to_string(),
            ));
        }
        if event.base_ref_seq < 0 {
            return Err(CoreError::InvalidProposal(
                "base_ref_seq must be non-negative".to_string(),
            ));
        }
        if *observed.entry(event.owner_item_id.clone()).or_insert(event.base_ref_seq)
            != event.base_ref_seq
        {
            return Err(CoreError::InvalidProposal(
                "inconsistent base_ref_seq for owner within a single commit".to_string(),
            ));
        }
    }
    Ok(observed)
}

/// Compare ref bases with their owners; a created owner's head is zero.
pub(super) fn check_ref_bases(
    repo_id: &RepoId,
    items: &HashMap<ItemId, ItemRow>,
    bases: &HashMap<ItemId, i64>,
) -> Result<HashMap<ItemId, i64>, CoreError> {
    let mut heads = HashMap::with_capacity(bases.len());
    for (owner_item_id, base_ref_seq) in bases {
        let row = items.get(owner_item_id).ok_or_else(|| CoreError::ItemNotFound {
            repo_id: repo_id.clone(),
            item_id: owner_item_id.clone(),
        })?;
        if row.ref_head_seq != *base_ref_seq {
            return Err(CoreError::RefBaseSeqMismatch {
                repo_id: repo_id.clone(),
                owner_item_id: owner_item_id.clone(),
                expected: row.ref_head_seq,
                found: *base_ref_seq,
            });
        }
        heads.insert(owner_item_id.clone(), row.ref_head_seq);
    }
    Ok(heads)
}

/// Number ref events after the commit's item events, which may tombstone an owner.
pub(super) fn assign_ref_seqs(
    repo_id: &RepoId,
    items: &HashMap<ItemId, ItemRow>,
    ref_events: &[RefEventProposal],
    ref_head_before: &HashMap<ItemId, i64>,
) -> Result<(Vec<i64>, HashMap<ItemId, i64>), CoreError> {
    let mut heads = ref_head_before.clone();
    let mut seqs = Vec::with_capacity(ref_events.len());
    for event in ref_events {
        let row = items
            .get(&event.owner_item_id)
            .expect("validated ref owner must be loaded");
        if matches!(row.lifecycle_state, LifecycleState::Tombstoned) {
            return Err(CoreError::ItemTombstoned {
                repo_id: repo_id.clone(),
                item_id: event.owner_item_id.clone(),
            });
        }
        let head = heads
            .get_mut(&event.owner_item_id)
            .expect("validated ref owner head must be loaded");
        *head = head
            .checked_add(1)
            .ok_or_else(|| CoreError::InvalidProposal("ref sequence overflow".to_string()))?;
        seqs.push(*head);
    }
    Ok((seqs, heads))
}

/// Load a commit's items. An item it creates gets a new row, and an explicit
/// create ID that already exists is refused.
#[allow(clippy::too_many_arguments)]
pub(super) fn seed_items(
    ctx: &InvariantContext<'_>,
    item_ids: &BTreeSet<ItemId>,
    event_item_ids: &[ItemId],
    create_item_ids: &HashSet<ItemId>,
    events: &[EventProposal],
    existing: &mut HashMap<ItemId, ItemRow>,
    created_at: Timestamp,
    author_id: &str,
    sequencing_mode: SequencingMode,
) -> Result<HashMap<ItemId, ItemRow>, CoreError> {
    let repo_id = ctx.repo_id;
    let tombstone = TombstoneFinality;
    let mode_rule = SequencingModeRule;
    let mut items = HashMap::with_capacity(item_ids.len());
    for item_id in item_ids {
        if !create_item_ids.contains(item_id) {
            let row = existing.remove(item_id).ok_or_else(|| CoreError::ItemNotFound {
                repo_id: repo_id.clone(),
                item_id: item_id.clone(),
            })?;
            tombstone.check(ctx, item_id, &row, &[])?;
            mode_rule.check(ctx, item_id, &row, &[])?;
            items.insert(item_id.clone(), row);
            continue;
        }
        if existing.contains_key(item_id) {
            return Err(CoreError::ItemAlreadyExists {
                repo_id: repo_id.clone(),
                item_id: item_id.clone(),
            });
        }
        let (idx, create) = events
            .iter()
            .enumerate()
            .find_map(|(idx, ev)| {
                ev.create_item
                    .as_ref()
                    .filter(|_| event_item_ids[idx] == *item_id)
                    .map(|create| (idx, create))
            })
            .expect("created item must have a corresponding event");
        let state = ItemState::from_create(create, sequencing_mode, None)
            .map_err(|error| map_create_error(repo_id, item_id, create, error))?;
        let row = new_item_row(repo_id, item_id.clone(), state, created_at, author_id.to_string());
        tombstone.check(ctx, item_id, &row, &events[idx..=idx])?;
        mode_rule.check(ctx, item_id, &row, &events[idx..=idx])?;
        items.insert(item_id.clone(), row);
    }
    Ok(items)
}

/// Apply events in order. Nothing follows a tombstone within a commit.
pub(super) fn apply_events(
    repo_id: &RepoId,
    items: &mut HashMap<ItemId, ItemRow>,
    events: &[EventProposal],
    event_item_ids: &[ItemId],
) -> Result<Vec<AppliedTransition>, CoreError> {
    let mut transitions = Vec::with_capacity(events.len());
    for (ev, item_id) in events.iter().zip(event_item_ids) {
        let row = items.get_mut(item_id).expect("item state should exist");
        if matches!(row.lifecycle_state, LifecycleState::Tombstoned) {
            return Err(CoreError::ItemTombstoned {
                repo_id: repo_id.clone(),
                item_id: item_id.clone(),
            });
        }
        let mut state = ItemState::from(&*row);
        let transition = apply_item_event(&mut state, ev)
            .map_err(|error| map_transition_error(repo_id, item_id, ev, error))?;
        state.write_to(row);
        transitions.push(transition);
    }
    Ok(transitions)
}

/// Check a commit's final items. Content-only commits skip tree checks: they
/// change no parent edge, so they cannot add a cycle or a missing parent.
/// `closure` holds the items whose ancestors must all be live.
pub(super) async fn check_final_state<T: StorageTxn + ?Sized>(
    ctx: &InvariantContext<'_>,
    txn: &mut T,
    items: &HashMap<ItemId, ItemRow>,
    events: &[EventProposal],
    closure: &BTreeSet<ItemId>,
) -> Result<(), CoreError> {
    let changes_edges = events
        .iter()
        .any(|ev| ev.create_item.is_some() || ev.set_parent.is_some());
    if changes_edges || !closure.is_empty() {
        check_parent_existence_and_type(ctx, txn, items).await?;
        let ancestors = check_folder_acyclicity(ctx, txn, items).await?;
        check_file_folder_live_parent_closure(ctx, items, &ancestors, closure)?;
    }
    let check_names = events.iter().any(|ev| {
        ev.create_item.is_some()
            || ev.set_parent.is_some()
            || ev.set_name.is_some()
            || ev.set_tombstoned.is_some()
            || ev.set_in_trash.is_some()
    });
    check_final_items(ctx, txn, items, check_names).await
}

/// The folders an event's structure depends on, which writers must lock.
pub(super) fn structural_targets(event: &EventProposal) -> impl Iterator<Item = &ItemId> {
    let created_parent = event
        .create_item
        .as_ref()
        .and_then(|create| create.parent_folder_id.as_ref());
    let moved_to = event.set_parent.as_ref().and_then(Option::as_ref);
    created_parent.into_iter().chain(moved_to).chain(link_target(event))
}

pub(super) fn reject_source_revisions(events: &[EventProposal]) -> Result<(), CoreError> {
    if events.iter().any(|ev| ev.source_revision().is_some()) {
        return Err(CoreError::InvalidProposal(
            "a federated event has no source revision".to_string(),
        ));
    }
    Ok(())
}

pub(super) struct CommitHeader<'a> {
    pub author_id: &'a AuthorId,
    pub committer_id: &'a CommitterId,
    pub message: Option<&'a str>,
    pub occurred_at: Option<Timestamp>,
    pub operation_id: Option<&'a str>,
    pub created_at: Timestamp,
}

/// Where each of a commit's events landed, by event index.
pub(super) struct EventPlacement<'a> {
    pub item_ids: &'a [ItemId],
    pub item_seqs: &'a [i64],
    pub transitions: &'a [AppliedTransition],
}

pub(super) struct CommitRows {
    pub sealed: SealedCommitInsert,
    pub event_rows: Vec<EventInsertRow>,
    pub ref_rows: Vec<RefEventInsertRow>,
}

impl Repo {
    /// Build a commit's rows and its identity from the final event payloads.
    pub(super) fn build_commit_rows(
        &self,
        repo_id: &RepoId,
        header: &CommitHeader<'_>,
        events: &[EventProposal],
        placement: &EventPlacement<'_>,
        items: &HashMap<ItemId, ItemRow>,
        ref_events: &[RefEventProposal],
        ref_seqs: &[i64],
    ) -> Result<CommitRows, CoreError> {
        let event_count = events
            .len()
            .checked_add(ref_events.len())
            .ok_or_else(|| CoreError::InvalidProposal("commit event count overflow".to_string()))?;
        let stored_count = |count: usize| {
            i32::try_from(count)
                .map_err(|_| CoreError::InvalidProposal("commit event count overflow".to_string()))
        };
        let commit_pk = CommitPk::new();
        let mut digest = Sha256::new();
        let mut payloads = self.validate_payload_limits(events, ref_events)?.into_iter();
        let mut event_rows = Vec::with_capacity(events.len());
        for (idx, (ev, payload)) in events.iter().zip(payloads.by_ref()).enumerate() {
            let item_id = &placement.item_ids[idx];
            let item = items.get(item_id).expect("event item id must exist in items map");
            validate_event_type_matches_item(item_id, item.item_type.as_str(), ev.event_type.as_str())?;
            let event_hash = compute_sha256_digest_bytes(&payload);
            digest.update([COMMIT_EVENT_FAMILY_ITEM]);
            digest.update(event_hash);
            let transition = &placement.transitions[idx];
            event_rows.push(EventInsertRow {
                repo_id: repo_id.clone(),
                commit_pk: commit_pk.clone(),
                commit_event_idx: idx as i32,
                item_id: item_id.clone(),
                item_seq: placement.item_seqs[idx],
                event_type: ev.event_type.clone(),
                metadata: ev.set_metadata.as_ref().and_then(|value| value.payload()).cloned(),
                payload,
                event_payload_digest: event_hash,
                created_at: header.created_at,
                parent_folder_id: transition.parent_folder_id_after.clone(),
                parent_folder_id_before: transition.parent_folder_id_before.clone(),
            });
        }
        let mut ref_rows = Vec::with_capacity(ref_events.len());
        for (idx, (event, payload)) in ref_events.iter().zip(payloads).enumerate() {
            let event_hash = compute_sha256_digest_bytes(&payload);
            digest.update([COMMIT_EVENT_FAMILY_REF]);
            digest.update(event_hash);
            ref_rows.push(RefEventInsertRow {
                repo_id: repo_id.clone(),
                commit_pk: commit_pk.clone(),
                commit_event_idx: stored_count(events.len() + idx)?,
                owner_item_id: event.owner_item_id.clone(),
                ref_seq: ref_seqs[idx],
                event_type: event.event_type.clone(),
                metadata: event.set_metadata.as_ref().and_then(|value| value.payload()).cloned(),
                payload,
                event_payload_digest: event_hash,
                created_at: header.created_at,
            });
        }
        let digest: [u8; 32] = digest.finalize().into();
        let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(digest))
            .expect("internal sha256 digest construction must be valid");
        let commit_id = compute_commit_id(&canonical_commit_bytes_for_hash(
            header.author_id,
            header.committer_id,
            header.occurred_at,
            header.message,
            event_count,
            &events_digest,
        )?);
        Ok(CommitRows {
            sealed: SealedCommitInsert {
                commit_pk,
                repo_seq: None,
                operation_id: header.operation_id.map(str::to_string),
                format_version: COMMIT_FORMAT_VERSION,
                author_id: header.author_id.to_string(),
                committer_id: header.committer_id.to_string(),
                message: header.message.map(str::to_string),
                occurred_at: header.occurred_at,
                created_at: header.created_at,
                commit_id,
                events_digest,
                event_count: stored_count(event_count)?,
            },
            event_rows,
            ref_rows,
        })
    }
}

pub(super) async fn insert_sealed_commit<T: StorageTxn + ?Sized>(
    txn: &mut T,
    repo_id: &RepoId,
    commit: &SealedCommitInsert,
) -> Result<(), CoreError> {
    let inserted = txn
        .insert_commit_sealed(
            repo_id,
            &commit.commit_pk,
            commit.repo_seq,
            commit.operation_id.as_deref(),
            commit.format_version,
            &commit.author_id,
            &commit.committer_id,
            commit.message.as_deref(),
            commit.occurred_at,
            commit.created_at,
            &commit.commit_id,
            &commit.events_digest,
            commit.event_count,
        )
        .await;
    match inserted {
        Ok(_) => Ok(()),
        Err(StorageError::UniqueViolation { .. }) => Err(map_insert_commit_unique_violation(
            txn,
            repo_id,
            commit.operation_id.as_deref(),
            &commit.commit_id,
        )
        .await?),
        Err(other) => Err(CoreError::StorageError(other)),
    }
}
