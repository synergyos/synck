use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;

use super::{
    CHILD_PAGE_SIZE, ChildPage, ItemMutation, ItemPage, ItemValue, MAX_ENTRIES, MIN_ENTRIES,
    NamespaceKey, NamespaceMutation, RootHeader, StateError, StateNode, StateNodeId, StateRootId,
    decode_node, encode_node, hash_node_bytes,
};
use crate::types::{ItemId, LifecycleState};

type StateFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StateError>> + Send + 'a>>;

#[derive(Clone, Copy)]
struct NamespacePageBounds<'a> {
    parent: Option<&'a ItemId>,
    cursor: Option<&'a NamespaceKey>,
    lower: Option<&'a NamespaceKey>,
    upper: Option<&'a NamespaceKey>,
}

#[async_trait]
pub trait StateNodeStore: Send {
    async fn load(&mut self, id: StateNodeId) -> Result<Vec<u8>, StateError>;

    /// Insert immutable bytes under their precomputed content address.
    /// Repeating the same `(id, bytes)` is idempotent; different bytes at an
    /// existing address must fail.
    async fn store(&mut self, id: StateNodeId, bytes: Vec<u8>) -> Result<(), StateError>;

    /// Start an isolated mutation batch. Nodes written before `finish_batch`
    /// are readable by the batch but are not part of durable state.
    async fn begin_batch(&mut self) -> Result<(), StateError>;

    /// Publish only staged nodes reachable from `roots` and discard the rest.
    /// Every referenced unstaged node must already exist in durable storage.
    async fn finish_batch(&mut self, roots: &[StateNodeId]) -> Result<(), StateError>;

    /// Discard every node staged by the active batch.
    async fn abort_batch(&mut self) -> Result<(), StateError>;

    #[cfg(test)]
    fn observe_transient_nodes(&mut self, _resident: usize) {}
}

#[derive(Default)]
pub struct MemoryNodeStore {
    nodes: HashMap<StateNodeId, Vec<u8>>,
    staged: Option<HashMap<StateNodeId, Vec<u8>>>,
    #[cfg(test)]
    loads: usize,
    #[cfg(test)]
    stores: usize,
    #[cfg(test)]
    stored_ids: Vec<StateNodeId>,
    #[cfg(test)]
    peak_transient_nodes: usize,
}

impl MemoryNodeStore {
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn reset_loads(&mut self) {
        self.loads = 0;
    }

    #[cfg(test)]
    pub(crate) fn load_count(&self) -> usize {
        self.loads
    }

    #[cfg(test)]
    pub(crate) fn reset_stores(&mut self) {
        self.stores = 0;
        self.stored_ids.clear();
    }

    #[cfg(test)]
    pub(crate) fn store_count(&self) -> usize {
        self.stores
    }

    #[cfg(test)]
    pub(crate) fn stored_ids(&self) -> &[StateNodeId] {
        &self.stored_ids
    }

    #[cfg(test)]
    pub(crate) fn reset_peak_transient_nodes(&mut self) {
        self.peak_transient_nodes = 0;
    }

    #[cfg(test)]
    pub(crate) fn peak_transient_nodes(&self) -> usize {
        self.peak_transient_nodes
    }

    #[cfg(test)]
    pub(crate) fn replace(&mut self, id: StateNodeId, bytes: Vec<u8>) {
        self.nodes.insert(id, bytes);
    }
}

#[async_trait]
impl StateNodeStore for MemoryNodeStore {
    async fn load(&mut self, id: StateNodeId) -> Result<Vec<u8>, StateError> {
        #[cfg(test)]
        {
            self.loads += 1;
        }
        if let Some(bytes) = self.staged.as_ref().and_then(|staged| staged.get(&id)) {
            return Ok(bytes.clone());
        }
        self.nodes
            .get(&id)
            .cloned()
            .ok_or(StateError::MissingNode(id))
    }

    async fn store(&mut self, id: StateNodeId, bytes: Vec<u8>) -> Result<(), StateError> {
        #[cfg(test)]
        {
            self.stores += 1;
            self.stored_ids.push(id);
        }
        let actual = hash_node_bytes(&bytes);
        if actual != id {
            return Err(StateError::HashMismatch {
                expected: id,
                actual,
            });
        }
        let existing = self
            .staged
            .as_ref()
            .and_then(|staged| staged.get(&id))
            .or_else(|| self.nodes.get(&id));
        match existing {
            Some(existing) if existing != &bytes => Err(StateError::InvalidTree(
                "same node hash has different bytes".to_string(),
            )),
            Some(_) => Ok(()),
            None => {
                if let Some(staged) = self.staged.as_mut() {
                    staged.insert(id, bytes);
                } else {
                    self.nodes.insert(id, bytes);
                }
                Ok(())
            }
        }
    }

    async fn begin_batch(&mut self) -> Result<(), StateError> {
        if self.staged.is_some() {
            return Err(StateError::Storage(
                "state-node mutation batch is already active".to_string(),
            ));
        }
        self.staged = Some(HashMap::new());
        Ok(())
    }

    async fn finish_batch(&mut self, roots: &[StateNodeId]) -> Result<(), StateError> {
        let staged = self.staged.as_ref().ok_or_else(|| {
            StateError::Storage("state-node mutation batch is not active".to_string())
        })?;
        let mut reachable = BTreeSet::new();
        let mut pending = roots.to_vec();
        while let Some(id) = pending.pop() {
            let Some(bytes) = staged.get(&id) else {
                if !self.nodes.contains_key(&id) {
                    return Err(StateError::MissingNode(id));
                }
                continue;
            };
            if !reachable.insert(id) {
                continue;
            }
            match decode_node(bytes)? {
                StateNode::ItemValue(_) | StateNode::NamespaceLeaf(_) => {}
                StateNode::ItemLeaf(entries) => {
                    pending.extend(entries.into_iter().map(|entry| entry.1));
                }
                StateNode::ItemInternal(entries) => {
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
        let staged = self.staged.take().expect("batch presence checked above");
        for id in reachable {
            let bytes = staged
                .get(&id)
                .expect("reachable staged node remains in the batch")
                .clone();
            self.nodes.insert(id, bytes);
        }
        Ok(())
    }

    async fn abort_batch(&mut self) -> Result<(), StateError> {
        if self.staged.take().is_none() {
            return Err(StateError::Storage(
                "state-node mutation batch is not active".to_string(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn observe_transient_nodes(&mut self, resident: usize) {
        self.peak_transient_nodes = self.peak_transient_nodes.max(resident);
    }
}

pub struct StateTree<'a, S> {
    store: &'a mut S,
}

impl<'a, S: StateNodeStore> StateTree<'a, S> {
    pub fn new(store: &'a mut S) -> Self {
        Self { store }
    }

    pub async fn empty_item_root(&mut self) -> Result<StateNodeId, StateError> {
        self.store_node(StateNode::ItemLeaf(Vec::new())).await
    }

    pub async fn empty_namespace_root(&mut self) -> Result<StateNodeId, StateError> {
        self.store_node(StateNode::NamespaceLeaf(Vec::new())).await
    }

    pub async fn store_root(&mut self, root: RootHeader) -> Result<StateRootId, StateError> {
        self.store_node(StateNode::Root(root))
            .await
            .map(StateRootId::from_node_id)
    }

    pub async fn read_root(&mut self, id: StateRootId) -> Result<RootHeader, StateError> {
        match self.load_node(id.node_id()).await? {
            StateNode::Root(root) => Ok(root),
            _ => Err(StateError::WrongNodeKind),
        }
    }

    pub async fn put_item(
        &mut self,
        root: StateNodeId,
        value: ItemValue,
    ) -> Result<StateNodeId, StateError> {
        let key = value.item_id.clone();
        let value_id = self.store_node(StateNode::ItemValue(value)).await?;
        apply_bulk::<ItemMap, _>(self.store, root, [BulkMutation::Put(key, value_id)]).await
    }

    pub async fn remove_item(
        &mut self,
        root: StateNodeId,
        item_id: &ItemId,
    ) -> Result<StateNodeId, StateError> {
        apply_bulk::<ItemMap, _>(self.store, root, [BulkMutation::Remove(item_id.clone())]).await
    }

    pub async fn apply_item_mutations(
        &mut self,
        root: StateNodeId,
        mutations: impl IntoIterator<Item = ItemMutation>,
    ) -> Result<StateNodeId, StateError> {
        self.store.begin_batch().await?;
        let result = async {
            let mut root = TransientRef::Stored(root);
            let mut previous = None;
            for mutation in mutations {
                match mutation {
                    ItemMutation::Put(value) => {
                        let key = value.item_id.clone();
                        prepare_ordered_mutation::<ItemMap, S>(
                            self.store,
                            &mut root,
                            &mut previous,
                            &key,
                            "item",
                        )
                        .await?;
                        let value = self.store_node(StateNode::ItemValue(value)).await?;
                        root = apply_bulk_mutation::<ItemMap, S>(
                            self.store,
                            root,
                            BulkMutation::Put(key, value),
                        )
                        .await?;
                    }
                    ItemMutation::Remove(key) => {
                        prepare_ordered_mutation::<ItemMap, S>(
                            self.store,
                            &mut root,
                            &mut previous,
                            &key,
                            "item",
                        )
                        .await?;
                        root = apply_bulk_mutation::<ItemMap, S>(
                            self.store,
                            root,
                            BulkMutation::Remove(key),
                        )
                        .await?;
                    }
                }
            }
            freeze_transient::<ItemMap, S>(self.store, root).await
        }
        .await;
        let root = match result {
            Ok(root) => root,
            Err(error) => {
                self.store.abort_batch().await?;
                return Err(error);
            }
        };
        if let Err(error) = self.store.finish_batch(&[root]).await {
            self.store.abort_batch().await?;
            return Err(error);
        }
        Ok(root)
    }

    pub async fn get_item(
        &mut self,
        root: StateNodeId,
        item_id: &ItemId,
    ) -> Result<Option<ItemValue>, StateError> {
        let Some(value_id) = lookup::<ItemMap, _>(self.store, root, item_id).await? else {
            return Ok(None);
        };
        match self.load_node(value_id).await? {
            StateNode::ItemValue(value) if &value.item_id == item_id => Ok(Some(value)),
            StateNode::ItemValue(_) => Err(StateError::InvalidTree(
                "item leaf key does not match item value".to_string(),
            )),
            _ => Err(StateError::WrongNodeKind),
        }
    }

    pub async fn put_namespace(
        &mut self,
        root: StateNodeId,
        key: NamespaceKey,
        item_id: ItemId,
    ) -> Result<StateNodeId, StateError> {
        apply_bulk::<NamespaceMap, _>(self.store, root, [BulkMutation::Put(key, item_id)]).await
    }

    pub async fn remove_namespace(
        &mut self,
        root: StateNodeId,
        key: &NamespaceKey,
    ) -> Result<StateNodeId, StateError> {
        apply_bulk::<NamespaceMap, _>(self.store, root, [BulkMutation::Remove(key.clone())]).await
    }

    pub async fn apply_namespace_mutations(
        &mut self,
        root: StateNodeId,
        mutations: impl IntoIterator<Item = NamespaceMutation>,
    ) -> Result<StateNodeId, StateError> {
        self.store.begin_batch().await?;
        let result = async {
            let mut root = TransientRef::Stored(root);
            let mut previous = None;
            for mutation in mutations {
                let (key, mutation) = match mutation {
                    NamespaceMutation::Put(key, item_id) => {
                        let mutation = BulkMutation::Put(key.clone(), item_id);
                        (key, mutation)
                    }
                    NamespaceMutation::Remove(key) => {
                        let mutation = BulkMutation::Remove(key.clone());
                        (key, mutation)
                    }
                };
                prepare_ordered_mutation::<NamespaceMap, S>(
                    self.store,
                    &mut root,
                    &mut previous,
                    &key,
                    "namespace",
                )
                .await?;
                root = apply_bulk_mutation::<NamespaceMap, S>(self.store, root, mutation).await?;
            }
            freeze_transient::<NamespaceMap, S>(self.store, root).await
        }
        .await;
        let root = match result {
            Ok(root) => root,
            Err(error) => {
                self.store.abort_batch().await?;
                return Err(error);
            }
        };
        if let Err(error) = self.store.finish_batch(&[root]).await {
            self.store.abort_batch().await?;
            return Err(error);
        }
        Ok(root)
    }

    pub async fn get_namespace(
        &mut self,
        root: StateNodeId,
        key: &NamespaceKey,
    ) -> Result<Option<ItemId>, StateError> {
        lookup::<NamespaceMap, _>(self.store, root, key).await
    }

    pub async fn all_items(
        &mut self,
        root: StateNodeId,
    ) -> Result<BTreeMap<ItemId, ItemValue>, StateError> {
        let entries = collect::<ItemMap, _>(self.store, root).await?;
        let mut values = BTreeMap::new();
        for (item_id, value_id) in entries {
            let value = match self.load_node(value_id).await? {
                StateNode::ItemValue(value) if value.item_id == item_id => value,
                StateNode::ItemValue(_) => {
                    return Err(StateError::InvalidTree(
                        "item leaf key does not match item value".to_string(),
                    ));
                }
                _ => return Err(StateError::WrongNodeKind),
            };
            values.insert(item_id, value);
        }
        Ok(values)
    }

    pub async fn all_namespace(
        &mut self,
        root: StateNodeId,
    ) -> Result<BTreeMap<NamespaceKey, ItemId>, StateError> {
        Ok(collect::<NamespaceMap, _>(self.store, root)
            .await?
            .into_iter()
            .collect())
    }

    /// Read one bounded item-id-ordered page without materializing the
    /// repository-wide item map.
    pub async fn item_page(
        &mut self,
        item_root: StateNodeId,
        cursor: Option<&ItemId>,
        limit: usize,
    ) -> Result<ItemPage, StateError> {
        if limit == 0 || limit > CHILD_PAGE_SIZE {
            return Err(StateError::InvalidTree(format!(
                "item page limit must be between 1 and {CHILD_PAGE_SIZE}"
            )));
        }
        let mut output = Vec::with_capacity(limit + 1);
        collect_item_page(self.store, item_root, cursor, limit + 1, &mut output).await?;
        let has_more = output.len() > limit;
        output.truncate(limit);
        let mut entries = Vec::with_capacity(output.len());
        for (item_id, value_id) in output {
            let value = match self.load_node(value_id).await? {
                StateNode::ItemValue(value) if value.item_id == item_id => value,
                StateNode::ItemValue(_) => {
                    return Err(StateError::InvalidTree(
                        "item leaf key does not match item value".to_string(),
                    ));
                }
                _ => return Err(StateError::WrongNodeKind),
            };
            entries.push(value);
        }
        let next_cursor = has_more.then(|| {
            entries
                .last()
                .expect("full item page has a last entry")
                .item_id
                .clone()
        });
        Ok(ItemPage {
            entries,
            next_cursor,
        })
    }

    pub async fn child_page(
        &mut self,
        item_root: StateNodeId,
        namespace_root: StateNodeId,
        parent_folder_id: Option<&ItemId>,
        cursor: Option<&NamespaceKey>,
    ) -> Result<ChildPage, StateError> {
        self.child_page_in_range(
            item_root,
            namespace_root,
            NamespacePageBounds {
                parent: parent_folder_id,
                cursor,
                lower: None,
                upper: None,
            },
            CHILD_PAGE_SIZE,
        )
        .await
    }

    /// Read at most `limit` children after `cursor` and strictly before the
    /// required upper bound.
    pub async fn child_page_before(
        &mut self,
        item_root: StateNodeId,
        namespace_root: StateNodeId,
        parent_folder_id: Option<&ItemId>,
        cursor: Option<&NamespaceKey>,
        upper_bound: &NamespaceKey,
        limit: usize,
    ) -> Result<ChildPage, StateError> {
        self.child_page_in_range(
            item_root,
            namespace_root,
            NamespacePageBounds {
                parent: parent_folder_id,
                cursor,
                lower: None,
                upper: Some(upper_bound),
            },
            limit,
        )
        .await
    }

    /// Read at most `limit` children after `cursor` and at or after the
    /// required lower bound.
    pub async fn child_page_from(
        &mut self,
        item_root: StateNodeId,
        namespace_root: StateNodeId,
        parent_folder_id: Option<&ItemId>,
        cursor: Option<&NamespaceKey>,
        lower_bound: &NamespaceKey,
        limit: usize,
    ) -> Result<ChildPage, StateError> {
        self.child_page_in_range(
            item_root,
            namespace_root,
            NamespacePageBounds {
                parent: parent_folder_id,
                cursor,
                lower: Some(lower_bound),
                upper: None,
            },
            limit,
        )
        .await
    }

    async fn child_page_in_range(
        &mut self,
        item_root: StateNodeId,
        namespace_root: StateNodeId,
        bounds: NamespacePageBounds<'_>,
        limit: usize,
    ) -> Result<ChildPage, StateError> {
        if limit == 0 || limit > CHILD_PAGE_SIZE {
            return Err(StateError::InvalidTree(format!(
                "namespace page limit must be between 1 and {CHILD_PAGE_SIZE}"
            )));
        }
        if bounds
            .cursor
            .is_some_and(|cursor| cursor.parent_folder_id.as_ref() != bounds.parent)
        {
            return Err(StateError::InvalidTree(
                "namespace cursor belongs to another parent".to_string(),
            ));
        }
        if bounds
            .upper
            .is_some_and(|upper_bound| upper_bound.parent_folder_id.as_ref() != bounds.parent)
        {
            return Err(StateError::InvalidTree(
                "namespace upper bound belongs to another parent".to_string(),
            ));
        }
        if bounds
            .lower
            .is_some_and(|lower_bound| lower_bound.parent_folder_id.as_ref() != bounds.parent)
        {
            return Err(StateError::InvalidTree(
                "namespace lower bound belongs to another parent".to_string(),
            ));
        }
        if bounds
            .lower
            .zip(bounds.upper)
            .is_some_and(|(lower_bound, upper_bound)| lower_bound >= upper_bound)
        {
            return Err(StateError::InvalidTree(
                "namespace lower bound must precede upper bound".to_string(),
            ));
        }
        let mut output = Vec::with_capacity(limit + 1);
        collect_namespace_page(self.store, namespace_root, bounds, limit + 1, &mut output).await?;
        let has_more = output.len() > limit;
        output.truncate(limit);
        let mut entries = Vec::with_capacity(output.len());
        for (key, item_id) in output {
            let Some(value) = self.get_item(item_root, &item_id).await? else {
                return Err(StateError::InvalidTree(
                    "namespace entry references a missing item".to_string(),
                ));
            };
            if value.lifecycle_state != LifecycleState::Live
                || value.parent_folder_id != key.parent_folder_id
                || value.name_key.as_ref() != Some(&key.name_key)
            {
                return Err(StateError::InvalidTree(
                    "namespace entry disagrees with item state".to_string(),
                ));
            }
            entries.push((key, value));
        }
        let next_cursor = has_more.then(|| {
            entries
                .last()
                .expect("full page has a last entry")
                .0
                .clone()
        });
        Ok(ChildPage {
            entries,
            next_cursor,
        })
    }

    pub async fn validate(
        &mut self,
        item_root: StateNodeId,
        namespace_root: StateNodeId,
    ) -> Result<(), StateError> {
        self.validated_state(item_root, namespace_root)
            .await
            .map(|_| ())
    }

    /// Read and validate the complete logical state without loading either map
    /// twice. Full-tree consumers need both maps to prove the live-namespace
    /// bijection before materializing paths.
    pub async fn validated_state(
        &mut self,
        item_root: StateNodeId,
        namespace_root: StateNodeId,
    ) -> Result<(BTreeMap<ItemId, ItemValue>, BTreeMap<NamespaceKey, ItemId>), StateError> {
        let items = self.all_items(item_root).await?;
        let namespace = self.all_namespace(namespace_root).await?;
        let mut expected = BTreeMap::new();
        for value in items.values() {
            if value.lifecycle_state == LifecycleState::Live
                && let Some(name_key) = &value.name_key
            {
                let key = NamespaceKey::new(value.parent_folder_id.clone(), name_key.clone());
                if expected.insert(key, value.item_id.clone()).is_some() {
                    return Err(StateError::InvalidTree(
                        "duplicate live namespace key".to_string(),
                    ));
                }
            }
        }
        if expected != namespace {
            return Err(StateError::InvalidTree(
                "live items and namespace are not a bijection".to_string(),
            ));
        }
        Ok((items, namespace))
    }

    pub async fn diff_items(
        &mut self,
        left: StateNodeId,
        right: StateNodeId,
    ) -> Result<BTreeSet<ItemId>, StateError> {
        diff::<ItemMap, _>(self.store, left, right).await
    }

    pub async fn diff_namespace(
        &mut self,
        left: StateNodeId,
        right: StateNodeId,
    ) -> Result<BTreeSet<NamespaceKey>, StateError> {
        diff::<NamespaceMap, _>(self.store, left, right).await
    }

    async fn load_node(&mut self, id: StateNodeId) -> Result<StateNode, StateError> {
        load_node(self.store, id).await
    }

    async fn store_node(&mut self, node: StateNode) -> Result<StateNodeId, StateError> {
        store_node(self.store, node).await
    }
}

trait Map: Send {
    type Key: Clone + Ord + Send + Sync;
    type Value: Clone + Eq + Send + Sync;

    fn decode(node: StateNode) -> Result<TreeNode<Self::Key, Self::Value>, StateError>;
    fn encode(node: TreeNode<Self::Key, Self::Value>) -> StateNode;
}

struct ItemMap;

impl Map for ItemMap {
    type Key = ItemId;
    type Value = StateNodeId;

    fn decode(node: StateNode) -> Result<TreeNode<Self::Key, Self::Value>, StateError> {
        match node {
            StateNode::ItemLeaf(entries) => Ok(TreeNode::Leaf(entries)),
            StateNode::ItemInternal(entries) => Ok(TreeNode::Internal(entries)),
            _ => Err(StateError::WrongNodeKind),
        }
    }

    fn encode(node: TreeNode<Self::Key, Self::Value>) -> StateNode {
        match node {
            TreeNode::Leaf(entries) => StateNode::ItemLeaf(entries),
            TreeNode::Internal(entries) => StateNode::ItemInternal(entries),
        }
    }
}

struct NamespaceMap;

impl Map for NamespaceMap {
    type Key = NamespaceKey;
    type Value = ItemId;

    fn decode(node: StateNode) -> Result<TreeNode<Self::Key, Self::Value>, StateError> {
        match node {
            StateNode::NamespaceLeaf(entries) => Ok(TreeNode::Leaf(entries)),
            StateNode::NamespaceInternal(entries) => Ok(TreeNode::Internal(entries)),
            _ => Err(StateError::WrongNodeKind),
        }
    }

    fn encode(node: TreeNode<Self::Key, Self::Value>) -> StateNode {
        match node {
            TreeNode::Leaf(entries) => StateNode::NamespaceLeaf(entries),
            TreeNode::Internal(entries) => StateNode::NamespaceInternal(entries),
        }
    }
}

enum TreeNode<K, V> {
    Leaf(Vec<(K, V)>),
    Internal(Vec<(K, StateNodeId)>),
}

impl<K, V> TreeNode<K, V> {
    fn len(&self) -> usize {
        match self {
            Self::Leaf(entries) => entries.len(),
            Self::Internal(entries) => entries.len(),
        }
    }
}

enum BulkMutation<K, V> {
    Put(K, V),
    Remove(K),
}

impl<K, V> BulkMutation<K, V> {
    fn key(&self) -> &K {
        match self {
            Self::Put(key, _) | Self::Remove(key) => key,
        }
    }
}

enum TransientRef<K, V> {
    Stored(StateNodeId),
    Loaded {
        original: Option<StateNodeId>,
        dirty: bool,
        node: TransientNode<K, V>,
    },
}

enum TransientNode<K, V> {
    Leaf(Vec<(K, V)>),
    Internal(Vec<TransientChild<K, V>>),
}

struct TransientChild<K, V> {
    max: K,
    child: TransientRef<K, V>,
}

struct TransientDelete {
    changed: bool,
    empty: bool,
    underflow: bool,
}

async fn apply_bulk<M: Map, S: StateNodeStore>(
    store: &mut S,
    root: StateNodeId,
    mutations: impl IntoIterator<Item = BulkMutation<M::Key, M::Value>>,
) -> Result<StateNodeId, StateError> {
    let mut root = TransientRef::Stored(root);
    let mut previous = None;
    for mutation in mutations {
        let key = mutation.key().clone();
        prepare_ordered_mutation::<M, S>(store, &mut root, &mut previous, &key, "bulk").await?;
        root = apply_bulk_mutation::<M, S>(store, root, mutation).await?;
    }
    freeze_transient::<M, S>(store, root).await
}

async fn prepare_ordered_mutation<M: Map, S: StateNodeStore>(
    store: &mut S,
    root: &mut TransientRef<M::Key, M::Value>,
    previous: &mut Option<M::Key>,
    key: &M::Key,
    map_name: &str,
) -> Result<(), StateError> {
    if previous.as_ref().is_some_and(|previous| previous >= key) {
        return Err(StateError::InvalidTree(format!(
            "{map_name} mutations are not strictly increasing"
        )));
    }
    compact_transient_for_key::<M, S>(store, root, key).await?;
    *previous = Some(key.clone());
    Ok(())
}

async fn apply_bulk_mutation<M: Map, S: StateNodeStore>(
    store: &mut S,
    mut root: TransientRef<M::Key, M::Value>,
    mutation: BulkMutation<M::Key, M::Value>,
) -> Result<TransientRef<M::Key, M::Value>, StateError> {
    match mutation {
        BulkMutation::Put(key, value) => {
            let edit = transient_insert::<M, S>(store, &mut root, key, value, true).await?;
            if let Some(right) = edit.right {
                let left_max = transient_ref_max(&root).ok_or_else(|| {
                    StateError::InvalidTree("split produced an empty left node".to_string())
                })?;
                root = TransientRef::Loaded {
                    original: None,
                    dirty: true,
                    node: TransientNode::Internal(vec![
                        TransientChild {
                            max: left_max,
                            child: root,
                        },
                        right,
                    ]),
                };
            }
        }
        BulkMutation::Remove(key) => {
            let edit = transient_delete::<M, S>(store, &mut root, &key, true).await?;
            if edit.changed {
                if edit.empty {
                    root = TransientRef::Loaded {
                        original: None,
                        dirty: true,
                        node: TransientNode::Leaf(Vec::new()),
                    };
                } else {
                    collapse_transient_root::<M, S>(store, &mut root).await?;
                }
            }
        }
    }
    #[cfg(test)]
    store.observe_transient_nodes(transient_loaded_nodes(&root));
    Ok(root)
}

#[cfg(test)]
fn transient_loaded_nodes<K, V>(reference: &TransientRef<K, V>) -> usize {
    match reference {
        TransientRef::Stored(_) => 0,
        TransientRef::Loaded {
            node: TransientNode::Leaf(_),
            ..
        } => 1,
        TransientRef::Loaded {
            node: TransientNode::Internal(entries),
            ..
        } => {
            1 + entries
                .iter()
                .map(|entry| transient_loaded_nodes(&entry.child))
                .sum::<usize>()
        }
    }
}

struct TransientInsert<K, V> {
    right: Option<TransientChild<K, V>>,
}

fn transient_insert<'a, M: Map + 'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    reference: &'a mut TransientRef<M::Key, M::Value>,
    key: M::Key,
    value: M::Value,
    is_root: bool,
) -> StateFuture<'a, TransientInsert<M::Key, M::Value>> {
    Box::pin(async move {
        load_transient_ref::<M, S>(store, reference, is_root).await?;
        let TransientRef::Loaded { dirty, node, .. } = reference else {
            unreachable!("transient reference was loaded")
        };
        match node {
            TransientNode::Leaf(entries) => {
                let changed = match entries.binary_search_by(|entry| entry.0.cmp(&key)) {
                    Ok(index) if entries[index].1 == value => false,
                    Ok(index) => {
                        entries[index].1 = value;
                        true
                    }
                    Err(index) => {
                        entries.insert(index, (key, value));
                        true
                    }
                };
                if !changed {
                    return Ok(TransientInsert { right: None });
                }
                *dirty = true;
                let right = if entries.len() > MAX_ENTRIES {
                    let right = TransientNode::Leaf(entries.split_off(MIN_ENTRIES));
                    Some(new_transient_child(right)?)
                } else {
                    None
                };
                Ok(TransientInsert { right })
            }
            TransientNode::Internal(entries) => {
                let index = entries
                    .iter()
                    .position(|entry| entry.max >= key)
                    .unwrap_or(entries.len() - 1);
                load_transient_child::<M, S>(store, &mut entries[index]).await?;
                let edit =
                    transient_insert::<M, S>(store, &mut entries[index].child, key, value, false)
                        .await?;
                let changed = edit.right.is_some()
                    || matches!(
                        entries[index].child,
                        TransientRef::Loaded { dirty: true, .. }
                    );
                if !changed {
                    return Ok(TransientInsert { right: None });
                }
                entries[index].max = transient_ref_max(&entries[index].child).ok_or_else(|| {
                    StateError::InvalidTree("insertion produced an empty child".to_string())
                })?;
                if let Some(right) = edit.right {
                    entries.insert(index + 1, right);
                }
                *dirty = true;
                let right = if entries.len() > MAX_ENTRIES {
                    let right = TransientNode::Internal(entries.split_off(MIN_ENTRIES));
                    Some(new_transient_child(right)?)
                } else {
                    None
                };
                Ok(TransientInsert { right })
            }
        }
    })
}

fn transient_delete<'a, M: Map + 'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    reference: &'a mut TransientRef<M::Key, M::Value>,
    key: &'a M::Key,
    is_root: bool,
) -> StateFuture<'a, TransientDelete> {
    Box::pin(async move {
        load_transient_ref::<M, S>(store, reference, is_root).await?;
        let TransientRef::Loaded { dirty, node, .. } = reference else {
            unreachable!("transient reference was loaded")
        };
        match node {
            TransientNode::Leaf(entries) => {
                let Ok(index) = entries.binary_search_by(|entry| entry.0.cmp(key)) else {
                    return Ok(TransientDelete {
                        changed: false,
                        empty: false,
                        underflow: false,
                    });
                };
                entries.remove(index);
                *dirty = true;
                Ok(TransientDelete {
                    changed: true,
                    empty: entries.is_empty(),
                    underflow: !is_root && entries.len() < MIN_ENTRIES,
                })
            }
            TransientNode::Internal(entries) => {
                let Some(index) = entries.iter().position(|entry| &entry.max >= key) else {
                    return Ok(TransientDelete {
                        changed: false,
                        empty: false,
                        underflow: false,
                    });
                };
                load_transient_child::<M, S>(store, &mut entries[index]).await?;
                let edit =
                    transient_delete::<M, S>(store, &mut entries[index].child, key, false).await?;
                if !edit.changed {
                    return Ok(edit);
                }
                if edit.empty {
                    entries.remove(index);
                } else {
                    entries[index].max =
                        transient_ref_max(&entries[index].child).ok_or_else(|| {
                            StateError::InvalidTree("deletion produced an empty child".to_string())
                        })?;
                }
                if edit.underflow && !entries.is_empty() {
                    let current = index.min(entries.len() - 1);
                    transient_rebalance::<M, S>(store, entries, current).await?;
                }
                *dirty = true;
                Ok(TransientDelete {
                    changed: true,
                    empty: entries.is_empty(),
                    underflow: !is_root && entries.len() < MIN_ENTRIES,
                })
            }
        }
    })
}

async fn collapse_transient_root<M: Map, S: StateNodeStore>(
    store: &mut S,
    root: &mut TransientRef<M::Key, M::Value>,
) -> Result<(), StateError> {
    loop {
        load_transient_ref::<M, S>(store, root, true).await?;
        let only_child = match root {
            TransientRef::Loaded {
                node: TransientNode::Internal(entries),
                ..
            } if entries.len() == 1 => Some(entries.remove(0).child),
            _ => None,
        };
        let Some(child) = only_child else {
            return Ok(());
        };
        *root = child;
    }
}

async fn transient_rebalance<M: Map, S: StateNodeStore>(
    store: &mut S,
    parent: &mut Vec<TransientChild<M::Key, M::Value>>,
    index: usize,
) -> Result<(), StateError> {
    if index > 0 {
        load_transient_child::<M, S>(store, &mut parent[index - 1]).await?;
        if transient_ref_len(&parent[index - 1].child) > MIN_ENTRIES {
            load_transient_child::<M, S>(store, &mut parent[index]).await?;
            let (left, current) = parent.split_at_mut(index);
            move_transient_from_left(&mut left[index - 1], &mut current[0])?;
            return Ok(());
        }
    }
    if index + 1 < parent.len() {
        load_transient_child::<M, S>(store, &mut parent[index + 1]).await?;
        if transient_ref_len(&parent[index + 1].child) > MIN_ENTRIES {
            load_transient_child::<M, S>(store, &mut parent[index]).await?;
            let (current, right) = parent.split_at_mut(index + 1);
            move_transient_from_right(&mut current[index], &mut right[0])?;
            return Ok(());
        }
    }
    if index > 0 {
        load_transient_child::<M, S>(store, &mut parent[index - 1]).await?;
        load_transient_child::<M, S>(store, &mut parent[index]).await?;
        let current = parent.remove(index);
        merge_transient_into_left(&mut parent[index - 1], current)?;
    } else if parent.len() > 1 {
        load_transient_child::<M, S>(store, &mut parent[0]).await?;
        load_transient_child::<M, S>(store, &mut parent[1]).await?;
        let right = parent.remove(1);
        merge_transient_into_left(&mut parent[0], right)?;
    }
    Ok(())
}

fn move_transient_from_left<K: Clone, V>(
    left: &mut TransientChild<K, V>,
    current: &mut TransientChild<K, V>,
) -> Result<(), StateError> {
    let (left_dirty, left_node) = loaded_transient_mut(&mut left.child);
    let (current_dirty, current_node) = loaded_transient_mut(&mut current.child);
    match (left_node, current_node) {
        (TransientNode::Leaf(left_entries), TransientNode::Leaf(current_entries)) => {
            current_entries.insert(0, left_entries.pop().expect("left lender is nonempty"));
        }
        (TransientNode::Internal(left_entries), TransientNode::Internal(current_entries)) => {
            current_entries.insert(0, left_entries.pop().expect("left lender is nonempty"));
        }
        _ => {
            return Err(StateError::InvalidTree(
                "tree levels contain different node kinds".to_string(),
            ));
        }
    }
    *left_dirty = true;
    *current_dirty = true;
    left.max = transient_ref_max(&left.child).expect("lender remains nonempty");
    current.max = transient_ref_max(&current.child).expect("borrower is nonempty");
    Ok(())
}

fn move_transient_from_right<K: Clone, V>(
    current: &mut TransientChild<K, V>,
    right: &mut TransientChild<K, V>,
) -> Result<(), StateError> {
    let (current_dirty, current_node) = loaded_transient_mut(&mut current.child);
    let (right_dirty, right_node) = loaded_transient_mut(&mut right.child);
    match (current_node, right_node) {
        (TransientNode::Leaf(current_entries), TransientNode::Leaf(right_entries)) => {
            current_entries.push(right_entries.remove(0));
        }
        (TransientNode::Internal(current_entries), TransientNode::Internal(right_entries)) => {
            current_entries.push(right_entries.remove(0));
        }
        _ => {
            return Err(StateError::InvalidTree(
                "tree levels contain different node kinds".to_string(),
            ));
        }
    }
    *current_dirty = true;
    *right_dirty = true;
    current.max = transient_ref_max(&current.child).expect("borrower is nonempty");
    right.max = transient_ref_max(&right.child).expect("lender remains nonempty");
    Ok(())
}

fn merge_transient_into_left<K: Clone, V>(
    left: &mut TransientChild<K, V>,
    right: TransientChild<K, V>,
) -> Result<(), StateError> {
    let right_node = into_loaded_transient(right.child);
    let (left_dirty, left_node) = loaded_transient_mut(&mut left.child);
    match (left_node, right_node) {
        (TransientNode::Leaf(left_entries), TransientNode::Leaf(right_entries)) => {
            left_entries.extend(right_entries);
        }
        (TransientNode::Internal(left_entries), TransientNode::Internal(right_entries)) => {
            left_entries.extend(right_entries);
        }
        _ => {
            return Err(StateError::InvalidTree(
                "tree levels contain different node kinds".to_string(),
            ));
        }
    }
    *left_dirty = true;
    left.max = transient_ref_max(&left.child).expect("merged node is nonempty");
    Ok(())
}

async fn load_transient_child<M: Map, S: StateNodeStore>(
    store: &mut S,
    child: &mut TransientChild<M::Key, M::Value>,
) -> Result<(), StateError> {
    load_transient_ref::<M, S>(store, &mut child.child, false).await?;
    if transient_ref_max(&child.child).as_ref() != Some(&child.max) {
        return Err(StateError::InvalidTree(
            "child maximum does not match parent".to_string(),
        ));
    }
    Ok(())
}

async fn load_transient_ref<M: Map, S: StateNodeStore>(
    store: &mut S,
    reference: &mut TransientRef<M::Key, M::Value>,
    is_root: bool,
) -> Result<(), StateError> {
    let TransientRef::Stored(id) = reference else {
        return Ok(());
    };
    let id = *id;
    let node = load_tree::<M, S>(store, id).await?;
    validate_occupancy(node.len(), is_root, matches!(node, TreeNode::Leaf(_)))?;
    let node = match node {
        TreeNode::Leaf(entries) => TransientNode::Leaf(entries),
        TreeNode::Internal(entries) => TransientNode::Internal(
            entries
                .into_iter()
                .map(|(max, id)| TransientChild {
                    max,
                    child: TransientRef::Stored(id),
                })
                .collect(),
        ),
    };
    *reference = TransientRef::Loaded {
        original: Some(id),
        dirty: false,
        node,
    };
    Ok(())
}

fn new_transient_child<K: Clone, V>(
    node: TransientNode<K, V>,
) -> Result<TransientChild<K, V>, StateError> {
    let max = transient_node_max(&node)
        .ok_or_else(|| StateError::InvalidTree("non-root node is empty".to_string()))?;
    Ok(TransientChild {
        max,
        child: TransientRef::Loaded {
            original: None,
            dirty: true,
            node,
        },
    })
}

fn transient_node_max<K: Clone, V>(node: &TransientNode<K, V>) -> Option<K> {
    match node {
        TransientNode::Leaf(entries) => entries.last().map(|entry| entry.0.clone()),
        TransientNode::Internal(entries) => entries.last().map(|entry| entry.max.clone()),
    }
}

fn transient_ref_max<K: Clone, V>(reference: &TransientRef<K, V>) -> Option<K> {
    match reference {
        TransientRef::Stored(_) => None,
        TransientRef::Loaded { node, .. } => transient_node_max(node),
    }
}

fn transient_ref_len<K, V>(reference: &TransientRef<K, V>) -> usize {
    match reference {
        TransientRef::Stored(_) => unreachable!("transient node length requires a loaded node"),
        TransientRef::Loaded { node, .. } => match node {
            TransientNode::Leaf(entries) => entries.len(),
            TransientNode::Internal(entries) => entries.len(),
        },
    }
}

fn loaded_transient_mut<K, V>(
    reference: &mut TransientRef<K, V>,
) -> (&mut bool, &mut TransientNode<K, V>) {
    match reference {
        TransientRef::Loaded { dirty, node, .. } => (dirty, node),
        TransientRef::Stored(_) => unreachable!("transient node must be loaded"),
    }
}

fn into_loaded_transient<K, V>(reference: TransientRef<K, V>) -> TransientNode<K, V> {
    match reference {
        TransientRef::Loaded { node, .. } => node,
        TransientRef::Stored(_) => unreachable!("transient node must be loaded"),
    }
}

fn compact_transient_for_key<'a, M: Map + 'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    reference: &'a mut TransientRef<M::Key, M::Value>,
    key: &'a M::Key,
) -> StateFuture<'a, ()> {
    Box::pin(async move {
        let TransientRef::Loaded {
            node: TransientNode::Internal(entries),
            ..
        } = reference
        else {
            return Ok(());
        };
        let target = entries
            .iter()
            .position(|entry| &entry.max >= key)
            .unwrap_or(entries.len() - 1);
        for (index, entry) in entries.iter_mut().enumerate() {
            if index == target {
                compact_transient_for_key::<M, S>(store, &mut entry.child, key).await?;
            } else if index + 1 == target || index == target + 1 {
                compact_transient_descendants::<M, S>(store, &mut entry.child).await?;
            } else {
                freeze_transient_in_place::<M, S>(store, &mut entry.child).await?;
            }
        }
        Ok(())
    })
}

fn compact_transient_descendants<'a, M: Map + 'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    reference: &'a mut TransientRef<M::Key, M::Value>,
) -> StateFuture<'a, ()> {
    Box::pin(async move {
        let TransientRef::Loaded {
            node: TransientNode::Internal(entries),
            ..
        } = reference
        else {
            return Ok(());
        };
        for entry in entries {
            freeze_transient_in_place::<M, S>(store, &mut entry.child).await?;
        }
        Ok(())
    })
}

fn freeze_transient_in_place<'a, M: Map + 'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    reference: &'a mut TransientRef<M::Key, M::Value>,
) -> StateFuture<'a, ()> {
    Box::pin(async move {
        match reference {
            TransientRef::Stored(_) => return Ok(()),
            TransientRef::Loaded {
                original: Some(id),
                dirty: false,
                ..
            } => {
                *reference = TransientRef::Stored(*id);
                return Ok(());
            }
            TransientRef::Loaded {
                original: None,
                dirty: false,
                ..
            } => unreachable!("new transient nodes are dirty"),
            TransientRef::Loaded { .. } => {}
        }

        if let TransientRef::Loaded {
            node: TransientNode::Internal(entries),
            ..
        } = reference
        {
            for entry in entries.iter_mut() {
                freeze_transient_in_place::<M, S>(store, &mut entry.child).await?;
            }
        }
        let node = match reference {
            TransientRef::Loaded {
                node: TransientNode::Leaf(entries),
                ..
            } => TreeNode::Leaf(entries.clone()),
            TransientRef::Loaded {
                node: TransientNode::Internal(entries),
                ..
            } => TreeNode::Internal(
                entries
                    .iter()
                    .map(|entry| match &entry.child {
                        TransientRef::Stored(id) => Ok((entry.max.clone(), *id)),
                        TransientRef::Loaded { .. } => Err(StateError::InvalidTree(
                            "transient child was not frozen".to_string(),
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            TransientRef::Stored(_) => unreachable!("stored nodes returned above"),
        };
        let id = store_node(store, M::encode(node)).await?;
        *reference = TransientRef::Stored(id);
        Ok(())
    })
}

fn freeze_transient<'a, M: Map + 'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    mut reference: TransientRef<M::Key, M::Value>,
) -> StateFuture<'a, StateNodeId> {
    Box::pin(async move {
        freeze_transient_in_place::<M, S>(store, &mut reference).await?;
        match reference {
            TransientRef::Stored(id) => Ok(id),
            TransientRef::Loaded { .. } => unreachable!("transient root was not frozen"),
        }
    })
}

async fn load_node<S: StateNodeStore>(
    store: &mut S,
    id: StateNodeId,
) -> Result<StateNode, StateError> {
    let bytes = store.load(id).await?;
    let actual = hash_node_bytes(&bytes);
    if actual != id {
        return Err(StateError::HashMismatch {
            expected: id,
            actual,
        });
    }
    decode_node(&bytes)
}

async fn store_node<S: StateNodeStore>(
    store: &mut S,
    node: StateNode,
) -> Result<StateNodeId, StateError> {
    let bytes = encode_node(&node)?;
    let id = hash_node_bytes(&bytes);
    store.store(id, bytes).await?;
    Ok(id)
}

async fn load_tree<M: Map, S: StateNodeStore>(
    store: &mut S,
    id: StateNodeId,
) -> Result<TreeNode<M::Key, M::Value>, StateError> {
    M::decode(load_node(store, id).await?)
}

async fn lookup<M: Map, S: StateNodeStore>(
    store: &mut S,
    mut id: StateNodeId,
    key: &M::Key,
) -> Result<Option<M::Value>, StateError> {
    let mut is_root = true;
    loop {
        match load_tree::<M, S>(store, id).await? {
            TreeNode::Leaf(entries) => {
                validate_occupancy(entries.len(), is_root, true)?;
                return Ok(entries
                    .binary_search_by(|entry| entry.0.cmp(key))
                    .ok()
                    .map(|index| entries[index].1.clone()));
            }
            TreeNode::Internal(entries) => {
                validate_occupancy(entries.len(), is_root, false)?;
                let Some((advertised_max, child_id)) = entries.iter().find(|entry| &entry.0 >= key)
                else {
                    return Ok(None);
                };
                let child = load_tree::<M, S>(store, *child_id).await?;
                if node_max(&child) != Some(advertised_max) {
                    return Err(StateError::InvalidTree(
                        "child maximum does not match parent".to_string(),
                    ));
                }
                id = *child_id;
                is_root = false;
            }
        }
    }
}

fn node_max<K, V>(node: &TreeNode<K, V>) -> Option<&K> {
    match node {
        TreeNode::Leaf(entries) => entries.last().map(|entry| &entry.0),
        TreeNode::Internal(entries) => entries.last().map(|entry| &entry.0),
    }
}

async fn collect<M: Map, S: StateNodeStore>(
    store: &mut S,
    root: StateNodeId,
) -> Result<Vec<(M::Key, M::Value)>, StateError> {
    let mut output = Vec::new();
    let mut leaf_depth = None;
    collect_recursive::<M, S>(store, root, true, 0, &mut leaf_depth, &mut output).await?;
    Ok(output)
}

fn collect_recursive<'a, M: Map + 'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    id: StateNodeId,
    is_root: bool,
    depth: usize,
    leaf_depth: &'a mut Option<usize>,
    output: &'a mut Vec<(M::Key, M::Value)>,
) -> StateFuture<'a, Option<(M::Key, M::Key)>> {
    Box::pin(async move {
        match load_tree::<M, S>(store, id).await? {
            TreeNode::Leaf(entries) => {
                validate_occupancy(entries.len(), is_root, true)?;
                match *leaf_depth {
                    Some(expected) if expected != depth => {
                        return Err(StateError::InvalidTree(
                            "tree leaves have different depths".to_string(),
                        ));
                    }
                    None => *leaf_depth = Some(depth),
                    _ => {}
                }
                let bounds = entries
                    .first()
                    .zip(entries.last())
                    .map(|(first, last)| (first.0.clone(), last.0.clone()));
                output.extend(entries);
                Ok(bounds)
            }
            TreeNode::Internal(entries) => {
                validate_occupancy(entries.len(), is_root, false)?;
                let mut minimum = None;
                let mut previous_max: Option<M::Key> = None;
                let mut maximum = None;
                for (advertised_max, child_id) in entries {
                    let Some((child_min, child_max)) = collect_recursive::<M, S>(
                        store,
                        child_id,
                        false,
                        depth + 1,
                        leaf_depth,
                        output,
                    )
                    .await?
                    else {
                        return Err(StateError::InvalidTree(
                            "internal node references an empty child".to_string(),
                        ));
                    };
                    if child_max != advertised_max {
                        return Err(StateError::InvalidTree(
                            "child maximum does not match parent".to_string(),
                        ));
                    }
                    if previous_max
                        .as_ref()
                        .is_some_and(|value| value >= &child_min)
                    {
                        return Err(StateError::InvalidTree(
                            "child ranges overlap or are unordered".to_string(),
                        ));
                    }
                    minimum.get_or_insert(child_min);
                    previous_max = Some(child_max.clone());
                    maximum = Some(child_max);
                }
                Ok(minimum.zip(maximum))
            }
        }
    })
}

fn validate_occupancy(count: usize, is_root: bool, leaf: bool) -> Result<(), StateError> {
    let valid = if is_root {
        if leaf {
            count <= MAX_ENTRIES
        } else {
            (2..=MAX_ENTRIES).contains(&count)
        }
    } else {
        (MIN_ENTRIES..=MAX_ENTRIES).contains(&count)
    };
    if valid {
        Ok(())
    } else {
        Err(StateError::InvalidTree(
            "invalid node occupancy".to_string(),
        ))
    }
}

async fn collect_namespace_page<S: StateNodeStore>(
    store: &mut S,
    id: StateNodeId,
    bounds: NamespacePageBounds<'_>,
    limit: usize,
    output: &mut Vec<(NamespaceKey, ItemId)>,
) -> Result<(), StateError> {
    collect_namespace_page_recursive(store, id, true, bounds, limit, output)
        .await
        .map(|_| ())
}

async fn collect_item_page<S: StateNodeStore>(
    store: &mut S,
    id: StateNodeId,
    cursor: Option<&ItemId>,
    limit: usize,
    output: &mut Vec<(ItemId, StateNodeId)>,
) -> Result<(), StateError> {
    collect_item_page_recursive(store, id, true, cursor, limit, output)
        .await
        .map(|_| ())
}

fn collect_item_page_recursive<'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    id: StateNodeId,
    is_root: bool,
    cursor: Option<&'a ItemId>,
    limit: usize,
    output: &'a mut Vec<(ItemId, StateNodeId)>,
) -> StateFuture<'a, ()> {
    Box::pin(async move {
        if output.len() >= limit {
            return Ok(());
        }
        match load_tree::<ItemMap, S>(store, id).await? {
            TreeNode::Leaf(entries) => {
                validate_occupancy(entries.len(), is_root, true)?;
                output.extend(
                    entries
                        .into_iter()
                        .filter(|(key, _)| cursor.is_none_or(|cursor| key > cursor))
                        .take(limit - output.len()),
                );
            }
            TreeNode::Internal(entries) => {
                validate_occupancy(entries.len(), is_root, false)?;
                for (max, child) in entries {
                    if cursor.is_some_and(|cursor| max <= *cursor) {
                        continue;
                    }
                    let child_node = load_tree::<ItemMap, S>(store, child).await?;
                    if node_max(&child_node) != Some(&max) {
                        return Err(StateError::InvalidTree(
                            "child maximum does not match parent".to_string(),
                        ));
                    }
                    collect_item_page_recursive(store, child, false, cursor, limit, output).await?;
                    if output.len() == limit {
                        break;
                    }
                }
            }
        }
        Ok(())
    })
}

fn collect_namespace_page_recursive<'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    id: StateNodeId,
    is_root: bool,
    bounds: NamespacePageBounds<'a>,
    limit: usize,
    output: &'a mut Vec<(NamespaceKey, ItemId)>,
) -> StateFuture<'a, bool> {
    Box::pin(async move {
        if output.len() >= limit {
            return Ok(false);
        }
        match load_tree::<NamespaceMap, S>(store, id).await? {
            TreeNode::Leaf(entries) => {
                validate_occupancy(entries.len(), is_root, true)?;
                for (key, value) in entries {
                    if bounds.cursor.is_some_and(|cursor| key <= *cursor) {
                        continue;
                    }
                    if bounds.lower.is_some_and(|lower_bound| key < *lower_bound) {
                        continue;
                    }
                    if bounds.upper.is_some_and(|upper_bound| key >= *upper_bound) {
                        return Ok(true);
                    }
                    match super::compare_optional_item_ids(
                        key.parent_folder_id.as_ref(),
                        bounds.parent,
                    ) {
                        std::cmp::Ordering::Less => continue,
                        std::cmp::Ordering::Greater => return Ok(true),
                        std::cmp::Ordering::Equal => output.push((key, value)),
                    }
                    if output.len() == limit {
                        break;
                    }
                }
            }
            TreeNode::Internal(entries) => {
                validate_occupancy(entries.len(), is_root, false)?;
                for (max, child) in entries {
                    if bounds.cursor.is_some_and(|cursor| max <= *cursor) {
                        continue;
                    }
                    if bounds.lower.is_some_and(|lower_bound| max < *lower_bound) {
                        continue;
                    }
                    if super::compare_optional_item_ids(
                        max.parent_folder_id.as_ref(),
                        bounds.parent,
                    ) == std::cmp::Ordering::Less
                    {
                        continue;
                    }
                    let child_node = load_tree::<NamespaceMap, S>(store, child).await?;
                    if node_max(&child_node) != Some(&max) {
                        return Err(StateError::InvalidTree(
                            "child maximum does not match parent".to_string(),
                        ));
                    }
                    let finished = collect_namespace_page_recursive(
                        store, child, false, bounds, limit, output,
                    )
                    .await?;
                    if finished {
                        return Ok(true);
                    }
                    if output.len() == limit {
                        break;
                    }
                }
            }
        }
        Ok(false)
    })
}

async fn diff<M: Map, S: StateNodeStore>(
    store: &mut S,
    left: StateNodeId,
    right: StateNodeId,
) -> Result<BTreeSet<M::Key>, StateError> {
    let mut changed = BTreeSet::new();
    diff_recursive::<M, S>(
        store,
        DiffNode {
            id: left,
            is_root: true,
            expected_max: None,
        },
        DiffNode {
            id: right,
            is_root: true,
            expected_max: None,
        },
        DiffRange {
            lower: None,
            upper: None,
        },
        &mut changed,
    )
    .await?;
    Ok(changed)
}

#[derive(Clone)]
struct DiffPart<K> {
    max: K,
    id: StateNodeId,
    is_root: bool,
}

#[derive(Clone)]
struct DiffNode<K> {
    id: StateNodeId,
    is_root: bool,
    expected_max: Option<K>,
}

#[derive(Clone)]
struct DiffRange<K> {
    lower: Option<K>,
    upper: Option<K>,
}

fn diff_recursive<'a, M: Map + 'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    left_ref: DiffNode<M::Key>,
    right_ref: DiffNode<M::Key>,
    range: DiffRange<M::Key>,
    changed: &'a mut BTreeSet<M::Key>,
) -> StateFuture<'a, ()> {
    Box::pin(async move {
        if left_ref.id == right_ref.id {
            if left_ref.expected_max.is_none() && right_ref.expected_max.is_none() {
                return Ok(());
            }
            let node = load_tree::<M, S>(store, left_ref.id).await?;
            validate_occupancy(
                node.len(),
                left_ref.is_root,
                matches!(&node, TreeNode::Leaf(_)),
            )?;
            validate_occupancy(
                node.len(),
                right_ref.is_root,
                matches!(&node, TreeNode::Leaf(_)),
            )?;
            if left_ref
                .expected_max
                .as_ref()
                .is_some_and(|expected| node_max(&node) != Some(expected))
                || right_ref
                    .expected_max
                    .as_ref()
                    .is_some_and(|expected| node_max(&node) != Some(expected))
            {
                return Err(StateError::InvalidTree(
                    "child maximum does not match parent".to_string(),
                ));
            }
            return Ok(());
        }
        let left = load_tree::<M, S>(store, left_ref.id).await?;
        let right = load_tree::<M, S>(store, right_ref.id).await?;
        validate_occupancy(
            left.len(),
            left_ref.is_root,
            matches!(&left, TreeNode::Leaf(_)),
        )?;
        validate_occupancy(
            right.len(),
            right_ref.is_root,
            matches!(&right, TreeNode::Leaf(_)),
        )?;
        if left_ref
            .expected_max
            .as_ref()
            .is_some_and(|expected| node_max(&left) != Some(expected))
            || right_ref
                .expected_max
                .as_ref()
                .is_some_and(|expected| node_max(&right) != Some(expected))
        {
            return Err(StateError::InvalidTree(
                "child maximum does not match parent".to_string(),
            ));
        }
        match (left, right) {
            (TreeNode::Leaf(left), TreeNode::Leaf(right)) => {
                diff_leaf_entries::<M>(
                    &left,
                    &right,
                    range.lower.as_ref(),
                    range.upper.as_ref(),
                    changed,
                );
            }
            (left, right) => {
                let left_parts = diff_parts(left_ref.id, left, left_ref.is_root);
                let right_parts = diff_parts(right_ref.id, right, right_ref.is_root);
                let mut left_index = first_part_after(&left_parts, range.lower.as_ref());
                let mut right_index = first_part_after(&right_parts, range.lower.as_ref());
                let mut range_lower = range.lower;

                while left_index < left_parts.len() && right_index < right_parts.len() {
                    let left_part = &left_parts[left_index];
                    let right_part = &right_parts[right_index];
                    let mut range_upper = if left_part.max <= right_part.max {
                        left_part.max.clone()
                    } else {
                        right_part.max.clone()
                    };
                    if range
                        .upper
                        .as_ref()
                        .is_some_and(|upper| upper < &range_upper)
                    {
                        range_upper = range.upper.as_ref().expect("upper checked above").clone();
                    }
                    diff_recursive::<M, S>(
                        store,
                        DiffNode {
                            id: left_part.id,
                            is_root: left_part.is_root,
                            expected_max: Some(left_part.max.clone()),
                        },
                        DiffNode {
                            id: right_part.id,
                            is_root: right_part.is_root,
                            expected_max: Some(right_part.max.clone()),
                        },
                        DiffRange {
                            lower: range_lower.clone(),
                            upper: Some(range_upper.clone()),
                        },
                        changed,
                    )
                    .await?;
                    range_lower = Some(range_upper.clone());
                    if left_part.max <= range_upper {
                        left_index += 1;
                    }
                    if right_part.max <= range_upper {
                        right_index += 1;
                    }
                    if range
                        .upper
                        .as_ref()
                        .is_some_and(|upper| &range_upper >= upper)
                    {
                        return Ok(());
                    }
                }

                collect_remaining_parts::<M, S>(
                    store,
                    &left_parts[left_index..],
                    range_lower.clone(),
                    range.upper.clone(),
                    changed,
                )
                .await?;
                collect_remaining_parts::<M, S>(
                    store,
                    &right_parts[right_index..],
                    range_lower,
                    range.upper,
                    changed,
                )
                .await?;
            }
        }
        Ok(())
    })
}

fn diff_parts<K: Clone, V>(
    id: StateNodeId,
    node: TreeNode<K, V>,
    is_root: bool,
) -> Vec<DiffPart<K>> {
    match node {
        TreeNode::Leaf(entries) => entries
            .last()
            .map(|entry| {
                vec![DiffPart {
                    max: entry.0.clone(),
                    id,
                    is_root,
                }]
            })
            .unwrap_or_default(),
        TreeNode::Internal(entries) => entries
            .into_iter()
            .map(|(max, id)| DiffPart {
                max,
                id,
                is_root: false,
            })
            .collect(),
    }
}

fn first_part_after<K: Ord>(parts: &[DiffPart<K>], lower: Option<&K>) -> usize {
    lower.map_or(0, |lower| parts.partition_point(|part| &part.max <= lower))
}

fn diff_leaf_entries<M: Map>(
    left: &[(M::Key, M::Value)],
    right: &[(M::Key, M::Value)],
    lower: Option<&M::Key>,
    upper: Option<&M::Key>,
    changed: &mut BTreeSet<M::Key>,
) {
    let mut left_index = lower.map_or(0, |lower| left.partition_point(|entry| &entry.0 <= lower));
    let mut right_index = lower.map_or(0, |lower| right.partition_point(|entry| &entry.0 <= lower));
    loop {
        let left_entry = left
            .get(left_index)
            .filter(|entry| upper.is_none_or(|upper| &entry.0 <= upper));
        let right_entry = right
            .get(right_index)
            .filter(|entry| upper.is_none_or(|upper| &entry.0 <= upper));
        match (left_entry, right_entry) {
            (None, None) => break,
            (Some(left), None) => {
                changed.insert(left.0.clone());
                left_index += 1;
            }
            (None, Some(right)) => {
                changed.insert(right.0.clone());
                right_index += 1;
            }
            (Some(left), Some(right)) => match left.0.cmp(&right.0) {
                std::cmp::Ordering::Less => {
                    changed.insert(left.0.clone());
                    left_index += 1;
                }
                std::cmp::Ordering::Greater => {
                    changed.insert(right.0.clone());
                    right_index += 1;
                }
                std::cmp::Ordering::Equal => {
                    if left.1 != right.1 {
                        changed.insert(left.0.clone());
                    }
                    left_index += 1;
                    right_index += 1;
                }
            },
        }
    }
}

fn collect_remaining_parts<'a, M: Map + 'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    parts: &'a [DiffPart<M::Key>],
    lower: Option<M::Key>,
    upper: Option<M::Key>,
    changed: &'a mut BTreeSet<M::Key>,
) -> StateFuture<'a, ()> {
    Box::pin(async move {
        let mut range_lower = lower;
        for part in parts {
            if range_lower.as_ref().is_some_and(|lower| &part.max <= lower) {
                continue;
            }
            let range_upper = match upper.as_ref() {
                Some(upper) if upper < &part.max => upper.clone(),
                _ => part.max.clone(),
            };
            collect_changed_range::<M, S>(
                store,
                part.id,
                part.is_root,
                range_lower.clone(),
                Some(range_upper.clone()),
                Some(part.max.clone()),
                changed,
            )
            .await?;
            range_lower = Some(range_upper.clone());
            if upper.as_ref().is_some_and(|upper| &range_upper >= upper) {
                break;
            }
        }
        Ok(())
    })
}

fn collect_changed_range<'a, M: Map + 'a, S: StateNodeStore + 'a>(
    store: &'a mut S,
    id: StateNodeId,
    is_root: bool,
    lower: Option<M::Key>,
    upper: Option<M::Key>,
    expected_max: Option<M::Key>,
    changed: &'a mut BTreeSet<M::Key>,
) -> StateFuture<'a, ()> {
    Box::pin(async move {
        let node = load_tree::<M, S>(store, id).await?;
        validate_occupancy(node.len(), is_root, matches!(&node, TreeNode::Leaf(_)))?;
        if expected_max
            .as_ref()
            .is_some_and(|expected| node_max(&node) != Some(expected))
        {
            return Err(StateError::InvalidTree(
                "child maximum does not match parent".to_string(),
            ));
        }
        match node {
            TreeNode::Leaf(entries) => {
                let start = lower.as_ref().map_or(0, |lower| {
                    entries.partition_point(|entry| &entry.0 <= lower)
                });
                for (key, _) in entries.into_iter().skip(start) {
                    if upper.as_ref().is_some_and(|upper| &key > upper) {
                        break;
                    }
                    changed.insert(key);
                }
            }
            TreeNode::Internal(entries) => {
                let start = lower.as_ref().map_or(0, |lower| {
                    entries.partition_point(|entry| &entry.0 <= lower)
                });
                let mut range_lower = lower;
                for (max, child) in entries.into_iter().skip(start) {
                    let range_upper = match upper.as_ref() {
                        Some(upper) if upper < &max => upper.clone(),
                        _ => max.clone(),
                    };
                    collect_changed_range::<M, S>(
                        store,
                        child,
                        false,
                        range_lower.clone(),
                        Some(range_upper.clone()),
                        Some(max),
                        changed,
                    )
                    .await?;
                    range_lower = Some(range_upper.clone());
                    if upper.as_ref().is_some_and(|upper| &range_upper >= upper) {
                        break;
                    }
                }
            }
        }
        Ok(())
    })
}
