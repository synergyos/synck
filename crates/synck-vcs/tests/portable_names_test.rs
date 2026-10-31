use serde::Deserialize;
use synck_vcs::naming::{compute_name_key, portable_name, suffixed_name};

#[derive(Deserialize)]
struct Vectors {
    unicode_version: String,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
struct Case {
    name: String,
    valid: bool,
    key: Option<String>,
    fixed: String,
}

#[test]
fn truncation_cannot_expose_a_reserved_or_empty_name() {
    let spaces = " ".repeat(300);
    for name in [
        format!("CON{spaces}x"),
        format!(".synck{spaces}x"),
        format!("{spaces}x"),
        format!("CON.{}", "a".repeat(251)),
    ] {
        let fixed = portable_name(&name);
        assert!(compute_name_key(&fixed).is_ok(), "{fixed:?}");
        assert!(fixed.len() <= 255);
        assert_eq!(portable_name(&fixed), fixed);
    }
}

#[test]
fn portable_names_match_language_neutral_vectors() {
    let vectors: Vectors =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/names.json")).unwrap();
    assert_eq!(vectors.unicode_version, "18.0.0");
    for case in vectors.cases {
        let result = compute_name_key(&case.name);
        assert_eq!(result.is_ok(), case.valid, "{:?}", case.name);
        assert_eq!(result.ok(), case.key, "{:?}", case.name);
        assert_eq!(portable_name(&case.name), case.fixed);
        assert!(compute_name_key(&case.fixed).is_ok());
        let collision = suffixed_name(&case.fixed, 2);
        assert!(collision.len() <= 255);
        assert!(compute_name_key(&collision).is_ok());
        assert_ne!(compute_name_key(&collision), compute_name_key(&case.fixed));
    }
}
