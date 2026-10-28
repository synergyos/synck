# Synck 0.2 repository format

This is the authoritative format-7 specification. A 0.2 reader accepts only
this repository format, commit format 6 and history bundle format 5. Schema
versions are independent of release numbers. There are no older-format readers
or migrations. The normative SQL and JSON vectors linked here are part of this
specification; no Rust source is needed to interpret a repository.

## Container and identifiers

A repository is a directory containing `.synck/` with:

- `config.json`: UTF-8 JSON configuration.
- `repository.sqlite3`: canonical history, schema [history.sql](history.sql).
- `objects/`: required directory; in WHOLE_FILE mode it holds framed objects.
- `object-store.sqlite3`: required only in CHUNKED mode, schema
  [object-store.sql](object-store.sql).
- `lock`: local coordination file. Locking is an implementation mechanism;
  readers must obtain a consistent snapshot, including SQLite WAL contents.

`repository.local.sqlite3`, `repository.state.sqlite3`, their SQLite sidecars,
and scan caches are disposable. Optional `agent-locks.sqlite3` holds local edit
reservations; it is neither canonical history nor a disposable cache. Archives
and imports omit it, and readers need not interpret it. See
[local agents](../local-repository.md#local-agents-sharing-a-checkout).
Checkout configuration and recovery intents in
`tmp/` are not disposable. Never copy only a live SQLite main file: use SQLite's
backup API or a quiescent, checkpointed snapshot. Treat repository paths as
untrusted; do not follow object or database symlinks outside the repository.

Configuration is an object with required `format_version: 7`, `repo_id`,
`authority` (`LOCAL`, `FOLLOWER`, or `INDEPENDENT_LOCAL`), optional
`storage_mode` (`WHOLE_FILE`, the default, or `CHUNKED`), optional `author`
(string), and optional `checkout`. Unknown fields are rejected. `checkout` has
`paths` (array of selected relative paths; empty means all), `hidden_files`
(`ignore`, default, or `track`) and optional `pending`. A pending checkout holds
`previous_paths` (null or an array) while transitioning its materialized paths.
Authority is local operational configuration, not a cryptographic credential.
An imported copy is a FOLLOWER. Independent activation is not a synchronization
or distributed fencing protocol.

Repository IDs, item IDs, physical `commit_pk` IDs, history generations and FILE
version IDs are UUIDv7, stored in lowercase hyphenated form. `import_job_id` is
UUIDv4 instead. Commit IDs, object IDs, event digests and checkpoints are
`sha256:` plus 64 lowercase hexadecimal digits in JSON, and raw 32-byte digests
in SQLite BLOB columns. They are distinct domains despite sharing spelling.
Authors and committers are application identity strings, not UUIDs.

Sequences are positive signed 64-bit integers in SQLite; ref bases also allow
zero. Event indexes and event counts fit positive signed 32-bit storage/API
integers (indexes start at zero). There is no smaller policy cap. SQLite time
strings are RFC 3339 UTC; canonical writer timestamps use nine fractional digits
and `Z`. Readers decode acceptance time to seconds and nanoseconds for checkpoint
hashing. The spelling used for the commit's hashed `occurred_at` is canonical.

## Canonical JSON

Hashed JSON is RFC 8785 JCS, encoded as UTF-8: no whitespace, object keys sorted
by UTF-16 code units, ECMAScript finite-number serialization, minimal prescribed
string escapes, and no duplicate keys or invalid Unicode. Strings are not
normalized by JCS. Name normalization is a separate admission rule. Preserve
integer values exactly: if conversion to the RFC 8785 number spelling changes
the mathematical integer, reject it and require a string. Do not assume every
integer above 2^53 is invalid; some are exactly representable.

For stored events, parse the strict shape below, serialize that shape canonically,
and require equality with the stored bytes. Unknown fields, explicit nulls where
an optional field must be omitted, noncanonical IDs and alternative spellings
cannot be silently removed or normalized before verifying their hash. Hashes
are SHA-256 without any extra delimiter or newline except where explicitly framed.

## Event envelopes

Every accepted item envelope is a JSON object with required `item_id` and
`event_type`. These are the only optional fields:

| Field | Meaning |
| --- | --- |
| `create_item` | `{ "item_type": string, "name": string, "parent_folder_id"?: UUIDv7 }`; omitted parent means root |
| `base_seq` | The item's positive head sequence before this commit; absent for an item created by this commit |
| `metadata_id` | ID of the event's erasable metadata object |
| `set_content` | Complete role map described below |
| `set_parent` | Folder UUIDv7, or explicit null to move to root; absence leaves the parent unchanged |
| `set_name` | New portable name |
| `set_in_trash` | Boolean; true enters TRASH, false restores LIVE |
| `set_tombstoned` | Boolean; true permanently enters TOMBSTONED |
| `set_is_federated` | Boolean mode transition |
| `set_federation_stream_id` | Nonempty federation stream string |

All other absent optional values are omitted, not null. Nested `create_item`
also rejects unknown fields. Inline metadata, inline content, parent commit IDs
and repository sequences do not occur in envelopes. Metadata belongs to the
event; families define how successive metadata payloads affect presentation.
Missing metadata cannot block structural replay of names, parents or lifecycle.

An attached-ref envelope has required `owner_item_id`, `event_type`,
`base_ref_seq`, and optional `metadata_id` and `set_content`. No other fields
are valid. Refs cannot change an owner's structure. Their ledger starts at one
and is independent of item sequence; an initial ref base is zero.

### Structural replay

Apply commits in published `repo_seq` order and their events in index order.
An item is created exactly once at item sequence one. Every next item event
increments its sequence by one. All events targeting an existing item in one
commit carry its head at the start of that commit as `base_seq`; all events for
an item created within that commit omit the base. Ref events similarly share
the pre-commit ref head and increment it consecutively. Refs require an existing
owner, which may have been created earlier in that commit.

Creation fixes item identity and type, starts lifecycle LIVE and sequencing mode
LOCAL unless federated bootstrap applies. Core event types use the item's type
or `ITEM` as their `::`-separated family. Apply structural fields to the evolving
in-commit state. Parents must exist in this repository and be FOLDER items;
root is null, and parent cycles are forbidden. Live siblings must have distinct
name keys. Tombstones are terminal; tombstoned items receive no more item events.
Trash is reversible and retains its history. Names and item types do not depend
on metadata presence. Creation time/author come from the creation commit.

FILE/FOLDER checked mutations additionally require parent-closed LIVE state at
the final commit boundary, including atomic descendant trash, restoration and
tombstoning. Temporary name conflicts during an atomic swap must be resolved
in the final state. Other item families retain Core's parent-existence rule.

LOCAL items accept append; FEDERATED items accept ingest. A transition to
federated requires `set_is_federated: true` and a nonempty stream; a return to
local requires false and no stream field. Ingest bootstrap materializes the
true/stream pair into creation, and permits one structurally pure same-stream
replay event in that bootstrap commit. Outside bootstrap, switching to federated
is a local append and switching to local is a federated ingest. At most one mode
change per item per commit is permitted. This is the engine's existing ingest
contract; it does not specify the future cross-machine sync protocol.

### FILE byte reconstruction

A FILE has a `FILE::CREATE` event and content revisions in
`FILE::SET_CONTENT`. Its content snapshot has exactly `bytes: object_id`.
Available revision metadata has `content.size`, the nonnegative application-byte
length, not the framed size. The local writer creates the first content revision
at item sequence two. Structural moves/renames do not create new byte content.

To export a tree at a published boundary, replay all complete commits through
that boundary, retain LIVE items with LIVE ancestry, and derive paths from the
parent IDs and names. For each FILE, select its latest `set_content` through
that boundary, retrieve `bytes`, verify the full object's ID and RAW framing,
and output application bytes after the 49-byte header. Verify `content.size`
when the corresponding metadata is present. Missing objects are unavailable,
never zero-length substitutes. Missing size metadata does not make verified
application bytes undecodable, although typed operations may require metadata.
For a single historical revision, stop the FILE ledger at its requested item
sequence. Labels resolve that same revision as specified below.

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
does not conflict with another item. NFC normalization uses Unicode 18.0.0 tables (unicode-normalization 0.1.26). The name vectors are language-neutral
JSON in `vectors/names.json`.

A proposed portable spelling normalizes NFC, replaces each forbidden character
with `_`, strips trailing ASCII spaces/dots, substitutes `_` for an empty result,
prefixes device names with `_`, and truncates on a UTF-8 boundary to 255 bytes.
Strip any space or dot exposed by truncation. For sibling collisions append
` (2)`, ` (3)`, etc., reserving suffix bytes before truncation. Filesystem
renaming requires explicit `--fix-names`; validation alone never renames files.
Non-UTF-8 names are errors, never guessed encodings.

Whole-path length is not a repository constraint: it depends on the checkout's
location. The Windows MSVC CLI embeds a `longPathAware` manifest and uses extended
absolute paths for filesystem operations. Windows status and checkout commands
warn on stderr for each observed/selected path whose ordinary absolute spelling
exceeds 260 UTF-16 code units, excluding the `\\?\` prefix. These warnings do
not reject history or change names. Other applications can have shorter limits.

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

## Salted objects

Object IDs have exactly the spelling `sha256:` followed by 64 lowercase
hexadecimal digits; readers reject uppercase rather than normalize it. All
object roles use the same identifier type. An object is the concatenation of:

1. The 15 ASCII bytes `SYNCK_OBJECT_V1` followed by a zero byte.
2. One codec byte: 0 for `RAW`, 1 for `UTF8`. Other values are invalid.
3. A fresh 32-byte salt from the operating system's cryptographic random source.
4. The application bytes, extending to the end of the object.

The object ID is SHA-256 over all these bytes, including codec and salt.
The header is 49 bytes. `RAW` accepts arbitrary bytes; `UTF8` requires valid
UTF-8 and preserves exact bytes, including NUL, line endings and normalization.
Empty objects are valid for either codec. The object format has no size cap;
individual item profiles can bound their payloads. Metadata uses `UTF8` objects
whose application bytes are canonical RFC 8785 JSON.

The salt is stored only inside the object, not in event envelopes. Removing an
object prevents its ID alone from confirming a content guess; this says nothing
about copies in chunks, caches or archives. Storage chunk deduplication is below
this framing and does not change object identity. Language-neutral vectors are
in `vectors/objects.json`.

UTF8 bodies use this framing. FILE and EMAIL source writers use RAW framing. Working-file
change detection uses plain SHA-256 digests only in the disposable local cache;
these digests are never object IDs. Chunked capture isolates the salted header
from file-data chunks so repeated content can still deduplicate. Exports strip
the frame; repository copies preserve the exact frame and its ID.

### Metadata payloads

A metadata payload is a `UTF8` object containing exactly one canonical RFC 8785
JSON object, at most 1,048,576 application bytes. Scalars and arrays at the root,
noncanonical JSON, duplicate keys and integers without a lossless canonical
representation are rejected. A writer uses a fresh salt even for identical JSON.
`vectors/metadata.json` fixes the JSON, stored bytes and digest independently.

SQLite stores available payloads in
`vcs_metadata_objects(repo_id TEXT, object_id BLOB, payload BLOB)`, with the first
two columns forming the primary key. `object_id` is the raw 32-byte digest;
`payload` includes the entire 49-byte header and JSON bytes. No foreign key makes
an event depend on object presence. Missing payloads return “not present”; present
payloads with invalid framing, codec, JSON or digest are corrupt. Verification
does not repair them. Re-inserting the same ID is idempotent only for exact bytes.

Bundle format 5 permits `metadata` records immediately after its header and
before history. Each has exactly `type` (`metadata`), `object_id` (canonical
`sha256:` spelling), and `payload_hex` (lowercase hexadecimal of the whole stored
object). Duplicate IDs and metadata records after history are rejected. Import
validates the payload and installs it in the same transaction as history; an
invalid record rolls back both. Complete SQLite copies preserve these rows.
Item and ref envelopes use the optional `metadata_id` field, containing the
canonical object ID string. Inline `set_metadata` is rejected on the wire.
Acceptance installs supplied metadata bytes in the same transaction as the
event. Readers may resolve the ID to verified bytes without changing the
envelope or its digest. Missing bytes leave the ID intact and are explicitly
reported as absent; typed interpretation requiring those fields cannot proceed.
`vectors/events.json` defines canonical envelopes, event digests, tagged event
sequences and commit hashes, including envelopes with metadata references.

## Content roles and closure

`set_content`, when present, is a complete map from role names to canonical
object ID strings. It contains at most 32 distinct roles. Each role matches
`[a-z][a-z0-9_]{0,31}`; duplicate names, invalid IDs, null values and nested values
are rejected. An omitted field leaves the previous content snapshot unchanged.
An empty map clears it. Inline text and JSON are not event content.

FILE content uses exactly `bytes`; CHAT message content uses `body` with a UTF8
object. EMAIL uses `source` and optionally `provider`; EMAIL attachments use
`source`. Their metadata contains descriptors, never object references. Unknown
families use the same structural map without requiring a typed reader.

Closure scans all retained item and attached-ref events, including deleted
items and superseded revisions. It collects each `metadata_id` and every value
in `set_content`. Neither current heads nor fields inside metadata establish
closure. Resolving content into a read view cannot change the serialized event.

Verification checks canonical history independently of the disposable checkout
cache. It reports `objects`, `metadata`, `missing_objects`, and
`missing_metadata`; missing references are valid. Present invalid payloads are
corruption. A complete archive requires both missing-reference lists to be empty
and copies exact stored bytes. Reader operations needing absent payloads fail
explicitly rather than substituting empty content.

### EMAIL originals and rendering

Local EML/MSG imports preserve the original in `source`. The metadata payload
contains `mailcal` header fields, parser version, source format/name/media type/
size, attachment count and import time. MIME attachments are subitems with their
own `source` role. Generated Markdown, HTML and RFC 822 are not persisted. The
local `render_email` reader recomputes presentation with the current parser
without changing history. Content-ID header strings in attachment metadata are
MIME identifiers, not Synck object references.

### EMAIL provider profile and names

The metadata payload's `mailcal` object has version `v: 2`; local imports use
`origin.kind: "local_import"`, `parser_version`, `parser_limits`, `source`
(`format`, `filename`, `mime_type`, `size_bytes`), `provider`, `account_id`,
`provider_message_id`, `provider_thread_id`, `internet_message_id`, `subject`,
`from`, `sender`, `to`, `cc`, `bcc`, `reply_to`, `sent_at`, `received_at`,
`has_attachments`, `attachment_count`, and `imported_at`. Provider/account fields
are null for local files. Header address entries have `name` and `email` (each string or null);
missing scalar headers/times are null and missing recipient lists are empty.
Metadata describes the content; it must not embed Synck object references.

A provider archive uses `source` for the original raw MIME and `provider` for
its original provider-native JSON, including provider snippet/body preview,
labels, categories and importance. These are received originals, not Synck's
renderings. Attachments use `EMAIL::ATTACHMENT::CREATE` events with `source`,
metadata under `meta` (filename, MIME type, size, inline flag,
Content-ID and provider attachment ID), and the EMAIL owner as `item_id`.
They are family subrecords within that item's ledger, not folder children.

The provider writer's default item name is `YYYY-MM-DD HHmm — Subject`, using
incoming received time or outgoing send-acceptance time in the importing
machine's local timezone. A missing subject becomes `(no subject)`. Apply the
portable-name repair and UTF-8-safe 255-byte truncation to the name alone;
`Re: Budget` becomes `Re_ Budget`. Preserve the actual subject in metadata.
An explicitly supplied name wins and is never recomputed. Append ` (2)`,
` (3)`, etc. only on a folder collision, reserving suffix bytes. Local EML/MSG
imports instead use the filename without its email extension. Names remain in
history when metadata is erased. Generated provider-import commit messages use
counts and source account, never message subjects or addresses.

The local writer implements file import. No provider acquisition client ships
in 0.2; this profile defines its stored-data boundary without adding an unused
provider writer or administrative policy layer.

### FILE revision labels

Named versions are `FILE_VERSION::*` events in a FILE's independent ref ledger.
There are no version-set items, synthetic item identities or item-ledger version
subitems. A ref's `set_content` must be absent. Its salted metadata contains
`file_item_id` (equal to its envelope owner) and a UUIDv7 `version_id`.

`CREATE` adds `operation_id` (a nonempty string unique among version creations),
`source_item_seq` (the positive sequence of a FILE content revision), `kind`
(`MAJOR` or `MINOR`), `label_source` (`INTERNAL` or `EXTERNAL`), signed integer
`order_key`, and optional string/null `description`. Optional `source_commit_id`
and `source_commit_event_idx` must agree with the referenced revision. The
revision must precede the ref in accepted order, including within one commit.
Only one live label may reference any given revision of a FILE.

`SET_DESCRIPTION`, `SET_KIND`, `SET_LABEL_SOURCE` and `SET_ORDER` change their
named field. `SET_REVISION` requires a new `source_item_seq` and may include
matching commit coordinates and a description. `TOMBSTONE` permanently removes
the label from the live set. Mutations require an existing live version.
All events in one commit use the ref head before that commit as `base_ref_seq`.
Content ID and size are read from the referenced FILE event, never stored again
in the label. Erasing that content therefore affects the revision and its label
without changing either ledger. Erasing label metadata makes the label's
presentation unavailable; structural history remains verifiable.

The local writer labels each new FILE's initial `SET_CONTENT` (item sequence 2)
in the same commit. Its first ref is sequence 1, `MINOR`, `INTERNAL`, with order
key 1024. Live labels sort by `(order_key, creation_ref_seq, version_id)`; starting
at 0.0, MAJOR increments the major and resets the minor, MINOR increments the
minor. UUIDs and sequences remain the identities; computed numbers are display.

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

Core SQLite uses application ID `0x53594E4B` and schema version 7. The adapter
creates this schema directly and refuses prior schema versions without modifying
their version stamps or definitions. There are no core or local schema migrations.
The current local projection schema is 13, in `repository.local.sqlite3`.
The state tree lives in `repository.state.sqlite3`. Neither database is copied
into a portable repository. These cache schema versions are implementation details.

Schema compatibility compares owned table names, STRICT typing, named columns
and their declared types, nullability, defaults, primary-key positions, unique
keys (including collations, sort direction, expressions and partial predicates),
and foreign-key columns and actions. Physical column order, index names, DDL
whitespace/comments/identifier quoting, and extra nonunique performance indexes
are not format identity. `INT` and `INTEGER` are equivalent declared types only when their SQLite key
and nullability semantics match; `INTEGER PRIMARY KEY` rowid behavior matters.
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

`repository.sqlite3` contains no `local_*` tables. FILE-head projections,
plaintext digests, filesystem observations and imported-email source observations live
in `repository.local.sqlite3`. Local transactions may read canonical history
through a read-only SQLite attachment; they cannot write it. Acceptance and local
projection updates are separate transactions. Existing recovery replays accepted
history after an interrupted projection update. Missing cache databases are
created and replayed under the checkout lock; existing malformed databases are
refused rather than silently reset. Local import-source observations derive
from EMAIL creation metadata, its original parent identity and source role;
only matching filename and exact bytes are recognized as already imported.
Changed working files are never promoted into history during cache rebuilding.
The hidden-files preference is `checkout.hidden_files` (`ignore` or `track`,
default `ignore`) in config.json. Durable operation intents and checkout
configuration are not disposable caches.

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

## Atomic subtree operations

Lifecycle operations have no aggregate descendant-count cap. They carry the exact
prepared descendant set and protect its item heads under the ordinary acceptance
locks. The transaction rereads at most the expected count plus one for each
root; an extra descendant or changed membership rejects the proposal as stale.
This query bound detects races; it does not impose a format or commit-size limit.

## Portable-name diagnostics

Filesystem discovery applies ignore rules before portable-name validation. It
collects all observed name violations into one diagnostic, with original and
proposed paths quoted so control characters remain visible. Valid sibling names
are reserved before repairing invalid spellings; equal valid case-fold keys keep
the lexicographically first spelling. Remaining conflicts receive ` (2)`,
` (3)`, and so on, with UTF-8 truncation before the suffix when needed.
Suggestions also avoid existing ignored filesystem entries. A renamed ancestor
is reflected in every reported descendant destination. Diagnostics do not rename
files or publish a repository when initialization fails.

## Explicit filesystem name repair

`init --fix-names` initializes repository metadata and then repairs non-portable
names. As with ordinary initialization, existing user files remain uncommitted.
`commit --fix-names` repairs names throughout the observed checkout, remaps any
selected paths, and records the selected changes in one ordinary commit. Both
commands report every rename. Ignored entries are never renamed or overwritten.
The JSON commit result includes `renamed` when this flag is used.

Repairs are local filesystem operations, not history rewrites. A local journal
records file and parent identities before mutation. Descendants move before
ancestors, through temporary sibling names, using no-replace renames. Each
completed rename is directory-synced before its progress record is synced.
Interrupted repairs resume on the next locked checkout open, checking identities
and refusing an occupied destination. Existing history and content IDs remain
unchanged until the subsequent ordinary commit. Initialization publishes its
metadata before repair so an interruption leaves recoverable state.

Importing working files uses these same initialization/commit commands. A
complete repository import preserves canonical names and hashes; it does not
repair or rewrite imported history.

## Canonical SQLite relations

[history.sql](history.sql) defines the complete column, key, type, default and
foreign-key contract. All tables are STRICT. Each repository container contains
one `repo_id`; every canonical row belongs to it. No `local_*` or state-tree
tables belong in this database. Additional nonunique indexes are allowed, but
additional owned tables, unique constraints, views or triggers are not.

- `vcs_commits` contains accepted commit headers. `commit_pk` is only a physical
  join key. A durable accepted commit has non-null `commit_id`, `events_digest`
  and `event_count`; the nullable fields allow transaction-local construction,
  not incomplete durable commits. `repo_seq` remains null until publication.
- `vcs_events` and `vcs_ref_events` contain canonical envelope bytes in `payload`,
  with their raw SHA-256 in `event_payload_digest`. Row identity, event type and
  accepted sequence must agree with the envelope and its replay. `created_at`
  equals the owning commit's acceptance time. `parent_folder_id` records the
  parent after the event. `parent_folder_id_before` records the old parent only
  when the envelope contains `set_parent`; otherwise it is null. For creation
  it is null. A move from root also has a null before-parent. This column is a
  move notification field, not an unconditional snapshot of the prior parent.
- `vcs_items` is the canonical accepted structural head, including accepted but
  unpublished changes. Replay must reproduce its name, name key, type, parent,
  lifecycle, item/ref heads, creation attribution and federation fields.
- `vcs_repo_activity_heads.next_repo_seq` is the next publication position and
  must exceed all published positions. `updated_at` is operational allocation
  time. `vcs_checkpoints` has exactly one matching row per published commit and
  none for unpublished commits. `vcs_history_generations` has one generation.
- `vcs_metadata_objects` stores available framed metadata objects. Absence is
  allowed; every present object must validate even if currently unreferenced.
- `vcs_federation_domains` records the accepted federation-domain cursor;
  `last_domain_seq` is nonnegative. Local-only repositories normally have no
  rows. Domain cursor metadata is operational provenance, not commit ancestry.

A published snapshot has no unsequenced commits. The current complete-export
command requires that state and refuses a snapshot containing unpublished
acceptances. It must not silently omit them. Live engines can validly have
unsequenced accepted commits: the sequencer must finish publication before a
complete published snapshot is exported. It must preserve per-item, ref and
structural dependencies; allocating a sequence before acceptance commits would
allow scans to skip late transactions and is forbidden.

### WHOLE_FILE storage

Every available content object is `.synck/objects/<64-lowercase-hex-digest>`.
There is no `sha256:` prefix in a filename and no fan-out directory. The file
contains the complete frame. Validate the file as a regular file and hash its
entire contents against its name before trusting decoded application bytes.
Metadata stays in `vcs_metadata_objects` in either storage mode.

### CHUNKED storage

`object-store.sqlite3` uses application ID `0x534F424A`, user version 1 and
[object-store.sql](object-store.sql). `store_identity` contains exactly row
`singleton = 1` with the same `repo_id` as configuration. `objects/` still exists
but content is read from the object database.

`blobs.digest` is the object's raw 32-byte ID; the historical SQL name `blobs`
does not introduce another ID type. `size_bytes` is the **framed** size.
`complete = 0` is private incomplete capture; it is not a readable object.
For `complete = 1`, join `blob_chunks` to `chunks`, sort by `ordinal`, require
consecutive ordinals starting at zero, decode each chunk and concatenate them.
The result must have exactly `size_bytes` bytes and hash to `blobs.digest`,
then pass object-frame validation. A missing referenced blob is thin history;
a claimed complete blob with missing or invalid chunks is corruption.

A chunk's digest is SHA-256 of its decoded bytes, without an object frame.
`raw_size` is 1..262144. Codec `RAW` stores exactly that many bytes. Codec `ZSTD`
stores exactly one Zstandard frame: no trailing bytes or concatenated frames,
decoded output exactly `raw_size`, frame window at most 262144 bytes. A stored
compressed payload is nonempty and its size multiplied by 100 is less than
`raw_size * 95`. These chunk codecs are separate from the object's codec byte.

Chunk boundaries have no semantic meaning. Readers must not reproduce a writer's
chunking algorithm. The current writer uses FastCDC and optionally Office ZIP
member boundaries, separates the salt header from application data, and stores
small inputs directly when compression is not profitable. Copying or changing
storage modes must preserve the exact framed bytes and hence object IDs.
Re-encoding chunks is allowed; replacing salts is not.

## Bundle record details

A bundle is UTF-8 newline-delimited JSON. Outer records need not use JCS; embedded
`payload_json` strings must contain exact canonical envelope bytes. A nonempty
header comes first, followed by zero or more metadata records, then complete
commit/event groups, then item-head records, then federation-domain records.
Blank body lines are ignored. Unknown fields and record types are rejected.
No event may appear outside its commit group or after the item section.

All records contain `type`. Other fields are:

| Type | Fields |
| --- | --- |
| `synck_vcs_bundle_header` | `bundle_format_version: 5`, `repo_id`, `history_generation`, `exported_at`, `schema: {subsystem, schema_id}`, `commit_format_version: 6` |
| `metadata` | `object_id`, `payload_hex` |
| `commit` | `commit_id`, `checkpoint`, `repo_seq`, `operation_id`, `import_job_id`, `format_version: 6`, `author_id`, `committer_id`, `message`, `occurred_at`, `created_at`, `event_count`, `events_digest` |
| `event` | `commit_id`, `repo_seq`, `commit_event_idx`, `item_id`, `item_seq`, `event_type`, `payload_json`, `event_payload_digest`, `created_at`, `parent_folder_id`, `parent_folder_id_before` |
| `ref_event` | `commit_id`, `repo_seq`, `commit_event_idx`, `owner_item_id`, `ref_seq`, `event_type`, `payload_json`, `event_payload_digest`, `created_at` |
| `item` | `item_id`, `item_type`, `parent_folder_id`, `name`, `name_key`, `head_seq`, `ref_head_seq`, `lifecycle_state`, `created_at`, `created_by`, `sequencing_mode`, `federation_stream_id` |
| `federation_domain` | `domain_id`, `last_domain_seq`, `created_at`, `updated_at` |

Optional commit message, author time, operation/import IDs, parents and stream
IDs are JSON null when absent. `ref_head_seq` may be omitted when zero.
Digest fields use prefixed hexadecimal spelling, except `payload_hex`, which
is unprefixed lower hex of the complete metadata frame. `schema` is source
provenance, not a schema migration instruction: the SQLite writer emits
`subsystem: "synck_vcs_sqlite", schema_id: "core-v7"`. The generic bundle
boundary also recognizes `synck_vcs_pg` provenance; that is not a 0.1 reader.

Commits are ordered by ascending positive `repo_seq`; gaps are allowed. Exactly
`event_count` event/ref records immediately follow each commit, with contiguous
indexes from zero, item records first and ref records after. Their repeated
commit ID, position and acceptance time must match. Item and domain sections
must not duplicate identities; item-head fields must match structural replay.
A complete archive must additionally supply every referenced object and the
container/configuration needed to interpret it. An NDJSON bundle alone does not.

## Independent verification procedure

1. Check format/configuration, database identities and schema structure; select
   a consistent database snapshot. Distinguish accepted unpublished work from
   a complete published snapshot before claiming a terminal checkpoint.
2. Parse every stored event/ref envelope with the strict shape and JCS rules;
   require exact canonical bytes and recompute its SHA-256. Match its row fields.
3. For each commit, check contiguous combined indexes, item-before-ref ordering,
   positive exact event count, tagged aggregate digest and canonical header hash.
   Reject duplicate commit IDs, operation IDs or publication positions.
4. Replay structure and item/ref bases, including final namespace uniqueness,
   parent acyclicity and terminal tombstones. Compare canonical item heads.
5. Recompute every checkpoint in order from the zero predecessor and acceptance
   timestamps. Compare its commit, sequence and digest, the generation and any
   externally trusted checkpoint. A locally self-consistent replacement is not
   proof of continuity with a previously trusted generation.
6. Collect all historical metadata IDs and role values. Validate every present
   object; report absent IDs separately. Check chunk-store integrity independently
   of object availability. Do not consult private caches to establish validity.
7. Decode requested FILE revisions from verified RAW frames. Publish extracted
   files only after validation; a streaming decoder can discover corruption late.

The language-neutral vectors under `vectors/` cover names, frames, metadata,
envelopes, commits, checkpoints and FILE labels. `vectors/file-folder/` contains
profile intent and conformance examples; its `events.json` is an **application
view**, with expanded `set_metadata` for comparing independently salted writes,
not an accepted-envelope encoding. The top-level `events.json` fixes accepted
bytes and hashes. These canonicalization cases may use synthetic event types
without satisfying structural family admission; they are not whole valid histories.
Neither example IDs nor synthetic digests imply object presence.

## Administrative replacement and retention boundary

Normal writes append history. Undo appends a compensating change. A future
explicit administrative rewrite may replace history under the same repository
ID, but must allocate a new generation, validate structural dependencies and
bases, recalculate changed event/commit hashes and checkpoints, and publish the
replacement atomically. Verification never makes corrupt data acceptable by
recalculating its hashes. Old cursors must fail continuity checks; clients must
reconcile the replacement explicitly and revalidate pending proposals. A reclone
is not the format's required reconciliation mechanism.

Original exports retain their generation, IDs, salts and bytes. Live-history
replacement cannot rewrite them. A checkpoint chain authenticates continuity
only relative to a trusted checkpoint; it neither imposes immutability nor proves
who accepted a record. Access controls, signatures and archival custody are
separate responsibilities.

A complete export is one consistent history cut, with every referenced metadata
and content object, decoding information and exact salts. Missing objects are
valid in thin repositories but prohibit a claim of complete preservation.
A separate regulated archive integration must durably capture accepted records,
including work awaiting sequencing, with resumable progress and verified
preservation acknowledgements. Required source history and objects remain pinned
until that acknowledgement. Destructive maintenance must not race capture or
remove the only required copy. A best-effort callback is insufficient.

The integration must preserve capture identity/progress across generation
replacement, and archive credentials must prevent live-repository administrators
from changing retained copies. Retention periods, legal holds, classification,
immutable storage, redundancy, retrieval and preservation of encryption keys
belong to that separate archival product. It must retain compatible readers or
verified migrations despite this core's pre-1.0 single-format-reader policy.
Periodic exports alone cannot preserve records deleted between exports; no
universal nightly cutoff or retention duration is assumed.

Synck 0.2 specifies these integration requirements but does not implement an
administrative rewrite command, capture pins, an archive acknowledgement service
or regulatory/WORM compliance. Its complete-export command is the foundation,
not a substitute for those controls.
