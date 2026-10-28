//! Salted object framing, independent of item families and physical storage.
use ring::rand::{SecureRandom, SystemRandom};
use sha2::{Digest, Sha256};

use crate::types::ObjectId;

pub const PREFIX: &[u8; 16] = b"SYNCK_OBJECT_V1\0";
pub const HEADER_BYTES: usize = PREFIX.len() + 1 + 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Raw,
    Utf8,
}

#[derive(Debug, thiserror::Error)]
pub enum ObjectError {
    #[error("invalid object framing")]
    Framing,
    #[error("unsupported object codec")]
    Codec,
    #[error("object digest mismatch")]
    Digest,
    #[error("object is not valid UTF-8")]
    Utf8,
    #[error("operating-system random source failed")]
    Random,
}

/// The salt belongs to the object, never to its reference or the event envelope.
#[derive(Clone, Debug)]
pub struct Header([u8; HEADER_BYTES]);

impl Header {
    pub fn new(codec: Codec) -> Result<Self, ObjectError> {
        let mut bytes = [0; HEADER_BYTES];
        bytes[..PREFIX.len()].copy_from_slice(PREFIX);
        bytes[PREFIX.len()] = match codec {
            Codec::Raw => 0,
            Codec::Utf8 => 1,
        };
        SystemRandom::new()
            .fill(&mut bytes[PREFIX.len() + 1..])
            .map_err(|_| ObjectError::Random)?;
        Ok(Self(bytes))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ObjectError> {
        if bytes.len() != HEADER_BYTES || !bytes.starts_with(PREFIX) {
            return Err(ObjectError::Framing);
        }
        if bytes[PREFIX.len()] > 1 {
            return Err(ObjectError::Codec);
        }
        Ok(Self(bytes.try_into().expect("checked length")))
    }

    pub fn codec(&self) -> Codec {
        match self.0[PREFIX.len()] {
            0 => Codec::Raw,
            1 => Codec::Utf8,
            _ => unreachable!(),
        }
    }

    pub fn as_bytes(&self) -> &[u8; HEADER_BYTES] {
        &self.0
    }
}

/// A verified object held in memory. Large RAW objects use the same Header and
/// stream their payload through the physical store instead of allocating this.
#[derive(Clone, Debug)]
pub struct Object {
    id: ObjectId,
    header: Header,
    payload: Vec<u8>,
}

impl Object {
    pub fn new(codec: Codec, bytes: &[u8]) -> Result<Self, ObjectError> {
        validate_bytes(codec, bytes)?;
        let header = Header::new(codec)?;
        let mut payload = Vec::with_capacity(HEADER_BYTES + bytes.len());
        payload.extend_from_slice(header.as_bytes());
        payload.extend_from_slice(bytes);
        Ok(Self {
            id: ObjectId::from_digest(Sha256::digest(&payload).into()),
            header,
            payload,
        })
    }

    pub fn verify(id: &ObjectId, payload: Vec<u8>) -> Result<Self, ObjectError> {
        let header = Header::parse(payload.get(..HEADER_BYTES).ok_or(ObjectError::Framing)?)?;
        if ObjectId::from_digest(Sha256::digest(&payload).into()) != *id {
            return Err(ObjectError::Digest);
        }
        validate_bytes(header.codec(), &payload[HEADER_BYTES..])?;
        Ok(Self {
            id: id.clone(),
            header,
            payload,
        })
    }

    pub fn id(&self) -> ObjectId {
        self.id.clone()
    }
    pub fn codec(&self) -> Codec {
        self.header.codec()
    }
    /// Complete stored bytes, including framing and salt.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    /// Application bytes, excluding framing and salt.
    pub fn bytes(&self) -> &[u8] {
        &self.payload[HEADER_BYTES..]
    }
    pub fn utf8(&self) -> Result<&str, ObjectError> {
        if self.codec() != Codec::Utf8 {
            return Err(ObjectError::Codec);
        }
        Ok(std::str::from_utf8(self.bytes()).expect("verified UTF-8"))
    }
}

fn validate_bytes(codec: Codec, bytes: &[u8]) -> Result<(), ObjectError> {
    if codec == Codec::Utf8 {
        std::str::from_utf8(bytes).map_err(|_| ObjectError::Utf8)?;
    }
    Ok(())
}
