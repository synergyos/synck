use synck_vcs::metadata::{MAX_METADATA_BYTES, Metadata};
use synck_vcs::object::{Codec, Object};
use synck_vcs::types::ObjectId;

#[test]
fn external_metadata_vector_and_salted_writes() {
    let vector: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/metadata.json")).unwrap();
    let id = ObjectId::parse(vector["id"].as_str().unwrap()).unwrap();
    let metadata = Metadata::verify(
        &id,
        hex::decode(vector["payload_hex"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata.value(), &vector["value"]);
    let first = Metadata::new(vector["value"].clone()).unwrap();
    let second = Metadata::new(vector["value"].clone()).unwrap();
    assert_ne!(first.id(), second.id());
    assert_eq!(first.value(), second.value());
}

#[test]
fn metadata_rejects_noncanonical_json_wrong_codec_and_oversize_values() {
    for bytes in [
        b"{\"x\": 1}".as_slice(),
        b"{\"x\":1,\"x\":1}",
        b"null",
        b"[]",
        b"{\"x\":9007199254740993}",
    ] {
        let object = Object::new(Codec::Utf8, bytes).unwrap();
        assert!(Metadata::verify(&object.id(), object.payload().to_vec()).is_err());
    }
    let raw = Object::new(Codec::Raw, b"{}").unwrap();
    assert!(Metadata::verify(&raw.id(), raw.payload().to_vec()).is_err());
    assert!(Metadata::new(serde_json::Value::Null).is_err());
    assert!(Metadata::new(serde_json::json!({"x":"a".repeat(MAX_METADATA_BYTES)})).is_err());
}
