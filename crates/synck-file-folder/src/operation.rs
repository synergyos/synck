use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};
use synck_vcs::types::{RepoId, Sha256Digest};

use crate::error::{SemanticError, SemanticErrorCode};
use crate::model::{BlobRef, Intent, ItemBase, Location};
use crate::{MAX_INTENTS, MAX_MESSAGE_BYTES, MAX_SAFE_JSON_INTEGER, PROFILE_ID};

pub fn normalize_intents(intents: &[Intent]) -> Result<Vec<Intent>, SemanticError> {
    if intents.is_empty() {
        return Err(SemanticError::new(
            SemanticErrorCode::EmptyIntent,
            "proposal has no intents",
        ));
    }
    if intents.len() > MAX_INTENTS {
        return Err(SemanticError::new(
            SemanticErrorCode::TooManyIntents,
            "proposal exceeds the intent limit",
        ));
    }

    for intent in intents {
        match intent {
            Intent::UpdateFolder { base_seq, .. } | Intent::UpdateFile { base_seq, .. } => {
                require_positive_seq(*base_seq)?
            }
            Intent::Trash { root, descendants } => {
                validate_item_bases(root, descendants)?;
            }
            Intent::Restore {
                root, descendants, ..
            } => validate_item_bases(root, descendants)?,
            Intent::CreateFolder { .. } | Intent::CreateFile { .. } => {}
        }
        match intent {
            Intent::CreateFile { content, .. }
            | Intent::UpdateFile {
                content: Some(content),
                ..
            } => validate_content(content)?,
            _ => {}
        }
        if matches!(
            intent,
            Intent::UpdateFile {
                location: None,
                content: None,
                ..
            }
        ) {
            return Err(SemanticError::new(
                SemanticErrorCode::EmptyIntent,
                "update_file has neither location nor content",
            ));
        }
    }

    let create_depths = create_depths(intents)?;
    let mut normalized = intents.to_vec();
    normalized.sort_by(|left, right| {
        let left_rank = intent_rank(left);
        let right_rank = intent_rank(right);
        left_rank.cmp(&right_rank).then_with(|| match left_rank {
            1 => create_depths
                .get(left.root_id())
                .cmp(&create_depths.get(right.root_id()))
                .then_with(|| left.root_id().cmp(right.root_id())),
            _ => left.root_id().cmp(right.root_id()),
        })
    });
    Ok(normalized)
}

fn require_positive_seq(seq: i64) -> Result<(), SemanticError> {
    if seq <= 0 {
        return Err(SemanticError::new(
            SemanticErrorCode::BaseSeqMismatch,
            "base_seq must be positive",
        ));
    }
    Ok(())
}

fn validate_item_bases(root: &ItemBase, descendants: &[ItemBase]) -> Result<(), SemanticError> {
    require_positive_seq(root.base_seq)?;
    let mut previous = None;
    for descendant in descendants {
        require_positive_seq(descendant.base_seq)?;
        if descendant.item_id == root.item_id
            || previous.is_some_and(|id| id >= &descendant.item_id)
        {
            return Err(SemanticError::new(
                SemanticErrorCode::ConflictingIntent,
                "lifecycle descendants must be unique, sorted, and exclude the root",
            ));
        }
        previous = Some(&descendant.item_id);
    }
    Ok(())
}

fn validate_content(content: &BlobRef) -> Result<(), SemanticError> {
    if content.size > MAX_SAFE_JSON_INTEGER {
        return Err(SemanticError::new(
            SemanticErrorCode::InvalidContent,
            "content size exceeds the exact JSON integer range",
        ));
    }
    Ok(())
}

fn intent_rank(intent: &Intent) -> u8 {
    match intent {
        Intent::Trash { .. } => 0,
        Intent::CreateFolder { .. } | Intent::CreateFile { .. } => 1,
        Intent::Restore { .. } => 2,
        Intent::UpdateFolder { .. } | Intent::UpdateFile { .. } => 3,
    }
}

fn create_depths(
    intents: &[Intent],
) -> Result<BTreeMap<synck_vcs::types::ItemId, usize>, SemanticError> {
    let creates: BTreeMap<_, _> = intents
        .iter()
        .filter_map(|intent| match intent {
            Intent::CreateFolder {
                item_id, location, ..
            } => Some((item_id.clone(), location.parent_item_id.clone())),
            Intent::CreateFile {
                item_id, location, ..
            } => Some((item_id.clone(), location.parent_item_id.clone())),
            _ => None,
        })
        .collect();
    let mut depths = BTreeMap::new();
    for item_id in creates.keys() {
        let mut current = item_id;
        let mut seen = BTreeSet::new();
        let mut depth = 0;
        while let Some(Some(parent_id)) = creates.get(current) {
            if !seen.insert(current.clone()) {
                return Err(SemanticError::new(
                    SemanticErrorCode::CycleDetected,
                    "create dependency cycle",
                ));
            }
            if !creates.contains_key(parent_id) {
                break;
            }
            depth += 1;
            current = parent_id;
        }
        depths.insert(item_id.clone(), depth);
    }
    Ok(depths)
}

pub fn canonical_operation_bytes(
    repo_id: &RepoId,
    message: Option<&str>,
    intents: &[Intent],
) -> Result<Vec<u8>, SemanticError> {
    if message.is_some_and(|value| value.len() > MAX_MESSAGE_BYTES) {
        return Err(SemanticError::new(
            SemanticErrorCode::MessageTooLarge,
            "message exceeds the UTF-8 byte limit",
        ));
    }
    let intents = normalize_intents(intents)?;
    let mut output = Vec::new();
    put_bytes(&mut output, b"synck-operation-v1");
    put_u64(&mut output, 1);
    put_bytes(&mut output, PROFILE_ID.as_bytes());
    put_bytes(&mut output, repo_id.as_str().as_bytes());
    put_optional_bytes(&mut output, message.map(str::as_bytes));
    put_u64(&mut output, intents.len() as u64);
    for intent in &intents {
        put_bytes(&mut output, intent.discriminant().as_bytes());
        encode_intent(&mut output, intent);
    }
    Ok(output)
}

pub fn compute_operation_id(
    repo_id: &RepoId,
    message: Option<&str>,
    intents: &[Intent],
) -> Result<Sha256Digest, SemanticError> {
    let digest: [u8; 32] =
        Sha256::digest(canonical_operation_bytes(repo_id, message, intents)?).into();
    Sha256Digest::parse(format!("sha256:{}", hex_string(digest))).map_err(|error| {
        SemanticError::new(
            SemanticErrorCode::InvalidIdentifier,
            format!("failed to construct operation id: {error}"),
        )
    })
}

fn encode_intent(output: &mut Vec<u8>, intent: &Intent) {
    match intent {
        Intent::CreateFolder { item_id, location } => {
            put_bytes(output, item_id.as_str().as_bytes());
            encode_location(output, location);
        }
        Intent::CreateFile {
            item_id,
            location,
            content,
        } => {
            put_bytes(output, item_id.as_str().as_bytes());
            encode_location(output, location);
            encode_content(output, content);
        }
        Intent::UpdateFolder {
            item_id,
            base_seq,
            location,
        } => {
            put_bytes(output, item_id.as_str().as_bytes());
            put_u64(output, *base_seq as u64);
            encode_location(output, location);
        }
        Intent::UpdateFile {
            item_id,
            base_seq,
            location,
            content,
        } => {
            put_bytes(output, item_id.as_str().as_bytes());
            put_u64(output, *base_seq as u64);
            encode_optional_location(output, location.as_ref());
            encode_optional_content(output, content.as_ref());
        }
        Intent::Trash { root, descendants } => {
            encode_item_base(output, root);
            encode_descendants(output, descendants);
        }
        Intent::Restore {
            root,
            trash_commit_id,
            descendants,
            location,
        } => {
            encode_item_base(output, root);
            put_bytes(output, trash_commit_id.as_str().as_bytes());
            encode_descendants(output, descendants);
            encode_optional_location(output, location.as_ref());
        }
    }
}

fn encode_location(output: &mut Vec<u8>, location: &Location) {
    match &location.parent_item_id {
        Some(parent_id) => {
            output.push(1);
            put_bytes(output, parent_id.as_str().as_bytes());
        }
        None => output.push(0),
    }
    put_bytes(output, location.name.as_bytes());
}

fn encode_content(output: &mut Vec<u8>, content: &BlobRef) {
    put_bytes(output, content.blob_id.as_str().as_bytes());
    put_u64(output, content.size);
}

fn encode_item_base(output: &mut Vec<u8>, item: &ItemBase) {
    put_bytes(output, item.item_id.as_str().as_bytes());
    put_u64(output, item.base_seq as u64);
}

fn encode_descendants(output: &mut Vec<u8>, descendants: &[ItemBase]) {
    put_u64(output, descendants.len() as u64);
    for descendant in descendants {
        encode_item_base(output, descendant);
    }
}

fn encode_optional_location(output: &mut Vec<u8>, location: Option<&Location>) {
    match location {
        Some(location) => {
            output.push(1);
            encode_location(output, location);
        }
        None => output.push(0),
    }
}

fn encode_optional_content(output: &mut Vec<u8>, content: Option<&BlobRef>) {
    match content {
        Some(content) => {
            output.push(1);
            encode_content(output, content);
        }
        None => output.push(0),
    }
}

fn put_optional_bytes(output: &mut Vec<u8>, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            output.push(1);
            put_bytes(output, value);
        }
        None => output.push(0),
    }
}

fn put_bytes(output: &mut Vec<u8>, value: &[u8]) {
    put_u64(output, value.len() as u64);
    output.extend_from_slice(value);
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn hex_string(bytes: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
