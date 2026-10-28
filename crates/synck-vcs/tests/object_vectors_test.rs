use sha2::{Digest, Sha256};
use synck_vcs::object::{Codec, HEADER_BYTES, Header, Object, PREFIX};
use synck_vcs::types::ObjectId;

#[test]
fn external_object_vectors() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/objects.json")).unwrap();
    for vector in vectors.as_array().unwrap() {
        let payload = hex::decode(vector["payload_hex"].as_str().unwrap()).unwrap();
        let id = ObjectId::parse(vector["id"].as_str().unwrap()).unwrap();
        let object = Object::verify(&id, payload.clone()).unwrap();
        assert_eq!(hex::encode(object.bytes()), vector["bytes_hex"]);
        assert_eq!(
            hex::encode(&payload[PREFIX.len() + 1..HEADER_BYTES]),
            vector["salt_hex"]
        );
        assert_eq!(
            object.codec(),
            if vector["codec"] == "RAW" {
                Codec::Raw
            } else {
                Codec::Utf8
            }
        );
        assert_eq!(object.utf8().is_ok(), object.codec() == Codec::Utf8);
        assert_eq!(ObjectId::from_digest(Sha256::digest(payload).into()), id);
    }
}

#[test]
fn salts_and_codec_are_bound_to_exact_bytes() {
    for codec in [Codec::Raw, Codec::Utf8] {
        let first = Object::new(codec, b"same bytes\0\r\n").unwrap();
        let second = Object::new(codec, first.bytes()).unwrap();
        assert_ne!(first.id(), second.id());
        assert_eq!(first.bytes(), second.bytes());
        let mut changed = first.payload().to_vec();
        changed[HEADER_BYTES - 1] ^= 1;
        assert!(Object::verify(&first.id(), changed).is_err());
    }
    assert!(Object::new(Codec::Raw, &[255]).is_ok());
    assert!(Object::new(Codec::Utf8, &[255]).is_err());
}

#[test]
fn malformed_objects_fail_even_with_matching_digests() {
    let object = Object::new(Codec::Utf8, b"x").unwrap();
    for offset in [0, PREFIX.len(), HEADER_BYTES] {
        let mut payload = object.payload().to_vec();
        payload[offset] = 255;
        let id = ObjectId::from_digest(Sha256::digest(&payload).into());
        assert!(Object::verify(&id, payload).is_err());
    }
    for len in 0..HEADER_BYTES {
        let payload = &object.payload()[..len];
        let id = ObjectId::from_digest(Sha256::digest(payload).into());
        assert!(Object::verify(&id, payload.to_vec()).is_err());
    }
    assert!(Header::parse(object.payload()).is_err());
}
