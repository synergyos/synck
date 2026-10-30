use chrono::DateTime;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    ContentPointer, FORMAT_VERSION, ItemValue, MAX_ENTRIES,
    MAX_INDEX_NODE_BYTES, NamespaceKey, RootHeader, StateError, StateNode, StateNodeId,
    item_id_bytes,
};
use crate::naming::compute_name_key;
use crate::types::{
    CommitId, ItemId, LifecycleState, RepoId, SequencingMode, sha256_prefixed_hex_from_digest_bytes,
};

const ITEM_VALUE: u8 = 0x01;
const ITEM_LEAF: u8 = 0x02;
const ITEM_INTERNAL: u8 = 0x03;
const NAMESPACE_LEAF: u8 = 0x04;
const NAMESPACE_INTERNAL: u8 = 0x05;
const ROOT: u8 = 0x06;

pub fn hash_node_bytes(bytes: &[u8]) -> StateNodeId {
    let mut hasher = Sha256::new();
    hasher.update(b"synck-state-node\0");
    hasher.update(bytes);
    StateNodeId::from_bytes(hasher.finalize().into())
}

pub fn encode_node(node: &StateNode) -> Result<Vec<u8>, StateError> {
    let mut output = Vec::new();
    output.push(FORMAT_VERSION);
    match node {
        StateNode::ItemValue(value) => {
            output.push(ITEM_VALUE);
            encode_item_value(&mut output, value)?;
        }
        StateNode::ItemLeaf(entries) => {
            output.push(ITEM_LEAF);
            encode_item_entries(&mut output, entries)?;
        }
        StateNode::ItemInternal(entries) => {
            output.push(ITEM_INTERNAL);
            encode_item_entries(&mut output, entries)?;
        }
        StateNode::NamespaceLeaf(entries) => {
            output.push(NAMESPACE_LEAF);
            encode_namespace_leaf(&mut output, entries)?;
        }
        StateNode::NamespaceInternal(entries) => {
            output.push(NAMESPACE_INTERNAL);
            encode_namespace_internal(&mut output, entries)?;
        }
        StateNode::Root(root) => {
            output.push(ROOT);
            encode_root(&mut output, root)?;
        }
    }
    if !matches!(node, StateNode::ItemValue(_) | StateNode::Root(_))
        && output.len() > MAX_INDEX_NODE_BYTES
    {
        return Err(StateError::InvalidEncoding(
            "index node exceeds 65536 bytes".to_string(),
        ));
    }
    Ok(output)
}

pub fn decode_node(bytes: &[u8]) -> Result<StateNode, StateError> {
    let mut decoder = Decoder::new(bytes);
    if decoder.u8()? != FORMAT_VERSION {
        return Err(invalid("unsupported format version"));
    }
    let kind = decoder.u8()?;
    if matches!(
        kind,
        ITEM_LEAF | ITEM_INTERNAL | NAMESPACE_LEAF | NAMESPACE_INTERNAL
    ) && bytes.len() > MAX_INDEX_NODE_BYTES
    {
        return Err(invalid("index node exceeds 65536 bytes"));
    }
    let node = match kind {
        ITEM_VALUE => StateNode::ItemValue(decode_item_value(&mut decoder)?),
        ITEM_LEAF => StateNode::ItemLeaf(decode_item_entries(&mut decoder)?),
        ITEM_INTERNAL => StateNode::ItemInternal(decode_item_entries(&mut decoder)?),
        NAMESPACE_LEAF => StateNode::NamespaceLeaf(decode_namespace_leaf(&mut decoder)?),
        NAMESPACE_INTERNAL => {
            StateNode::NamespaceInternal(decode_namespace_internal(&mut decoder)?)
        }
        ROOT => StateNode::Root(decode_root(&mut decoder)?),
        _ => return Err(invalid("unknown node kind")),
    };
    decoder.finish()?;
    Ok(node)
}

fn encode_item_value(output: &mut Vec<u8>, value: &ItemValue) -> Result<(), StateError> {
    validate_item_value(value)?;
    output.extend_from_slice(&item_id_bytes(&value.item_id));
    encode_string(output, &value.item_type)?;
    encode_optional_item_id(output, value.parent_folder_id.as_ref());
    encode_optional_string(output, value.name.as_deref())?;
    encode_optional_string(output, value.name_key.as_deref())?;
    encode_optional_string(output, value.record_key.as_deref())?;
    encode_optional_item_id(output, value.link_folder_id.as_ref());
    encode_positive_i64(output, value.head_seq, "head sequence")?;
    encode_non_negative_i64(output, value.ref_head_seq, "ref head sequence")?;
    output.push(match value.lifecycle_state {
        LifecycleState::Live => 1,
        LifecycleState::Trash => 2,
        LifecycleState::Tombstoned => 3,
    });
    output.extend_from_slice(&value.created_at.timestamp().to_be_bytes());
    output.extend_from_slice(&value.created_at.timestamp_subsec_nanos().to_be_bytes());
    encode_string(output, &value.created_by)?;
    output.push(match value.sequencing_mode {
        SequencingMode::Local => 1,
        SequencingMode::Federated => 2,
    });
    match &value.federation_stream_id {
        None => output.push(0),
        Some(value) => {
            output.push(1);
            encode_string(output, value)?;
        }
    }
    match &value.content {
        None => output.push(0),
        Some(content) => {
            output.push(1);
            encode_positive_i64(output, content.item_seq, "content item sequence")?;
            output.extend_from_slice(&content.event_digest);
        }
    }
    Ok(())
}

fn decode_item_value(decoder: &mut Decoder<'_>) -> Result<ItemValue, StateError> {
    let item_id = decoder.item_id()?;
    let item_type = decoder.string_u64()?;
    let parent_folder_id = decoder.optional_item_id()?;
    let name = decoder.optional_string()?;
    let name_key = decoder.optional_string()?;
    let record_key = decoder.optional_string()?;
    let link_folder_id = decoder.optional_item_id()?;
    let head_seq = decoder.positive_i64("head sequence")?;
    let ref_head_seq = decoder.non_negative_i64("ref head sequence")?;
    let lifecycle_state = match decoder.u8()? {
        1 => LifecycleState::Live,
        2 => LifecycleState::Trash,
        3 => LifecycleState::Tombstoned,
        _ => return Err(invalid("unknown lifecycle tag")),
    };
    let seconds = decoder.i64()?;
    let nanos = decoder.u32()?;
    if nanos > 999_999_999 {
        return Err(invalid("invalid timestamp nanoseconds"));
    }
    let created_at = DateTime::from_timestamp(seconds, nanos)
        .ok_or_else(|| invalid("timestamp is out of range"))?;
    let created_by = decoder.string_u64()?;
    let sequencing_mode = match decoder.u8()? {
        1 => SequencingMode::Local,
        2 => SequencingMode::Federated,
        _ => return Err(invalid("unknown sequencing tag")),
    };
    let federation_stream_id = match decoder.u8()? {
        0 => None,
        1 => Some(decoder.string_u64()?),
        _ => return Err(invalid("unknown federation option tag")),
    };
    let content = match decoder.u8()? {
        0 => None,
        1 => Some(ContentPointer {
            item_seq: decoder.positive_i64("content item sequence")?,
            event_digest: decoder.array()?,
        }),
        _ => return Err(invalid("unknown content option tag")),
    };
    let value = ItemValue {
        item_id,
        item_type,
        parent_folder_id,
        name,
        name_key,
        record_key,
        link_folder_id,
        head_seq,
        ref_head_seq,
        lifecycle_state,
        created_at,
        created_by,
        sequencing_mode,
        federation_stream_id,
        content,
    };
    validate_item_value(&value)?;
    Ok(value)
}

fn validate_item_value(value: &ItemValue) -> Result<(), StateError> {
    match (&value.name, &value.name_key) {
        (Some(name), Some(name_key)) => {
            let computed_name_key = compute_name_key(name)
                .map_err(|error| invalid(format!("invalid item name: {error}")))?;
            if computed_name_key != *name_key {
                return Err(invalid("item name key does not match its display name"));
            }
            if value.record_key.is_some() || value.link_folder_id.is_some() {
                return Err(invalid("a tree item has no record key or link"));
            }
        }
        (None, None) => {
            if value.parent_folder_id.is_some() {
                return Err(invalid("a record has no parent"));
            }
        }
        _ => return Err(invalid("item name and name key disagree")),
    }
    match (value.sequencing_mode, value.federation_stream_id.as_deref()) {
        (SequencingMode::Local, None) => {}
        (SequencingMode::Federated, Some(stream)) if !stream.is_empty() => {}
        _ => return Err(invalid("sequencing mode and federation stream disagree")),
    }
    if value
        .content
        .as_ref()
        .is_some_and(|content| content.item_seq > value.head_seq)
    {
        return Err(invalid("content item sequence exceeds item head"));
    }
    Ok(())
}

fn encode_item_entries(
    output: &mut Vec<u8>,
    entries: &[(ItemId, StateNodeId)],
) -> Result<(), StateError> {
    encode_count(output, entries.len())?;
    ensure_sorted(entries.iter().map(|entry| &entry.0))?;
    for (item_id, hash) in entries {
        output.extend_from_slice(&item_id_bytes(item_id));
        output.extend_from_slice(hash.as_bytes());
    }
    Ok(())
}

fn decode_item_entries(
    decoder: &mut Decoder<'_>,
) -> Result<Vec<(ItemId, StateNodeId)>, StateError> {
    let count = decoder.count()?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push((
            decoder.item_id()?,
            StateNodeId::from_bytes(decoder.array()?),
        ));
    }
    ensure_sorted(entries.iter().map(|entry| &entry.0))?;
    Ok(entries)
}

fn encode_namespace_leaf(
    output: &mut Vec<u8>,
    entries: &[(NamespaceKey, ItemId)],
) -> Result<(), StateError> {
    encode_count(output, entries.len())?;
    ensure_sorted(entries.iter().map(|entry| &entry.0))?;
    for (key, item_id) in entries {
        encode_namespace_key(output, key)?;
        output.extend_from_slice(&item_id_bytes(item_id));
    }
    Ok(())
}

fn decode_namespace_leaf(
    decoder: &mut Decoder<'_>,
) -> Result<Vec<(NamespaceKey, ItemId)>, StateError> {
    let count = decoder.count()?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push((decoder.namespace_key()?, decoder.item_id()?));
    }
    ensure_sorted(entries.iter().map(|entry| &entry.0))?;
    Ok(entries)
}

fn encode_namespace_internal(
    output: &mut Vec<u8>,
    entries: &[(NamespaceKey, StateNodeId)],
) -> Result<(), StateError> {
    encode_count(output, entries.len())?;
    ensure_sorted(entries.iter().map(|entry| &entry.0))?;
    for (key, hash) in entries {
        encode_namespace_key(output, key)?;
        output.extend_from_slice(hash.as_bytes());
    }
    Ok(())
}

fn decode_namespace_internal(
    decoder: &mut Decoder<'_>,
) -> Result<Vec<(NamespaceKey, StateNodeId)>, StateError> {
    let count = decoder.count()?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push((
            decoder.namespace_key()?,
            StateNodeId::from_bytes(decoder.array()?),
        ));
    }
    ensure_sorted(entries.iter().map(|entry| &entry.0))?;
    Ok(entries)
}

fn encode_namespace_key(output: &mut Vec<u8>, key: &NamespaceKey) -> Result<(), StateError> {
    encode_optional_item_id(output, key.parent_folder_id.as_ref());
    let length =
        u16::try_from(key.name_key.len()).map_err(|_| invalid("namespace key exceeds u16"))?;
    if key.name_key.len() > 3060 {
        return Err(invalid("namespace key exceeds 3060 bytes"));
    }
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(key.name_key.as_bytes());
    Ok(())
}

fn encode_root(output: &mut Vec<u8>, root: &RootHeader) -> Result<(), StateError> {
    output.extend_from_slice(&uuid_bytes(root.repo_id.as_str()));
    if root.repo_seq < 0 || (root.repo_seq == 0) != root.commit_id.is_none() {
        return Err(invalid("invalid root sequence/commit pairing"));
    }
    output.extend_from_slice(&(root.repo_seq as u64).to_be_bytes());
    match &root.commit_id {
        None => output.push(0),
        Some(commit_id) => {
            output.push(1);
            output.extend_from_slice(&sha256_id_bytes(commit_id.as_str())?);
        }
    }
    output.extend_from_slice(root.item_map_root.as_bytes());
    output.extend_from_slice(root.namespace_root.as_bytes());
    Ok(())
}

fn decode_root(decoder: &mut Decoder<'_>) -> Result<RootHeader, StateError> {
    let repo_id = RepoId::parse(Uuid::from_bytes(decoder.array()?).to_string())
        .map_err(|_| invalid("invalid repository UUIDv7"))?;
    let repo_seq_u64 = decoder.u64()?;
    let repo_seq = i64::try_from(repo_seq_u64).map_err(|_| invalid("root sequence overflow"))?;
    let commit_id = match decoder.u8()? {
        0 => None,
        1 => Some(
            CommitId::parse(sha256_prefixed_hex_from_digest_bytes(decoder.array()?))
                .map_err(|_| invalid("invalid commit digest"))?,
        ),
        _ => return Err(invalid("unknown commit option tag")),
    };
    if (repo_seq == 0) != commit_id.is_none() {
        return Err(invalid("invalid root sequence/commit pairing"));
    }
    Ok(RootHeader {
        repo_id,
        repo_seq,
        commit_id,
        item_map_root: StateNodeId::from_bytes(decoder.array()?),
        namespace_root: StateNodeId::from_bytes(decoder.array()?),
    })
}

fn encode_count(output: &mut Vec<u8>, count: usize) -> Result<(), StateError> {
    if count > MAX_ENTRIES {
        return Err(invalid("node has more than 16 entries"));
    }
    output.push(count as u8);
    Ok(())
}

fn encode_positive_i64(output: &mut Vec<u8>, value: i64, field: &str) -> Result<(), StateError> {
    if value <= 0 {
        return Err(invalid(format!("{field} must be positive")));
    }
    output.extend_from_slice(&(value as u64).to_be_bytes());
    Ok(())
}

fn encode_non_negative_i64(
    output: &mut Vec<u8>,
    value: i64,
    field: &str,
) -> Result<(), StateError> {
    if value < 0 {
        return Err(invalid(format!("{field} must be non-negative")));
    }
    output.extend_from_slice(&(value as u64).to_be_bytes());
    Ok(())
}

fn encode_string(output: &mut Vec<u8>, value: &str) -> Result<(), StateError> {
    let length = u64::try_from(value.len()).map_err(|_| invalid("string length overflow"))?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn encode_optional_string(output: &mut Vec<u8>, value: Option<&str>) -> Result<(), StateError> {
    match value {
        None => output.push(0),
        Some(value) => {
            output.push(1);
            encode_string(output, value)?;
        }
    }
    Ok(())
}

fn encode_optional_item_id(output: &mut Vec<u8>, value: Option<&ItemId>) {
    match value {
        None => output.push(0),
        Some(item_id) => {
            output.push(1);
            output.extend_from_slice(&item_id_bytes(item_id));
        }
    }
}

fn ensure_sorted<'a, T: Ord + 'a>(values: impl Iterator<Item = &'a T>) -> Result<(), StateError> {
    let mut previous: Option<&T> = None;
    for value in values {
        if previous.is_some_and(|previous| previous >= value) {
            return Err(invalid("node keys are not strictly increasing"));
        }
        previous = Some(value);
    }
    Ok(())
}

fn uuid_bytes(value: &str) -> [u8; 16] {
    Uuid::parse_str(value)
        .expect("validated UUID identifier must remain valid")
        .into_bytes()
}

fn sha256_id_bytes(value: &str) -> Result<[u8; 32], StateError> {
    let hex = value
        .strip_prefix("sha256:")
        .ok_or_else(|| invalid("invalid SHA-256 identifier"))?;
    let bytes = hex::decode(hex).map_err(|_| invalid("invalid SHA-256 identifier"))?;
    bytes
        .try_into()
        .map_err(|_| invalid("invalid SHA-256 identifier length"))
}

fn invalid(message: impl Into<String>) -> StateError {
    StateError::InvalidEncoding(message.into())
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn finish(&self) -> Result<(), StateError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid("trailing bytes"))
        }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], StateError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or_else(|| invalid("offset overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| invalid("truncated field"))?;
        self.position = end;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], StateError> {
        self.take(N)?
            .try_into()
            .map_err(|_| invalid("truncated fixed field"))
    }

    fn u8(&mut self) -> Result<u8, StateError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, StateError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, StateError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, StateError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, StateError> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    fn positive_i64(&mut self, field: &str) -> Result<i64, StateError> {
        let value = self.u64()?;
        if value == 0 || value > i64::MAX as u64 {
            return Err(invalid(format!("invalid {field}")));
        }
        Ok(value as i64)
    }

    fn non_negative_i64(&mut self, field: &str) -> Result<i64, StateError> {
        let value = self.u64()?;
        if value > i64::MAX as u64 {
            return Err(invalid(format!("invalid {field}")));
        }
        Ok(value as i64)
    }

    fn count(&mut self) -> Result<usize, StateError> {
        let count = usize::from(self.u8()?);
        if count > MAX_ENTRIES {
            return Err(invalid("node has more than 16 entries"));
        }
        Ok(count)
    }

    fn item_id(&mut self) -> Result<ItemId, StateError> {
        ItemId::parse(Uuid::from_bytes(self.array()?).to_string())
            .map_err(|_| invalid("invalid item UUIDv7"))
    }

    fn optional_item_id(&mut self) -> Result<Option<ItemId>, StateError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.item_id()?)),
            _ => Err(invalid("unknown item option tag")),
        }
    }

    fn optional_string(&mut self) -> Result<Option<String>, StateError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.string_u64()?)),
            _ => Err(invalid("unknown string option tag")),
        }
    }

    fn string_u64(&mut self) -> Result<String, StateError> {
        let length = self.u64()?;
        let remaining = self.bytes.len() - self.position;
        if length > remaining as u64 {
            return Err(invalid("string length exceeds remaining bytes"));
        }
        let length = usize::try_from(length).map_err(|_| invalid("string length overflow"))?;
        let bytes = self.take(length)?;
        let value = std::str::from_utf8(bytes).map_err(|_| invalid("string is not UTF-8"))?;
        Ok(value.to_owned())
    }

    fn namespace_key(&mut self) -> Result<NamespaceKey, StateError> {
        let parent_folder_id = self.optional_item_id()?;
        let length = usize::from(self.u16()?);
        if length > 3060 {
            return Err(invalid("namespace key exceeds 3060 bytes"));
        }
        let bytes = self.take(length)?;
        let name_key = std::str::from_utf8(bytes)
            .map_err(|_| invalid("namespace key is not UTF-8"))?
            .to_owned();
        Ok(NamespaceKey::new(parent_folder_id, name_key))
    }
}
