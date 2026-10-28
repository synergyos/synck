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

/// Decode a streamed stored object into application bytes. Callers must discard
/// output unless `finish` succeeds; a digest mismatch is detected at EOF.
pub struct Decoder<W> {
    target: W,
    expected: ObjectId,
    header: [u8; HEADER_BYTES],
    header_len: usize,
    codec: Option<Codec>,
    digest: Sha256,
    size: u64,
    utf8_tail: [u8; 4],
    utf8_tail_len: usize,
}

impl<W: std::io::Write> Decoder<W> {
    pub fn new(expected: ObjectId, target: W) -> Self {
        Self {
            target,
            expected,
            header: [0; HEADER_BYTES],
            header_len: 0,
            codec: None,
            digest: Sha256::new(),
            size: 0,
            utf8_tail: [0; 4],
            utf8_tail_len: 0,
        }
    }

    pub fn finish(self) -> Result<(u64, Codec), ObjectError> {
        let codec = self.codec.ok_or(ObjectError::Framing)?;
        if self.utf8_tail_len != 0 {
            return Err(ObjectError::Utf8);
        }
        if ObjectId::from_digest(self.digest.finalize().into()) != self.expected {
            return Err(ObjectError::Digest);
        }
        Ok((self.size, codec))
    }

    fn validate_utf8(&mut self, mut bytes: &[u8]) -> Result<(), ObjectError> {
        while self.utf8_tail_len > 0 && !bytes.is_empty() {
            self.utf8_tail[self.utf8_tail_len] = bytes[0];
            self.utf8_tail_len += 1;
            bytes = &bytes[1..];
            match std::str::from_utf8(&self.utf8_tail[..self.utf8_tail_len]) {
                Ok(_) => self.utf8_tail_len = 0,
                Err(error) if error.error_len().is_none() && self.utf8_tail_len < 4 => {}
                Err(_) => return Err(ObjectError::Utf8),
            }
        }
        if let Err(error) = std::str::from_utf8(bytes) {
            if error.error_len().is_some() {
                return Err(ObjectError::Utf8);
            }
            let tail = &bytes[error.valid_up_to()..];
            self.utf8_tail[..tail.len()].copy_from_slice(tail);
            self.utf8_tail_len = tail.len();
        }
        Ok(())
    }
}

impl<W: std::io::Write> std::io::Write for Decoder<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind};
        let mut application = bytes;
        if self.header_len < HEADER_BYTES {
            let count = application.len().min(HEADER_BYTES - self.header_len);
            self.header[self.header_len..self.header_len + count]
                .copy_from_slice(&application[..count]);
            self.header_len += count;
            application = &application[count..];
            if self.header_len == HEADER_BYTES {
                self.codec = Some(
                    Header::parse(&self.header)
                        .map_err(|e| Error::new(ErrorKind::InvalidData, e))?
                        .codec(),
                );
            }
        }
        if self.codec == Some(Codec::Utf8) {
            self.validate_utf8(application)
                .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
        }
        self.target.write_all(application)?;
        self.digest.update(bytes);
        self.size = self
            .size
            .checked_add(application.len() as u64)
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "object size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.target.flush()
    }
}
