//! Immutable, content-addressed historical repository state.
//!
//! This module owns the format and tree mechanics only. Backend persistence and
//! projection are deliberately separate concerns.

mod codec;
mod projector;
mod storage;
mod tree;

pub use codec::{decode_node, encode_node, hash_node_bytes};
pub use projector::{
    StateProjectionReport, StateProjectionSource, catch_up_state_index,
    catch_up_state_index_bounded, initialize_state_index,
};
pub use storage::{
    ProjectionItem, StagedNamespaceMutation, StateIndexStore, StateProjectionHead,
    StateProjectorStore, StateRootRecord, StateVerificationBatch,
};
pub use tree::{MemoryNodeStore, StateNodeStore, StateTree};

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::types::{CommitId, ItemId, LifecycleState, RepoId, SequencingMode, Timestamp};

pub const LEGACY_FORMAT_VERSION: u8 = 1;
pub const FORMAT_VERSION: u8 = 2;
pub const MAX_ENTRIES: usize = 16;
pub const MIN_ENTRIES: usize = 8;
pub const MAX_INDEX_NODE_BYTES: usize = 65_536;
pub const CHILD_PAGE_SIZE: usize = 256;

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StateNodeId([u8; 32]);

impl StateNodeId {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for StateNodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "sha256:{}", hex::encode(self.0))
    }
}

impl fmt::Display for StateNodeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "sha256:{}", hex::encode(self.0))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StateRootId(StateNodeId);

impl StateRootId {
    pub fn from_node_id(id: StateNodeId) -> Self {
        Self(id)
    }

    pub fn node_id(&self) -> StateNodeId {
        self.0
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

impl fmt::Display for StateRootId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContentPointer {
    pub item_seq: i64,
    pub event_digest: [u8; 32],
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ItemValue {
    pub item_id: ItemId,
    pub item_type: String,
    pub parent_folder_id: Option<ItemId>,
    pub name: String,
    pub name_key: String,
    pub head_seq: i64,
    pub ref_head_seq: i64,
    pub lifecycle_state: LifecycleState,
    pub created_at: Timestamp,
    pub created_by: String,
    pub sequencing_mode: SequencingMode,
    pub federation_stream_id: Option<String>,
    pub content: Option<ContentPointer>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamespaceKey {
    pub parent_folder_id: Option<ItemId>,
    pub name_key: String,
}

impl NamespaceKey {
    pub fn new(parent_folder_id: Option<ItemId>, name_key: String) -> Self {
        Self {
            parent_folder_id,
            name_key,
        }
    }

    /// Backend-independent byte order used by projector scratch tables.
    pub fn canonical_sort_key(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(17 + self.name_key.len());
        match self.parent_folder_id.as_ref() {
            None => bytes.push(0),
            Some(parent) => {
                bytes.push(1);
                bytes.extend_from_slice(&item_id_bytes(parent));
            }
        }
        bytes.extend_from_slice(self.name_key.as_bytes());
        bytes
    }
}

impl Ord for NamespaceKey {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_optional_item_ids(
            self.parent_folder_id.as_ref(),
            other.parent_folder_id.as_ref(),
        )
        .then_with(|| self.name_key.as_bytes().cmp(other.name_key.as_bytes()))
    }
}

impl PartialOrd for NamespaceKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_optional_item_ids(left: Option<&ItemId>, right: Option<&ItemId>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(left), Some(right)) => item_id_bytes(left).cmp(&item_id_bytes(right)),
    }
}

pub(crate) fn item_id_bytes(item_id: &ItemId) -> [u8; 16] {
    Uuid::parse_str(item_id.as_str())
        .expect("validated ItemId must remain a UUID")
        .into_bytes()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootHeader {
    pub repo_id: RepoId,
    pub repo_seq: i64,
    pub commit_id: Option<CommitId>,
    pub item_map_root: StateNodeId,
    pub namespace_root: StateNodeId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StateNode {
    ItemValue(ItemValue),
    ItemLeaf(Vec<(ItemId, StateNodeId)>),
    ItemInternal(Vec<(ItemId, StateNodeId)>),
    NamespaceLeaf(Vec<(NamespaceKey, ItemId)>),
    NamespaceInternal(Vec<(NamespaceKey, StateNodeId)>),
    Root(RootHeader),
}

// Boxing `Put` would add a heap allocation to every state update only to make
// this short-lived internal enum smaller.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ItemMutation {
    Put(ItemValue),
    Remove(ItemId),
}

#[derive(Clone, Debug)]
pub enum NamespaceMutation {
    Put(NamespaceKey, ItemId),
    Remove(NamespaceKey),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildPage {
    pub entries: Vec<(NamespaceKey, ItemValue)>,
    pub next_cursor: Option<NamespaceKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemPage {
    pub entries: Vec<ItemValue>,
    pub next_cursor: Option<ItemId>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum StateError {
    #[error("invalid state encoding: {0}")]
    InvalidEncoding(String),
    #[error("state node hash mismatch: expected {expected}, actual {actual}")]
    HashMismatch {
        expected: StateNodeId,
        actual: StateNodeId,
    },
    #[error("missing state node: {0}")]
    MissingNode(StateNodeId),
    #[error("state storage error: {0}")]
    Storage(String),
    #[error("unexpected state node kind")]
    WrongNodeKind,
    #[error("invalid state tree: {0}")]
    InvalidTree(String),
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::{TimeZone, Utc};

    use super::*;

    const GOLDEN: &[(&str, usize, &str)] = &[
        (
            "010200",
            3,
            "a4cb5298285bc79a31b3188835a97a39ae860dc614cacc651177b159f5db52d0",
        ),
        (
            "010400",
            3,
            "1372b7bbb2d660f3ac2e68d1464a3397d7922510ae2352c4a94b5b72293594dd",
        ),
        (
            "010601890f2a7c0070008000000000000001000000000000000000a4cb5298285bc79a31b3188835a97a39ae860dc614cacc651177b159f5db52d01372b7bbb2d660f3ac2e68d1464a3397d7922510ae2352c4a94b5b72293594dd",
            91,
            "ae432b5fcc6da6311afa69558dca32f86f72e41e23a72b6e43963409c09ae048",
        ),
        (
            "010101890f2a7c0070008000000000000002000000000000000446494c4500000000000000000a5265706f72742e747874000000000000000a7265706f72742e74787400000000000000030100000000649f6c80000000000000000000000005616c69636501000100000000000000022222222222222222222222222222222222222222222222222222222222222222",
            144,
            "f703695a2ed73f0c87845c7f555ed68117b62bfea91d06486ed3843b9b621da5",
        ),
        (
            "01020101890f2a7c0070008000000000000002f703695a2ed73f0c87845c7f555ed68117b62bfea91d06486ed3843b9b621da5",
            51,
            "829cc552e00addd33769fb26acd60ecdfc373cf261c40c1654bb48d0ff6ccf9b",
        ),
        (
            "01040100000a7265706f72742e74787401890f2a7c0070008000000000000002",
            32,
            "c950369b1bcac9dbcfeab3f7231ea0ee9c50a0ed13b9d308c7c8e309f6939205",
        ),
        (
            "01030201890f2a7c0070008000000000000002829cc552e00addd33769fb26acd60ecdfc373cf261c40c1654bb48d0ff6ccf9b01890f2a7c00700080000000000000033333333333333333333333333333333333333333333333333333333333333333",
            99,
            "a51bfd1289a4a5f9f4a4ae59411596cdd403a6e597b1977aae18859c421d1cde",
        ),
        (
            "010601890f2a7c00700080000000000000010000000000000007011111111111111111111111111111111111111111111111111111111111111111829cc552e00addd33769fb26acd60ecdfc373cf261c40c1654bb48d0ff6ccf9bc950369b1bcac9dbcfeab3f7231ea0ee9c50a0ed13b9d308c7c8e309f6939205",
            123,
            "9e66704854ef6d3575dd235c1767ae8cbddeaa8d004cbce534c16117ec13a287",
        ),
    ];

    fn item_id(number: u64) -> ItemId {
        ItemId::parse(format!("01890f2a-7c00-7000-8000-{number:012x}")).unwrap()
    }

    fn item_value(number: u64, head_seq: i64) -> ItemValue {
        let item_id = item_id(number);
        ItemValue {
            item_id,
            item_type: "FILE".to_string(),
            parent_folder_id: None,
            name: format!("file-{number:04}.txt"),
            name_key: format!("file-{number:04}.txt"),
            head_seq,
            ref_head_seq: 0,
            lifecycle_state: LifecycleState::Live,
            created_at: Utc.timestamp_opt(1_688_169_600, 0).unwrap(),
            created_by: "alice".to_string(),
            sequencing_mode: SequencingMode::Local,
            federation_stream_id: None,
            content: Some(ContentPointer {
                item_seq: head_seq,
                event_digest: [number as u8; 32],
            }),
        }
    }

    async fn store_test_node(store: &mut MemoryNodeStore, node: StateNode) -> StateNodeId {
        let bytes = encode_node(&node).unwrap();
        let id = hash_node_bytes(&bytes);
        store.store(id, bytes).await.unwrap();
        id
    }

    async fn reachable_nodes(
        store: &mut MemoryNodeStore,
        roots: impl IntoIterator<Item = StateNodeId>,
    ) -> BTreeSet<StateNodeId> {
        let mut reachable = BTreeSet::new();
        let mut pending: Vec<_> = roots.into_iter().collect();
        while let Some(id) = pending.pop() {
            if !reachable.insert(id) {
                continue;
            }
            match decode_node(&store.load(id).await.unwrap()).unwrap() {
                StateNode::ItemValue(_) | StateNode::NamespaceLeaf(_) => {}
                StateNode::ItemLeaf(entries) | StateNode::ItemInternal(entries) => {
                    pending.extend(entries.into_iter().map(|entry| entry.1));
                }
                StateNode::NamespaceInternal(entries) => {
                    pending.extend(entries.into_iter().map(|entry| entry.1));
                }
                StateNode::Root(root) => {
                    pending.push(root.item_map_root);
                    pending.push(root.namespace_root);
                }
            }
        }
        reachable
    }

    #[test]
    fn legacy_golden_vectors_decode_and_upgrade_to_current_encoding() {
        for (hex_bytes, expected_len, expected_hash) in GOLDEN {
            let bytes = hex::decode(hex_bytes).unwrap();
            assert_eq!(bytes.len(), *expected_len);
            assert_eq!(
                hex::encode(hash_node_bytes(&bytes).as_bytes()),
                *expected_hash
            );
            let node = decode_node(&bytes).unwrap();
            let upgraded = encode_node(&node).unwrap();
            assert_eq!(upgraded[0], FORMAT_VERSION);
            assert_eq!(decode_node(&upgraded).unwrap(), node);
        }
    }

    #[test]
    fn decoder_rejects_truncation_trailing_bytes_and_untrusted_lengths() {
        let bytes = hex::decode(GOLDEN[3].0).unwrap();
        assert!(decode_node(&bytes[..bytes.len() - 1]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_node(&trailing).is_err());

        let mut oversized = vec![FORMAT_VERSION, 1];
        oversized.extend_from_slice(&item_id_bytes(&item_id(2)));
        oversized.extend_from_slice(&u64::MAX.to_be_bytes());
        assert!(decode_node(&oversized).is_err());

        assert!(decode_node(&[FORMAT_VERSION, 0xff]).is_err());

        let mut unpaired_content = hex::decode(GOLDEN[3].0).unwrap();
        let content_tag = unpaired_content.len() - 41;
        unpaired_content[content_tag] = 0;
        assert!(decode_node(&unpaired_content).is_err());

        let mut unknown_content_tag = hex::decode(GOLDEN[3].0).unwrap();
        unknown_content_tag[content_tag] = 2;
        assert!(decode_node(&unknown_content_tag).is_err());
    }

    #[tokio::test]
    async fn item_tree_matches_reference_map_through_deterministic_mutations() {
        let mut store = MemoryNodeStore::default();
        let mut root = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let mut expected = BTreeMap::new();
        let mut state = 0x4d595df4d0f33173_u64;

        for step in 1..=2_000_i64 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let number = 1 + state % 300;
            let mut tree = StateTree::new(&mut store);
            if state & 3 == 0 {
                root = tree.remove_item(root, &item_id(number)).await.unwrap();
                expected.remove(&item_id(number));
            } else {
                let value = item_value(number, step);
                root = tree.put_item(root, value.clone()).await.unwrap();
                expected.insert(value.item_id.clone(), value);
            }
            let actual = StateTree::new(&mut store).all_items(root).await.unwrap();
            assert_eq!(actual, expected);
        }
    }

    #[tokio::test]
    async fn namespace_tree_matches_reference_map_through_deterministic_mutations() {
        let mut store = MemoryNodeStore::default();
        let mut root = StateTree::new(&mut store)
            .empty_namespace_root()
            .await
            .unwrap();
        let mut expected = BTreeMap::new();
        let mut state = 0x9e3779b97f4a7c15_u64;

        for _ in 0..1_200 {
            state = state
                .wrapping_mul(2_862_933_555_777_941_757)
                .wrapping_add(3_037_000_493);
            let number = 1 + state % 240;
            let key = NamespaceKey::new(None, format!("file-{number:04}.txt"));
            let mut tree = StateTree::new(&mut store);
            if state & 3 == 0 {
                root = tree.remove_namespace(root, &key).await.unwrap();
                expected.remove(&key);
            } else {
                let value = item_id(1 + state.rotate_left(17) % 240);
                root = tree
                    .put_namespace(root, key.clone(), value.clone())
                    .await
                    .unwrap();
                expected.insert(key, value);
            }
            assert_eq!(
                StateTree::new(&mut store)
                    .all_namespace(root)
                    .await
                    .unwrap(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn ordered_batch_boundaries_do_not_change_roots() {
        let values: Vec<_> = (1..=180).map(|number| item_value(number, 1)).collect();

        let mut first_store = MemoryNodeStore::default();
        let first_empty = StateTree::new(&mut first_store)
            .empty_item_root()
            .await
            .unwrap();
        let first_root = StateTree::new(&mut first_store)
            .apply_item_mutations(first_empty, values.iter().cloned().map(ItemMutation::Put))
            .await
            .unwrap();

        let mut second_store = MemoryNodeStore::default();
        let mut second_root = StateTree::new(&mut second_store)
            .empty_item_root()
            .await
            .unwrap();
        for batch in values.chunks(17) {
            second_root = StateTree::new(&mut second_store)
                .apply_item_mutations(second_root, batch.iter().cloned().map(ItemMutation::Put))
                .await
                .unwrap();
        }
        assert_eq!(first_root, second_root);
    }

    #[tokio::test]
    async fn bulk_mutation_publishes_only_the_final_reachable_nodes() {
        let mut store = MemoryNodeStore::default();
        let empty = StateTree::new(&mut store).empty_item_root().await.unwrap();
        store.reset_stores();
        let root = StateTree::new(&mut store)
            .apply_item_mutations(
                empty,
                (1..=180).map(|number| ItemMutation::Put(item_value(number, 1))),
            )
            .await
            .unwrap();
        assert_eq!(store.load_count(), 1);
        let reachable = reachable_nodes(&mut store, [root]).await;
        assert_eq!(store.len(), reachable.len() + 1);
        assert_eq!(store.store_count(), reachable.len());
        assert!(!reachable.contains(&empty));
    }

    #[tokio::test]
    async fn ordered_bulk_mutation_keeps_only_a_bounded_transient_cursor() {
        let mut store = MemoryNodeStore::default();
        let empty = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let root = StateTree::new(&mut store)
            .apply_item_mutations(
                empty,
                (1..=4_096).map(|number| ItemMutation::Put(item_value(number, 1))),
            )
            .await
            .unwrap();

        store.reset_peak_transient_nodes();
        store.reset_stores();
        let mutations = (1..=4_096).map(|number| {
            if number % 2 == 0 {
                ItemMutation::Remove(item_id(number))
            } else {
                ItemMutation::Put(item_value(number, 2))
            }
        });
        let root = StateTree::new(&mut store)
            .apply_item_mutations(root, mutations)
            .await
            .unwrap();

        let reachable = reachable_nodes(&mut store, [root]).await;
        assert!(
            store.stored_ids().iter().all(|id| reachable.contains(id)),
            "bulk mutation staged a node outside the final closure"
        );
        assert!(
            store.peak_transient_nodes() <= 32,
            "resident transient nodes={}",
            store.peak_transient_nodes()
        );
        let expected = (1..=4_096)
            .filter(|number| number % 2 != 0)
            .map(|number| (item_id(number), item_value(number, 2)))
            .collect();
        assert_eq!(
            StateTree::new(&mut store).all_items(root).await.unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn ordered_delete_batch_stages_only_its_final_tree() {
        let mut store = MemoryNodeStore::default();
        let empty = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let root = StateTree::new(&mut store)
            .apply_item_mutations(
                empty,
                (1..=512).map(|number| ItemMutation::Put(item_value(number, 1))),
            )
            .await
            .unwrap();

        store.reset_stores();
        let root = StateTree::new(&mut store)
            .apply_item_mutations(
                root,
                (1..=512).map(|number| ItemMutation::Remove(item_id(number))),
            )
            .await
            .unwrap();

        let reachable = reachable_nodes(&mut store, [root]).await;
        assert!(
            store.stored_ids().iter().all(|id| reachable.contains(id)),
            "ordered deletion staged a node outside the final closure"
        );
        assert!(
            StateTree::new(&mut store)
                .all_items(root)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn ordered_insert_batch_stages_only_its_final_tree() {
        let mut store = MemoryNodeStore::default();
        let empty = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let root = StateTree::new(&mut store)
            .apply_item_mutations(
                empty,
                (2..=1_024)
                    .step_by(2)
                    .map(|number| ItemMutation::Put(item_value(number, 1))),
            )
            .await
            .unwrap();

        store.reset_stores();
        let root = StateTree::new(&mut store)
            .apply_item_mutations(
                root,
                (1..1_024)
                    .step_by(2)
                    .map(|number| ItemMutation::Put(item_value(number, 1))),
            )
            .await
            .unwrap();

        let reachable = reachable_nodes(&mut store, [root]).await;
        assert!(
            store.stored_ids().iter().all(|id| reachable.contains(id)),
            "ordered insertion staged a node outside the final closure"
        );
        assert_eq!(
            StateTree::new(&mut store)
                .all_items(root)
                .await
                .unwrap()
                .len(),
            1_024
        );
    }

    #[tokio::test]
    async fn bulk_mutation_is_independent_of_batch_boundaries_for_mixed_changes() {
        let mut bulk_store = MemoryNodeStore::default();
        let mut bulk_root = StateTree::new(&mut bulk_store)
            .empty_item_root()
            .await
            .unwrap();
        let mut point_store = MemoryNodeStore::default();
        let mut point_root = StateTree::new(&mut point_store)
            .empty_item_root()
            .await
            .unwrap();
        for number in 1..=220 {
            let value = item_value(number, 1);
            bulk_root = StateTree::new(&mut bulk_store)
                .put_item(bulk_root, value.clone())
                .await
                .unwrap();
            point_root = StateTree::new(&mut point_store)
                .put_item(point_root, value)
                .await
                .unwrap();
        }

        let mutations: Vec<_> = (1..=280)
            .filter_map(|number| match number % 5 {
                0 => Some(ItemMutation::Remove(item_id(number))),
                1 | 2 => Some(ItemMutation::Put(item_value(number, 2))),
                _ => None,
            })
            .collect();
        bulk_root = StateTree::new(&mut bulk_store)
            .apply_item_mutations(bulk_root, mutations.clone())
            .await
            .unwrap();
        for mutation in mutations {
            point_root = match mutation {
                ItemMutation::Put(value) => StateTree::new(&mut point_store)
                    .put_item(point_root, value)
                    .await
                    .unwrap(),
                ItemMutation::Remove(key) => StateTree::new(&mut point_store)
                    .remove_item(point_root, &key)
                    .await
                    .unwrap(),
            };
        }

        assert_eq!(bulk_root, point_root);
        assert_eq!(
            StateTree::new(&mut bulk_store)
                .all_items(bulk_root)
                .await
                .unwrap(),
            StateTree::new(&mut point_store)
                .all_items(point_root)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn mutation_batch_rejects_missing_roots_and_children_atomically() {
        let mut store = MemoryNodeStore::default();
        let missing = StateNodeId::from_bytes([0x55; 32]);
        assert_eq!(
            StateTree::new(&mut store)
                .apply_item_mutations(missing, [])
                .await,
            Err(StateError::MissingNode(missing))
        );

        let missing_value = StateNodeId::from_bytes([0x66; 32]);
        let staged = StateNode::ItemLeaf(vec![(item_id(1), missing_value)]);
        let staged_bytes = encode_node(&staged).unwrap();
        let staged_id = hash_node_bytes(&staged_bytes);
        store.begin_batch().await.unwrap();
        store.store(staged_id, staged_bytes).await.unwrap();
        assert_eq!(
            store.finish_batch(&[staged_id]).await,
            Err(StateError::MissingNode(missing_value))
        );
        store.abort_batch().await.unwrap();
        assert_eq!(
            store.load(staged_id).await,
            Err(StateError::MissingNode(staged_id))
        );

        let value_id = store_test_node(&mut store, StateNode::ItemValue(item_value(1, 1))).await;
        let valid = StateNode::ItemLeaf(vec![(item_id(1), value_id)]);
        let valid_bytes = encode_node(&valid).unwrap();
        let valid_id = hash_node_bytes(&valid_bytes);
        store.begin_batch().await.unwrap();
        store.store(valid_id, valid_bytes.clone()).await.unwrap();
        store.finish_batch(&[valid_id]).await.unwrap();
        assert_eq!(store.load(valid_id).await.unwrap(), valid_bytes);
    }

    #[tokio::test]
    async fn bulk_mutation_rejects_unsorted_or_duplicate_keys() {
        let mut store = MemoryNodeStore::default();
        let root = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let mutations = [
            ItemMutation::Put(item_value(2, 1)),
            ItemMutation::Put(item_value(1, 1)),
        ];
        assert!(
            StateTree::new(&mut store)
                .apply_item_mutations(root, mutations)
                .await
                .is_err()
        );
        assert_eq!(store.len(), 1);
        assert!(
            StateTree::new(&mut store)
                .apply_item_mutations(root, [ItemMutation::Put(item_value(1, 1))])
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn namespace_pages_have_exact_boundaries_and_no_gaps() {
        for count in [255_u64, 256, 257, 700] {
            let mut store = MemoryNodeStore::default();
            let mut item_root = StateTree::new(&mut store).empty_item_root().await.unwrap();
            item_root = StateTree::new(&mut store)
                .apply_item_mutations(
                    item_root,
                    (1..=count).map(|number| ItemMutation::Put(item_value(number, 1))),
                )
                .await
                .unwrap();
            let mut root = StateTree::new(&mut store)
                .empty_namespace_root()
                .await
                .unwrap();
            let mutations = (1..=count).map(|number| {
                let key = NamespaceKey::new(None, format!("file-{number:04}.txt"));
                NamespaceMutation::Put(key, item_id(number))
            });
            root = StateTree::new(&mut store)
                .apply_namespace_mutations(root, mutations)
                .await
                .unwrap();

            let mut cursor = None;
            let mut seen = Vec::new();
            loop {
                let page = StateTree::new(&mut store)
                    .child_page(item_root, root, None, cursor.as_ref())
                    .await
                    .unwrap();
                assert!(page.entries.len() <= CHILD_PAGE_SIZE);
                seen.extend(page.entries.into_iter().map(|entry| entry.1.item_id));
                let Some(next) = page.next_cursor else {
                    break;
                };
                cursor = Some(next);
            }
            assert_eq!(seen, (1..=count).map(item_id).collect::<Vec<_>>());
        }
    }

    #[tokio::test]
    async fn item_pages_have_exact_boundaries_and_no_gaps() {
        for count in [1_u64, 255, 256, 257, 700] {
            let mut store = MemoryNodeStore::default();
            let mut root = StateTree::new(&mut store).empty_item_root().await.unwrap();
            root = StateTree::new(&mut store)
                .apply_item_mutations(
                    root,
                    (1..=count).map(|number| ItemMutation::Put(item_value(number, 1))),
                )
                .await
                .unwrap();

            let mut cursor = None;
            let mut seen = Vec::new();
            loop {
                let page = StateTree::new(&mut store)
                    .item_page(root, cursor.as_ref(), CHILD_PAGE_SIZE)
                    .await
                    .unwrap();
                assert!(page.entries.len() <= CHILD_PAGE_SIZE);
                seen.extend(page.entries.into_iter().map(|entry| entry.item_id));
                let Some(next) = page.next_cursor else {
                    break;
                };
                cursor = Some(next);
            }
            assert_eq!(seen, (1..=count).map(item_id).collect::<Vec<_>>());
        }
    }

    #[tokio::test]
    async fn namespace_page_stops_after_the_requested_parent_range() {
        let mut store = MemoryNodeStore::default();
        let parent = item_id(900);
        let mut values = Vec::new();
        values.push(ItemMutation::Put(item_value(1, 1)));
        for number in 2..=800 {
            let mut value = item_value(number, 1);
            value.parent_folder_id = Some(parent.clone());
            values.push(ItemMutation::Put(value));
        }
        let empty_items = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let item_root = StateTree::new(&mut store)
            .apply_item_mutations(empty_items, values)
            .await
            .unwrap();

        let empty_namespace = StateTree::new(&mut store)
            .empty_namespace_root()
            .await
            .unwrap();
        let namespace_root = StateTree::new(&mut store)
            .apply_namespace_mutations(
                empty_namespace,
                std::iter::once(NamespaceMutation::Put(
                    NamespaceKey::new(None, "file-0001.txt".to_string()),
                    item_id(1),
                ))
                .chain((2..=800).map(|number| {
                    NamespaceMutation::Put(
                        NamespaceKey::new(Some(parent.clone()), format!("file-{number:04}.txt")),
                        item_id(number),
                    )
                })),
            )
            .await
            .unwrap();

        store.reset_loads();
        let page = StateTree::new(&mut store)
            .child_page(item_root, namespace_root, None, None)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert!(page.next_cursor.is_none());
        assert!(store.load_count() < 32, "loads={}", store.load_count());
    }

    #[tokio::test]
    async fn namespace_range_seek_skips_a_large_middle_interval() {
        let mut store = MemoryNodeStore::default();
        let mut values = Vec::new();
        let mut namespace = Vec::new();
        for number in 1..=4_098 {
            let name = match number {
                1 => "!before".to_string(),
                4_098 => "z-after".to_string(),
                _ => format!("middle-{number:04}"),
            };
            let mut value = item_value(number, 1);
            value.name.clone_from(&name);
            value.name_key.clone_from(&name);
            values.push(ItemMutation::Put(value));
            namespace.push(NamespaceMutation::Put(
                NamespaceKey::new(None, name),
                item_id(number),
            ));
        }
        let empty_items = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let item_root = StateTree::new(&mut store)
            .apply_item_mutations(empty_items, values)
            .await
            .unwrap();
        let empty_namespace = StateTree::new(&mut store)
            .empty_namespace_root()
            .await
            .unwrap();
        let namespace_root = StateTree::new(&mut store)
            .apply_namespace_mutations(empty_namespace, namespace)
            .await
            .unwrap();
        let middle_start = NamespaceKey::new(None, "middle-".to_string());
        let middle_end = NamespaceKey::new(None, "middle.".to_string());

        store.reset_loads();
        let before = StateTree::new(&mut store)
            .child_page_before(
                item_root,
                namespace_root,
                None,
                None,
                &middle_start,
                CHILD_PAGE_SIZE,
            )
            .await
            .unwrap();
        assert_eq!(before.entries.len(), 1);
        assert_eq!(before.entries[0].1.name, "!before");
        assert!(before.next_cursor.is_none());
        assert!(store.load_count() < 32, "loads={}", store.load_count());

        store.reset_loads();
        let after = StateTree::new(&mut store)
            .child_page_from(
                item_root,
                namespace_root,
                None,
                None,
                &middle_end,
                CHILD_PAGE_SIZE,
            )
            .await
            .unwrap();
        assert_eq!(after.entries.len(), 1);
        assert_eq!(after.entries[0].1.name, "z-after");
        assert!(after.next_cursor.is_none());
        assert!(store.load_count() < 32, "loads={}", store.load_count());
    }

    #[tokio::test]
    async fn validation_rejects_namespace_item_disagreement() {
        let mut store = MemoryNodeStore::default();
        let item_root = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let item_root = StateTree::new(&mut store)
            .put_item(item_root, item_value(1, 1))
            .await
            .unwrap();
        let namespace_root = StateTree::new(&mut store)
            .empty_namespace_root()
            .await
            .unwrap();
        let namespace_root = StateTree::new(&mut store)
            .put_namespace(
                namespace_root,
                NamespaceKey::new(None, "wrong.txt".to_string()),
                item_id(1),
            )
            .await
            .unwrap();
        assert!(
            StateTree::new(&mut store)
                .validate(item_root, namespace_root)
                .await
                .is_err()
        );
        assert!(
            StateTree::new(&mut store)
                .child_page(item_root, namespace_root, None, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn validation_rejects_missing_and_non_live_namespace_targets() {
        let mut store = MemoryNodeStore::default();
        let item_root = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let item_root = StateTree::new(&mut store)
            .put_item(item_root, item_value(1, 1))
            .await
            .unwrap();
        let empty_namespace = StateTree::new(&mut store)
            .empty_namespace_root()
            .await
            .unwrap();
        assert!(
            StateTree::new(&mut store)
                .validate(item_root, empty_namespace)
                .await
                .is_err()
        );

        let mut trashed = item_value(1, 1);
        trashed.lifecycle_state = LifecycleState::Trash;
        let empty_items = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let trashed_items = StateTree::new(&mut store)
            .put_item(empty_items, trashed)
            .await
            .unwrap();
        let namespace = StateTree::new(&mut store)
            .empty_namespace_root()
            .await
            .unwrap();
        let namespace = StateTree::new(&mut store)
            .put_namespace(
                namespace,
                NamespaceKey::new(None, "file-0001.txt".to_string()),
                item_id(1),
            )
            .await
            .unwrap();
        assert!(
            StateTree::new(&mut store)
                .validate(trashed_items, namespace)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn root_reader_uses_the_typed_root_identity() {
        let StateNode::Root(expected) = decode_node(&hex::decode(GOLDEN[7].0).unwrap()).unwrap()
        else {
            panic!("golden root decoded as another node kind");
        };
        let mut store = MemoryNodeStore::default();
        let root_id = StateTree::new(&mut store)
            .store_root(expected.clone())
            .await
            .unwrap();
        assert_eq!(
            StateTree::new(&mut store).read_root(root_id).await.unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn reader_rejects_item_leaf_key_value_disagreement() {
        let mut store = MemoryNodeStore::default();
        let value_id = store_test_node(&mut store, StateNode::ItemValue(item_value(2, 1))).await;
        let root = store_test_node(
            &mut store,
            StateNode::ItemLeaf(vec![(item_id(1), value_id)]),
        )
        .await;
        assert!(matches!(
            StateTree::new(&mut store).get_item(root, &item_id(1)).await,
            Err(StateError::InvalidTree(_))
        ));
    }

    #[tokio::test]
    async fn reader_rejects_hash_valid_wrong_child_maximum() {
        let mut store = MemoryNodeStore::default();
        let fake_value = StateNodeId::from_bytes([0x44; 32]);
        let left_entries = (1..=8)
            .map(|number| (item_id(number), fake_value))
            .collect();
        let right_entries = (9..=16)
            .map(|number| (item_id(number), fake_value))
            .collect();
        let left = store_test_node(&mut store, StateNode::ItemLeaf(left_entries)).await;
        let right = store_test_node(&mut store, StateNode::ItemLeaf(right_entries)).await;
        let root = store_test_node(
            &mut store,
            StateNode::ItemInternal(vec![(item_id(7), left), (item_id(16), right)]),
        )
        .await;
        assert!(matches!(
            StateTree::new(&mut store).all_items(root).await,
            Err(StateError::InvalidTree(_))
        ));
    }

    #[tokio::test]
    async fn merkle_diff_validates_the_maximum_of_a_shared_child() {
        let mut store = MemoryNodeStore::default();
        let fake_value = StateNodeId::from_bytes([0x44; 32]);
        let shared = store_test_node(
            &mut store,
            StateNode::ItemLeaf(
                (1..=8)
                    .map(|number| (item_id(number), fake_value))
                    .collect(),
            ),
        )
        .await;
        let left_tail = store_test_node(
            &mut store,
            StateNode::ItemLeaf(
                (9..=16)
                    .map(|number| (item_id(number), fake_value))
                    .collect(),
            ),
        )
        .await;
        let right_tail = store_test_node(
            &mut store,
            StateNode::ItemLeaf(
                (9..=17)
                    .map(|number| (item_id(number), fake_value))
                    .collect(),
            ),
        )
        .await;
        let left = store_test_node(
            &mut store,
            StateNode::ItemInternal(vec![(item_id(7), shared), (item_id(16), left_tail)]),
        )
        .await;
        let right = store_test_node(
            &mut store,
            StateNode::ItemInternal(vec![(item_id(7), shared), (item_id(17), right_tail)]),
        )
        .await;

        assert!(matches!(
            StateTree::new(&mut store).diff_items(left, right).await,
            Err(StateError::InvalidTree(_))
        ));
    }

    #[test]
    fn codec_rejects_invalid_item_semantics_and_namespace_bound() {
        let mut value = item_value(1, 1);
        value.name_key = "wrong".to_string();
        assert!(encode_node(&StateNode::ItemValue(value)).is_err());

        let key = NamespaceKey::new(None, "x".repeat(3061));
        assert!(encode_node(&StateNode::NamespaceLeaf(vec![(key, item_id(1))])).is_err());
    }

    #[tokio::test]
    async fn loads_recompute_the_node_hash() {
        let mut store = MemoryNodeStore::default();
        let root = StateTree::new(&mut store).empty_item_root().await.unwrap();
        store.replace(root, vec![FORMAT_VERSION, 2, 0, 0]);
        assert!(matches!(
            StateTree::new(&mut store).all_items(root).await,
            Err(StateError::HashMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn node_store_rejects_bytes_under_the_wrong_identity() {
        let mut store = MemoryNodeStore::default();
        let bytes = encode_node(&StateNode::ItemLeaf(Vec::new())).unwrap();
        assert!(matches!(
            store
                .store(StateNodeId::from_bytes([0x55; 32]), bytes)
                .await,
            Err(StateError::HashMismatch { .. })
        ));
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn merkle_diff_reports_only_changed_item_keys() {
        let mut store = MemoryNodeStore::default();
        let mut left = StateTree::new(&mut store).empty_item_root().await.unwrap();
        for number in 1..=100 {
            left = StateTree::new(&mut store)
                .put_item(left, item_value(number, 1))
                .await
                .unwrap();
        }
        let mut right = left;
        right = StateTree::new(&mut store)
            .put_item(right, item_value(50, 2))
            .await
            .unwrap();
        right = StateTree::new(&mut store)
            .remove_item(right, &item_id(75))
            .await
            .unwrap();
        assert_eq!(
            StateTree::new(&mut store)
                .diff_items(left, right)
                .await
                .unwrap(),
            BTreeSet::from([item_id(50), item_id(75)])
        );
    }

    #[tokio::test]
    async fn merkle_diff_remains_bounded_when_child_boundaries_change() {
        let mut store = MemoryNodeStore::default();
        let empty = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let left = StateTree::new(&mut store)
            .apply_item_mutations(
                empty,
                (1..=2_048).map(|number| ItemMutation::Put(item_value(number, 1))),
            )
            .await
            .unwrap();
        let right = StateTree::new(&mut store)
            .put_item(left, item_value(3_000, 1))
            .await
            .unwrap();

        store.reset_loads();
        assert_eq!(
            StateTree::new(&mut store)
                .diff_items(left, right)
                .await
                .unwrap(),
            BTreeSet::from([item_id(3_000)])
        );
        assert!(store.load_count() < 200, "loads={}", store.load_count());
    }

    #[tokio::test]
    async fn merkle_diff_matches_reference_for_different_tree_shapes() {
        let mut store = MemoryNodeStore::default();
        let empty = StateTree::new(&mut store).empty_item_root().await.unwrap();
        let left_values: BTreeMap<_, _> = (1..=600)
            .filter(|number| number % 7 != 0)
            .map(|number| {
                let value = item_value(number, 1);
                (value.item_id.clone(), value)
            })
            .collect();
        let right_values: BTreeMap<_, _> = (1..=600)
            .filter(|number| number % 11 != 0)
            .map(|number| {
                let value = item_value(number, if number % 13 == 0 { 2 } else { 1 });
                (value.item_id.clone(), value)
            })
            .collect();
        let left = StateTree::new(&mut store)
            .apply_item_mutations(empty, left_values.values().cloned().map(ItemMutation::Put))
            .await
            .unwrap();
        let right = StateTree::new(&mut store)
            .apply_item_mutations(empty, right_values.values().cloned().map(ItemMutation::Put))
            .await
            .unwrap();
        let expected: BTreeSet<_> = left_values
            .keys()
            .chain(right_values.keys())
            .filter(|item_id| left_values.get(*item_id) != right_values.get(*item_id))
            .cloned()
            .collect();

        assert_eq!(
            StateTree::new(&mut store)
                .diff_items(left, right)
                .await
                .unwrap(),
            expected
        );
    }
}
