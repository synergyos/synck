//! Event sequencing helpers.
//!
//! This module keeps the sequencing rules in one place instead of scattering
//! them through the repo logic.

use std::collections::HashMap;

use crate::types::{ItemId, ItemRow};

/// Stateless helper for allocating repository and item sequences.
#[derive(Default)]
pub struct Sequencer;

impl Sequencer {
    pub fn new() -> Self {
        Self
    }

    /// Assign per-event `item_seq` values for a local append.
    ///
    /// For each event in order, we:
    /// - look up its target item,
    /// - increment that item's `head_seq`,
    /// - and return the new value as the event's `item_seq`.
    pub fn assign_local_item_seqs(
        items: &mut HashMap<ItemId, ItemRow>,
        event_item_ids: &[ItemId],
    ) -> Vec<i64> {
        let mut seqs = Vec::with_capacity(event_item_ids.len());

        for item_id in event_item_ids {
            let row = items
                .get_mut(item_id)
                .expect("item state should exist before sequencing");
            let next_seq = row.head_seq + 1;
            row.head_seq = next_seq;
            seqs.push(next_seq);
        }

        seqs
    }
}
