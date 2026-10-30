//! Canonical content snapshots: bounded role names pointing to salted objects.
use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::object::{Object, ObjectError};
use crate::types::ObjectId;

pub const MAX_CONTENT_ROLES: usize = 32;
pub const MAX_ROLE_BYTES: usize = 32;

/// Resolved objects are a read view only; serialization always emits IDs.
#[derive(Clone, Debug, Default)]
pub struct EventContent {
    roles: BTreeMap<String, ObjectId>,
    objects: BTreeMap<ObjectId, Object>,
}

impl EventContent {
    pub fn new(roles: BTreeMap<String, ObjectId>) -> Result<Self, String> {
        if roles.len() > MAX_CONTENT_ROLES || roles.keys().any(|role| !valid_role(role)) {
            return Err(
                "content requires at most 32 roles, each matching [a-z][a-z0-9_]{0,31}".into(),
            );
        }
        Ok(Self {
            roles,
            objects: BTreeMap::new(),
        })
    }

    pub fn bytes(id: ObjectId) -> Self {
        Self::one("bytes", id)
    }
    pub fn body(id: ObjectId) -> Self {
        Self::one("body", id)
    }
    pub fn source(id: ObjectId) -> Self {
        Self::one("source", id)
    }
    fn one(role: &str, id: ObjectId) -> Self {
        Self {
            roles: BTreeMap::from([(role.into(), id)]),
            objects: BTreeMap::new(),
        }
    }
    pub fn get(&self, role: &str) -> Option<&ObjectId> {
        self.roles.get(role)
    }
    pub fn values(&self) -> impl Iterator<Item = &ObjectId> {
        self.roles.values()
    }
    pub fn roles(&self) -> &BTreeMap<String, ObjectId> {
        &self.roles
    }
    pub fn only(&self, role: &str) -> Option<&ObjectId> {
        (self.roles.len() == 1).then(|| self.get(role)).flatten()
    }
    pub fn resolve(&mut self, object: Object) -> Result<(), ObjectError> {
        let id = object.id();
        if !self.roles.values().any(|value| *value == id) {
            return Err(ObjectError::Digest);
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
                while let Some((role, id)) = map.next_entry::<String, ObjectId>()? {
                    if !valid_role(&role)
                        || roles.len() == MAX_CONTENT_ROLES
                        || roles.insert(role, id).is_some()
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
