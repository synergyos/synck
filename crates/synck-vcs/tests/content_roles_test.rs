use synck_vcs::types::{ContentRef, EventContent, ObjectId};

#[test]
fn roles_are_bounded_unique_and_strictly_typed() {
    let id = ObjectId::from_digest([42; 32]);
    let entry = serde_json::json!({"id":id,"size":7});
    let empty: EventContent = serde_json::from_str("{}").unwrap();
    assert_eq!(empty.values().count(), 0);
    let valid = serde_json::json!({"bytes":entry,"source":entry,"provider":entry});
    let roles: EventContent = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(roles.size("bytes"), Some(7));
    assert_eq!(serde_json::to_value(roles).unwrap(), valid);
    for role in ["", "Body", "body-part", "é", "1body", &"a".repeat(33)] {
        assert!(serde_json::from_value::<EventContent>(serde_json::json!({role:entry})).is_err());
    }
    for value in [
        serde_json::json!({"text":"inline"}),
        serde_json::json!({"json":{}}),
        serde_json::json!({"body":null}),
        serde_json::json!({"body":id}),
        serde_json::json!({"body":{"id":id}}),
        serde_json::json!({"body":{"size":7}}),
        serde_json::json!({"body":{"id":id,"size":-1}}),
        serde_json::json!({"body":{"id":id,"size":1u64 << 63}}),
        serde_json::json!({"body":{"id":id,"size":7,"form":"PLAIN"}}),
    ] {
        assert!(serde_json::from_value::<EventContent>(value).is_err());
    }
    assert!(
        serde_json::from_str::<EventContent>(&format!("{{\"body\":{entry},\"body\":{entry}}}"))
            .is_err()
    );
    let too_many: std::collections::BTreeMap<_, _> = (0..33)
        .map(|i| {
            let size = 7;
            (
                format!("role{i}"),
                ContentRef {
                    id: id.clone(),
                    size,
                },
            )
        })
        .collect();
    assert!(EventContent::new(too_many.clone()).is_err());
    assert!(
        serde_json::from_value::<EventContent>(serde_json::to_value(too_many).unwrap()).is_err()
    );
    let oversized = ContentRef {
        id: id.clone(),
        size: 1 << 63,
    };
    assert!(EventContent::new([("bytes".to_string(), oversized)].into()).is_err());
}
