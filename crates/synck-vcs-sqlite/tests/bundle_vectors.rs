use std::io::Cursor;

use synck_vcs_sqlite::SqliteStorage;

/// A bundle is accepted whole or not at all, so truncation can never pass.
#[tokio::test]
async fn external_bundle_vectors() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/spec/vectors/bundle.json")).unwrap();
    for (index, case) in vectors["cases"].as_array().unwrap().iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let target = SqliteStorage::new(directory.path().join(format!("{index}.sqlite")));
        let bundle = case["bundle"].as_str().unwrap().as_bytes().to_vec();
        let result = target.import_bundle(&mut Cursor::new(bundle)).await;
        assert_eq!(result.is_ok(), case["valid"].as_bool().unwrap(), "{name}: {:?}", result.err());
    }
}
