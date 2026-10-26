//! Finite FILE resource-fork ref payload semantics.
//! Storage, commit pairing and publication are owned by the caller.

use serde::Deserialize;
use synck_vcs::{
    errors::CoreError,
    types::{BlobId, EventContent, RefEventProposal},
};

pub const SET: &str = "FILE_RESOURCE_FORK::SET";
pub const CLEAR: &str = "FILE_RESOURCE_FORK::CLEAR";
const PREFIX: &str = "FILE_RESOURCE_FORK::";
pub const MAX_CANONICAL_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Blob {
    pub blob_id: BlobId,
    pub size: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum ResourceFork {
    Absent,
    Present(Blob),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Activation {
    pub primary_item_seq: i64,
    pub primary_commit_event_idx: i32,
    pub fork: ResourceFork,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetMetadata {
    primary_item_seq: i64,
    primary_commit_event_idx: i32,
    size: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClearMetadata {
    primary_item_seq: i64,
    primary_commit_event_idx: i32,
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::InvalidProposal(format!("FILE_RESOURCE_FORK: {}", message.into()))
}

pub fn is_resource_event(event_type: &str) -> bool {
    event_type == "FILE_RESOURCE_FORK" || event_type.starts_with(PREFIX)
}

/// Decode only the finite resource namespace. Other ref families stay owned by
/// their existing consumers; arbitrary ref metadata never names resources.
pub fn decode_activation(event: &RefEventProposal) -> Result<Option<Activation>, CoreError> {
    if !is_resource_event(&event.event_type) {
        return Ok(None);
    }
    if event.initial_ref_seq.is_some() || !(0..=MAX_CANONICAL_INTEGER).contains(&event.base_ref_seq)
    {
        return Err(invalid("invalid ref sequence envelope"));
    }
    let metadata = event
        .set_metadata
        .clone()
        .ok_or_else(|| invalid("missing metadata"))?;
    let activation = match event.event_type.as_str() {
        SET => {
            let meta: SetMetadata =
                serde_json::from_value(metadata).map_err(|e| invalid(e.to_string()))?;
            if !(1..=MAX_CANONICAL_INTEGER).contains(&meta.size) {
                return Err(invalid(
                    "SET size must be positive; use CLEAR for an empty fork",
                ));
            }
            let Some(EventContent::Blob { blob_id }) = &event.set_content else {
                return Err(invalid("SET requires first-class Blob content"));
            };
            Activation {
                primary_item_seq: meta.primary_item_seq,
                primary_commit_event_idx: meta.primary_commit_event_idx,
                fork: ResourceFork::Present(Blob {
                    blob_id: BlobId::parse(blob_id).map_err(|e| invalid(e.to_string()))?,
                    size: meta.size,
                }),
            }
        }
        CLEAR => {
            let meta: ClearMetadata =
                serde_json::from_value(metadata).map_err(|e| invalid(e.to_string()))?;
            if event.set_content.is_some() {
                return Err(invalid("CLEAR must not carry content"));
            }
            Activation {
                primary_item_seq: meta.primary_item_seq,
                primary_commit_event_idx: meta.primary_commit_event_idx,
                fork: ResourceFork::Absent,
            }
        }
        _ => return Err(invalid("unknown resource event")),
    };
    if !(1..=MAX_CANONICAL_INTEGER).contains(&activation.primary_item_seq)
        || activation.primary_commit_event_idx < 0
    {
        return Err(invalid("invalid primary revision coordinates"));
    }
    Ok(Some(activation))
}
