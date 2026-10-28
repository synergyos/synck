# Portable repositories

A portable repository is a directory containing `.synck/` canonical history and
objects. It need not have a working tree. Its exact layout, schema, encodings and
verification procedure are defined by the [format specification](spec/format.md).

## Read and verify

```sh
synck repository verify ./copy
synck repository log ./copy
synck item list --path ./copy
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
original application bytes, without their salted frame. Historical reads use a
published boundary; refs and item histories retain their separate sequences.

## Complete copies

```sh
synck archive ./archive
synck repository import ./archive ./my-copy
synck repository checkout ./my-copy --author "Alex"
```

Archive/import verifies the source, obtains a consistent SQLite backup and
transfers all historically referenced metadata/content. Missing payloads or
unpublished acceptance prevent claiming a complete archive. Copies preserve
repository ID, history generation, commit/event/checkpoint identities and exact
object frames. Cache databases and working files are not copied. Destination
publication uses a private staging directory and refuses to overwrite an
existing destination. Interrupted work does not publish a partial archive.

WHOLE_FILE stores one framed object per file; CHUNKED stores deduplicated chunks
in SQLite. `archive --storage-mode WHOLE_FILE|CHUNKED` can convert the physical
representation without changing any object ID. Never copy a live SQLite main
file without its WAL snapshot; the CLI uses SQLite backup.

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
`create-chat` and `append-chat`; see [integration](integration.md). Recovery of an
accepted but incompletely projected write uses `synck repository finalize PATH`.
This explicit recovery verifies published checkpoints, repairs a missing or
incorrect publication counter from the last published position, then publishes
pending accepted commits and catches up projections. It preserves commit IDs
and working-file edits; a corrupt published chain is refused. Acceptance never
reads or locks sequencer state and never allocates `repo_seq` in its transaction.
It succeeds independently, leaving accepted commits with `repo_seq = NULL` until
the asynchronous sequencer publishes them. A counter mismatch stops sequencing,
not acceptance. If the CLI's post-acceptance publication fails, it reports
`ACCEPTED_NEEDS_RECOVERY` with the accepted commit ID and `repository finalize`
guidance; the nonzero command exit does not mean the commit was rejected.
Repeating a pending write under a new operation ID risks a duplicate accepted
action. Neither acceptance nor ordinary publication silently repairs the counter.

Each implementation runs at most one sequencer transaction per repository,
locking its activity head until the consecutive batch, checkpoints and updated
counter commit together. A rollback leaves no gap. Row-locking backends may use
`SKIP LOCKED` to select unsequenced commits, while retaining that repository-wide
sequencer lock. See the [publication contract](spec/format.md).

## Retention boundary

A verified complete archive is suitable input to a separate archival service.
It is not itself immutable storage. Continuous regulated capture needs durable
progress, pins until verified acknowledgement, independent permissions, holds,
retrieval and long-term reader availability. It must cover accepted unpublished
records and survive history replacement. The [retention contract](spec/format.md#administrative-replacement-and-retention-boundary)
states those requirements without claiming that 0.2 implements them.
