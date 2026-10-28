use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synck_vcs::naming::compute_name_key;
use synck_vcs::types::{
    BlobId, COMMIT_EVENT_FAMILY_ITEM, COMMIT_EVENT_FAMILY_REF, COMMIT_FORMAT_VERSION,
    CommitEventRow, CommitRow, EventContent, EventProposal, EventRow, IMPORT_COMMIT_FORMAT_VERSION,
    ItemId, REF_COMMIT_FORMAT_VERSION, REF_IMPORT_COMMIT_FORMAT_VERSION, RefEventProposal,
    RefEventRow, canonical_event_proposal_bytes, canonical_ref_event_proposal_bytes,
    canonical_stored_commit_bytes_for_hash, compute_commit_id, compute_sha256_digest_bytes,
    sha256_prefixed_hex_from_digest_bytes,
};

use crate::error::{SemanticError, SemanticErrorCode};
use crate::model::{ChangeGroup, FileFolderKind, ImmutableItemKind, ItemChange, ItemHead};
use crate::{MAX_EVENT_PAYLOAD_BYTES, MAX_MESSAGE_BYTES, is_profile_event_type};

const KNOWN_FILE_OMISSIONS: &[&str] = &[
    "FILE::SET_METADATA",
    "FILE::FEDERATE",
    "FILE::REVISION_SUPPRESS",
    "FILE::REVISION_UNSUPPRESS",
    "FILE::VERSION::CREATE",
    "FILE::VERSION::SET_METADATA",
    "FILE::VERSION::TOMBSTONE",
];
const KNOWN_FOLDER_OMISSIONS: &[&str] = &[
    "FOLDER::SET_METADATA",
    "FOLDER::FEDERATE",
    "FOLDER::WORKFLOW_RUNTIME",
];
const KNOWN_ITEM_OMISSIONS: &[&str] = &[
    "ITEM::DATA_LINK::PUT",
    "ITEM::DATA_LINK::DELETE",
    "ITEM::SOURCE_MODIFIED_AT::REPAIR",
];

pub fn project_commit(
    commit: &CommitRow,
    events: &[EventRow],
    immutable_kinds: &[ImmutableItemKind],
) -> Result<Option<ChangeGroup>, SemanticError> {
    project_commit_with_refs(commit, events, &[], immutable_kinds)
}

/// Project one complete accepted commit. Ref-bearing commit formats require the
/// complete tagged event list so the aggregate digest and commit ID remain
/// authenticated before out-of-profile ref events are omitted.
pub fn project_commit_events(
    commit: &CommitRow,
    events: &[CommitEventRow],
    immutable_kinds: &[ImmutableItemKind],
) -> Result<Option<ChangeGroup>, SemanticError> {
    let mut item_events = Vec::new();
    let mut ref_events = Vec::new();
    let mut refs_started = false;
    for event in events {
        match event {
            CommitEventRow::Item(event) if !refs_started => item_events.push(event.clone()),
            CommitEventRow::Item(_) => {
                return Err(error(
                    SemanticErrorCode::MalformedProfileEvent,
                    "item event follows a ref event",
                ));
            }
            CommitEventRow::Ref(event) => {
                refs_started = true;
                ref_events.push(event.clone());
            }
        }
    }
    project_commit_with_refs(commit, &item_events, &ref_events, immutable_kinds)
}

fn project_commit_with_refs(
    commit: &CommitRow,
    events: &[EventRow],
    ref_events: &[RefEventRow],
    immutable_kinds: &[ImmutableItemKind],
) -> Result<Option<ChangeGroup>, SemanticError> {
    let repo_seq = commit.repo_seq.filter(|value| *value > 0).ok_or_else(|| {
        error(
            SemanticErrorCode::MalformedProfileEvent,
            "projected commit must have a positive repo_seq",
        )
    })?;
    let ref_format = matches!(
        commit.format_version,
        REF_COMMIT_FORMAT_VERSION | REF_IMPORT_COMMIT_FORMAT_VERSION
    );
    let total_events = events.len().checked_add(ref_events.len()).ok_or_else(|| {
        error(
            SemanticErrorCode::MalformedProfileEvent,
            "commit event count overflows",
        )
    })?;
    if !matches!(
        commit.format_version,
        COMMIT_FORMAT_VERSION
            | IMPORT_COMMIT_FORMAT_VERSION
            | REF_COMMIT_FORMAT_VERSION
            | REF_IMPORT_COMMIT_FORMAT_VERSION
    ) || commit.event_count < 0
        || commit.event_count as usize != total_events
        || ref_format == ref_events.is_empty()
    {
        return Err(error(
            SemanticErrorCode::MalformedProfileEvent,
            "commit envelope does not match its complete event list",
        ));
    }
    let kind_map = exact_kind_map(immutable_kinds)?;
    let parsed = parse_and_verify_events(commit, repo_seq, events, ref_events, ref_format)?;
    let created_types = collect_created_types(&parsed)?;
    validate_kind_map_exactness(&parsed, &created_types, &kind_map)?;

    let mut changes = Vec::new();
    let mut heads: BTreeMap<ItemId, ItemHead> = BTreeMap::new();
    let mut last_item_seq: BTreeMap<ItemId, i64> = BTreeMap::new();
    let mut item_bases: BTreeMap<ItemId, Option<i64>> = BTreeMap::new();
    for (row, proposal) in parsed {
        let created_in_commit = created_types.contains_key(&row.item_id);
        let created_type = created_types.get(&row.item_id);
        let kind = created_type
            .and_then(|item_type| FileFolderKind::from_item_type(item_type))
            .or_else(|| {
                kind_map
                    .get(&row.item_id)
                    .and_then(|item_type| FileFolderKind::from_item_type(item_type))
            });
        if created_in_commit
            && !last_item_seq.contains_key(&row.item_id)
            && proposal.create_item.is_none()
        {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "the first event for a created item is not CREATE",
            ));
        }
        validate_item_sequence(
            &mut last_item_seq,
            &row.item_id,
            row.item_seq,
            proposal.base_seq,
        )?;
        validate_item_base(
            &mut item_bases,
            &row.item_id,
            proposal.base_seq,
            created_in_commit,
        )?;
        validate_prefix(
            &proposal.event_type,
            created_type.or_else(|| kind_map.get(&row.item_id)),
            kind,
        )?;

        if let Some(kind) = kind {
            heads.insert(
                row.item_id.clone(),
                ItemHead {
                    item_id: row.item_id.clone(),
                    item_type: kind,
                    head_seq: row.item_seq,
                },
            );
            if let Some(event) = classify_profile_event(&proposal, kind)? {
                changes.push(ItemChange {
                    item_id: row.item_id.clone(),
                    item_type: kind,
                    item_seq: row.item_seq,
                    event,
                });
            }
        }
    }
    if heads.is_empty() {
        return Ok(None);
    }
    let group = ChangeGroup {
        repo_id: commit.repo_id.clone(),
        repo_seq,
        commit_id: commit.commit_id.clone(),
        message: commit.message.clone(),
        accepted_at: commit.created_at,
        changes,
        item_heads: heads.into_values().collect(),
    };
    Ok(Some(group))
}

pub fn validate_projected_proposal(
    message: Option<&str>,
    events: &[EventProposal],
    immutable_kinds: &[ImmutableItemKind],
) -> Result<(), SemanticError> {
    if message.is_some_and(|value| value.len() > MAX_MESSAGE_BYTES) {
        return Err(error(
            SemanticErrorCode::MessageTooLarge,
            "message exceeds the UTF-8 byte limit",
        ));
    }
    classify_proposal_events(events, immutable_kinds).map(|_| ())
}

/// Validate a complete mixed proposal and return only its normalized public
/// FILE/FOLDER events in their original relative order.
///
/// Product events are accepted only when their exact omission shape is known.
/// Permitted CREATE and FILE content decorations are stripped from the result.
pub fn project_proposal_events(
    events: &[EventProposal],
    immutable_kinds: &[ImmutableItemKind],
) -> Result<Vec<EventProposal>, SemanticError> {
    let (changes, _) = classify_proposal_events(events, immutable_kinds)?;
    Ok(changes.into_iter().map(|change| change.event).collect())
}

fn classify_proposal_events(
    events: &[EventProposal],
    immutable_kinds: &[ImmutableItemKind],
) -> Result<(Vec<ItemChange>, Vec<ItemHead>), SemanticError> {
    if events.is_empty() {
        return Err(error(
            SemanticErrorCode::TooManyEvents,
            "complete proposal event count is outside the profile limit",
        ));
    }
    validate_proposal_payload_limits(events)?;
    let kind_map = exact_kind_map(immutable_kinds)?;
    let created_types = collect_created_types_from_proposals(events)?;
    let event_targets: BTreeSet<ItemId> = events
        .iter()
        .map(|proposal| require_event_item_id(proposal).cloned())
        .collect::<Result<_, _>>()?;
    let expected_kind_ids: BTreeSet<ItemId> = event_targets
        .difference(&created_types.keys().cloned().collect())
        .cloned()
        .collect();
    if expected_kind_ids != kind_map.keys().cloned().collect() {
        return Err(error(
            SemanticErrorCode::MalformedProfileEvent,
            "immutable item-kind input is not exact",
        ));
    }

    let mut changes = Vec::new();
    let mut heads = BTreeMap::new();
    let mut item_bases = BTreeMap::new();
    let mut seen_items = BTreeSet::new();
    for proposal in events {
        let item_id = require_event_item_id(proposal)?;
        let created_in_commit = created_types.contains_key(item_id);
        if created_in_commit && seen_items.insert(item_id.clone()) && proposal.create_item.is_none()
        {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "the first event for a created item is not CREATE",
            ));
        }
        validate_item_base(
            &mut item_bases,
            item_id,
            proposal.base_seq,
            created_in_commit,
        )?;
        let created_type = created_types.get(item_id);
        let kind = created_type
            .and_then(|item_type| FileFolderKind::from_item_type(item_type))
            .or_else(|| {
                kind_map
                    .get(item_id)
                    .and_then(|item_type| FileFolderKind::from_item_type(item_type))
            });
        validate_prefix(
            &proposal.event_type,
            created_type.or_else(|| kind_map.get(item_id)),
            kind,
        )?;
        if let Some(kind) = kind {
            heads.insert(
                item_id.clone(),
                ItemHead {
                    item_id: item_id.clone(),
                    item_type: kind,
                    head_seq: i64::MAX,
                },
            );
            if let Some(event) = classify_profile_event(proposal, kind)? {
                changes.push(ItemChange {
                    item_id: item_id.clone(),
                    item_type: kind,
                    item_seq: i64::MAX,
                    event,
                });
            }
        }
    }

    Ok((changes, heads.into_values().collect()))
}

fn parse_and_verify_events<'a>(
    commit: &CommitRow,
    repo_seq: i64,
    events: &'a [EventRow],
    ref_events: &[RefEventRow],
    ref_format: bool,
) -> Result<Vec<(&'a EventRow, EventProposal)>, SemanticError> {
    let mut parsed = Vec::with_capacity(events.len());
    let mut aggregate_digest = Sha256::new();
    let mut ref_sequences: BTreeMap<ItemId, (i64, i64)> = BTreeMap::new();
    for (index, row) in events.iter().enumerate() {
        if row.repo_id != commit.repo_id
            || row.commit_id != commit.commit_id
            || row.repo_seq != Some(repo_seq)
            || row.commit_event_idx != index as i32
        {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "event envelope does not match its commit",
            ));
        }
        let digest = compute_sha256_digest_bytes(&row.payload);
        if sha256_prefixed_hex_from_digest_bytes(digest) != row.event_payload_digest.as_str() {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "event payload digest mismatch",
            ));
        }
        if ref_format {
            aggregate_digest.update([COMMIT_EVENT_FAMILY_ITEM]);
        }
        aggregate_digest.update(digest);
        let proposal: EventProposal = serde_json::from_slice(&row.payload).map_err(|failure| {
            error(
                SemanticErrorCode::MalformedProfileEvent,
                format!("event payload does not decode strictly: {failure}"),
            )
        })?;
        if canonical_event_proposal_bytes(&proposal).map_err(core_canonical_error)? != row.payload {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "event payload is not canonical",
            ));
        }
        if proposal.item_id.as_ref() != Some(&row.item_id) || proposal.event_type != row.event_type
        {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "event payload identity does not match its row",
            ));
        }
        parsed.push((row, proposal));
    }
    for (offset, row) in ref_events.iter().enumerate() {
        let index = events.len().checked_add(offset).ok_or_else(|| {
            error(
                SemanticErrorCode::MalformedProfileEvent,
                "commit event index overflows",
            )
        })?;
        if row.repo_id != commit.repo_id
            || row.commit_id != commit.commit_id
            || row.repo_seq != Some(repo_seq)
            || row.commit_event_idx != index as i32
        {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "ref event envelope does not match its commit",
            ));
        }
        let digest = compute_sha256_digest_bytes(&row.payload);
        if sha256_prefixed_hex_from_digest_bytes(digest) != row.event_payload_digest.as_str() {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "ref event payload digest mismatch",
            ));
        }
        let proposal: RefEventProposal =
            serde_json::from_slice(&row.payload).map_err(|failure| {
                error(
                    SemanticErrorCode::MalformedProfileEvent,
                    format!("ref event payload does not decode strictly: {failure}"),
                )
            })?;
        if canonical_ref_event_proposal_bytes(&proposal).map_err(core_canonical_error)?
            != row.payload
        {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "ref event payload is not canonical",
            ));
        }
        if proposal.owner_item_id != row.owner_item_id || proposal.event_type != row.event_type {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "ref event payload identity does not match its row",
            ));
        }
        if proposal.base_ref_seq < 0 || row.ref_seq < 1 {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "ref event sequence is not positive",
            ));
        }
        match ref_sequences.entry(row.owner_item_id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                let expected_ref_seq = match proposal.initial_ref_seq {
                    Some(initial_ref_seq)
                        if proposal.event_type == "FILE_VERSION::LEGACY_BASELINE"
                            && proposal.base_ref_seq == 0
                            && initial_ref_seq >= 1 =>
                    {
                        initial_ref_seq
                    }
                    Some(_) => {
                        return Err(error(
                            SemanticErrorCode::MalformedProfileEvent,
                            "ref event has an invalid initial sequence",
                        ));
                    }
                    None if proposal.event_type == "FILE_VERSION::LEGACY_BASELINE" => {
                        return Err(error(
                            SemanticErrorCode::MalformedProfileEvent,
                            "legacy ref baseline has no initial sequence",
                        ));
                    }
                    None => proposal.base_ref_seq.checked_add(1).ok_or_else(|| {
                        error(
                            SemanticErrorCode::MalformedProfileEvent,
                            "ref event sequence overflows",
                        )
                    })?,
                };
                if row.ref_seq != expected_ref_seq {
                    return Err(error(
                        SemanticErrorCode::MalformedProfileEvent,
                        "ref event sequence does not follow its base",
                    ));
                }
                entry.insert((proposal.base_ref_seq, row.ref_seq));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let (base_ref_seq, previous_ref_seq) = *entry.get();
                if proposal.base_ref_seq != base_ref_seq
                    || proposal.initial_ref_seq.is_some()
                    || proposal.event_type == "FILE_VERSION::LEGACY_BASELINE"
                {
                    return Err(error(
                        SemanticErrorCode::MalformedProfileEvent,
                        "ref events for one owner have inconsistent bases",
                    ));
                }
                let expected_ref_seq = previous_ref_seq.checked_add(1).ok_or_else(|| {
                    error(
                        SemanticErrorCode::MalformedProfileEvent,
                        "ref event sequence overflows",
                    )
                })?;
                if row.ref_seq != expected_ref_seq {
                    return Err(error(
                        SemanticErrorCode::MalformedProfileEvent,
                        "ref event sequence is not contiguous",
                    ));
                }
                entry.get_mut().1 = row.ref_seq;
            }
        }
        aggregate_digest.update([COMMIT_EVENT_FAMILY_REF]);
        aggregate_digest.update(digest);
    }
    let events_digest: [u8; 32] = aggregate_digest.finalize().into();
    if sha256_prefixed_hex_from_digest_bytes(events_digest) != commit.events_digest.as_str() {
        return Err(error(
            SemanticErrorCode::MalformedProfileEvent,
            "commit events_digest mismatch",
        ));
    }
    let commit_bytes = canonical_stored_commit_bytes_for_hash(
        commit.format_version,
        &commit.author_id,
        commit.message.as_deref(),
        commit.parent_commit_id.as_ref(),
        commit.import_job_id.as_deref(),
        events.len() + ref_events.len(),
        &commit.events_digest,
    )
    .map_err(core_canonical_error)?;
    if compute_commit_id(&commit_bytes) != commit.commit_id {
        return Err(error(
            SemanticErrorCode::MalformedProfileEvent,
            "commit_id does not match the canonical envelope",
        ));
    }
    Ok(parsed)
}

fn collect_created_types(
    parsed: &[(&EventRow, EventProposal)],
) -> Result<BTreeMap<ItemId, String>, SemanticError> {
    collect_created_types_from_proposals(
        &parsed
            .iter()
            .map(|(_, proposal)| proposal.clone())
            .collect::<Vec<_>>(),
    )
}

fn collect_created_types_from_proposals(
    events: &[EventProposal],
) -> Result<BTreeMap<ItemId, String>, SemanticError> {
    let mut created = BTreeMap::new();
    for proposal in events {
        if let Some(create) = &proposal.create_item {
            let item_id = require_event_item_id(proposal)?.clone();
            if created.insert(item_id, create.item_type.clone()).is_some() {
                return Err(error(
                    SemanticErrorCode::MalformedProfileEvent,
                    "one item has more than one create event",
                ));
            }
        }
    }
    Ok(created)
}

fn validate_kind_map_exactness(
    parsed: &[(&EventRow, EventProposal)],
    created: &BTreeMap<ItemId, String>,
    kinds: &BTreeMap<ItemId, String>,
) -> Result<(), SemanticError> {
    let targets: BTreeSet<ItemId> = parsed.iter().map(|(row, _)| row.item_id.clone()).collect();
    let created_ids: BTreeSet<ItemId> = created.keys().cloned().collect();
    let expected: BTreeSet<ItemId> = targets.difference(&created_ids).cloned().collect();
    if expected != kinds.keys().cloned().collect() {
        return Err(error(
            SemanticErrorCode::MalformedProfileEvent,
            "immutable item-kind input is not exact",
        ));
    }
    Ok(())
}

fn exact_kind_map(
    immutable_kinds: &[ImmutableItemKind],
) -> Result<BTreeMap<ItemId, String>, SemanticError> {
    let mut kinds = BTreeMap::new();
    for entry in immutable_kinds {
        if kinds
            .insert(entry.item_id.clone(), entry.item_type.clone())
            .is_some()
        {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "immutable item-kind input contains a duplicate",
            ));
        }
    }
    Ok(kinds)
}

fn validate_item_sequence(
    last: &mut BTreeMap<ItemId, i64>,
    item_id: &ItemId,
    item_seq: i64,
    base_seq: Option<i64>,
) -> Result<(), SemanticError> {
    let expected = last
        .get(item_id)
        .copied()
        .map(|previous| previous.saturating_add(1))
        .unwrap_or_else(|| base_seq.unwrap_or(0).saturating_add(1));
    if item_seq <= 0 || item_seq != expected {
        return Err(error(
            SemanticErrorCode::MalformedProfileEvent,
            "item sequence is not positive and contiguous within the commit",
        ));
    }
    last.insert(item_id.clone(), item_seq);
    Ok(())
}

fn validate_item_base(
    bases: &mut BTreeMap<ItemId, Option<i64>>,
    item_id: &ItemId,
    base_seq: Option<i64>,
    created_in_commit: bool,
) -> Result<(), SemanticError> {
    let base_has_valid_shape = if created_in_commit {
        base_seq.is_none()
    } else {
        base_seq.is_some_and(|value| value > 0)
    };
    if !base_has_valid_shape {
        return Err(error(
            SemanticErrorCode::MalformedProfileEvent,
            "event base_seq does not match item creation state",
        ));
    }
    if let Some(previous) = bases.get(item_id) {
        if *previous != base_seq {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "events for one item do not share one base_seq",
            ));
        }
    } else {
        bases.insert(item_id.clone(), base_seq);
    }
    Ok(())
}

fn validate_prefix(
    event_type: &str,
    immutable_item_type: Option<&String>,
    profile_kind: Option<FileFolderKind>,
) -> Result<(), SemanticError> {
    let prefix = event_type.split("::").next().unwrap_or_default();
    let matches = match profile_kind {
        Some(kind) => prefix == kind.as_str() || prefix == "ITEM",
        None => immutable_item_type.is_some_and(|kind| prefix == kind || prefix == "ITEM"),
    };
    if !matches {
        return Err(error(
            SemanticErrorCode::MalformedProfileEvent,
            "event prefix does not match immutable target kind",
        ));
    }
    Ok(())
}

fn classify_profile_event(
    proposal: &EventProposal,
    kind: FileFolderKind,
) -> Result<Option<EventProposal>, SemanticError> {
    let verb = proposal.event_type.as_str();
    if KNOWN_ITEM_OMISSIONS.contains(&verb)
        || (kind == FileFolderKind::File && KNOWN_FILE_OMISSIONS.contains(&verb))
        || (kind == FileFolderKind::Folder && KNOWN_FOLDER_OMISSIONS.contains(&verb))
    {
        require_omission_shape(proposal)?;
        return Ok(None);
    }
    if !is_profile_event_type(kind, verb) {
        return Err(error(
            SemanticErrorCode::UnknownProfileEvent,
            "unknown event targets a FILE/FOLDER item",
        ));
    }

    let expected_prefix = kind.as_str();
    let mut normalized = proposal.clone();
    match verb
        .strip_prefix(expected_prefix)
        .and_then(|rest| rest.strip_prefix("::"))
    {
        Some("CREATE") => {
            require_create_shape(proposal, kind)?;
            normalized.set_metadata = None;
        }
        Some("SET_PARENT") => require_only(proposal, Modifier::Parent)?,
        Some("SET_NAME") => {
            require_only(proposal, Modifier::Name)?;
            compute_name_key(proposal.set_name.as_deref().expect("shape checked")).map_err(
                |failure| {
                    error(
                        SemanticErrorCode::MalformedProfileEvent,
                        format!("invalid profile name: {failure}"),
                    )
                },
            )?;
        }
        Some("SET_CONTENT") if kind == FileFolderKind::File => {
            normalize_content_event(proposal, &mut normalized)?;
        }
        Some("TRASH") => require_lifecycle_shape(proposal, true, false)?,
        Some("RESTORE") => require_lifecycle_shape(proposal, false, false)?,
        Some("TOMBSTONE") => require_lifecycle_shape(proposal, false, true)?,
        _ => {
            return Err(error(
                SemanticErrorCode::UnknownProfileEvent,
                "unknown event targets a FILE/FOLDER item",
            ));
        }
    }
    Ok(Some(normalized))
}

#[derive(Clone, Copy)]
enum Modifier {
    Parent,
    Name,
}

fn require_create_shape(
    proposal: &EventProposal,
    kind: FileFolderKind,
) -> Result<(), SemanticError> {
    let valid = proposal.base_seq.is_none()
        && proposal
            .create_item
            .as_ref()
            .is_some_and(|create| create.item_type == kind.as_str())
        && proposal.set_content.is_none()
        && proposal.set_parent.is_none()
        && proposal.set_name.is_none()
        && proposal.set_tombstoned.is_none()
        && proposal.set_in_trash.is_none()
        && proposal.set_is_federated.is_none()
        && proposal.set_federation_stream_id.is_none();
    require_shape(valid, "malformed CREATE event")?;
    compute_name_key(&proposal.create_item.as_ref().expect("shape checked").name).map_err(
        |failure| {
            error(
                SemanticErrorCode::MalformedProfileEvent,
                format!("invalid profile create name: {failure}"),
            )
        },
    )?;
    Ok(())
}

fn require_only(proposal: &EventProposal, modifier: Modifier) -> Result<(), SemanticError> {
    let valid = proposal.base_seq.is_some_and(|value| value > 0)
        && proposal.create_item.is_none()
        && proposal.set_metadata.is_none()
        && proposal.set_content.is_none()
        && match modifier {
            Modifier::Parent => proposal.set_parent.is_some() && proposal.set_name.is_none(),
            Modifier::Name => proposal.set_parent.is_none() && proposal.set_name.is_some(),
        }
        && proposal.set_tombstoned.is_none()
        && proposal.set_in_trash.is_none()
        && proposal.set_is_federated.is_none()
        && proposal.set_federation_stream_id.is_none();
    require_shape(valid, "malformed structural event")
}

fn normalize_content_event(
    proposal: &EventProposal,
    normalized: &mut EventProposal,
) -> Result<(), SemanticError> {
    let valid_base = proposal.base_seq.is_none_or(|value| value > 0)
        && proposal.create_item.is_none()
        && proposal.set_parent.is_none()
        && proposal.set_name.is_none()
        && proposal.set_tombstoned.is_none()
        && proposal.set_in_trash.is_none()
        && proposal.set_is_federated.is_none()
        && proposal.set_federation_stream_id.is_none()
        && matches!(proposal.set_content, Some(EventContent::Blob { .. }));
    require_shape(valid_base, "malformed FILE::SET_CONTENT event")?;
    let EventContent::Blob { blob_id } = proposal
        .set_content
        .as_ref()
        .expect("content shape checked")
    else {
        unreachable!("content shape checked")
    };
    BlobId::parse(blob_id).map_err(|failure| {
        error(
            SemanticErrorCode::MalformedProfileEvent,
            format!("invalid FILE blob id: {failure}"),
        )
    })?;
    let metadata = proposal
        .set_metadata
        .as_ref()
        .and_then(Value::as_object)
        .ok_or_else(|| {
            error(
                SemanticErrorCode::MalformedProfileEvent,
                "FILE::SET_CONTENT metadata is missing",
            )
        })?;
    if metadata.keys().any(|key| {
        !matches!(
            key.as_str(),
            "content" | "upload_intent_id" | "revision_origin" | "provenance"
        )
    }) {
        return Err(error(
            SemanticErrorCode::MalformedProfileEvent,
            "FILE::SET_CONTENT has an unknown metadata decoration",
        ));
    }
    let size = metadata
        .get("content")
        .and_then(Value::as_object)
        .filter(|content| content.len() == 1)
        .and_then(|content| content.get("size"))
        .and_then(Value::as_u64)
        .filter(|size| *size <= crate::MAX_SAFE_JSON_INTEGER)
        .ok_or_else(|| {
            error(
                SemanticErrorCode::MalformedProfileEvent,
                "FILE::SET_CONTENT has invalid content size metadata",
            )
        })?;
    if let Some(upload_intent_id) = metadata.get("upload_intent_id") {
        require_nonempty_string(upload_intent_id, "upload_intent_id")?;
    }
    if let Some(revision_origin) = metadata.get("revision_origin")
        && !matches!(revision_origin.as_str(), Some("LOCAL" | "FEDERATED"))
    {
        return Err(error(
            SemanticErrorCode::MalformedProfileEvent,
            "revision_origin must be LOCAL or FEDERATED",
        ));
    }
    if let Some(provenance) = metadata.get("provenance") {
        validate_provenance(provenance)?;
    }
    normalized.set_metadata = Some(json!({"content": {"size": size}}));
    Ok(())
}

fn validate_provenance(value: &Value) -> Result<(), SemanticError> {
    let provenance = value.as_object().ok_or_else(|| {
        error(
            SemanticErrorCode::MalformedProfileEvent,
            "provenance must be an object",
        )
    })?;
    if provenance.keys().any(|key| {
        !matches!(
            key.as_str(),
            "source" | "committed_by" | "human_editor_ids" | "primary_human_editor_id"
        )
    }) {
        return Err(error(
            SemanticErrorCode::MalformedProfileEvent,
            "provenance has an unknown field",
        ));
    }
    require_nonempty_string(
        provenance.get("source").ok_or_else(|| {
            error(
                SemanticErrorCode::MalformedProfileEvent,
                "provenance.source is required",
            )
        })?,
        "provenance.source",
    )?;
    require_nonempty_string(
        provenance.get("committed_by").ok_or_else(|| {
            error(
                SemanticErrorCode::MalformedProfileEvent,
                "provenance.committed_by is required",
            )
        })?,
        "provenance.committed_by",
    )?;
    let editor_values = provenance
        .get("human_editor_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            error(
                SemanticErrorCode::MalformedProfileEvent,
                "provenance.human_editor_ids is required",
            )
        })?;
    let mut editors = BTreeSet::new();
    for editor in editor_values {
        let editor = require_nonempty_string(editor, "provenance.human_editor_ids")?;
        if !editors.insert(editor) {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "provenance.human_editor_ids contains a duplicate",
            ));
        }
    }
    if let Some(primary) = provenance.get("primary_human_editor_id")
        && !primary.is_null()
    {
        let primary = require_nonempty_string(primary, "provenance.primary_human_editor_id")?;
        if !editors.contains(primary) {
            return Err(error(
                SemanticErrorCode::MalformedProfileEvent,
                "primary_human_editor_id is not present in human_editor_ids",
            ));
        }
    }
    Ok(())
}

fn require_nonempty_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, SemanticError> {
    value
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            error(
                SemanticErrorCode::MalformedProfileEvent,
                format!("{field} must be a nonempty string"),
            )
        })
}

fn require_lifecycle_shape(
    proposal: &EventProposal,
    in_trash: bool,
    tombstone: bool,
) -> Result<(), SemanticError> {
    let valid = proposal.base_seq.is_some_and(|value| value > 0)
        && proposal.create_item.is_none()
        && proposal.set_metadata.is_none()
        && proposal.set_content.is_none()
        && proposal.set_parent.is_none()
        && proposal.set_name.is_none()
        && proposal.set_tombstoned == tombstone.then_some(true)
        && proposal.set_in_trash == (!tombstone).then_some(in_trash)
        && proposal.set_is_federated.is_none()
        && proposal.set_federation_stream_id.is_none();
    require_shape(valid, "malformed lifecycle event")
}

fn require_omission_shape(proposal: &EventProposal) -> Result<(), SemanticError> {
    let no_profile_modifier = proposal.create_item.is_none()
        && proposal.set_parent.is_none()
        && proposal.set_name.is_none()
        && proposal.set_tombstoned.is_none()
        && proposal.set_in_trash.is_none();
    let product_modifier_is_valid = match proposal.event_type.as_str() {
        "FILE::FEDERATE" | "FOLDER::FEDERATE" => {
            proposal.set_metadata.is_none()
                && proposal.set_content.is_none()
                && matches!(
                    (
                        proposal.set_is_federated,
                        proposal.set_federation_stream_id.as_ref()
                    ),
                    (Some(true), Some(_)) | (Some(false), None)
                )
        }
        "FILE::VERSION::CREATE" => {
            has_metadata(proposal)
                && matches!(proposal.set_content, Some(EventContent::Blob { .. }))
                && no_federation_modifier(proposal)
        }
        "FILE::VERSION::SET_METADATA" => {
            has_metadata(proposal)
                && proposal
                    .set_content
                    .as_ref()
                    .is_none_or(|content| matches!(content, EventContent::Blob { .. }))
                && no_federation_modifier(proposal)
        }
        "FILE::VERSION::TOMBSTONE" => {
            has_metadata(proposal)
                && proposal.set_content.is_none()
                && no_federation_modifier(proposal)
        }
        _ => {
            has_metadata(proposal)
                && proposal.set_content.is_none()
                && no_federation_modifier(proposal)
        }
    };
    let valid = no_profile_modifier && product_modifier_is_valid;
    require_shape(valid, "known omitted event carries a profile modifier")
}

fn has_metadata(proposal: &EventProposal) -> bool {
    proposal
        .set_metadata
        .as_ref()
        .is_some_and(|metadata| !metadata.is_null())
}

fn no_federation_modifier(proposal: &EventProposal) -> bool {
    proposal.set_is_federated.is_none() && proposal.set_federation_stream_id.is_none()
}

fn validate_proposal_payload_limits(events: &[EventProposal]) -> Result<(), SemanticError> {
    for proposal in events {
        let payload = canonical_event_proposal_bytes(proposal).map_err(core_canonical_error)?;
        if payload.len() > MAX_EVENT_PAYLOAD_BYTES {
            return Err(error(
                SemanticErrorCode::TooManyEvents,
                "one event exceeds the payload byte limit",
            ));
        }
    }
    Ok(())
}

fn require_event_item_id(proposal: &EventProposal) -> Result<&ItemId, SemanticError> {
    proposal.item_id.as_ref().ok_or_else(|| {
        error(
            SemanticErrorCode::MalformedProfileEvent,
            "profile event has no materialized item_id",
        )
    })
}

fn require_shape(valid: bool, detail: &'static str) -> Result<(), SemanticError> {
    if !valid {
        return Err(error(SemanticErrorCode::MalformedProfileEvent, detail));
    }
    Ok(())
}

fn core_canonical_error(failure: synck_vcs::errors::CoreError) -> SemanticError {
    error(
        SemanticErrorCode::MalformedProfileEvent,
        format!("Core canonicalization failed: {failure}"),
    )
}


fn error(code: SemanticErrorCode, detail: impl Into<String>) -> SemanticError {
    SemanticError::new(code, detail)
}

#[cfg(test)]
mod tests {
    use synck_vcs::types::{
        CommitId, REF_COMMIT_FORMAT_VERSION, RefEventProposal, RefEventRow, RepoId, Sha256Digest,
        canonical_ref_event_proposal_bytes, canonical_stored_commit_bytes_for_hash, now,
    };

    use super::*;

    #[test]
    fn ref_commit_requires_and_verifies_the_complete_tagged_envelope() {
        let repo_id = RepoId::new();
        let owner_item_id = ItemId::new();
        let proposal = RefEventProposal {
            owner_item_id: owner_item_id.clone(),
            event_type: "FILE_VERSION::CREATE".to_string(),
            set_metadata: Some(serde_json::json!({"file_item_id": owner_item_id.as_str()})),
            set_content: None,
            base_ref_seq: 0,
            initial_ref_seq: None,
        };
        let payload = canonical_ref_event_proposal_bytes(&proposal).unwrap();
        let payload_digest = compute_sha256_digest_bytes(&payload);
        let mut aggregate = Sha256::new();
        aggregate.update([COMMIT_EVENT_FAMILY_REF]);
        aggregate.update(payload_digest);
        let events_digest = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
            aggregate.finalize().into(),
        ))
        .unwrap();
        let author = "author".to_string();
        let commit_bytes = canonical_stored_commit_bytes_for_hash(
            REF_COMMIT_FORMAT_VERSION,
            &author,
            None,
            None,
            None,
            1,
            &events_digest,
        )
        .unwrap();
        let commit_id = compute_commit_id(&commit_bytes);
        let commit = CommitRow {
            repo_id: repo_id.clone(),
            commit_id: commit_id.clone(),
            parent_commit_id: None,
            repo_seq: Some(1),
            format_version: REF_COMMIT_FORMAT_VERSION,
            import_job_id: None,
            author_id: author.clone(),
            committer_id: author,
            message: None,
            occurred_at: None,
            created_at: now(),
            event_count: 1,
            events_digest,
        };
        let row = RefEventRow {
            repo_id,
            owner_item_id,
            ref_seq: 1,
            event_type: proposal.event_type,
            commit_id,
            commit_event_idx: 0,
            repo_seq: Some(1),
            payload,
            event_payload_digest: Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(
                payload_digest,
            ))
            .unwrap(),
            created_at: now(),
        };

        assert!(
            project_commit_events(&commit, &[CommitEventRow::Ref(row.clone())], &[])
                .unwrap()
                .is_none()
        );
        assert_eq!(
            project_commit(&commit, &[], &[]).unwrap_err().code,
            SemanticErrorCode::MalformedProfileEvent
        );

        let mut corrupt_sequence = row.clone();
        corrupt_sequence.ref_seq = 2;
        assert_eq!(
            project_commit_events(&commit, &[CommitEventRow::Ref(corrupt_sequence)], &[],)
                .unwrap_err()
                .code,
            SemanticErrorCode::MalformedProfileEvent
        );

        let mut corrupt = row;
        corrupt.payload.push(b' ');
        assert_eq!(
            project_commit_events(&commit, &[CommitEventRow::Ref(corrupt)], &[])
                .unwrap_err()
                .code,
            SemanticErrorCode::MalformedProfileEvent
        );
    }
}
