use sha2::{Digest, Sha256};
use synck_vcs::object::{
    EncryptedFrame, Form, HEADER_BYTES, Header, Object, PREFIX, content_form, encrypted_aad,
    plain_id, verify_encrypted_frame, verify_stored,
};
use synck_vcs::types::{ItemId, ObjectId, RepoId};

fn vectors(name: &str) -> Vec<serde_json::Value> {
    let path = format!("{}/../../docs/spec/vectors/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn external_object_vectors() {
    for vector in vectors("objects.json") {
        let name = vector["name"].as_str().unwrap();
        let payload = hex::decode(vector["payload_hex"].as_str().unwrap()).unwrap();
        let id = ObjectId::parse(vector["id"].as_str().unwrap()).unwrap();
        let form = match vector["form"].as_str().unwrap() {
            "PLAIN" => Form::Plain,
            "SALTED" => Form::Salted,
            other => panic!("{name}: unknown form {other}"),
        };
        assert_eq!(ObjectId::from_digest(Sha256::digest(&payload).into()), id, "{name}");
        let result = verify_stored(&id, form, &payload);
        assert_eq!(result.is_ok(), vector["valid"].as_bool().unwrap(), "{name}");
        if result.is_err() {
            continue;
        }
        let bytes = hex::decode(vector["bytes_hex"].as_str().unwrap()).unwrap();
        match form {
            Form::Plain => {
                assert_eq!(payload, bytes, "{name}");
                assert_eq!(plain_id(&bytes), id, "{name}");
            }
            _ => {
                let object = Object::verify(&id, payload.clone()).unwrap();
                assert_eq!(object.bytes(), bytes, "{name}");
                assert_eq!(
                    hex::encode(&payload[PREFIX.len() + 1..HEADER_BYTES]),
                    vector["salt_hex"],
                    "{name}"
                );
            }
        }
    }
}

#[test]
fn external_encrypted_frame_and_aad_vectors() {
    for vector in vectors("encrypted.json") {
        let name = vector["name"].as_str().unwrap();
        let stored = hex::decode(vector["payload_hex"].as_str().unwrap()).unwrap();
        let id = ObjectId::parse(vector["id"].as_str().unwrap()).unwrap();
        assert_eq!(ObjectId::from_digest(Sha256::digest(&stored).into()), id, "{name}");
        assert_eq!(
            verify_encrypted_frame(&stored).is_ok(),
            vector["frame_valid"].as_bool().unwrap(),
            "{name}"
        );
        if let Some(aad) = vector["aad_hex"].as_str() {
            let frame = EncryptedFrame::parse(&stored).unwrap();
            let repo_id = RepoId::parse(vector["repo_id"].as_str().unwrap()).unwrap();
            let item_id = ItemId::parse(vector["item_id"].as_str().unwrap()).unwrap();
            let computed = encrypted_aad(
                frame.scheme,
                frame.key_epoch,
                &repo_id,
                &item_id,
                vector["role"].as_str().unwrap(),
            )
            .unwrap();
            assert_eq!(hex::encode(computed), aad, "{name}");
        }
    }
}

#[test]
fn salts_are_bound_to_exact_bytes() {
    let first = Object::new(b"same bytes\0\r\n").unwrap();
    let second = Object::new(first.bytes()).unwrap();
    assert_ne!(first.id(), second.id());
    assert_eq!(first.bytes(), second.bytes());
    let mut changed = first.payload().to_vec();
    changed[HEADER_BYTES - 1] ^= 1;
    assert!(Object::verify(&first.id(), changed).is_err());
    assert!(Object::new(&[255]).is_err());
}

#[test]
fn the_role_selects_the_form() {
    assert_eq!(content_form(false, "FILE", "bytes"), Form::Plain);
    assert_eq!(content_form(false, "EMAIL", "source"), Form::Plain);
    assert_eq!(content_form(false, "CHAT", "source"), Form::Plain);
    assert_eq!(content_form(false, "CHAT", "body"), Form::Salted);
    assert_eq!(content_form(true, "FILE", "bytes"), Form::Encrypted);
    assert_eq!(content_form(true, "FILE", "sealed_key"), Form::Opaque);
    assert_eq!(content_form(false, "FILE", "sealed_key"), Form::Opaque);
}

#[test]
fn malformed_objects_fail_even_with_matching_digests() {
    let object = Object::new(b"x").unwrap();
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
