# Portable repositories

A portable repository is a directory containing `.synck/` canonical history and
objects. It need not have a working tree. Its exact layout, schema, encodings and
verification procedure are defined by the [format specification](spec/format.md).

## Read and verify

```sh
synck repository verify ./copy
synck repository log ./copy
synck item list --path ./copy
synck item list --records --path ./copy
synck item show ITEM_ID --path ./copy
synck item export FILE_ID ./report.pdf --path ./copy
```

Opening validates identity and schema without repairing history. Explicit
verification checks event/commit hashes, structure, publication checkpoints,
metadata and objects. Missing payloads are reported separately from corruption.
A valid thin repository can support structural history while some typed reads
or exports are unavailable. Existing malformed caches or databases are not
silently reset by opening them.

The typed reader supports FILE/FOLDER, CHAT and EMAIL. FILE exports return exact
original application bytes. Historical reads use a
published boundary; refs and item histories retain their separate sequences.

## Complete copies

```sh
synck archive ./archive
synck repository import ./archive ./my-copy
synck repository checkout ./my-copy --author "Alex"
```

Archive/import verifies the source, obtains a consistent SQLite backup and
transfers all historically referenced metadata/content. Missing payloads
prevent claiming a complete archive. Copies preserve
repository ID, history generation, commit/event/checkpoint identities and exact
object frames. Cache databases and working files are not copied. Destination
publication uses a private staging directory and refuses to overwrite an
existing destination. Interrupted work does not publish a partial archive.

WHOLE_FILE stores one object per file; CHUNKED stores deduplicated chunks
in SQLite. `archive --storage-mode WHOLE_FILE|CHUNKED` can convert the physical
representation without changing any object ID. Never copy a live SQLite main
file without its WAL snapshot; the CLI uses SQLite backup.

### Copies from a sync server

```sh
SYNCK_TOKEN=... synck repository import https://sync.example/v2/repositories/0190... ./copy
synck repository verify ./copy
```

`repository import` also takes a [sync server](sync-server.md) repository URL,
with a read token in `SYNCK_TOKEN`, and makes the same follower copy over
protocol 2. The copy keeps the server's history exactly as published: its
generation, positions, acceptance times, checkpoints and envelopes, so its head
checkpoint is the server's. It verifies every history page and replays every
commit, downloads every object history names and runs `repository verify`
before it publishes the directory. An encrypted repository is copied the same
way: names are tags, metadata and content stay ciphertext, and frames verify
without a key. Keys are the product profile's and are never copied, so this
CLI can verify the copy and list its history but not read its content. A server
that cannot serve an object history names fails the copy. Federation domain
cursors, which protocol 2 does not carry, are not copied.

## Checkout and authority

A checkout materializes FILE/FOLDER state so ordinary applications can edit it.
`repository checkout --include PATH` can select subtrees. Selection changes
which paths are on disk, not which history is retained. Unselected paths must
not be interpreted as deletions. Checkout transitions and their intents are
durable; scan and projection caches are rebuildable.

Imported copies are FOLLOWERs. An independent editable continuation has no
synchronization/rejoin guarantee. `repository activate` is an explicit local
source-retirement acknowledgement, not a distributed fencing mechanism.
It cannot prove another writer has stopped. Follow the command's required
acknowledgements and preserve the source until its replacement is verified.

Application integrations can use direct `repository create-file`, `save-file`,
`duplicate`, `import-email`, `create-chat` and `append-chat`; see [integration](integration.md). Recovery of an
accepted but incompletely projected write uses `synck repository finalize PATH`.
The transaction that accepts a commit also publishes it, with its `repo_seq` and
checkpoint, so a repository never holds accepted, unpublished commits. This
explicit recovery catches up the derived indexes and the checkout after an
accepted write; it preserves commit IDs and working-file edits. If that
post-acceptance work fails, the CLI reports `ACCEPTED_NEEDS_RECOVERY` with the
accepted commit ID and `repository finalize` guidance; the nonzero command exit
does not mean the commit was rejected. Repeating such a write under a new
operation ID risks a duplicate accepted action. See the
[publication contract](spec/format.md).

## Retention boundary

A verified complete archive is suitable input to a separate archival service.
It is not itself immutable storage. Continuous regulated capture needs durable
progress, pins until verified acknowledgement, independent permissions, holds,
retrieval and long-term reader availability. It must cover accepted unpublished
records and survive history replacement. Periodic exports alone cannot preserve
records deleted between exports.

A future administrative rewrite may replace history under the same repository ID. It
must allocate a new generation, validate structural dependencies and bases, and
publish the replacement atomically. Clients must reconcile explicitly and revalidate
pending proposals; a reclone is not the required mechanism. Original exports keep
their generation, IDs, salts and bytes.

Archive credentials must prevent live-repository administrators from changing
retained copies. Retention periods, legal holds, classification, immutable storage,
redundancy and preservation of encryption keys belong to that separate product,
which must retain compatible readers or verified migrations despite this project's
pre-1.0 single-format policy. Synck 0.2 implements none of this: no administrative
rewrite command, capture pins, acknowledgement service or WORM compliance. Its
complete export is the foundation, not a substitute for those controls.
