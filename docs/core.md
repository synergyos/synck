# Core engine

The [format specification](spec/format.md) defines persisted bytes, hashes,
SQLite relations, names, structural replay and publication checkpoints. It is
authoritative. This document describes the engine's behavior and boundaries.

## Acceptance and publication

An item has a stable UUID and its own consecutive event sequence. Moving or
renaming it preserves its identity. Attached refs have a separate sequence under
the same owner. A commit atomically groups item and ref events; item events come
first. There is no staging area, branch graph or repository-wide parent commit.

`Repo::append` accepts changes to locally authoritative items. The storage
transaction locks touched items and namespace guards, checks bases and structure,
materializes IDs/sequences, stores metadata objects and envelopes, and seals the
commit hash. All changes succeed or none do. Multiple events for one item use
the same pre-commit base. Applications should use checked FILE/FOLDER intents for
filesystem mutations so subtree closure and namespace swaps are validated together.

Acceptance and repository publication are separate. The sequencer assigns
`repo_seq` and its checkpoint atomically after acceptance. Waiting for publication
must not retain item/folder acceptance locks. Publication failure delays repo-wide
visibility; it does not undo an accepted write. Per-item reads can see accepted
unpublished work. Repository scans use published positions and event indexes.

Never allocate repository positions in independent acceptance transactions:
transactions can finish out of order and a cursor would then skip a late commit.
The SQLite adapter serializes publication, preserving accepted dependencies.
A commit's hash identifies its content; its checkpoint binds its placement and
acceptance time. The history generation distinguishes explicit replacement from
ordinary extension. Checkpoints do not implement authentication or retention.

## Embedding boundaries

The engine provides structural integrity, sequencing, canonicalization,
idempotency and verification. The application supplies authenticated author and
committer identities, permissions, object durability, business rules and user
interface. An author string in a hash does not authenticate the author.

`ingest` handles the existing federated-item/domain authority contract. It is
not a cross-machine synchronization client. The [central synchronization
protocol](spec/protocol.md) and `synck-sync` crate define shared wire values and
verification. 0.2 does not ship the old replica or remote-protocol crates.

Optional operation IDs identify retries. Retrying an accepted operation must
resolve the original outcome, not create a second operation with a new ID.
Handle stable error categories rather than comparing prose. In particular,
conflicts require rereading the current base; accepted-but-incomplete outcomes
require recovery/finalization, not blind resubmission.

The generic Core validates structure without understanding every item family's
metadata. Typed 0.2 readers cover FILE, FOLDER, CHAT and EMAIL and their supported
message/attachment/ref events. Unsupported typed families fail explicitly while
remaining structurally verifiable. Do not add family-specific reachability:
all metadata references and content-role values are discoverable in envelopes.

Sequence subitem readers retain `content_position` separately from
`latest_position`: renaming or trashing a chat file does not create a new content
revision. `subitems_in_family` borrows the matching family without cloning a
chat transcript. Disposable reader caches use version 2; version-1 caches must
be rebuilt from verified history. Repository formats and accepted bytes are
unchanged.

## Storage and recovery

`synck-vcs` is adapter independent. `synck-vcs-sqlite` supplies the current
SQLite implementation. Writes belong in the adapter transaction APIs, not direct
SQL from consumers. Direct mutation bypasses bases, closure, idempotency and
publication invariants.

Canonical history is in `repository.sqlite3`. Historical-state trees and checkout
projections live in separate rebuildable databases. Missing caches are replayed
from accepted history. Verification checks canonical history and objects; a
cached tree root is not an integrity proof.

History can be valid while payloads are absent. Typed presentation and exports
that need an absent object fail explicitly. Present objects with wrong hashes or
framing are corrupt. Verification never repairs history by changing hashes.

Normal undo appends compensating history. Administrative history replacement,
erasure and regulated retention integration are not implemented commands in 0.2.
Their required boundaries are specified in the [format](spec/format.md#administrative-replacement-and-retention-boundary).

## API and compatibility

Rust APIs are pre-1.0. Each release reads only its own persisted repository
format; see [compatibility](formats.md). Embedders must pin a tested revision and
keep the engine, adapter and typed profile revisions together. SynckHub remains
on its separately pinned 0.1 revision until its own migration work is approved.
