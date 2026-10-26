# Format and compatibility scope

This note describes compatibility boundaries implemented by the workspace
crates. It does not define replacement encodings or change existing formats.
[Core engine](core.md) provides the engine data model, API invariants and
canonical commit/event contract. [Local repositories](local-repository.md)
provides the SQLite, working-tree, object-store and archive contract. Separate
remote, replica, historical-index and shared-item encodings remain defined by
the implementations and their checked-in canonical vectors.

The `synck-vcs` engine is independent of its persistence backend. Its
`StorageBackend` and `StorageTxn` interfaces define transaction, ordering,
constraint, read and error semantics; canonical event/commit bytes are owned by
the engine. This repository includes `synck-vcs-sqlite` for local persistence.
SynckHub's PostgreSQL adapter, `synck-vcs-postgres`, implements the same engine
interfaces in a separate repository and is not included here. The SQLite local
directory/archive format is a distribution contract, not a restriction on engine
backends.

| Boundary | Source owner | Compatibility evidence in this tree |
| --- | --- | --- |
| Core item/ref event and commit bytes | `synck-vcs` | `tests/canonical_vectors_test.rs` and canonicalization tests |
| FILE/FOLDER intents and canonical events | `synck-file-folder` | `tests/profile_v1.rs` and `tests/fixtures/v1/` |
| Remote v1 messages and validation | `synck-remote-protocol` | `tests/protocol_v1.rs` and `tests/fixtures/v1/` |
| Sealed-history pages and SQLite installation | `synck-vcs-replica` | Unit tests and `tests/maximum_page_fixture_test.rs` |
| Local repository, content and archive behavior | `synck-local` | `tests/local_repository.rs`, `tests/inspection.rs` and `tests/cli.rs` |
| Shared item vocabulary, typed readers and body encoding | `synck-items` | Public module definitions and focused reader tests |
| Complete format-6 data repositories and verified copying | `synck-local` | `tests/portable_repository.rs` and [data repository contract](portable-repository.md) |

Core stores canonical history and validates its structural invariants. It does
not infer all application semantics from event names. Shared item modules provide
storage-free semantics and readers for supported persisted families; unknown
domain presentation remains an explicit limit.
Referenced bodies, attachments, blobs and historical versions must be available
and verified separately when constructing a complete copy.

Local repositories use `.synck/` metadata and SQLite. Treat that directory and
its durable content as repository data. Do not edit its database or history
files manually, and do not treat an uncoordinated copy of a live SQLite database
as a verified backup. Use the tool's archive, copy and integrity commands for
supported local workflows. Format-6 repositories are directly openable SQLite
directories whose exact bytes live in `.synck/objects/` (`WHOLE_FILE`) or
`.synck/object-store.sqlite3` (`CHUNKED`). Other repository format versions are
rejected.

The public Rust API is pre-1.0. Persisted identities, accepted canonical bytes,
historical state, archive formats and versioned remote/replica protocols must
retain their existing compatibility rules. A source reorganization or a new
crate version does not authorize changing them. Unsupported data or format
versions must be reported explicitly rather than silently discarded.
