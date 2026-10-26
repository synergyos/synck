use std::collections::BTreeSet;

use chrono::DateTime;
use serde::de::{DeserializeOwned, Error as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use synck_vcs::types::{ItemId, RepoId, Sha256Digest};

use crate::{ErrorCode, MAX_JSON_BYTES, PROTOCOL_VERSION, ValidationError};

pub fn decode_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ValidationError> {
    if bytes.len() > MAX_JSON_BYTES {
        return Err(ValidationError::new(ErrorCode::JsonTooLarge));
    }

    let mut duplicate_check = serde_json::Deserializer::from_slice(bytes);
    DuplicateChecked::deserialize(&mut duplicate_check)
        .map_err(|_| ValidationError::new(ErrorCode::InvalidRequest))?;
    duplicate_check
        .end()
        .map_err(|_| ValidationError::new(ErrorCode::InvalidRequest))?;

    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| ValidationError::new(ErrorCode::InvalidRequest))?;
    let version = value
        .as_object()
        .and_then(|object| object.get("protocol_version"));
    match version {
        Some(Value::Number(number)) if !number.is_i64() && !number.is_u64() => {
            return Err(ValidationError::new(ErrorCode::InvalidRequest));
        }
        Some(Value::Number(number)) if number.as_u64() == Some(u64::from(PROTOCOL_VERSION)) => {}
        _ => {
            return Err(ValidationError::new(ErrorCode::UnsupportedProtocolVersion));
        }
    }
    validate_canonical_wire_values(None, &value)?;

    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let decoded = T::deserialize(&mut decoder)
        .map_err(|_| ValidationError::new(ErrorCode::InvalidRequest))?;
    decoder
        .end()
        .map_err(|_| ValidationError::new(ErrorCode::InvalidRequest))?;
    Ok(decoded)
}

pub fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, ValidationError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| ValidationError::new(ErrorCode::InternalError))?;
    if bytes.len() > MAX_JSON_BYTES {
        return Err(ValidationError::new(ErrorCode::JsonTooLarge));
    }
    Ok(bytes)
}

fn validate_canonical_wire_values(
    field: Option<&str>,
    value: &Value,
) -> Result<(), ValidationError> {
    match value {
        Value::Object(object) => {
            for (name, child) in object {
                validate_canonical_wire_values(Some(name), child)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                validate_canonical_wire_values(field, child)?;
            }
        }
        Value::String(raw) => match field {
            Some("repo_id" | "server_id") => {
                let parsed = RepoId::parse(raw)
                    .map_err(|_| ValidationError::new(ErrorCode::InvalidIdentifier))?;
                if parsed.as_str() != raw {
                    return Err(ValidationError::new(ErrorCode::InvalidIdentifier));
                }
            }
            Some("item_id" | "parent_item_id" | "parent_folder_id") => {
                let parsed = ItemId::parse(raw)
                    .map_err(|_| ValidationError::new(ErrorCode::InvalidIdentifier))?;
                if parsed.as_str() != raw {
                    return Err(ValidationError::new(ErrorCode::InvalidIdentifier));
                }
            }
            Some(
                "blob_id" | "operation_id" | "commit_id" | "tip_commit_id" | "trash_commit_id",
            ) => {
                let parsed = Sha256Digest::parse(raw)
                    .map_err(|_| ValidationError::new(ErrorCode::InvalidIdentifier))?;
                if parsed.as_str() != raw {
                    return Err(ValidationError::new(ErrorCode::InvalidIdentifier));
                }
            }
            Some("accepted_at" | "expires_at") => {
                if !raw.ends_with('Z') || DateTime::parse_from_rfc3339(raw).is_err() {
                    return Err(ValidationError::new(ErrorCode::InvalidRequest));
                }
            }
            _ => {}
        },
        Value::Number(number) if !number.is_i64() && !number.is_u64() => {
            return Err(ValidationError::new(ErrorCode::InvalidRequest));
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

struct DuplicateChecked;

impl<'de> Deserialize<'de> for DuplicateChecked {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateVisitor)
    }
}

struct DuplicateVisitor;

impl<'de> Visitor<'de> for DuplicateVisitor {
    type Value = DuplicateChecked;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("valid JSON without duplicate object keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(A::Error::custom(format!("duplicate object key: {key}")));
            }
            map.next_value::<DuplicateChecked>()?;
        }
        Ok(DuplicateChecked)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<DuplicateChecked>()?.is_some() {}
        Ok(DuplicateChecked)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(DuplicateChecked)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(DuplicateChecked)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(DuplicateChecked)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(DuplicateChecked)
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(DuplicateChecked)
    }

    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(DuplicateChecked)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateChecked)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        DuplicateChecked::deserialize(deserializer)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateChecked)
    }
}
