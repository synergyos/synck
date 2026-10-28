# Synck 0.2 repository format

Status: implementation in progress. Only the sections below are specified;
this document is not yet the complete release specification.

## Portable names

An item name is nonempty NFC-normalized UTF-8, at most 255 bytes. It is not
`.` or `..`, contains none of `< > : " / \ | ? *`, U+0000–U+001F or U+007F,
and does not end in ASCII space or dot. Unicode format characters, including
emoji joiners, are allowed.

Windows device stems are forbidden, case-insensitively, including when followed
by an extension: CON, PRN, AUX, NUL, CONIN$, CONOUT$, COM1–COM9 and LPT1–LPT9.
COM and LPT followed by superscript ¹, ² or ³ are also forbidden. Spaces at the
end of the stem before its first dot do not escape the restriction.

Sibling uniqueness uses Unicode 17.0.0 default full case folding of the NFC
name, using CaseFolding.txt entries C and F, never locale-sensitive/Turkic
folding. Preserve the display spelling. A case-only rename is allowed when it
does not conflict with another item. Normalization uses the pinned
unicode-normalization 0.1.26 tables. The name vectors are language-neutral
JSON in `vectors/names.json`.

A proposed portable spelling normalizes NFC, replaces each forbidden character
with `_`, strips trailing ASCII spaces/dots, substitutes `_` for an empty result,
prefixes device names with `_`, and truncates on a UTF-8 boundary to 255 bytes.
Strip any space or dot exposed by truncation. For sibling collisions append
` (2)`, ` (3)`, etc., reserving suffix bytes before truncation. Filesystem
renaming requires explicit `--fix-names`; validation alone never renames files.
Non-UTF-8 names are errors, never guessed encodings.

## Source attribution

The generated case-folding table uses the Unicode 17.0.0 data from
<https://www.unicode.org/Public/17.0.0/ucd/CaseFolding.txt>, under the
[Unicode License V3](unicode-license.txt).

## Publication checkpoints

Acceptance remains atomic per touched items and folder guards. Accepted commits
have no repository position until the separate sequencer publishes them.
Publication assigns the sequence and its checkpoint in the same transaction;
waiting for publication must not hold acceptance locks.

The checkpoint is SHA-256 over the concatenation, without separators, of:

1. The 19 ASCII bytes `synck-checkpoint-v1`.
2. The preceding checkpoint's raw 32 bytes (32 zero bytes at genesis).
3. The positive repository sequence as an unsigned 64-bit big-endian integer.
4. The commit ID's raw 32 digest bytes, without its `sha256:` prefix.
5. The acceptance timestamp's Unix seconds as a signed 64-bit big-endian integer.
6. Its nanosecond fraction as an unsigned 32-bit big-endian integer, below 10^9.

Sequences increase strictly; gaps are allowed. Leap-second timestamps are not
representable by this framing. Author time is distinct from acceptance time.
Vectors in `vectors/checkpoints.json` include a sequence gap and fractional time.
The chain binds publication order relative to a trusted checkpoint. It does not
by itself authenticate authority or detect the removal of an unknown suffix.
A checkpoint is not a regulatory retention seal.

SQLite publishes `vcs_checkpoints(repo_id, repo_seq, commit_id, checkpoint)`
with raw 32-byte digest values. `vcs_history_generations(repo_id, generation)`
holds a UUIDv7 identifying this history generation. Copies preserve that value.
A future administrative rewrite must replace the generation, validate its new
history and rebuild checkpoints before publishing the replacement atomically.
Client cursors must bind repository ID, generation, position and checkpoint;
a generation mismatch cannot be treated as an ordinary forward extension.

## Commit size

The format imposes no fixed per-commit event-count or aggregate byte cap.
Storage integers and available resources still bound representable commits.
Local commits are atomic, including moves and renames; no batch command or
batch recovery journal is part of 0.2. Pagination limits apply to individual
read requests, never to the validity of a stored commit.

## Commit identity

Commit format 6 is the only accepted encoding. The canonical header is RFC 8785
JSON with exactly these fields (including explicit nulls for absent values):
`object_type` (`vcs_commit`), `format_version` (6), `author_id`, `committer_id`,
`occurred_at`, `message`, `import_job_id`, `event_count`, and `events_digest`.
The optional origin timestamp is UTC RFC 3339 with exactly nine fractional digits
and `Z`; leap seconds are rejected. An import job is null or a canonical lowercase
UUIDv4. The positive event count is the number of item and attached-reference
events combined. The commit ID is SHA-256 of these canonical UTF-8 bytes,
represented as `sha256:` followed by 64 lowercase hexadecimal digits.

Each event digest is SHA-256 of its canonical envelope bytes. The aggregate
`events_digest` hashes the concatenation, in commit-event-index order, of one
family byte followed by the raw 32-byte event digest. The family byte is 0 for
an item event and 1 for an attached-reference event. Item events precede attached
reference events, and indexes are contiguous from zero across both families.
Every commit uses this framing, including commits with no references.

Repository ID, repository sequence, acceptance timestamp, operation ID, and the
physical commit-row surrogate do not participate in commit identity. There is
no parent-commit field. The publication checkpoint authenticates sequence and
acceptance time separately. Changing author, committer, origin timestamp,
message, import identity, or events changes the commit ID. The independently
calculated vectors in `vectors/commits.json` include item-only, reference-only,
and mixed commits, distinct committer identity and nanosecond timestamps.

## History bundle boundary

The NDJSON history bundle accepts version 5 only, with commit format 6. Its header
carries `history_generation`, a canonical UUIDv7, alongside `repo_id`, export time,
and source-schema provenance. Each commit record carries its `checkpoint` as a
`sha256:` digest; it has no parent-commit field. `operation_id` and `import_job_id`
are required keys whose values may be null. A non-null operation ID must be
nonblank. The importer preserves the generation and reconstructs and verifies
every checkpoint from the accepted commit IDs, positions and acceptance times.

Import is atomic into an empty repository. Invalid history or a checkpoint
mismatch rolls back both canonical history and generation. Imported copies keep
the same generation; an empty imported history still installs the generation.
A history bundle by itself is not a complete archival export: object bytes and
all data required for reconstruction must accompany it for that purpose.

## Typed reader scope

The 0.2 reader does not interpret hosted TASK, TASK_LIST, POST, TABLE, DATABASE,
CALENDAR, CALENDAR_EVENT,
TIME_SETTINGS, TIME_BOOK, TIME_ENTRY, CALL, CHAT_ORGANIZATION or FEDERATED_MOUNT
state. Requesting typed presentation for these families returns an explicit
unsupported-state error. Core hash verification remains independent of family
presentation. No calendar recurrence engine, hosted organization reducer or
call/time-tracking state machine is shipped for these absent local writers.

FILE resource-fork presentation and `FILE_RESOURCE_FORK::*` typed events are
not part of 0.2. Ordinary FILE bytes and explicitly supported content roles are
the portable content boundary.

## SQLite schema version policy

Core SQLite uses application ID `0x53594E4B` and schema version 6. The adapter
creates this schema directly and refuses prior schema versions without modifying
their version stamps or definitions. There are no core or local schema migrations.
The current local projection schema is 11, in `repository.local.sqlite3`.
The state tree lives in `repository.state.sqlite3`. Neither database is copied
into a portable repository. These version markers do not make this draft a release.

Schema compatibility compares owned table names, STRICT typing, named columns
and their declared types, nullability, defaults, primary-key positions, unique
keys (including collations, sort direction, expressions and partial predicates),
and foreign-key columns and actions. Physical column order, index names, DDL
whitespace/comments/identifier quoting, and extra nonunique performance indexes
are not format identity. `INT` and `INTEGER` are equivalent declared types.
Owned views and triggers are rejected. Uniqueness expressions and defaults are
compared as SQL tokens with string literals preserved exactly. Record validators
remain responsible for semantic bounds; CHECK-clause spelling is not compared.

## Historical-state cache

The checkpoint chain proves ordered history. The historical-state tree does not
belong to canonical history and does not contribute to any commit or checkpoint
hash. SQLite stores its `vcs_state_nodes`, `vcs_state_roots`, and
`vcs_state_projection_heads` tables in a separate `repository.state.sqlite3`
file. These are implementation details, not portable format requirements.
Verification neither requires nor trusts this cache. Historical listings create
and replay a missing cache under the repository lock; explicit index rebuilding
repairs damaged cache contents without modifying canonical history.

## Checkout database

`repository.sqlite3` contains no `local_*` tables. Local settings, FILE-head
projections, filesystem observations and imported-email source observations live
in `repository.local.sqlite3`. Local transactions may read canonical history
through a read-only SQLite attachment; they cannot write it. Acceptance and local
projection updates are separate transactions. Existing recovery replays accepted
history after an interrupted projection update.

A complete portable copy backs up the canonical database and transfers referenced
objects. It does not copy checkout observations, settings or the state-tree cache.
Enabling a checkout initializes that machine's local database. Unused workspace,
batch-import and obsolete storage-mode tables do not exist in the current schema.

## Ref sequence origin

An item's attached ref ledger starts at sequence 1. Every subsequent ref event
advances it by exactly one, independently of the item's ordinary event sequence.
All ref events for one owner in a commit carry the same pre-commit
`base_ref_seq`. `initial_ref_seq` is not a field and must be rejected as unknown.
There is no legacy-baseline migration API. Typed FILE readers do not load frozen
sidecar history or supplemental legacy-sidecar roots.
