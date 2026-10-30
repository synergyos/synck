//! Stored object forms, independent of item families and physical storage.
//!
//! An object's ID is always SHA-256 of its stored bytes. The repository and the
//! role select the form, never the bytes: a PLAIN file may begin with any magic.
use ring::rand::{SecureRandom, SystemRandom};
use sha2::{Digest, Sha256};

use crate::types::{ObjectId, RepoId};

pub const PREFIX: &[u8; 16] = b"SYNCK_OBJECT_V1\0";
pub const HEADER_BYTES: usize = PREFIX.len() + 1 + 32;
const UTF8_CODEC: u8 = 1;

pub const ENCRYPTED_PREFIX: &[u8; 16] = b"SYNCK_ENCRYPTED\0";
const ENCRYPTED_AAD_PREFIX: &[u8; 18] = b"synck-encrypted-v1";
pub const SCHEME_XCHACHA20_POLY1305: u16 = 1;
const XCHACHA_NONCE_BYTES: usize = 24;
const POLY1305_TAG_BYTES: usize = 16;
/// The role of an event's metadata object; no `set_content` map may use it.
pub const METADATA_ROLE: &str = "metadata";
/// A one-item key sealed to the repository's ingest key. Opaque, and unframed.
pub const SEALED_KEY_ROLE: &str = "sealed_key";

/// How a stored object must be validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Form {
    Plain,
    Salted,
    Encrypted,
    /// A role this reader cannot interpret, or a sealed key: ID only.
    Opaque,
}

const SALTED_ITEM_TYPES: &[&str] = &[
    "FILE",
    "FOLDER",
    "EMAIL",
    "CHAT",
    "TASK_LIST",
    "TASK",
    "POST",
    "CALENDAR",
    "CALENDAR_EVENT",
    "CALL",
];

/// The form of a content role. Metadata has no role here: it is SALTED, or
/// ENCRYPTED in an encrypted repository.
pub fn content_form(encrypted: bool, item_type: &str, role: &str) -> Form {
    if role == SEALED_KEY_ROLE {
        return Form::Opaque;
    }
    if encrypted {
        return Form::Encrypted;
    }
    match (item_type, role) {
        ("FILE", "bytes") | ("EMAIL" | "CHAT", "source") => Form::Plain,
        _ if SALTED_ITEM_TYPES.contains(&item_type) => Form::Salted,
        _ => Form::Opaque,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ObjectError {
    #[error("invalid object framing")]
    Framing,
    #[error("unsupported object codec")]
    Codec,
    #[error("unsupported encryption scheme")]
    Scheme,
    #[error("object digest mismatch")]
    Digest,
    #[error("object is not valid UTF-8")]
    Utf8,
    #[error("operating-system random source failed")]
    Random,
}

/// Check a stored object of a known form without any key.
pub fn verify_stored(id: &ObjectId, form: Form, stored: &[u8]) -> Result<(), ObjectError> {
    if plain_id(stored) != *id {
        return Err(ObjectError::Digest);
    }
    match form {
        Form::Plain | Form::Opaque => Ok(()),
        Form::Salted => Object::verify(id, stored.to_vec()).map(|_| ()),
        Form::Encrypted => verify_encrypted_frame(stored),
    }
}

/// Check an encrypted frame without any key. An unknown scheme is unreadable,
/// not corrupt: its nonce and tag belong to the scheme.
pub fn verify_encrypted_frame(stored: &[u8]) -> Result<(), ObjectError> {
    match EncryptedFrame::parse(stored) {
        Ok(_) | Err(ObjectError::Scheme) => Ok(()),
        Err(error) => Err(error),
    }
}

/// The ID of stored bytes, whatever their form. For a PLAIN file this is its
/// ordinary SHA-256, so `sha256sum` confirms it.
pub fn plain_id(bytes: &[u8]) -> ObjectId {
    ObjectId::from_digest(Sha256::digest(bytes).into())
}

/// Passes stored bytes through, and proves them against their ID at the end. The
/// caller discards its output unless `finish` succeeds.
pub struct VerifyingWriter<W> {
    id: ObjectId,
    out: W,
    hasher: Sha256,
    size: u64,
}

impl<W: std::io::Write> VerifyingWriter<W> {
    pub fn new(id: ObjectId, out: W) -> Self {
        Self {
            id,
            out,
            hasher: Sha256::new(),
            size: 0,
        }
    }

    pub fn finish(self) -> Result<u64, ObjectError> {
        if ObjectId::from_digest(self.hasher.finalize().into()) != self.id {
            return Err(ObjectError::Digest);
        }
        Ok(self.size)
    }
}

impl<W: std::io::Write> std::io::Write for VerifyingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.out.write(bytes)?;
        self.hasher.update(&bytes[..count]);
        self.size += count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// The parts of an encrypted object. A verifier needs no key to check them; a
/// reader with no key for a scheme or epoch reports the content as unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncryptedFrame<'a> {
    pub scheme: u16,
    pub key_epoch: u64,
    pub nonce: &'a [u8],
    pub ciphertext: &'a [u8],
}

impl<'a> EncryptedFrame<'a> {
    pub fn parse(stored: &'a [u8]) -> Result<Self, ObjectError> {
        let rest = stored
            .strip_prefix(ENCRYPTED_PREFIX.as_slice())
            .ok_or(ObjectError::Framing)?;
        if rest.len() < 10 {
            return Err(ObjectError::Framing);
        }
        let (scheme, rest) = rest.split_at(2);
        let (key_epoch, rest) = rest.split_at(8);
        let scheme = u16::from_be_bytes(scheme.try_into().expect("two bytes"));
        if scheme == 0 {
            return Err(ObjectError::Framing);
        }
        if scheme != SCHEME_XCHACHA20_POLY1305 {
            return Err(ObjectError::Scheme);
        }
        if rest.len() < XCHACHA_NONCE_BYTES + POLY1305_TAG_BYTES {
            return Err(ObjectError::Framing);
        }
        let (nonce, ciphertext) = rest.split_at(XCHACHA_NONCE_BYTES);
        Ok(Self {
            scheme,
            key_epoch: u64::from_be_bytes(key_epoch.try_into().expect("eight bytes")),
            nonce,
            ciphertext,
        })
    }
}

/// Scheme 1 associated data: an object is valid at only one place.
pub fn encrypted_aad(
    scheme: u16,
    key_epoch: u64,
    repo_id: &RepoId,
    item_id: &crate::types::ItemId,
    role: &str,
) -> Result<Vec<u8>, ObjectError> {
    let raw = |value: &str| {
        uuid::Uuid::parse_str(value)
            .map(uuid::Uuid::into_bytes)
            .map_err(|_| ObjectError::Framing)
    };
    let role_length = u8::try_from(role.len()).map_err(|_| ObjectError::Framing)?;
    let mut aad = Vec::with_capacity(ENCRYPTED_AAD_PREFIX.len() + 43 + role.len());
    aad.extend_from_slice(ENCRYPTED_AAD_PREFIX);
    aad.extend_from_slice(&scheme.to_be_bytes());
    aad.extend_from_slice(&key_epoch.to_be_bytes());
    aad.extend_from_slice(&raw(repo_id.as_str())?);
    aad.extend_from_slice(&raw(item_id.as_str())?);
    aad.push(role_length);
    aad.extend_from_slice(role.as_bytes());
    Ok(aad)
}

/// The salt belongs to the object, never to its reference or the event envelope.
#[derive(Clone, Debug)]
pub struct Header([u8; HEADER_BYTES]);

impl Header {
    pub fn new() -> Result<Self, ObjectError> {
        let mut bytes = [0; HEADER_BYTES];
        bytes[..PREFIX.len()].copy_from_slice(PREFIX);
        bytes[PREFIX.len()] = UTF8_CODEC;
        SystemRandom::new()
            .fill(&mut bytes[PREFIX.len() + 1..])
            .map_err(|_| ObjectError::Random)?;
        Ok(Self(bytes))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ObjectError> {
        if bytes.len() != HEADER_BYTES || !bytes.starts_with(PREFIX) {
            return Err(ObjectError::Framing);
        }
        if bytes[PREFIX.len()] != UTF8_CODEC {
            return Err(ObjectError::Codec);
        }
        Ok(Self(bytes.try_into().expect("checked length")))
    }

    pub fn as_bytes(&self) -> &[u8; HEADER_BYTES] {
        &self.0
    }
}

/// A verified SALTED object held in memory.
#[derive(Clone, Debug)]
pub struct Object {
    id: ObjectId,
    payload: Vec<u8>,
}

impl Object {
    /// Salt UTF-8 application bytes.
    pub fn new(bytes: &[u8]) -> Result<Self, ObjectError> {
        std::str::from_utf8(bytes).map_err(|_| ObjectError::Utf8)?;
        let header = Header::new()?;
        let mut payload = Vec::with_capacity(HEADER_BYTES + bytes.len());
        payload.extend_from_slice(header.as_bytes());
        payload.extend_from_slice(bytes);
        Ok(Self {
            id: ObjectId::from_digest(Sha256::digest(&payload).into()),
            payload,
        })
    }

    pub fn verify(id: &ObjectId, payload: Vec<u8>) -> Result<Self, ObjectError> {
        Header::parse(payload.get(..HEADER_BYTES).ok_or(ObjectError::Framing)?)?;
        if ObjectId::from_digest(Sha256::digest(&payload).into()) != *id {
            return Err(ObjectError::Digest);
        }
        std::str::from_utf8(&payload[HEADER_BYTES..]).map_err(|_| ObjectError::Utf8)?;
        Ok(Self {
            id: id.clone(),
            payload,
        })
    }

    pub fn id(&self) -> ObjectId {
        self.id.clone()
    }
    /// Complete stored bytes, including framing and salt.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    /// Application bytes, excluding framing and salt.
    pub fn bytes(&self) -> &[u8] {
        &self.payload[HEADER_BYTES..]
    }
    pub fn utf8(&self) -> &str {
        std::str::from_utf8(self.bytes()).expect("verified UTF-8")
    }
}
