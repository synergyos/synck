# synck-vcs

Synck's storage-independent, append-only version-control engine. Its entry point
is `synck_vcs::repo::Repo`.

The engine records immutable item events and attached-ref events in atomic commits.
Item sequences serialize changes to an item; repository sequencing publishes an
ordered history. Accepted current state and published historical checkpoints are
different coordinates.

The engine enforces structural invariants, canonical encoding, content-addressed
commit identities and semantic verification. Storage adapters implement its
`StorageBackend` and `StorageTxn` traits, passed to `Repo::new` through a `Storage`
handle. The engine has no database-client dependency. This repository includes
`synck-vcs-sqlite` for the local library and CLI; SynckHub uses a separate
`synck-vcs-postgres` adapter that is not included here.

Use `synck-file-folder` for file/folder operations, `synck-items` for shared stored
data interpretation and `synck-local` for the local library and `synck` CLI. The
engine does not implement application permissions, networking or object storage.
It provides derived historical-state trees and projection contracts, while
application search and domain read models remain outside the engine.

From the workspace root, a focused canonical-encoding check is:

```sh
cargo test --locked -p synck-vcs --test canonical_vectors_test
```

For a standalone local walkthrough, see the workspace README and
`crates/synck-local/examples/local_repository.rs`.
The public API is pre-1.0; persisted-format compatibility is a separate contract.
Concurrent callers can submit changes to the same repository. Core's transactional
conflict locks and head checks preserve one authoritative history per item;
backend locking determines which transactions can run concurrently. Git-style
branches and merges are not part of the current model.
