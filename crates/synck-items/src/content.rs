//! The single plaintext logical-body wire codec. No encryption or key storage.
use ring::rand::{SecureRandom, SystemRandom};
use sha2::{Digest, Sha256};
use synck_vcs::types::ContentId;

pub const STORAGE_CODEC: &str = "PLAINTEXT_UTF8_V1";
pub const PREFIX: &[u8; 19] = b"SYNCK_UTF8_BODY_V1\0";
pub const FRAMING_BYTES: usize = PREFIX.len() + 32;
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_PAYLOAD_BYTES: usize = FRAMING_BYTES + MAX_BODY_BYTES;
pub const MAX_BATCH_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error("unsupported content codec")]
    Codec,
    #[error("content payload size is out of bounds")]
    Size,
    #[error("invalid content: {0}")]
    Invalid(&'static str),
    #[error("operating-system random source failed")]
    Random,
}

/// Verified current-codec bytes. Fields are private to keep construction honest.
#[derive(Clone, Debug)]
pub struct ContentObject {
    id: ContentId,
    payload: Vec<u8>,
}

pub fn body_length(codec: &str, payload_len: usize) -> Result<usize, ContentError> {
    if codec != STORAGE_CODEC {
        return Err(ContentError::Codec);
    }
    if !(FRAMING_BYTES..=MAX_PAYLOAD_BYTES).contains(&payload_len) {
        return Err(ContentError::Size);
    }
    Ok(payload_len - FRAMING_BYTES)
}

impl ContentObject {
    pub fn new(body: &str) -> Result<Self, ContentError> {
        if body.len() > MAX_BODY_BYTES {
            return Err(ContentError::Size);
        }
        let mut payload = Vec::with_capacity(FRAMING_BYTES + body.len());
        payload.extend_from_slice(PREFIX);
        payload.resize(FRAMING_BYTES, 0);
        SystemRandom::new()
            .fill(&mut payload[PREFIX.len()..])
            .map_err(|_| ContentError::Random)?;
        payload.extend_from_slice(body.as_bytes());
        Ok(Self {
            id: ContentId::from_digest(Sha256::digest(&payload).into()),
            payload,
        })
    }

    pub fn verify(id: ContentId, codec: &str, payload: Vec<u8>) -> Result<Self, ContentError> {
        body_length(codec, payload.len())?;
        if !payload.starts_with(PREFIX) {
            return Err(ContentError::Invalid("prefix"));
        }
        if ContentId::from_digest(Sha256::digest(&payload).into()) != id {
            return Err(ContentError::Invalid("hash"));
        }
        std::str::from_utf8(&payload[FRAMING_BYTES..])
            .map_err(|_| ContentError::Invalid("UTF-8"))?;
        Ok(Self { id, payload })
    }

    pub fn id(&self) -> ContentId {
        self.id
    }
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    pub fn body(&self) -> &str {
        std::str::from_utf8(&self.payload[FRAMING_BYTES..]).expect("verified at construction")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_utf8_and_distinct_salts() {
        for body in ["", " \0🦀é\r\n", "same message"] {
            let a = ContentObject::new(body).unwrap();
            let b = ContentObject::new(body).unwrap();
            assert_ne!(a.id(), b.id());
            assert_eq!(a.payload().len(), FRAMING_BYTES + body.len());
            assert_eq!(
                ContentObject::verify(a.id(), STORAGE_CODEC, a.payload().to_vec())
                    .unwrap()
                    .body(),
                body
            );
        }
    }

    #[test]
    fn size_and_codec_boundaries() {
        assert_eq!(FRAMING_BYTES, 51);
        assert_eq!(MAX_PAYLOAD_BYTES, 2_097_203);
        assert!(ContentObject::new(&"x".repeat(MAX_BODY_BYTES)).is_ok());
        assert!(ContentObject::new(&"x".repeat(MAX_BODY_BYTES + 1)).is_err());
        for len in [0, FRAMING_BYTES - 1, MAX_PAYLOAD_BYTES + 1] {
            assert!(body_length(STORAGE_CODEC, len).is_err());
        }
        assert!(body_length("UNKNOWN", FRAMING_BYTES).is_err());
    }

    #[test]
    fn rejects_corruption_even_under_a_self_consistent_hash() {
        let a = ContentObject::new("test").unwrap();
        let mut bytes = a.payload().to_vec();
        bytes[FRAMING_BYTES] ^= 1;
        assert!(ContentObject::verify(a.id(), STORAGE_CODEC, bytes.clone()).is_err());
        bytes[0] ^= 1;
        let id = ContentId::from_digest(Sha256::digest(&bytes).into());
        assert!(ContentObject::verify(id, STORAGE_CODEC, bytes).is_err());
        let mut bytes = a.payload().to_vec();
        bytes[FRAMING_BYTES] = 255;
        let id = ContentId::from_digest(Sha256::digest(&bytes).into());
        assert!(ContentObject::verify(id, STORAGE_CODEC, bytes).is_err());
    }
}
