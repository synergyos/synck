use synck_vcs::types::{EventContent, ObjectId};

#[test]
fn roles_are_bounded_unique_and_strictly_typed() {
    let id = ObjectId::from_digest([42; 32]);
    let empty: EventContent = serde_json::from_str("{}").unwrap();
    assert_eq!(empty.values().count(), 0);
    let valid = serde_json::json!({"bytes":id,"source":id,"provider":id});
    let roles: EventContent = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(serde_json::to_value(roles).unwrap(), valid);
    for role in ["", "Body", "body-part", "é", "1body", &"a".repeat(33)] {
        assert!(serde_json::from_value::<EventContent>(serde_json::json!({role:id})).is_err());
    }
    for value in [
        serde_json::json!({"text":"inline"}),
        serde_json::json!({"json":{}}),
        serde_json::json!({"body":null}),
    ] {
        assert!(serde_json::from_value::<EventContent>(value).is_err());
    }
    assert!(
        serde_json::from_str::<EventContent>(&format!("{{\"body\":\"{id}\",\"body\":\"{id}\"}}"))
            .is_err()
    );
    let too_many: std::collections::BTreeMap<_, _> =
        (0..33).map(|i| (format!("role{i}"), id.clone())).collect();
    assert!(EventContent::new(too_many.clone()).is_err());
    assert!(
        serde_json::from_value::<EventContent>(serde_json::to_value(too_many).unwrap()).is_err()
    );
}
