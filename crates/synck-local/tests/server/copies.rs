//! `synck repository import URL`: a follower copy over protocol 2, exactly as
//! the server published its history.

use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use synck_local::server::tokens::{self, Access};
use synck_sync::{Cursor, RepositoryIdentity};
use synck_vcs::config::CoreConfig;
use synck_vcs::repo::Repo;
use synck_vcs::storage::{Storage, StorageBackend as _};
use synck_vcs::types::{CommitProposal, EventProposal, ItemId, ObjectId, RepoId};
use synck_vcs_sqlite::SqliteStorage;

use super::clients::{fixture, ok, start, synck};
use super::common::object_id;

/// Stored bytes of an encrypted frame: scheme 2, then one segment holding
/// `size` plain bytes. A frame checks without its key, so any bytes of this
/// shape serve.
fn frame(fill: u8, size: usize) -> Vec<u8> {
    let mut bytes = synck_vcs::object::ENCRYPTED_PREFIX.to_vec();
    bytes.extend_from_slice(&2u16.to_be_bytes());
    bytes.extend_from_slice(&1u64.to_be_bytes());
    bytes.resize(bytes.len() + 19 + size + 16, fill);
    bytes
}

fn storage(root: &Path, encrypted: bool) -> SqliteStorage {
    SqliteStorage::new(root.join(".synck/repository.sqlite3")).with_encrypted(encrypted)
}

/// The identity and head the repository at `root` holds.
fn head(root: &Path, repo_id: &RepoId, encrypted: bool) -> (RepositoryIdentity, Cursor) {
    let storage = storage(root, encrypted);
    let identity = storage.history_identity(repo_id).unwrap().unwrap();
    let head = storage.history_head(&identity).unwrap();
    (identity, head)
}

/// An encrypted repository as a product profile with keys writes one: a
/// folder holding a file, named by tags, with encrypted metadata and content.
async fn encrypted_origin(root: &Path) -> (RepoId, Vec<(ObjectId, Vec<u8>)>) {
    let metadata = root.join(".synck");
    std::fs::create_dir_all(metadata.join("objects")).unwrap();
    std::fs::write(metadata.join("lock"), b"").unwrap();
    let repo_id = RepoId::new();
    std::fs::write(
        metadata.join("config.json"),
        serde_json::to_vec(&json!({
            "format_version": 8,
            "repo_id": repo_id,
            "author": "owner",
            "authority": "LOCAL",
            "encrypted": true,
        }))
        .unwrap(),
    )
    .unwrap();
    let storage = storage(root, true);
    storage.init_schema().await.unwrap();
    storage
        .create_history(&RepositoryIdentity {
            repo_id: repo_id.clone(),
            generation: uuid::Uuid::now_v7(),
        })
        .unwrap();
    let content = frame(1, 5);
    let content_id = object_id(&content);
    std::fs::write(
        metadata.join("objects").join(&content_id.as_str()[7..]),
        &content,
    )
    .unwrap();
    let objects: Vec<(ObjectId, Vec<u8>)> = [frame(2, 40), frame(3, 40)]
        .into_iter()
        .map(|bytes| (object_id(&bytes), bytes))
        .collect();
    storage.put_metadata_objects(&repo_id, &objects).unwrap();
    let core = Repo::new(
        Storage::new(Arc::new(storage)),
        CoreConfig {
            encrypted: true,
            follows_domains: false,
        },
    );
    // Events name encrypted metadata by ID, as on the wire.
    let tag = |fill: u8| format!("tag:{}", hex::encode([fill; 32]));
    let folder = ItemId::new();
    let events = [
        json!({
            "item_id": folder,
            "event_type": "FOLDER::CREATE",
            "create_item": {"item_type": "FOLDER", "name": tag(1)},
            "metadata_id": objects[0].0,
        }),
        json!({
            "item_id": ItemId::new(),
            "event_type": "FILE::CREATE",
            "create_item": {"item_type": "FILE", "parent_folder_id": folder, "name": tag(2)},
            "metadata_id": objects[1].0,
            "set_content": {"bytes": {"id": content_id, "size": content.len()}},
        }),
    ];
    core.append(
        repo_id.clone(),
        CommitProposal {
            author_id: "owner".into(),
            committer_id: "owner".into(),
            message: None,
            occurred_at: None,
            operation_id: Some("first".into()),
            events: events
                .into_iter()
                .map(|event| serde_json::from_value::<EventProposal>(event).unwrap())
                .collect(),
        },
    )
    .await
    .unwrap();
    let mut objects = objects;
    objects.push((content_id, content));
    (repo_id, objects)
}

#[tokio::test]
async fn an_encrypted_repository_is_copied_as_stored() {
    let parent = tempfile::tempdir().unwrap();
    let state = parent.path().join("state");
    synck_local::server::init(&state).unwrap();
    let origin = parent.path().join("origin");
    let (repo_id, objects) = encrypted_origin(&origin).await;
    let reader = tokens::add(&state, "backup", Access::Read, &[repo_id.clone()]).unwrap();
    let (_running, address) = start(&state, &[origin.clone()], "127.0.0.1:0".parse().unwrap());
    let url = format!("http://{address}/v2/repositories/{}", repo_id.as_str());

    // A working tree needs keys this client does not hold.
    let refused = synck(
        parent.path(),
        &reader,
        &["clone", &url, "tree", "--author", "laptop"],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("synck repository import"));

    let copy = parent.path().join("copy");
    ok(
        parent.path(),
        &reader,
        &["repository", "import", &url, copy.to_str().unwrap()],
    );
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(copy.join(".synck/config.json")).unwrap()).unwrap();
    assert_eq!(config["authority"], "FOLLOWER");
    assert_eq!(config["encrypted"], true);
    // The server's generation and checkpoint chain, so its head.
    assert_eq!(head(&copy, &repo_id, true), head(&origin, &repo_id, true));
    let copied = storage(&copy, true);
    for (id, bytes) in &objects[..2] {
        assert_eq!(
            copied.metadata_object(&repo_id, id).unwrap().as_ref(),
            Some(bytes)
        );
    }
    let (content_id, content) = &objects[2];
    assert_eq!(
        &std::fs::read(copy.join(".synck/objects").join(&content_id.as_str()[7..])).unwrap(),
        content
    );
    ok(
        parent.path(),
        &reader,
        &["repository", "verify", copy.to_str().unwrap()],
    );
    ok(
        parent.path(),
        &reader,
        &["repository", "log", copy.to_str().unwrap()],
    );
}

#[tokio::test]
async fn a_repository_is_copied_from_its_server_as_a_follower() {
    let fixture = fixture().await;
    let copy = fixture.parent.path().join("copy");
    ok(
        fixture.parent.path(),
        &fixture.writer,
        &[
            "repository",
            "import",
            &fixture.url,
            copy.to_str().unwrap(),
            "--storage-mode",
            "CHUNKED",
        ],
    );
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(copy.join(".synck/config.json")).unwrap()).unwrap();
    let repo_id = RepoId::parse(config["repo_id"].as_str().unwrap()).unwrap();
    assert_eq!(config["authority"], "FOLLOWER");
    assert_eq!(config["storage_mode"], "CHUNKED");
    assert_eq!(
        head(&copy, &repo_id, false),
        head(&fixture.origin, &repo_id, false)
    );
    ok(
        fixture.parent.path(),
        &fixture.writer,
        &["repository", "verify", copy.to_str().unwrap()],
    );
    let listed = ok(
        fixture.parent.path(),
        &fixture.writer,
        &["item", "list", "--path", copy.to_str().unwrap()],
    );
    assert!(listed.contains("brief.txt"), "{listed}");
    // A destination that exists is never written.
    assert!(
        !synck(
            fixture.parent.path(),
            &fixture.writer,
            &["repository", "import", &fixture.url, copy.to_str().unwrap()],
        )
        .status
        .success()
    );
}
