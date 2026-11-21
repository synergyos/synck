//! Canonical content snapshots: bounded role names pointing to stored objects.
use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::object::{Object, ObjectError};
use crate::types::ObjectId;

pub const MAX_CONTENT_ROLES: usize = 32;
pub const MAX_ROLE_BYTES: usize = 32;
/// The largest declared object size, the largest signed 64-bit integer.
pub const MAX_CONTENT_SIZE: u64 = i64::MAX as u64;

/// One content entry: an object and the byte length of its stored form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContentRef {
    pub id: ObjectId,
    pub size: u64,
}

impl<'de> Deserialize<'de> for ContentRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Entry {
            id: ObjectId,
            size: u64,
        }
        let entry = Entry::deserialize(deserializer)?;
        if entry.size > MAX_CONTENT_SIZE {
            return Err(serde::de::Error::custom("content size exceeds 2^63 - 1"));
        }
        Ok(Self {
            id: entry.id,
            size: entry.size,
        })
    }
}

/// Resolved objects are a read view only; serialization always emits entries.
#[derive(Clone, Debug, Default)]
pub struct EventContent {
    roles: BTreeMap<String, ContentRef>,
    objects: BTreeMap<ObjectId, Object>,
}

impl EventContent {
    pub fn new(roles: BTreeMap<String, ContentRef>) -> Result<Self, String> {
        if roles.len() > MAX_CONTENT_ROLES || roles.keys().any(|role| !valid_role(role)) {
            return Err(
                "content requires at most 32 roles, each matching [a-z][a-z0-9_]{0,31}".into(),
            );
        }
        if roles.values().any(|entry| entry.size > MAX_CONTENT_SIZE) {
            return Err("content size exceeds 2^63 - 1".into());
        }
        Ok(Self {
            roles,
            objects: BTreeMap::new(),
        })
    }

    pub fn bytes(id: ObjectId, size: u64) -> Self {
        Self::one("bytes", id, size)
    }
    pub fn body(id: ObjectId, size: u64) -> Self {
        Self::one("body", id, size)
    }
    pub fn source(id: ObjectId, size: u64) -> Self {
        Self::one("source", id, size)
    }
    fn one(role: &str, id: ObjectId, size: u64) -> Self {
        Self {
            roles: BTreeMap::from([(role.into(), ContentRef { id, size })]),
            objects: BTreeMap::new(),
        }
    }
    pub fn get(&self, role: &str) -> Option<&ObjectId> {
        self.roles.get(role).map(|entry| &entry.id)
    }
    /// The stored size of a role's object.
    pub fn size(&self, role: &str) -> Option<u64> {
        self.roles.get(role).map(|entry| entry.size)
    }
    pub fn values(&self) -> impl Iterator<Item = &ObjectId> {
        self.roles.values().map(|entry| &entry.id)
    }
    pub fn roles(&self) -> &BTreeMap<String, ContentRef> {
        &self.roles
    }
    pub fn only(&self, role: &str) -> Option<&ObjectId> {
        (self.roles.len() == 1).then(|| self.get(role)).flatten()
    }
    /// Attach a verified object. Every entry naming it must declare its stored
    /// length.
    pub fn resolve(&mut self, object: Object) -> Result<(), ObjectError> {
        let id = object.id();
        let size = object.payload().len() as u64;
        let mut named = self
            .roles
            .values()
            .filter(|entry| entry.id == id)
            .peekable();
        if named.peek().is_none() {
            return Err(ObjectError::Digest);
        }
        if named.any(|entry| entry.size != size) {
            return Err(ObjectError::Size);
        }
        self.objects.insert(id, object);
        Ok(())
    }
    pub fn object(&self, role: &str) -> Option<&Object> {
        self.objects.get(self.get(role)?)
    }
}

fn valid_role(role: &str) -> bool {
    role != crate::object::METADATA_ROLE
        && !role.is_empty()
        && role.len() <= MAX_ROLE_BYTES
        && role.as_bytes()[0].is_ascii_lowercase()
        && role
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl Serialize for EventContent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.roles.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for EventContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Roles;
        impl<'de> serde::de::Visitor<'de> for Roles {
            type Value = EventContent;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a content role map")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut roles = BTreeMap::new();
                while let Some((role, entry)) = map.next_entry::<String, ContentRef>()? {
                    if !valid_role(&role)
                        || roles.len() == MAX_CONTENT_ROLES
                        || roles.insert(role, entry).is_some()
                    {
                        return Err(serde::de::Error::custom(
                            "invalid, duplicate or excessive content role",
                        ));
                    }
                }
                Ok(EventContent {
                    roles,
                    objects: BTreeMap::new(),
                })
            }
        }
        deserializer.deserialize_map(Roles)
    }
}
