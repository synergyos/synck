//! Small erasable event metadata, stored as canonical JSON in salted objects.
use crate::errors::CoreError;
use crate::object::{Codec, Object, ObjectError};
use crate::types::{ObjectId, canonical_json_bytes};

pub const MAX_METADATA_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error(transparent)]
    Object(#[from] ObjectError),
    #[error(transparent)]
    Canonical(#[from] CoreError),
    #[error("metadata must be a canonical JSON object of at most 1 MiB")]
    Invalid,
}

#[derive(Clone, Debug)]
pub struct Metadata {
    object: Object,
    value: serde_json::Value,
}

impl Metadata {
    pub fn new(value: serde_json::Value) -> Result<Self, MetadataError> {
        if !value.is_object() {
            return Err(MetadataError::Invalid);
        }
        let bytes = canonical_json_bytes(&value)?;
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(MetadataError::Invalid);
        }
        Ok(Self {
            object: Object::new(Codec::Utf8, &bytes)?,
            value,
        })
    }

    pub fn verify(id: &ObjectId, payload: Vec<u8>) -> Result<Self, MetadataError> {
        if payload.len() > crate::object::HEADER_BYTES + MAX_METADATA_BYTES {
            return Err(MetadataError::Invalid);
        }
        let object = Object::verify(id, payload)?;
        let value: serde_json::Value =
            serde_json::from_str(object.utf8()?).map_err(|_| MetadataError::Invalid)?;
        if !value.is_object() || canonical_json_bytes(&value)? != object.bytes() {
            return Err(MetadataError::Invalid);
        }
        Ok(Self { object, value })
    }

    pub fn id(&self) -> ObjectId {
        self.object.id()
    }
    pub fn payload(&self) -> &[u8] {
        self.object.payload()
    }
    pub fn value(&self) -> &serde_json::Value {
        &self.value
    }
}
