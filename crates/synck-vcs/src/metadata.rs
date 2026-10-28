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

/// The wire form is always an ID. Available bytes are only a verified in-memory
/// view, so loading or erasing a payload cannot change an event's hash.
#[derive(Clone, Debug)]
pub enum MetadataRef {
    Available(Metadata),
    Absent(ObjectId),
}

impl MetadataRef {
    pub fn new(value: serde_json::Value) -> Result<Self, MetadataError> {
        Metadata::new(value).map(Self::Available)
    }
    pub fn id(&self) -> ObjectId {
        match self {
            Self::Available(metadata) => metadata.id(),
            Self::Absent(id) => id.clone(),
        }
    }
    pub fn payload(&self) -> Option<&Metadata> {
        match self {
            Self::Available(metadata) => Some(metadata),
            Self::Absent(_) => None,
        }
    }
    pub fn value(&self) -> Option<&serde_json::Value> {
        self.payload().map(Metadata::value)
    }
    pub fn as_object(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.value().and_then(serde_json::Value::as_object)
    }
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.value()?.get(key)
    }
    pub fn resolve(&mut self, metadata: Metadata) -> Result<(), MetadataError> {
        if self.id() != metadata.id() {
            return Err(ObjectError::Digest.into());
        }
        *self = Self::Available(metadata);
        Ok(())
    }
}

impl serde::Serialize for MetadataRef {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.id().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for MetadataRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        ObjectId::deserialize(deserializer).map(Self::Absent)
    }
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
