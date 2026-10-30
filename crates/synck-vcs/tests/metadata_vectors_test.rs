use synck_vcs::metadata::{MAX_METADATA_BYTES, Metadata};
use synck_vcs::object::Object;
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
fn metadata_rejects_noncanonical_json_and_oversize_values() {
    for bytes in [
        b"{\"x\": 1}".as_slice(),
        b"{\"x\":1,\"x\":1}",
        b"null",
        b"[]",
        b"{\"x\":9007199254740993}",
    ] {
        let object = Object::new(bytes).unwrap();
        assert!(Metadata::verify(&object.id(), object.payload().to_vec()).is_err());
    }
    assert!(Metadata::new(serde_json::Value::Null).is_err());
    assert!(Metadata::new(serde_json::json!({"x":"a".repeat(MAX_METADATA_BYTES)})).is_err());
}

#[test]
fn event_metadata_wire_is_id_only_and_resolution_preserves_identity() {
    use synck_vcs::metadata::MetadataRef;
    use synck_vcs::types::EventProposal;
    let event = EventProposal::from_fields(
        serde_json::json!({"event_type":"CUSTOM::UPDATE", "set_metadata":{"secret":"private"}}),
    )
    .unwrap();
    let wire = serde_json::to_value(&event).unwrap();
    assert!(wire.get("set_metadata").is_none());
    assert!(wire["metadata_id"].is_string());
    let mut decoded: EventProposal = serde_json::from_value(wire.clone()).unwrap();
    assert!(matches!(decoded.set_metadata, Some(MetadataRef::Absent(_))));
    let reference = decoded.set_metadata.as_mut().unwrap();
    assert!(
        reference
            .resolve(Metadata::new(serde_json::json!({"different":true})).unwrap())
            .is_err()
    );
    reference
        .resolve(
            event
                .set_metadata
                .as_ref()
                .unwrap()
                .payload()
                .unwrap()
                .clone(),
        )
        .unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
    assert!(
        serde_json::from_value::<EventProposal>(
            serde_json::json!({"event_type":"CUSTOM::UPDATE", "set_metadata":{}})
        )
        .is_err()
    );
}
