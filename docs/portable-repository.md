# Complete local data repositories

This contract defines the format-6 local data repository: canonical history in
Core SQLite schema 5 and the exact retained content closure. It opens without a
checkout, source service, platform database or account. Format 6 is the only
supported repository format. This SQLite layout is a local
distribution format, not a restriction on Core storage-backend adapters.

## Local directory

```text
repository/
  .synck/
    config.json
    repository.sqlite3
    objects/<64 lowercase hexadecimal SHA-256 digits>  # WHOLE_FILE
    object-store.sqlite3                              # CHUNKED
    lock
```

This is the customer export and archive format: an ordinary ready-to-open local
repository. No separate transfer manifest, NDJSON history package, `bodies/`
directory or import conversion is required before reading it.

`config.json` records `format_version: 6`, the repository ID,
`storage_mode: "WHOLE_FILE" | "CHUNKED"` and
`authority: "FOLLOWER" | "LOCAL" | "INDEPENDENT_LOCAL"`. A new follower omits `author`; a local writer
requires it. Existing history retains its original authors, committers, timestamps
and IDs. An author string identifies attribution, not an authenticated identity.
Anyone possessing a complete copy can read all included retained history; local
storage does not evaluate product permissions.

Opening acquires the repository lock, validates the known format and Core schema,
and uses read-only database connections for reads. SQLite can create transient
WAL/SHM sidecars; read-only describes canonical data access, not a promise of zero
filesystem activity. It does not scan a working
tree, repair accepted history or run platform projections. Explicit verification
and recovery are separate operations. Public mutations require exclusive mutable
access to the open handle; the lock excludes other tool opens. An archive/read
cannot overlap a supported writer on the same handle. Direct external database
or object mutation bypasses this contract. Working-copy tools require an explicit
checkout; a bare format-6 repository remains a data-only reader.

## Two physical layouts, unchanged logical identities

`storage_mode` in `config.json` is authoritative for format 6, including an enabled
checkout. Old format-6 configurations without this field mean `WHOLE_FILE`;
opening them does not rewrite configuration. Unknown modes fail explicitly.
New `repository init` repositories default to `CHUNKED`;
`--storage-mode WHOLE_FILE` selects an exact-byte whole-file store. Export producers may continue
to produce `WHOLE_FILE` repositories.

In `WHOLE_FILE`, file/attachment bytes and salted body payloads share
`.synck/objects/`. Each physical filename is the SHA-256 of its exact stored
bytes, without a prefix, so ordinary `shasum -a 256` checks remain sufficient.
In `CHUNKED`, `object-store.sqlite3` is required and authoritative; `objects/`
is normally empty. The repository-bound SQLite chunk store uses FastCDC v2020 (32 KiB minimum, 64 KiB target, 256 KiB maximum),
SHA-256 chunk deduplication and conditional zstd compression. Valid DOCX, XLSX
and PPTX captures use the existing bounded Office ZIP validator and grouped
member regions; unsupported packages use ordinary FastCDC. Conversion of
hash-named whole objects recognizes Office packages with the same validator.

Neither chunk boundaries nor compression participate in `blob_id` or
`content_id`. Both remain SHA-256 identities of the original exact bytes;
salted body payloads are never regenerated during conversion. Reads reconstruct
and hash the full logical object before accepting it. Body reads also validate
the size bound and original codec. Commit, create-file, save-file, append-chat
and email-import writes all honor the recorded mode.

`BlobId` and `ContentId` remain distinct logical references. A file reference reads
and verifies raw bytes and size; a body reference validates and decodes the exact
salted `PLAINTEXT_UTF8_V1` payload. Decoder selection follows the canonical typed
reference, never a guess based on the stored bytes. A file may contain bytes that
look like a body payload without being interpreted as a body.

Identical exact bytes occupy one object even when both logical reference classes
name them; both reference validations still apply. Verification reports `blobs`
and `bodies` as logical closure counts, not disjoint physical file counts.
Hydration creates a presentation copy of an event. It never rewrites canonical
payloads, salts, hashes, commit/event identities or existing content IDs.

Every retained item and ref event participates in closure discovery, including
trashed/tombstoned items, historical file versions and superseded bodies. Use
canonical Blob/Content references and the imported-email
metadata references. Do not infer completeness from current heads or arbitrary
JSON strings that resemble hashes. A published export/copy contains exactly the
union of the referenced closure; all required objects pass their applicable
hash/codec checks.

## Reading and copying

Current folder listings use accepted Core namespace rows, ordered by canonical
name key and item ID with bounded pages. They do not reconstruct every item or
wait for a platform cursor. Current pages are not a frozen multi-request snapshot.
Item reads replay only the requested owner's history and attached refs; historical
reads pin a sealed checkpoint and bind page cursors to it. FILE or imported-EMAIL
source extraction checks bytes and size before publishing an absent destination
without replacement.
Unknown typed presentation must not prevent canonical history preservation.

`item show` uses the shared [incremental reader](../crates/synck-items/src/reader.rs)
for supported FILE/FOLDER history, chats, email and frozen file-version sidecars.
Task, post, table/database,
calendars/events, calls, timekeeping and organization/mount reducers are absent
from 0.2. Support depends on the retained event vocabulary; an unknown typed
event can fail presentation without preventing a verified canonical copy.
Reading a family does not imply that all of its mutations are exposed locally.

```sh
synck repository verify /path/to/exported-repository
synck item list --path /path/to/exported-repository
synck item show <item-id> --path /path/to/exported-repository
synck repository import /path/to/exported-repository /path/to/new-copy
synck repository archive /path/to/new-archive --path /path/to/exported-repository
```

Import is a verified repository copy; it requires no author and creates no import
commit. Archive also produces a ready-to-open repository. Both preserve canonical
history, repository/item identities, domain cursors and exact content, and create
a follower without an automatic remote. They preserve the source storage mode
by default. `--storage-mode WHOLE_FILE|CHUNKED` explicitly converts the private
copy before publication; the source is unchanged. Chunked-to-chunked copies
retain the existing chunk mappings and copy a SQLite snapshot pruned to the
complete retained-history closure, including bodies and attachments. Existing
targets are never overwritten or merged. Refresh means another complete verified copy in a new destination.
A failed refresh leaves the previous complete copy usable.

The source is locked during copying. Native SQLite backup captures a consistent
database, including committed WAL contents, and required exact objects are copied
once into private staging. All canonical
history, replayed item/ref heads, historical state and complete content closure
are verified before no-replace publication and parent-directory sync. A failure
before publication leaves the destination absent. Publication followed by a parent
sync failure is a distinct uncertain-durability outcome: inspect the visible copy
instead of assuming nothing was installed. Process crashes can leave unpublished
private staging directories; a fresh invocation does not implicitly resume them.

### Physical storage optimization

```sh
synck repository import /path/to/export /path/to/copy --storage-mode CHUNKED
synck repository archive /path/to/whole-copy --path /path/to/repository --storage-mode WHOLE_FILE
cd /path/to/repository
synck storage optimize
```

`storage optimize` works on bare format-6 repositories and checkouts. It verifies
the complete repository, captures whole objects into the chunk store, verifies
and syncs that store, then atomically publishes `CHUNKED` in `config.json`.
Only afterward are whole objects removed. Canonical SQLite history, commit IDs,
blob IDs and content IDs do not change. Optimization requires the repository
lock and rejects a pending checkout-selection transition. A failed attempt before
the mode switch leaves whole files authoritative; retry resumes the incomplete
chunk capture. After the switch, only chunks are authoritative, and retry verifies
then removes leftover whole files. Readers never fall back between layouts.
A chunked repository without leftovers is an idempotent no-op after verification.
Use a new archive/import destination for conversion back to `WHOLE_FILE`.

Verification covers the canonical retained-history closure and validates chunk
mappings, chunk hashes, decompression and reconstructed logical hashes. Missing
or corrupt mappings, payloads or bodies prevent a verified copy from publishing.
Private copy verification also rejects incomplete blobs and orphan chunks.

Older repository formats are rejected. Import accepts complete format-6
repositories and preserves canonical history and content IDs.

## Producer consistency

A producer exports one complete accepted checkpoint: all visible accepted commits
must be sealed and repository-sequenced in the same snapshot as final item rows,
domain cursors and bodies. Sequencing before opening a snapshot is insufficient;
a new accepted write can arrive between them. Reject pending history rather than
silently omit accepted writes or mix a newer namespace with an older prefix.

An export producer may stage the existing bundle-v5 stream plus verified objects
and an in-memory `BundleSnapshot` (repository ID, terminal sequence, history length
and SHA-256) to materialize SQLite. These are implementation inputs, not another
customer package format. The length/hash cover all records, including final item
rows and domain cursors; individual commit hashes cannot detect truncation between
those final records. The snapshot checkpoint must equal the imported terminal
sequence, including zero for an empty repository. Hashes establish integrity, not
source authorization or a digital signature.

Missing content, unsupported codecs, malformed canonical history or any failed
verification prevent publication. A copy cannot supply bytes it never received.
The producer publishes only the completed normal repository; source configuration,
credentials, private services and intermediate bundle files are excluded.

## Local writer authority

Produced and copied repositories start as followers. Opening and reading need no
author selection. Possession of the bytes and changing machine do not grant the
source's writer authority. Explicit activation selects the author for future
writes and acknowledges retirement of the former local writer:

```sh
synck repository activate /path/to/repository --author example --source-retired
```

Retirement is an operational prerequisite, not distributed fencing: activation
cannot stop an inaccessible source from continuing to write. Bare-repository
continuation is limited to typed FILE creation/content saves, imported EMAIL
creation and message appends to an existing CHAT. Filesystem checkout adds the file/folder
operations described below. Continuation preserves repository/item identities
and previous canonical bytes, and does not recreate every application workflow or expose unrestricted raw-event commands. Externally
sequenced items retain their sequencing modes and stream identities; local
activation cannot take over their authority.

Accepted local history remains durable if later sequencing/indexing fails. Report
acceptance separately and require explicit finalize/recovery rather than replaying
the mutation. Opening or reading never performs that recovery.

A customer's recoverable state ends at the last complete copy in their possession.
Later source-only writes are absent. There is no Git-style branching, merging or
disconnected multiwriter convergence. New email-file imports use the shared parser
and retain their original immutable source bytes (see Imported email sources).

## Optional editable filesystem checkout

A complete export can become an independent local editing copy while its hosted
source continues operating. This is explicit independent continuation, not source
retirement, federation takeover, branching/merging or a synchronization remote.
There is no push, pull or supported rejoin. Existing `activate --source-retired`
retains its separate writer-handoff acknowledgment contract.

```sh
synck repository checkout /path/to/exported-repository --author local-editor
# Or materialize only literal selected subtrees and their ancestor folders:
synck repository checkout /path/to/exported-repository --author local-editor \
  --include documents --include other-folder/report.pdf
cd /path/to/exported-repository
# Edit, create, rename, move or delete ordinary files/folders in Finder/Explorer.
synck status
synck commit -m "Edit local files"
synck repository checkout-selection . --include documents/project
synck repository checkout-selection . --all
synck repository verify .
```

Initial checkout requires a root containing only `.synck`; existing filesystem
entries are never adopted or overwritten. Preflight validates selected local
FILE/FOLDER paths on the destination filesystem before activation. Externally
sequenced items cannot be selected for writing. FILEs below application owners
without a folder-only ancestor chain have no filesystem representation and remain
inside the complete repository. Chats, email, records, attachments, original
principals and complete history are retained in the same canonical SQLite/object
store; checkout creates no initialization commit and filters no database rows.

Checkout records `INDEPENDENT_LOCAL`, the chosen future author and a `checkout`
selection in config. The separate `repository.local.sqlite3` holds `local_*` tables for content heads, inode identities
and disposable scanner observations. Durable filesystem operation journals and
their staging directories live directly under `.synck/`; `.synck/tmp/` holds
non-authoritative temporary writes. A bounded pending selection descriptor
refers to a separately synced `checkout-source.json` manifest. `PortableRepository::open`
still performs no scanning or recovery. Explicit `Repository::discover` of an
enabled checkout resumes that work under the same repository lock.

Selections are literal case-sensitive canonical slash paths, without glob syntax.
An empty list selects the whole filesystem view. Omitted files are never deletion
candidates. Selection replacement requires a clean existing view, rechecks it
before recording the source manifest, and rejects modified/untracked entries in
newly selected paths. The existing journal removes clean deselected files and
materializes newly selected files without a history change. Changed source bytes
block recovery rather than being overwritten. Keep the bytes and resolve the
change before retrying. Move within a selected subtree normally; widen selection
before moving an item across its boundary. A folder deletion cannot cascade into
omitted files or non-filesystem children; such commits fail before acceptance.

Checkouts support status, ordinary/selected/batch commits, editing, restore,
restore-tree, file inspection/export and versions. Revert and recover append
checked FILE/FOLDER commits; deleted items can be recovered by unambiguous path
or item ID. Sparse selection and subtree closure checks still apply.
`fsck` and `repository verify` emit the same report and check checkout state.
Both compare the derived file heads (content sequence, blob ID and size) with
canonical history; syntactically valid but incorrect cache values are errors.
Checkout creation rebuilds these heads using the same replay as ordinary recovery.

In a checkout, `repository create-file` and `repository save-file` write the
selected filesystem path and use the ordinary selected-commit path. Dirty targets,
occupied creation paths and paths outside the selection are refused. New files
must be included by the current ignore rules. An unchanged save returns the
current commit without adding history. If acceptance fails after writing the
file, the supplied bytes remain as an uncommitted edit: inspect status and commit
them normally. Finish pending checkout recovery with `synck status` before using
these direct file writers. Bare repository writes retain their existing behavior.
Both writers accept a message, author, origin time and operation ID, and
`save-file` accepts a content base; [Integrating an application](integration.md)
defines those options and their errors.

Revert operates on filesystem commits and refuses a commit that also changes
non-filesystem items, rather than applying only part of its inverse.
`storage` reports referenced/unreachable logical objects and bytes; `gc` verifies
history before collecting unreachable storage under the repository lock in either
mode. `archive` aliases complete repository copying, including `--storage-mode`.
Parallel workspaces and network workflows are not exposed by the 0.1 CLI.
Copies reject a pending checkout-selection transition or nonempty `.synck/tmp/`.
They preserve all canonical data, strip filesystem tracking and local activation,
and reopen as bare followers; copying does not recover the source filesystem.
Complete checkouts reuse the working-copy stat/digest cache and filesystem
monitor for status and commit. The CLI starts the monitor automatically; library
callers use an already-running monitor or fall back to cached full scans. Monitor
candidates are checked for moves across the checkout boundary before selection
filtering. Reset or unavailable monitors use the ordinary baseline scan, retaining
timestamp-race checks and ignore-rule invalidation.

The public library exposes `PortableRepository::enable_checkout(&mut self,
author, paths)`, `PortableRepository::checkout_paths()`,
`Repository::set_checkout_paths(&mut self, paths)` and
`Repository::checkout_paths()`. The mutating enable/selection methods are async;
selection readers return `Option<&[String]>`, distinguishing bare from full
checkout. Native clients may use normal `Repository` file editing and commit APIs
after explicit enablement, while retaining `PortableRepository` for mixed data
inspection and complete copies.

## Imported email sources

New `.eml` and `.msg` sources always materialize as EMAIL objects, including
ordinary complete-checkout commits and `repository create-file`. The selected
source filename determines its format even when the requested item name changes.
The final email suffix is removed from the requested name when present. Malformed
or over-limit messages fail explicitly; they never fall back to FILE. Existing
historical FILE identities are never converted to EMAIL. Edits or renames that
would retain or introduce a mail-source suffix are rejected; import a new EMAIL
identity instead.

The storage-free parser is provided by `synck-items`.
`mailcal` version 2 metadata records `origin.kind=local_import`, the original
filename, format, MIME type, length and exact source blob; decoded participants,
independent sent/received dates and bodies are retained. Every MIME attachment,
including inline content, is a durable EMAIL attachment subitem in the same
commit. Exact complete-checkout `show --events` reads verify canonical event
history for all accepted item families without requiring a FILE/FOLDER semantic
delta; raw event presentation retains owner type, name and parent identity. MSG retains its exact original blob independently from generated RFC822.
Commit author/time identify the local operation and are independent of email
sender/header dates. Local imports accept at most 64 MiB source and decoded
attachments, 1,000 attachments, 2,000 MIME parts and 64 nesting levels. HTML is
retained as inert content; parsing does not fetch external resources.

Original checkout source files remain unchanged after acceptance. Local schema
11 records their path, proposed EMAIL ID and source digest before canonical
append. Only mappings joined to an accepted live EMAIL suppress an unchanged
source from subsequent status/commit. Failed appends cannot hide a source and
accepted appends have no later mapping-insert crash window. Changed source bytes
become additions again; they cannot overwrite the earlier EMAIL. A commit whose
new EMAIL would take the name of a LIVE sibling fails with `INVALID` before
append and explains that the changed message needs a new file name. Copies strip
these local observations and retain all canonical source/body/attachment blobs.
Changing checkout selection preserves accepted source files and the directories
needed to hold them, even outside the selected view. Source recognition follows
the original parent folder identity through filesystem and `synck mv` folder
moves; another copy at a different path remains a new import. A changed source
blocks selection changes that would include it; selection never overwrites those
bytes.

Create a complete checkout directly, preserving any ordinary files uncommitted:

```sh
synck repository init /path/to/local-directory --author local-editor
```

`synck init [DIR] --author <author>` and `synck repository init` share the same
initialization path. Both create a CHUNKED independent checkout, record hidden
files as IGNORE, and commit only the created/preserved `.synckignore` as the first
commit. Existing ordinary files remain uncommitted. Default rules cover OS junk,
Office/editor temporary files and unambiguous dependency caches such as
`node_modules/` and `__pycache__/`. Ordinary `target/`, `dist/` and `build/`
folders are not ignored. Explicit `--storage-mode WHOLE_FILE` selects whole-file
storage. `synck_local::init` is the same initializer for library callers.
Restoring a copy is a verified `import_repository` copy into a bare follower,
followed by `enable_checkout` when files are wanted on disk.

## Local chats and attachments

`repository create-chat <title>` creates a CHAT named `chat-<UUIDv7>` and records
its ROOM profile with `CHAT::SET_PROFILE` in the same commit. `repository
append-chat <id> <body-file> --attachment <file>` stores a salted message body and
embedded attachment bytes in the selected storage mode. The attachment names its
parent message sequence. Chats have no filesystem representation, while their
canonical history and content closure survive archive/import unchanged. Email
imports likewise retain original source, bodies and attachments in both modes.
