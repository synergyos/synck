//! Structural and sequencing invariants.
//!
//! This module defines a minimal invariant framework plus a few concrete
//! invariants that we enforce on every write. The goal is to keep the core
//! checks centralized and testable instead of scattering ad-hoc conditionals
//! across the repo logic.
//!
//! Note: Some invariants are structural (parent existence, acyclicity) and are
//! only enforced on commits that can affect structure. Content-only commits
//! should not pay a tree-walk tax.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::errors::CoreError;
use crate::storage::StorageTxn;
use crate::types::{EventProposal, ItemId, ItemRow, LifecycleState, RepoId, SequencingMode};

/// Operation kind for which invariants are evaluated.
#[derive(Clone, Copy, Debug)]
pub enum OperationKind {
    Append,
    IngestFederated,
}

/// Context passed to invariant checks.
pub struct InvariantContext<'a> {
    pub repo_id: &'a RepoId,
    pub op: OperationKind,
}

/// Invariant interface.
pub trait Invariant {
    fn name(&self) -> &'static str;

    fn check(
        &self,
        ctx: &InvariantContext<'_>,
        item_id: &ItemId,
        item_row: &ItemRow,
        item_events: &[EventProposal],
    ) -> Result<(), CoreError>;
}

/// Tombstone finality: once an item is tombstoned, no new events are allowed.
pub struct TombstoneFinality;

impl Invariant for TombstoneFinality {
    fn name(&self) -> &'static str {
        "tombstone_finality"
    }

    fn check(
        &self,
        ctx: &InvariantContext<'_>,
        item_id: &ItemId,
        item_row: &ItemRow,
        _item_events: &[EventProposal],
    ) -> Result<(), CoreError> {
        if matches!(item_row.lifecycle_state, LifecycleState::Tombstoned) {
            return Err(CoreError::ItemTombstoned {
                repo_id: ctx.repo_id.clone(),
                item_id: item_id.clone(),
            });
        }
        Ok(())
    }
}

/// Enforce that all items with a parent reference have a valid parent that is a
/// folder, using the in-commit view first and falling back to storage.
pub async fn check_parent_existence_and_type<T>(
    ctx: &InvariantContext<'_>,
    txn: &mut T,
    items: &HashMap<ItemId, ItemRow>,
) -> Result<(), CoreError>
where
    T: StorageTxn + ?Sized,
{
    let mut missing_parents: HashSet<ItemId> = HashSet::new();
    for row in items.values() {
        if let Some(parent_id) = &row.parent_folder_id
            && !items.contains_key(parent_id)
        {
            missing_parents.insert(parent_id.clone());
        }
    }

    let mut fetched: HashMap<ItemId, ItemRow> = HashMap::new();
    if !missing_parents.is_empty() {
        let ids: Vec<ItemId> = missing_parents.iter().cloned().collect();
        for row in txn.get_items(ctx.repo_id, &ids).await? {
            fetched.insert(row.item_id.clone(), row);
        }
    }

    for row in items.values() {
        if let Some(parent_id) = &row.parent_folder_id {
            let parent_row = if let Some(parent) = items.get(parent_id) {
                parent
            } else {
                fetched
                    .get(parent_id)
                    .ok_or_else(|| CoreError::ParentNotFound {
                        repo_id: ctx.repo_id.clone(),
                        parent_item_id: parent_id.clone(),
                    })?
            };

            if parent_row.item_type != "FOLDER" {
                return Err(CoreError::ParentNotFolder {
                    repo_id: ctx.repo_id.clone(),
                    parent_item_id: parent_id.clone(),
                });
            }
        }
    }

    Ok(())
}

/// Enforce that the folder hierarchy remains acyclic for all touched items.
///
/// We walk parent links for each item using the in-commit view when available
/// and falling back to storage for untouched ancestors. We assume the
/// pre-commit graph is acyclic; any cycle detected here must involve at least
/// one of the items in `items`.
pub async fn check_folder_acyclicity<T>(
    ctx: &InvariantContext<'_>,
    txn: &mut T,
    items: &HashMap<ItemId, ItemRow>,
) -> Result<HashMap<ItemId, ItemRow>, CoreError>
where
    T: StorageTxn + ?Sized,
{
    let mut cache: HashMap<ItemId, ItemRow> = items.clone();
    let mut pending: HashSet<ItemId> = HashSet::new();

    for row in items.values() {
        if let Some(parent_id) = &row.parent_folder_id
            && !cache.contains_key(parent_id)
        {
            pending.insert(parent_id.clone());
        }
    }

    while !pending.is_empty() {
        let batch: Vec<ItemId> = pending.drain().collect();
        let fetched = txn.get_items(ctx.repo_id, &batch).await?;
        for row in fetched {
            let parent_id = row.parent_folder_id.clone();
            let id = row.item_id.clone();
            cache.insert(id.clone(), row);
            if let Some(parent) = parent_id
                && !cache.contains_key(&parent)
            {
                pending.insert(parent);
            }
        }
    }

    for start_id in items.keys() {
        let mut seen = BTreeSet::new();
        let mut current = Some(start_id.clone());
        while let Some(curr) = current {
            if !seen.insert(curr.clone()) {
                return Err(CoreError::CycleDetected {
                    repo_id: ctx.repo_id.clone(),
                    item_id: start_id.clone(),
                });
            }

            let parent = cache
                .get(&curr)
                .and_then(|row| row.parent_folder_id.clone());

            current = parent;
        }
    }

    Ok(cache)
}

/// Require the final parent chain of selected LIVE FILE/FOLDER items to contain
/// only LIVE folders.
///
/// `ancestor_items` is the exact final-view cache returned by
/// `check_folder_acyclicity`, so this check performs no second storage walk.
pub fn check_file_folder_live_parent_closure(
    ctx: &InvariantContext<'_>,
    items: &HashMap<ItemId, ItemRow>,
    ancestor_items: &HashMap<ItemId, ItemRow>,
    affected_item_ids: &BTreeSet<ItemId>,
) -> Result<(), CoreError> {
    for item_id in affected_item_ids {
        let Some(item) = items.get(item_id) else {
            continue;
        };
        if !matches!(item.lifecycle_state, LifecycleState::Live)
            || !matches!(item.item_type.as_str(), "FILE" | "FOLDER")
        {
            continue;
        }

        let mut parent_id = item.parent_folder_id.clone();
        while let Some(current_parent_id) = parent_id {
            let parent = items
                .get(&current_parent_id)
                .or_else(|| ancestor_items.get(&current_parent_id))
                .ok_or_else(|| CoreError::ParentNotFound {
                    repo_id: ctx.repo_id.clone(),
                    parent_item_id: current_parent_id.clone(),
                })?;
            if parent.item_type != "FOLDER" {
                return Err(CoreError::ParentNotFolder {
                    repo_id: ctx.repo_id.clone(),
                    parent_item_id: current_parent_id,
                });
            }
            if !matches!(parent.lifecycle_state, LifecycleState::Live) {
                return Err(CoreError::ParentNotLive {
                    repo_id: ctx.repo_id.clone(),
                    parent_item_id: current_parent_id,
                });
            }
            parent_id = parent.parent_folder_id.clone();
        }
    }
    Ok(())
}

/// Sequencing mode rules: append vs ingest routing.
pub struct SequencingModeRule;

impl Invariant for SequencingModeRule {
    fn name(&self) -> &'static str {
        "sequencing_mode_rule"
    }

    fn check(
        &self,
        ctx: &InvariantContext<'_>,
        item_id: &ItemId,
        item_row: &ItemRow,
        _item_events: &[EventProposal],
    ) -> Result<(), CoreError> {
        match (ctx.op, item_row.sequencing_mode) {
            (OperationKind::Append, SequencingMode::Local) => Ok(()),
            (OperationKind::Append, SequencingMode::Federated) => Err(CoreError::ItemIsFederated {
                repo_id: ctx.repo_id.clone(),
                item_id: item_id.clone(),
            }),
            (OperationKind::IngestFederated, SequencingMode::Federated) => Ok(()),
            (OperationKind::IngestFederated, SequencingMode::Local) => {
                Err(CoreError::ItemNotFederated {
                    repo_id: ctx.repo_id.clone(),
                    item_id: item_id.clone(),
                })
            }
        }
    }
}
