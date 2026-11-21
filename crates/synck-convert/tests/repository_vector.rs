//! The format 7 repository vector converts to exactly the format 8 one, which
//! the vector generator writes independently.
use std::collections::BTreeMap;

use rusqlite::Connection;
use synck_convert::{Converter, Kind, SourceCommit, SourceEvent};
use synck_vcs::codec::envelope_metadata_id;
use synck_vcs::types::{
    CommitId, ObjectId, RepoId, commit_id_from_digest_bytes, parse_hashed_time,
};

const REPO: &str = "019a2900-0000-7000-8000-000000000001";

fn database(schema: &str, rows: &str) -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(schema).unwrap();
    connection.execute_batch(rows).unwrap();
    connection
}

fn objects(connection: &Connection) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut statement = connection
        .prepare("SELECT object_id, payload FROM vcs_metadata_objects")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// Events by (repo_seq, commit_event_idx), item and ref events together.
fn events(connection: &Connection) -> BTreeMap<(i64, i64), (Kind, Vec<u8>)> {
    let mut found = BTreeMap::new();
    for (table, kind) in [("vcs_events", Kind::Item), ("vcs_ref_events", Kind::Ref)] {
        let mut statement = connection
            .prepare(&format!(
                "SELECT c.repo_seq, e.commit_event_idx, e.payload FROM {table} e \
                 JOIN vcs_commits c USING (repo_id, commit_pk)"
            ))
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok(((row.get(0)?, row.get(1)?), (kind, row.get(2)?)))
            })
            .unwrap();
        found.extend(rows.map(Result::unwrap));
    }
    found
}

/// The stored size of every content object, read from the format 8 vector's
/// object directory: the same bytes and IDs as format 7's.
fn sizes() -> BTreeMap<ObjectId, u64> {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/spec/vectors/repository/objects");
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            (
                ObjectId::parse(format!("sha256:{name}")).unwrap(),
                entry.metadata().unwrap().len(),
            )
        })
        .collect()
}

fn commits(connection: &Connection) -> Vec<(i64, CommitId, Vec<u8>)> {
    let mut statement = connection
        .prepare("SELECT repo_seq, commit_id, checkpoint FROM vcs_commits ORDER BY repo_seq")
        .unwrap();
    statement
        .query_map([], |row| {
            let id: Vec<u8> = row.get(1)?;
            Ok((
                row.get(0)?,
                commit_id_from_digest_bytes(&id).unwrap(),
                row.get(2)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn the_format_7_vector_converts_to_the_format_8_vector() {
    let source = database(
        include_str!("format7/history.sql"),
        include_str!("format7/rows.sql"),
    );
    let target = database(
        include_str!("../../../docs/spec/history.sql"),
        include_str!("../../../docs/spec/vectors/repository/rows.sql"),
    );
    let source_objects = objects(&source);
    let sizes = sizes();
    let source_events = events(&source);
    let generation: String = source
        .query_row(
            "SELECT generation FROM vcs_history_generations",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut converter = Converter::new(&RepoId::parse(REPO).unwrap(), &generation).unwrap();

    let mut statement = source
        .prepare(
            "SELECT commit_id, repo_seq, author_id, committer_id, message, occurred_at, \
             created_at FROM vcs_commits ORDER BY repo_seq",
        )
        .unwrap();
    let sources: Vec<SourceCommit> = statement
        .query_map([], |row| {
            let id: Vec<u8> = row.get(0)?;
            let created_at: String = row.get(6)?;
            Ok(SourceCommit {
                commit_id: commit_id_from_digest_bytes(&id).unwrap(),
                repo_seq: row.get(1)?,
                author_id: row.get(2)?,
                committer_id: row.get(3)?,
                message: row.get(4)?,
                occurred_at: row.get(5)?,
                created_at: parse_hashed_time(&created_at).unwrap(),
                events: Vec::new(),
            })
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();

    let mut converted_commits = Vec::new();
    let mut converted_payloads = BTreeMap::new();
    let mut written = BTreeMap::new();
    for mut source_commit in sources {
        let repo_seq = source_commit.repo_seq;
        for (kind, payload) in source_events
            .range((repo_seq, 0)..(repo_seq + 1, 0))
            .map(|(_, event)| event)
        {
            let metadata =
                serde_json::from_slice::<serde_json::Value>(payload).unwrap()["metadata_id"]
                    .as_str()
                    .map(|id| hex::decode(&id["sha256:".len()..]).unwrap())
                    .and_then(|id| source_objects.get(&id).cloned());
            source_commit.events.push(SourceEvent {
                kind: *kind,
                payload: payload.clone(),
                metadata,
                sizes: sizes.clone(),
            });
        }
        let commit = converter.convert(source_commit).unwrap();
        for metadata in &commit.metadata {
            written.insert(
                hex::decode(&metadata.id().as_str()["sha256:".len()..]).unwrap(),
                metadata.payload().to_vec(),
            );
        }
        for (index, payload) in commit.payloads.into_iter().enumerate() {
            converted_payloads.insert((repo_seq, index as i64), payload);
        }
        converted_commits.push((repo_seq, commit.commit_id, commit.checkpoint.to_vec()));
    }

    assert_eq!(converted_commits, commits(&target));
    let target_payloads: BTreeMap<_, _> = events(&target)
        .into_iter()
        .map(|(position, (_, payload))| (position, payload))
        .collect();
    assert_eq!(converted_payloads, target_payloads);
    // The target holds every object a converted envelope names: a source
    // object kept as it was, or one the conversion wrote.
    let mut named = BTreeMap::new();
    for payload in converted_payloads.values() {
        if let Some(id) = envelope_metadata_id(payload).unwrap() {
            let id = hex::decode(&id.as_str()["sha256:".len()..]).unwrap();
            let bytes = written
                .get(&id)
                .or_else(|| source_objects.get(&id))
                .unwrap()
                .clone();
            named.insert(id, bytes);
        }
    }
    assert_eq!(named, objects(&target));
    assert_eq!(converter.dropped_mailcal, 0);
}
