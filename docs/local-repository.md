# Local checkouts and the `synck` CLI

This document defines how the `synck` CLI and the `synck-local` library work
with a repository's checkout: the working tree of ordinary files and folders,
its local metadata, scanning, commits, recovery and the command contract. Every
repository uses the format-6 layout defined in [Portable
repositories](portable-repository.md); that contract covers configuration,
storage modes, complete verification, copies, item reads, typed writes and
activation. Core event, commit, identity, naming, and sequencing semantics are
defined by [Core engine](core.md). Core uses storage backend traits; SQLite is
the adapter used by this local distribution.

A checkout is enabled by `synck init` or by `synck repository checkout`. Bare
opens perform no working-tree scanning or recovery. A checkout reuses the same
repository lock, preserves all non-file data, and may select literal subtrees.

## 1. Product boundary

The standalone distribution provides the `synck` CLI and the `synck-local` Rust
library. A checkout has one working tree containing a `.synck/` metadata
directory. Local use requires no server, PostgreSQL, login, network, browser or
background process.

Historical views pin an exact sequenced commit and page its namespace without
materializing the whole tree into the working folder. Extracting one historical
FILE verifies its bytes before publishing it into the caller's cache directory.
New commits do not move an already-open historical view.

The public `synck-local` surface is explicit: `open_historical_view` returns an
opaque `HistoricalView` pinned to the authenticated root;
`historical_children` returns one `HistoricalPage` and an opaque continuation
cursor bound to that root and parent. Its 256-entry limit counts only visible
files and folders; hidden `FILE_VERSION_SET` items never create empty or
underfilled user pages.
`materialize_historical_file` returns an owning `HistoricalFile` handle for one
verified cache publication and holds an
operating-system lease for that handle's lifetime. Dropping that handle attempts
safe cleanup. `cleanup_historical_cache` removes only direct children with a
complete canonical ownership manifest whose lease is not held by a running
process, and leaves every active, unknown, linked, or malformed entry alone.

The working tree versions recursive directories and regular files:

- directories are `FOLDER` items;
- admitted regular files are `FILE` items;
- committed `.eml` and `.msg` files become typed `EMAIL` items, described in
  [Portable repositories](portable-repository.md#imported-email-sources);
- the working-tree root is a container, not an item;
- top-level items have `parent_folder_id = NULL`;
- commits are manual and include either the complete observed delta or the
  explicitly selected file/folder scopes;
- ordinary filesystem renames and moves preserve the existing item identity
  only when the stored filesystem identity and stable birth incarnation resolve
  uniquely;
- `synck mv` provides an explicit, crash-recoverable move command.

Items without a place in the folder tree, such as chats, and files owned by
other items are never written to the working tree; they remain readable through
the item commands.

Local authority has no branch, tag, merge, rebase, staging area, symlink,
executable-bit, hard-link identity, federation, authentication, authorization,
or cloud-sync behavior. General help and `instructions` need no repository.
Copies made by `archive` are complete repositories without a checkout; they
are disaster recovery, not repository synchronization.

## 2. Repository discovery and identity

`synck init [<directory>] --author <value> [--storage-mode CHUNKED|WHOLE_FILE]`
creates a repository with a checkout in the directory, by default the current
one; `synck repository init` is the same command, and `synck_local::init` is
the same library entry point. `author` must be non-empty and is stored
unchanged as an opaque provenance string. The repository's authority is
`INDEPENDENT_LOCAL`, its storage mode defaults to `CHUNKED`, and hidden files
default to `IGNORE`. The command creates one baseline commit containing only
`.synckignore`; it does not commit any other existing file.

Initialization:

1. Refuse an existing `.synck` entry in the directory or any ancestor.
2. If a regular `.synckignore` exists, validate it and preserve its exact
   bytes. Reject any other kind or invalid syntax.
3. Build the complete repository in a private staging directory: for an
   existing directory, a unique root entry named `.synck-init-<uuid>`.
4. Create the configuration, lock files, reserved `objects/` and `tmp/`
   directories, the repository database, the local checkout schema and, for
   `CHUNKED`, the object database.
5. Write the preserved or default `.synckignore` into the staging tree, commit
   only that file with the fixed message `Initialize Synck repository`, and
   verify the complete staged repository.
6. Publish: install a created `.synckignore` without replacement, or confirm a
   preserved one is unchanged, then rename the staged `.synck` into place with
   no-replace semantics and sync the working-tree root. Other visible files
   remain uncommitted additions.

A failure before publication removes the staging directory and leaves the
target unchanged; an existing ignore file is never deleted or rewritten. The
fixed default template is:

```gitignore
# macOS archive metadata
__MACOSX/

# Windows metadata
Thumbs.db
ehthumbs.db
ehthumbs_vista.db
desktop.ini
/$RECYCLE.BIN/
/System Volume Information/

# Microsoft Office temporary files
~$*

# Editor temporary files
*~
*.swp
*.swo
*.tmp

# Dependency and tool caches
node_modules/
__pycache__/
*.pyc
.pytest_cache/
.mypy_cache/
.venv/
.next/
```

Ordinary folder names such as `target/`, `dist/` and `build/` are not ignored
by default.

Regular files are opened with read and write access for the durability flush;
Windows `FlushFileBuffers` requires a write-capable handle. Unix directories
are opened as real read-only directory descriptors and passed to `fsync`; an
`O_PATH` capability descriptor is not a durability handle.

A crash before the rename leaves no `.synck/` in the target; a crash after the
rename leaves a complete, verified repository. `.synck-init-<uuid>` is a reserved root pattern
and is never versioned. Concurrent initialization has one publisher; every
loser fails without modifying the published repository.

Other commands walk ancestors from the current directory and stop at the first
filesystem entry named `.synck`. That exact entry must be a valid repository;
an incomplete or corrupt nearer entry must not fall through to an ancestor.

`.synck/config.json` is the strict format-6 configuration defined in [Portable
repositories](portable-repository.md). A checkout records `author`, an
`INDEPENDENT_LOCAL` or `LOCAL` authority and a `checkout` entry with its
selected paths; an empty list selects everything:

```json
{"storage_mode":"CHUNKED","format_version":6,"repo_id":"<UUIDv7>","author":"<opaque non-empty string>","authority":"INDEPENDENT_LOCAL","checkout":{"paths":[]}}
```

Any other format version is rejected with `unsupported repository format;
format 6 is required`. Unknown fields are errors. This config contains no
absolute path, credential, tenant, account or platform identifier.

The author is repository-local configuration for future commits. Read it with
`synck config author` and replace it with
`synck config set-author <non-empty-value>`. Replacement writes and syncs a
complete temporary config, atomically replaces `config.json`, and syncs the
metadata directories before success. It preserves the repository ID and format
version and never rewrites the author recorded by an accepted commit.

The repository also stores one authoritative hidden-files discovery mode in
SQLite. `IGNORE` is the default; `TRACK` makes untracked dot-prefixed and
platform-hidden entries eligible for discovery. Read it with
`synck config hidden-files` and change it with
`synck config set-hidden-files ignore|track`. A change invalidates the
disposable worktree scan baseline before it returns, so the next status or
commit is a complete observation under the new policy. Copies preserve this
setting.

## 3. Metadata layout

```text
.synck/
  config.json
  repository.sqlite3
  object-store.sqlite3             # required when storage mode is CHUNKED
  lock
  monitor.lock                     # monitor lifetime lock target
  monitor.sock                     # Unix only, present while monitor runs
  move.json                         # present only during an interrupted synck mv
  working-tree-edit.json            # interrupted library working-tree edit
  working-tree-edit-stage/          # quarantine owned by working-tree-edit.json
  recover.json                      # present only during an interrupted synck recover
  revert.json                       # present only during an interrupted synck revert
  revert-stage/                     # present only while revert.json exists
  worktree-restore.json             # present only during synck restore-tree
  worktree-restore-stage/           # present only while worktree-restore.json exists
  objects/                          # WHOLE_FILE: flat SHA-256-named objects
  tmp/
```

`config.json` and `repository.sqlite3` are always authoritative repository
state. `storage_mode` in `config.json` selects exactly one byte authority, as
defined in [Portable repositories](portable-repository.md):

- `WHOLE_FILE`: `.synck/objects/` contains complete objects, each named by the
  SHA-256 of its exact bytes; `object-store.sqlite3` is absent or is
  non-authoritative resumable optimization state.
- `CHUNKED`: `object-store.sqlite3` is authoritative and `objects/` is normally
  empty. A matching whole-file object is never a fallback for a missing chunked
  blob.

Before reading the mode, discovery permits `object-store.sqlite3` to be absent
or a no-follow regular file. After reading it, `CHUNKED` requires the file and
`WHOLE_FILE` permits either state. Directories, links, and Windows reparse points
are rejected before SQLite opens the path. Unknown modes fail closed.
The same no-link rule applies to `.synck/` itself, every required metadata file
and directory, and every whole-file object;
repository cleanup must never traverse a Windows junction.
SQLite WAL and SHM sidecars for either database are permitted only as regular,
non-reparse files while their main database exists. Linked, non-file, and
orphan sidecars fail discovery before SQLite opens the database family.

`lock` is the repository-operation advisory-lock target. `monitor.lock` is a
separate stable target held only for the lifetime of the optional filesystem
monitor. Existing repositories create and sync a missing regular
`monitor.lock` while holding `lock`; any other entry kind is an error. The
monitor never opens SQLite or holds `lock`. On Unix, only the `monitor.lock`
owner may replace a stale real `monitor.sock`, and it removes the socket on
shutdown only when the path still names the socket it bound. The socket and
monitor lock contain no repository state and are excluded from copies.

`move.json`,
`working-tree-edit.json`, `recover.json`,
`revert.json` and `worktree-restore.json` are strict, durable
operation intents and are absent outside their command's execution or recovery.
Their matching staging directories are authoritative recovery state only while
the intent exists; an orphan staging directory or simultaneous intent files are
integrity errors.
`tmp/` contains only non-authoritative interrupted writes, including an
unpublished first optimization database. SQLite may create the validated
transient WAL and SHM sidecars for either live database; completed copies
contain none.

All repository commands acquire one exclusive nonblocking advisory lock on
`.synck/lock` for their complete operation. Contention fails immediately with
`repository busy`. Lock-file contents carry no state.

A library caller holds the repository lock for the lifetime of its `Repository`
or `PortableRepository` handle. Internal adapters share the same lock owner;
the last owning view explicitly unlocks before closing the descriptor. A Unix
child retaining an inherited descriptor between fork and exec must not prolong
the parent's operation lock. Library callers may query an
already-running monitor, but do not locate an executable or start one. Only the
`synck` CLI starts the installed `synck` binary in its hidden foreground
monitor-server mode.

After acquiring the lock, commands remove every entry below `.synck/tmp/`
without following links. Unique temporary files are created with create-new
semantics. A file already installed at its final digest path is not temporary
and is never removed automatically.

## 4. SQLite storage

The bundled SQLite library must include the WAL-reset corruption fix (SQLite
3.51.3 or later). The workspace uses rusqlite 0.39 with bundled SQLite 3.51.3;
the regression checks the linked library rather than only the Cargo manifest.
This dependency update changes no repository schema, identity or wire format.

`repository.sqlite3` uses the SQLite VCS backend and contains these logical
Core relations:

- `vcs_items`;
- `vcs_commits`;
- `vcs_events`;
- `vcs_ref_events`;
- `vcs_repo_activity_heads`;
- `vcs_federation_domains`;
- `vcs_checkpoints`;
- `vcs_history_generations`.

The separate `repository.local.sqlite3` contains these local relations:

- `local_schema_meta`;
- `local_projection_state(repo_id, applied_repo_seq)`;
- `local_file_heads(repo_id, item_id, item_seq, blob_id, size_bytes)`;
- `local_worktree_identities(repo_id, item_id, node_kind, device_id, file_id,
  birth_seconds, birth_nanos, path)`;
- `local_worktree_entries(repo_id, path, node_kind, device_id, file_id,
  birth_seconds, birth_nanos, size_bytes, mtime_seconds, mtime_nanos,
  change_seconds, change_nanos, observed_blob_id, stat_valid)`, the last
  coherent observation of every visible regular file and folder;
- `local_worktree_scan_state(repo_id, ignore_blob_id, monitor_instance_id,
  monitor_sequence)`, the ignore-rule and monitor boundary paired with those
  entries;
- `local_repository_settings`, recording hidden-file discovery policy;
- `local_email_import_sources`, recording disposable complete-checkout source
  observations rather than canonical EMAIL content.

The SQLite backend implements the complete `StorageBackend` and `StorageTxn`
contracts. Local commands exercise VCS mutations through `Repo`; they do not
reimplement commit construction, sequencing, hashing, or structural invariants.

The core SQLite schema uses `application_id = 0x53594E4B` (`SYNK`) and
`user_version = 6`. Only that schema is accepted; there are no upgrade or
compatibility paths. Initialization creates the current schema directly.
`local_schema_meta` independently identifies local projection schema 11; older
local schemas are also refused without migration. Repository-owned tables are
SQLite `STRICT` tables. Format and schema markers do not excuse missing required tables, columns,
unique keys or foreign keys. Definitions are checked structurally; see the
[format specification](spec/format.md#sqlite-schema-version-policy).

The local schema has exactly one repository-settings row:

```sql
CREATE TABLE local_repository_settings (
    repo_id TEXT PRIMARY KEY,
    hidden_files TEXT NOT NULL CHECK (hidden_files IN ('IGNORE', 'TRACK'))
) STRICT;
```

FILE cache rows require exact eight-byte device and file identities,
non-negative size, normalized modification time, a canonical SHA-256 blob ID,
and `stat_valid` in `{0,1}`. Birth/incarnation and change-time pairs are each
jointly null or present. Null change time requires `stat_valid = 0`.
Birth/incarnation is identity evidence only; when unavailable, off-path identity
matching is disabled for that entry. FOLDER rows contain no file-only fields.
Monitor instance and non-negative sequence are jointly null or present. These
rows are disposable machine-local observations, never accepted VCS state.

The version-8 worktree tables are exactly:

```sql
CREATE TABLE local_worktree_entries (
    repo_id TEXT NOT NULL,
    path TEXT NOT NULL CHECK (path <> ''),
    node_kind TEXT NOT NULL CHECK (node_kind IN ('FILE', 'FOLDER')),
    device_id BLOB NOT NULL CHECK (length(device_id) = 8),
    file_id BLOB NOT NULL CHECK (length(file_id) = 8),
    birth_seconds INTEGER,
    birth_nanos INTEGER,
    size_bytes INTEGER,
    mtime_seconds INTEGER,
    mtime_nanos INTEGER,
    change_seconds INTEGER,
    change_nanos INTEGER,
    observed_blob_id TEXT,
    stat_valid INTEGER,
    CHECK (
        (birth_seconds IS NULL AND birth_nanos IS NULL)
        OR (birth_seconds IS NOT NULL
            AND birth_nanos IS NOT NULL
            AND birth_nanos BETWEEN 0 AND 999999999)
    ),
    CHECK (
        (node_kind = 'FILE'
            AND size_bytes IS NOT NULL AND size_bytes >= 0
            AND mtime_seconds IS NOT NULL
            AND mtime_nanos IS NOT NULL
            AND mtime_nanos BETWEEN 0 AND 999999999
            AND observed_blob_id IS NOT NULL
            AND length(observed_blob_id) = 71
            AND substr(observed_blob_id, 1, 7) = 'sha256:'
            AND substr(observed_blob_id, 8) NOT GLOB '*[^0-9a-f]*'
            AND stat_valid IS NOT NULL AND stat_valid IN (0, 1)
            AND ((change_seconds IS NULL AND change_nanos IS NULL)
                OR (change_seconds IS NOT NULL
                    AND change_nanos IS NOT NULL
                    AND change_nanos BETWEEN 0 AND 999999999))
            AND (change_seconds IS NOT NULL OR stat_valid = 0))
        OR
        (node_kind = 'FOLDER'
            AND size_bytes IS NULL
            AND mtime_seconds IS NULL AND mtime_nanos IS NULL
            AND change_seconds IS NULL AND change_nanos IS NULL
            AND observed_blob_id IS NULL AND stat_valid IS NULL)
    ),
    PRIMARY KEY (repo_id, path)
) STRICT;
CREATE TABLE local_worktree_scan_state (
    repo_id TEXT PRIMARY KEY,
    ignore_blob_id TEXT CHECK (
        ignore_blob_id IS NULL
        OR (length(ignore_blob_id) = 71
            AND substr(ignore_blob_id, 1, 7) = 'sha256:'
            AND substr(ignore_blob_id, 8) NOT GLOB '*[^0-9a-f]*')
    ),
    monitor_instance_id TEXT,
    monitor_sequence INTEGER,
    CHECK (
        (monitor_instance_id IS NULL AND monitor_sequence IS NULL)
        OR (monitor_instance_id IS NOT NULL
            AND length(monitor_instance_id) = 36
            AND monitor_sequence IS NOT NULL
            AND monitor_sequence >= 0)
    )
) STRICT;
```

`object-store.sqlite3` is a separate local physical store. It has
`application_id = 0x534F424A` (`SOBJ`), `user_version = 1`, 4096-byte pages,
`journal_mode=WAL`, and `auto_vacuum=INCREMENTAL`. Those pragmas and the exact
schema are format. Its only identity row contains the singleton marker and the
configured repository ID; no table duplicates the schema version.

Schema version 1 contains exactly these strict tables and index:

```sql
CREATE TABLE store_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    repo_id TEXT NOT NULL CHECK (repo_id <> '')
) STRICT;
CREATE TABLE blobs (
    digest BLOB PRIMARY KEY CHECK (length(digest) = 32),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    complete INTEGER NOT NULL CHECK (complete IN (0, 1))
) STRICT;
CREATE TABLE chunks (
    digest BLOB PRIMARY KEY CHECK (length(digest) = 32),
    raw_size INTEGER NOT NULL CHECK (raw_size BETWEEN 1 AND 262144),
    codec TEXT NOT NULL CHECK (codec IN ('RAW', 'ZSTD')),
    payload BLOB NOT NULL,
    CHECK (
        (codec = 'RAW' AND length(payload) = raw_size)
        OR
        (codec = 'ZSTD' AND length(payload) > 0
            AND length(payload) * 100 < raw_size * 95)
    )
) STRICT;
CREATE TABLE blob_chunks (
    blob_digest BLOB NOT NULL CHECK (length(blob_digest) = 32),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    chunk_digest BLOB NOT NULL CHECK (length(chunk_digest) = 32),
    PRIMARY KEY (blob_digest, ordinal),
    FOREIGN KEY (blob_digest) REFERENCES blobs(digest) ON DELETE CASCADE,
    FOREIGN KEY (chunk_digest) REFERENCES chunks(digest)
) STRICT;
CREATE INDEX idx_blob_chunks_chunk_digest
    ON blob_chunks(chunk_digest);
```

Exactly one identity row is required. Complete zero-byte blobs have no mapping
rows. Every nonempty complete blob has contiguous ordinals beginning at zero;
the same chunk digest may appear at multiple ordinals. No reference counts are
stored.

Every writable object-store connection sets and reads back
`foreign_keys=ON` and `synchronous=FULL` before beginning a transaction. Every
open validates application ID, user version, WAL mode, exact schema, and
repository identity. Unknown or mismatched state fails closed.

Canonical UUIDv7 identifiers are lowercase TEXT. Commit IDs and 32-byte digests
are raw BLOB. Blob IDs inside canonical event JSON remain lowercase SHA-256
TEXT. Counts and sequences are checked SQLite INTEGER.
Timestamps are TEXT in one canonical UTC form:

```text
YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ
```

Exactly nine fractional digits are required. Noncanonical stored timestamps are
errors. This fixed-width form preserves chronological SQLite ordering.

Existing writable databases are validated before any persistent pragma is
issued. Schema initialization alone establishes `journal_mode=WAL`, and only
after proving that the database is unversioned and contains no non-internal
schema objects. Writable connections set `foreign_keys=ON`, a five-second busy
timeout, and `synchronous=FULL`, then start write work with `BEGIN IMMEDIATE`.
Read-only verification connections do not issue write pragmas and verify the
stored application, journal, and schema state.
The repository database contains only the contracted `vcs_*` and `local_*`
schema objects. Any other non-internal table, view, index, or trigger is an
integrity error detected before local schema migration.

SQLite errors map to the existing VCS storage taxonomy:

- uniqueness constraint failures: `UniqueViolation`;
- `BUSY` or `LOCKED` after the busy timeout: `Overloaded`;
- `IOERR`, `CANTOPEN`, `FULL`, or `READONLY`: `Io`;
- corrupt schema/data and otherwise unmapped failures: `Other`.

The local file format contains exactly the `repo_id` in `config.json`. Every
repository-scoped authoritative and `local_*` row must use it, and
`vcs_federation_domains` must remain empty. Direct backend tests may exercise multiple repositories and the
federation cursor; a local repository may not contain either.

## 5. Filesystem and path rules

Working-tree access is rooted at one pre-opened capability directory. Commands
reject absolute paths, lexical `..` escape, and symlink or reparse traversal in
any component. Directory enumeration, metadata reads, and file reads are
authoritative inputs; failure aborts with the relative path. An unreadable
tracked path is never interpreted as deleted.

Names must decode losslessly to Unicode and satisfy the core Synck naming
contract. Portable `name_key` collisions and platform-forbidden names are
errors.

The scanner accepts directories and regular files only. A discoverable or
tracked symlink, reparse point, socket, device, FIFO, or other unsupported entry
aborts without history mutation. Unsupported entries below a wholly ignored,
untracked subtree need not be visited.

Reserved entries are:

- the repository root's `.synck/`;
- root `.synck-init-<uuid>` entries;
- `.synck-tmp-<uuid>` in any working-tree directory.

The root `.synck/` and reserved temporary patterns are excluded. A non-root
`.synck` reached through a discoverable or tracked path is an error.

## 6. Ignore rules

Only a root `.synckignore` is read. It uses Git-ignore syntax. No parent Git
repository, global Git configuration, editor list, or operating-system ignore
source is loaded.

Hidden-file discovery is a repository setting, not an ignore-file convention.
The default `IGNORE` mode excludes every untracked dot-prefixed file or
directory at any depth, plus entries with the macOS `UF_HIDDEN` flag or Windows
`HIDDEN` or `SYSTEM` attribute. A hidden directory excludes its untracked
descendants. The root `.synckignore` remains the reserved exception. `TRACK`
mode removes only this hidden-state filter; ordinary `.synckignore` rules still
apply. The default ignore file separately excludes known macOS archive,
Windows metadata, and Microsoft Office temporary entries. Existing
`.synckignore` files are preserved byte-for-byte during initialization.
Repositories created by development builds that emitted `.*` and
`!/.synckignore` retain those ordinary rules; remove and commit them before
using `hidden-files=track` to include all dot-prefixed entries. Synck never
silently rewrites a tracked ignore file during schema migration.

Ignore rules affect untracked discovery only. Every tracked path is compared
even if a later rule ignores it. An ignored directory prefix is traversed when
it contains a tracked descendant. Therefore:

- a tracked file that later matches an ignore rule remains tracked;
- a tracked file that later gains a native hidden attribute remains tracked;
- an ignore negation cannot override `hidden-files=ignore`;
- `hidden-files=track` makes hidden entries eligible, after which ordinary
  ignore rules and negations decide whether they are included;
- deleting a tracked file below an ignored directory records a deletion;
- an untracked `.synckignore` may ignore itself;
- once committed, `.synckignore` remains tracked.

## 7. Scanning and commit construction

Every working-tree operation remains correct from a complete recursive scan
under the repository lock. The version-8 worktree cache avoids rereading file
content when the cached path or unique filesystem identity, kind, identity,
size, modification time, and platform change time all match. Unix uses inode
change time; Windows uses `FILE_BASIC_INFO.ChangeTime`. Missing change time
disables reuse for that entry. A cache hit is never item-identity evidence.

Before hashing, the scanner captures the complete fingerprint from the opened
no-follow file handle. It streams SHA-256 and then requires kind, identity,
size, modification time, and change time to remain exactly equal. A mismatch
aborts the scan and publishes no cache state. Cache reuse is also forbidden
when either modification or change time is not strictly older than the scan
start minus two seconds. This conservative racy-file rule may briefly rehash a
new save but cannot report changed bytes as clean. Dirty digests are cached too,
so repeated status of the same unchanged dirty file does not reread it.

The complete scanner still enumerates the tree and applies the existing ignore,
name, no-follow, unsupported-entry, and tracked-descendant rules. It uses the
cache only to skip content reads. When a valid monitor token exists, the
incremental scanner starts from the last complete cached snapshot, re-inspects
the reported paths and parents, drops vanished cached subtrees, and recursively
enumerates every directory candidate. Candidate scopes are normalized,
deduplicated, and minimized before access. Coalesced directory events therefore
rescan that directory; a moved-in populated directory is discovered in full.
The final delta still comes from the ordinary identity reconciliation.

The root `.synckignore` is read and hashed before incremental scanning. Any
presence or content change forces complete discovery. A monitor candidate for
`.synckignore` invalidates the current incremental or baseline scan because the
rules already loaded by that scan may be stale. Synck reloads the rules and
restarts the bounded baseline handshake. If they change again during the final
attempt, Synck performs one fresh complete scan and publishes no monitor token.
A complete scan replaces the cache; an incremental scan updates only affected
scopes. Cache rows, identity rows, ignore digest, and monitor token are
published in one SQLite transaction only after scanning, reconciliation, and
delta construction succeed. A failed command retains the preceding coherent
cache boundary.

The committed tree is built from paginated LIVE `vcs_items` and the local FILE
content-head projection. The working tree is compared by normalized relative
path, kind, filesystem identity plus birth incarnation, and file SHA-256:

- only in working tree: create;
- existing FILE with another hash: set content;
- only in committed tree: trash;
- one uniquely matched tracked filesystem identity and birth incarnation at
  another path: rename, move, or move-and-rename according to whether its name,
  parent, or both changed;
- FILE/FOLDER kind differs at one path: report and commit deletion of the old
  identity, then addition of the new identity and subtree;
- equal path, kind, and hash: unchanged.

After each scan, the derived identity cache records each matched item's path,
kind, operating-system file identity, and stable birth/creation incarnation.
On Unix identity is device and inode and incarnation is the no-follow birth time
when the filesystem exposes it; on Windows identity is volume serial and file
index and incarnation is creation time. A later scan preserves an `item_id`
across an ordinary Finder, Explorer, or terminal move only when one stored
`(device,file_id,birth)` tuple matches one current entry of the same kind.
Without a stable incarnation, off-path identity matching is forbidden and the
delta is delete-plus-create. After exact identity matching, one unique derived
identity row may retain its item at that row's exact previously observed path
when the replacement has the same kind. This lets an editor atomically replace
a file after Synck has already observed its uncommitted relocation. The same
rule applies at the exact committed path. Replacement before Synck has observed
an off-path relocation remains delete-plus-create. Size, content, extension,
timestamps other than birth, and notification pairing are never substitutes.
This prevents a newly created file from inheriting a deleted file's history when
the filesystem reuses its inode or file index.

NTFS name tunneling is the only exception to exact birth-time equality. Windows
may give a renamed file the creation time of a recently deleted file at the
destination name. Synck accepts that transition only for one FILE whose unique
volume/file index follows the vacated source and whose new creation time exactly
equals the displaced destination's stored creation time. Otherwise the ordinary
delete-plus-create rule applies; content remains irrelevant to identity.

When a folder identity moves, unmatched tracked descendants are mapped by their
relative suffix below that folder. This preserves descendant identity when an
editor replaces a child file during the move. Copied bytes, equal hashes,
timestamps, and filesystem notifications are never identity evidence. If the
stored or current identity is unavailable or ambiguous, the scanner reports
delete-plus-create rather than guessing. The cache is derived local state; VCS
items and events remain authoritative.

The optional monitor is a correctness-independent accelerator. It installs one
native recursive watch before publishing a random process-instance UUID and a
monotonic sequence. It records only root-relative candidate paths in bounded
memory; it does not read ignore rules, hash files, infer moves, open SQLite, or
construct VCS events. Both endpoints of a complete rename are candidates.
Watcher rescan requests, errors, incomplete renames, unrepresentable paths,
unknown events, overflow, retained-history loss, restart, and repository root
or `.synck/` replacement invalidate coverage.

Before answering `QUERY`, the server creates one unique ephemeral cookie below
`.synck/` and waits within the same request deadline until the native watcher
observes it. Events delivered before that cookie are incorporated into the
response boundary; cookie create/remove events themselves are ignored. Failure
to observe the cookie makes the client use a complete scan. This ordering
barrier prevents a filesystem edit whose native notification is still queued
from being incorrectly omitted by an immediate status call.

The monitor protocol is strict versioned JSON framed by a four-byte unsigned
big-endian length and limited to 8 MiB and 65,536 paths. `QUERY`, `STATUS`, and
`STOP` bind every request to the repository ID. Unix uses the exact
`.synck/monitor.sock` and requires a same-effective-user peer. Windows uses a
current-user, local-only named pipe with repository binding and validates its
access list and both client/server user SIDs. One two-second monotonic total deadline
covers connection, authentication, complete request write, complete response
read, and validation on both sides.

Monitor absence, timeout, authentication/protocol failure, restart, or `RESET`
never makes an ordinary command fail or claim an empty dirty set: the operation
performs a metadata-assisted complete scan and publishes no token. Establishing
coverage takes a baseline sequence, a complete scan, and a follow-up query;
events through the returned sequence are incorporated before cache and token
are committed atomically. One repeated coverage loss ends with a complete scan
and null token rather than a retry loop. Events after the returned boundary
remain for the next command.

Failure of a query against an existing cached token reloads `.synckignore`,
performs one complete scan, and publishes a null token without querying the
same failed endpoint again. A valid `RESET`, rather than a transport or protocol
failure, enters the bounded baseline handshake.

Visible FILE/FOLDER events use the validation and deterministic order in the
[shared profile](../crates/synck-file-folder/src/lib.rs): trash children-first,
creates parents-first,
restores parents-first, then ordinary updates, with item IDs as the stable tie
breaker. For one relocated item, `SET_PARENT` precedes `SET_NAME`; FILE content
then follows as `SET_CONTENT`. A new FILE's automatic named version is an
attached ref event in the same commit, after that FILE's visible create and
content events.

The local adapter consumes the shared `conformance.json` corpus. For accepted
cases it must reproduce the shared canonical profile events both before append
and after classifying a complete accepted commit; local FILE_VERSION_SET
sidecars may remain adjacent but disappear from the profile projection. Shared
semantic failures remain typed `SemanticError` values inside `LocalError`; the
adapter does not recover a code by parsing display text.

New folders and files receive `ItemId::new()` identities before proposal
construction. Existing-item events use the exact current `head_seq` as
`base_seq`. One UUIDv4 operation ID applies to the attempt.

The filesystem event profile is limited to `FOLDER::CREATE`,
`FOLDER::SET_PARENT`, `FOLDER::SET_NAME`, `FOLDER::TRASH`, `FOLDER::RESTORE`,
`FILE::CREATE`, `FILE::SET_PARENT`, `FILE::SET_NAME`, `FILE::SET_CONTENT`,
`FILE::TRASH`, and `FILE::RESTORE`. `FILE::MOVE` and `FOLDER::MOVE` are rejected
as unknown profile events; there is no reader, translation, migration, or
fallback for them. The local repository also
replays the shared `FILE_VERSION_SET::CREATE` and `FILE_VERSION_SET::VERSION_*`
events only as frozen format-3 history. Sidecars are top-level VCS items with
deterministic identities, but are never filesystem nodes. New writes use the
owning FILE's attached ref ledger and never create or mutate a sidecar. In
format version 3, top-level filesystem names
beginning with `.__synck_file_version_set_` are reserved for those internal
items; full and incremental scanning, creating, moving, recovering, or reverting
a visible FILE or FOLDER into that root interval fails before a commit is
accepted. Names below ordinary folders are unaffected. The local profile
requires every named version
to resolve to a real FILE content revision; it does not admit degraded
import rows with no `source_item_seq`. A missing or null version-content size is
resolved from that FILE revision. `VERSION_REPLACE_CONTENT` is a patch: omitted
source coordinates retain their previous values. RESTORE uses the shared core
trash transition with `set_in_trash = false`. Rename and parent change are
separate canonical events in the same atomic Core commit and carry the same
existing-item `base_seq`. A FILE that is also edited adds `FILE::SET_CONTENT`
after those structural events with that same baseline.
`FILE::SET_CONTENT` contains `EventContent::Blob` and exactly this metadata:

```json
{
  "content": {
    "size": 0
  }
}
```

`size` is the actual non-negative byte length and is at most
`9,007,199,254,740,991`. Local FILE/FOLDER commits use the shared 10,000-event
limit and the shared 16 MiB canonical projected-group admission limit. One Core
commit is never split. An explicit batch command may append several
independently atomic Core commits.

A clean commit prints `nothing to commit`, succeeds, and writes no empty
commit.

For every new FILE, `commit` appends `FILE::CREATE`, its first
`FILE::SET_CONTENT`, and one attached `FILE_VERSION::CREATE` ref in the same
commit. The initial named version points to FILE item sequence 2, uses ref
sequence 1, is `MINOR`/`INTERNAL`, and displays as `0.01`. A multi-file
working-tree delta remains one commit with one context message; each new FILE
receives its own initial ref and label. Existing historical FILEs are never
backfilled.

## 8. Object durability and scan semantics

`blob_id` remains `sha256:<hex64>` over the exact complete original byte stream.
Chunk boundaries and compressed bytes do not enter VCS events or commit hashes.

In `WHOLE_FILE` mode, objects remain at:

```text
.synck/objects/<first-two-hex>/<full-sha256-hex>
```

In `CHUNKED` mode, FastCDC v2020 uses fixed 32 KiB minimum, 64 KiB target,
256 KiB maximum, and normalization level 1. Each nonempty raw chunk is SHA-256
addressed. Zstd level 6 is retained only when its payload is strictly smaller
than 95% of raw size; otherwise the raw bytes are stored. Readers decode into a
fixed 256 KiB bound, reject malformed, trailing, concatenated, underflowing, or
overflowing frames, verify every chunk, then verify complete size and SHA-256.

Newly captured blobs whose trusted working-tree extension is DOCX, XLSX, or
PPTX receive one additional physical optimization before that same chunk writer
runs. `WHOLE_FILE` mode never invokes this policy. A bounded `rawzip` validator
must prove a complete ordinary ZIP32 package with
exactly one `[Content_Types].xml` member and the matching Word, Excel, or
PowerPoint core member. It rejects encryption, multi-disk archives, ZIP64,
unsupported methods or flags, duplicate names, inconsistent local and central
headers, overlapping ranges, trailing bytes, more than 65,535 members, or more
than 8 MiB of central-directory bytes. Ineligible content is an ordinary file
and uses generic FastCDC.

For eligible packages, each complete local-entry byte section at least 32 KiB
is one FastCDC region, consecutive smaller sections share one region, and the
central directory is a final independent region. The bytes are not extracted,
decompressed, normalized, or rewritten. The full original stream still passes
through the existing whole-file hasher, chunk verifier, encoder, batched writer,
and completion protocol. Existing complete blobs retain their original mapping;
renames and later references do not rechunk them. PDF, media, ordinary ZIP, and
all other content continue to use generic FastCDC.

For every new or changed file, the command opens the source without following
links, records identity/size/mtime, streams it through the full-file hash and
bounded chunker, and confirms the source metadata, expected scan digest, and
byte count did not change. A changed source aborts the whole commit. Chunk and
mapping writes commit in batches capped at 8 MiB stored payload or 1024 rows.
A logical blob remains incomplete until every mapping and the final checks are
durable. Recovery deletes its mappings in bounded batches, then deletes the
mapping-free incomplete blob row.

An existing whole-file object or complete chunked blob is fully verified before
reuse. Existing chunks are decoded and compared to the incoming raw chunk;
their codec and compressed bytes are not identity. New physical data becomes
durable before the checked FILE/FOLDER append may reference its logical blob.

The commit records the deterministic state observed by its scan and stable file
captures. A filesystem edit after a path was observed is an uncommitted change
for the next scan; ordinary filesystems do not provide a global snapshot. A
failed append may leave unreachable objects but cannot leave history pointing
at absent bytes. `synck storage` reports these objects and explicit `synck gc`
removes them; neither command treats historical but non-head content as
unreachable.

`Repo::append_file_folder_checked` is the point of no return. It validates any
recursive lifecycle closure and descendant heads in the same transaction that
accepts the commit. After it accepts the commit, ordinary append success requires
the CLI to sequence the accepted commit and catch up the local FILE-head
projection through it. It does not require historical-state projection. If
sequencing or FILE-head projection fails after acceptance, the command reports
`AcceptedNeedsRecovery` with the accepted commit ID. The next locked command
completes the pending work without appending the same working-tree state again.

## 9. Derived local projection

`vcs_items`, commits, events, and the selected physical object store are
authoritative.
`local_projection_state`, `local_file_heads`, and
`local_worktree_identities` are disposable.
`vcs_state_nodes`, `vcs_state_roots`, and `vcs_state_projection_heads` are a
separate disposable Core historical-state projection in `repository.state.sqlite3`.
Verification ignores this cache; historical listings rebuild it when absent. Unlike the local FILE
head cache, their canonical bytes are shared by SQLite and PostgreSQL and are
defined by the [Core historical-state module](../crates/synck-vcs/src/state/mod.rs).
`local_file_heads.item_seq` identifies the latest content event and may be less
than `vcs_items.head_seq` after a structural move. A later content event uses
the actual item head as its base sequence.

The ordinary locked-command recovery boundary sequences pending commits and
replays FILE-head events after `applied_repo_seq` in one SQLite transaction
before a command reads that projection. A missing FILE-head projection rebuilds
from repo sequence one. Authoritative rows are never repaired from projection
rows. This recovery does not initialize or catch up the separate historical-
state projection.

Projection catch-up reads its durable cursor and checks for a later complete
commit before loading immutable item kinds. A caught-up projection performs no
item-kind query proportional to repository size. For each pending complete
commit, it loads kinds for exactly the distinct existing event targets in that
commit; create events carry their own kind. A rebuild applies the same exact
per-commit rule instead of preloading all repository kinds.

One public command boundary owns recovery. Historical helpers invoked after
that boundary do not start recovery again. Complete worktree scans remain part
of commands whose semantics require them, but FILE/FOLDER materialization adds
no second all-item current-state or kind scan merely to validate a known delta.

Before a root-backed historical read, that command additionally initializes a
missing historical-state head or catches an existing head up through the
selected sequence in bounded complete-commit transactions. A failure in this
command-owned historical catch-up is reported as that command's failure; it is
not append recovery. A missing head makes every residual state row unpublished
reset work. A missing/corrupt root at or below a published head is an integrity
error and never triggers an implicit replay fallback.

## 10. Commands

### `synck instructions`

Print the exact UTF-8 operating guide compiled into the executable so that it
always describes the installed Synck version and remains available without a
repository or network connection. It teaches creating and committing, move and
rename handling, inspection, recovery, copies, email and chats, and the narrow
purpose of batch commits. It is an operating guide,
not a duplicate command reference; `--help` remains the complete command
inventory.

The command is deterministic, does not discover or recover a repository, and
does not inspect the working tree. It does not support `--json`; its concise
plain text is the contract for both people and agents.

### `synck info`

Print the discovered repository root, local format version, repository ID,
configured author, and current accepted head. An empty repository prints
`head none`; a non-empty repository prints the head repository sequence and
commit ID. The command does not run `fsck` or calculate storage totals; those
remain the responsibility of their explicit commands.

### `synck context`

Print one bounded, read-only orientation summary for a person or agent before
it changes the repository. The summary contains the same repository identity,
format, author, and accepted head as `info`; the exact working-tree change
count and first 100 deterministic `status` rows; and metadata for the five most
recent sequenced commits. It does not include historical change rows, file
contents, diffs, storage totals, or integrity verification.

The working-tree section reports whether it is clean, how many rows are shown,
and whether the list is truncated. The recent-commit section contains
repository sequence, complete commit ID, UTC timestamp, author, and optional
message. The repository lock makes the head, status, and recent history one
consistent local observation with respect to other Synck commands; the
working-tree scan retains the ordinary `status` race checks against concurrent
filesystem edits. Use `status` for the complete current delta, `log` for more
history, and `fsck` for verification.

### Native working-tree inspection and editing

Native embedders use `Repository::inspect_working_tree` for one authoritative scan containing the
current commit delta and every live filesystem FILE/FOLDER entry, including entries excluded by
`.synckignore` or native hidden metadata. Ignore rules and native hidden metadata govern commits;
they do not hide local bytes from an inspector or from the subtree fence for a recursive operation.
The delta, identity reconciliation, and complete tree are derived from the same monitor-fenced
filesystem observation; inspection does not combine an ignore-aware scan with a later complete
scan. Each `WorkingTreeEntry` carries its
current repository-relative display path, kind, size/time metadata, and the durable item ID and
sequences when identity reconciliation matched a tracked item. New entries have no item ID. The
entry's private filesystem identity and content digest are mutation authority; callers retain the
exact returned value and must not construct a write target from its display path.

`create_working_folder`, `create_working_file`, `import_working_files`,
`rename_working_entry`, `move_working_entry`, and `delete_working_entry` modify only the canonical
working tree. They append no commit. A later `status` reports
their ordinary Add, Move, Rename, or Delete rows and the user commits them through the existing
commit operation. Destination parents use retained `WorkingTreeEntry` values; `None` means the
working-tree root, which is a container and cannot itself be renamed, moved, or deleted.

Before mutation, the repository rescans and requires each supplied entry to retain the exact path,
kind, filesystem identity, size, content digest, and, for a folder, complete subtree digest. Names
are one valid Synck name, reserved
control names are rejected, destinations never overwrite, and a folder cannot move below itself.
Directory traversal and mutation use no-follow directory handles. A supplied non-root parent is
opened and its filesystem identity is compared with the retained parent entry before any create,
import, quarantine, or publication step; reopening the same display path is not authority.
Move, rename, and delete first rename the source object without replacement into the fixed
`.synck/working-tree-edit-stage/entry` quarantine, validate the exact quarantined object and full
subtree, then publish or remove it. The paired versioned `working-tree-edit.json` intent records the
prepared, quarantined, or published phase together with the exact source object and parent
identities. Intent creation, replacement, reading, and cleanup stay relative to the retained
repository metadata handle. After each rename, the destination directory is synced before the
source directory so the new name is durable before removal of the old name. Discovery restores an
exact quarantined source, accepts a published move only when the destination still contains that
exact object, repeats outstanding directory syncs in destination-first order before clearing the
intent, and rejects ambiguous occupied paths without overwriting them. The retained source and
destination directory handles remain both mutation and durability authority. Windows opens the
flush-capable handle relative to that retained authority, never through the ambient display path.

External imports open each source once without following links or reparse points, retain that
handle through the copy, reject duplicate canonical Synck destination names, stage and sync each
file beside its destination, and publish without replacement. A multi-file import is not atomic: a later I/O failure may leave
earlier files as visible uncommitted additions, so an embedding must reconcile after every outcome.

### `synck config author` and `synck config set-author <author>`

`author` prints the opaque non-empty author used for future ordinary commits.
`set-author` durably replaces that value and prints the stored result. Existing
commits retain their recorded authors. An empty author is rejected without
changing the configuration.

### `synck config hidden-files` and `synck config set-hidden-files <mode>`

`hidden-files` prints `ignore` or `track`. `set-hidden-files` accepts exactly
those two values, durably changes the repository policy, invalidates the scan
baseline, and prints the stored result. It does not alter accepted history or
make an ordinary commit. Already tracked items remain tracked in either mode.

### Stable JSON output

The following checkout commands support the schema-version-5 `--json`
output contract:

```text
synck info
synck context
synck monitor start|status|stop
synck config author
synck config set-author <author>
synck config hidden-files
synck config set-hidden-files ignore|track
synck status
synck commit [-m <message>] [<path>...]
synck revert <revision> -m <message>
synck log [-n <n>|--limit <n>] [--oneline|--events]
synck show <revision> [--events]
synck history <path>
synck compare <from-revision> <to-revision>
synck diff [<from-revision> <to-revision>]
synck deleted
synck versions <path>
synck version list <path> [--all]
synck storage
```

On success each listed invocation prints one compact JSON object followed by a
newline to standard output. `synck --json fsck` prints the complete-repository
verification report described in [Portable repositories](portable-repository.md)
instead. Every other top-level object contains `"schema_version": 5`. Lists retain the deterministic order of
the corresponding text command. Optional values are JSON `null`, byte counts
and sequences are JSON integers, timestamps are RFC 3339 strings, and IDs are
their canonical strings. `info.root` is the platform display form of the
discovered absolute path. Every change object has exactly `kind`, `path`,
`source_path`, and `content_changed`. Version 4 removes the synthetic `TYPE`
kind: when a FILE and FOLDER exchange the same path, the old identity is one
`DELETE` row and the new identity is one `ADD` row. `kind` is one of `ADD`,
`MODIFY`, `RENAME`, `MOVE`, `MOVE_RENAME`, or `DELETE`. `source_path` is
non-null for the three relocation kinds and may also
retain a modified descendant's old resolved path when only an ancestor moved.
`content_changed` is true for `MODIFY` and for a relocated FILE whose content
also changed; it is false for the other rows.
Version 5 adds the explicit `findings_omitted` completeness field to `fsck`;
all other command fields retain their version-4 meaning.

Runtime failures print one object to standard error with `schema_version` and
an `error` object containing stable `kind` and human-readable `message` fields.
FILE/FOLDER profile validation uses kind `FILE_FOLDER`; the message retains the
typed semantic code and detail from the shared profile.
An interrupted batch uses kind `BATCH_COMMIT_INCOMPLETE` and adds exactly one
`accepted_commit_ids` array containing the canonical IDs accepted during that
invocation or recovery pass. Other runtime error objects do not contain that
field. A refused `repository save-file --base-content` uses kind `CONFLICT`
and adds exactly one `current` object with the FILE's `item_id`, `head_seq`,
`blob_id` and `size`.
Command-line parsing failures use kind `USAGE`. Other commands routed through
the checkout JSON check reject `--json` with kind `INVALID` before the
requested operation.

The `repository ...` and `item ...` command families always emit their own
structured JSON success responses, even without `--json`. They do not use this
uniform schema-version-5 success envelope: mutation receipts carry version 1,
while item, log and verification responses use their respective result types.
With `--json`, their failures use the version-5 error envelope.

Exit status is 0 on success, including clean or dirty `status`; 1 for runtime,
validation, storage, or recovery failure; and 2 for command-line usage errors
or integrity failures reported by `fsck`. JSON `error.kind` distinguishes the
two exit-2 cases: an integrity failure uses kind `INTEGRITY`.

`compare --json` additionally records the resolved complete commit ID and
repository sequence for both endpoints; it never leaves a moving `HEAD` alias
as the machine-readable authority.

Mutation results use the same version-5 envelope, without changing existing
command fields:

- `commit` returns `commit_id`, the accepted canonical commit ID or `null` when
  nothing was committed. Path selection does not change the result shape.
  a clean selection returns `[]` and `0` respectively.
- `revert <revision>` returns the new inverse `commit_id`.

Both forms of `diff --json` return `entries` in the ordinary diff order. Each
entry has `old_path` and `new_path` (null for an absent side), `kind` (`PATCH`,
`BINARY`, or `TOO_LARGE`), and `patch` (the unified diff string for `PATCH`, null
otherwise). The existing text size limit also applies to JSON. An empty diff
returns `entries: []`.

### Recovery before command execution

Checkout commands using `Repository::discover` complete persisted crash
recovery before executing the requested command. Those effects belong to
the previously requested operation. Repository reads may also sequence accepted
activity and catch up derived projections. In the command descriptions,
`read-only` means that the requested command initiates no new commit or
working-tree mutation; it does not mean the repository metadata remains
byte-for-byte unchanged. Bare `PortableRepository::open` and the `repository`/
`item` read commands have their separate no-implicit-recovery contract.

### Revision names

Commands that select accepted commits (`show`, `revert`, `compare`, historical
`diff`, and `export-tree`) accept a complete canonical commit ID, `HEAD`,
`HEAD~N`, or a canonical positive decimal repository sequence. Decimal
selection is exact: a sequence gap does not select the preceding commit. Zero,
signs, whitespace, leading zeroes, overflow, and non-digits are invalid.
`HEAD~0` is HEAD; increasing non-negative decimal `N` selects older commits in
newest-first log order. Offsets are strictly less than 1000. Missing, malformed,
noncanonical, or out-of-range revisions are `INVALID`. There are no short IDs,
ambiguous prefixes, refs, branches, or general revision-expression language.

### `synck status`

Print one deterministic root-relative row per changed item. `A`, `M`, and `D`
mean add, content modification, and delete. If a FILE and FOLDER exchange the
same path, the old identity is a `D` row and the new identity is an `A` row. A
location
change is classified from the item's old and new structural state: `RN` changes
only its name, `MV` changes only its parent, and `MV+RN` changes both. Prefix
the structural marker with `M+` when that same FILE also changed content,
producing `M+RN`, `M+MV`, or `M+MV+RN`. Every relocation prints
`<marker> <old-path> -> <new-path>`. A clean working tree prints
`nothing to commit, working tree clean`. Clean and dirty results both exit 0;
scan/storage failures exit 1. JSON output remains the stable status envelope
with an empty `changes` array for a clean working tree.

Human output uses color only when standard output supports it and the user's
terminal environment permits it. `A` is bright blue, `M` and a relocation's
complete marker when it begins with `M+` are bright yellow, `D` is bright red,
and pure `RN`, `MV`, `MV+RN`, and the clean status message are bright green.
Interactive markers occupy a column as wide as the longest marker in the
displayed change set, followed by one space, so paths align without unused
padding. Paths themselves are never colored. Redirected human output remains
compact and is unstyled by default; `NO_COLOR` disables color and JSON never
contains ANSI styling.
`CLICOLOR_FORCE` explicitly enables terminal presentation in redirected human
output.

Before a CLI worktree operation, `synck` idempotently attempts to start the
repository monitor when no authenticated server answers. Startup failure is
silent for the ordinary command, which uses a complete scan. Library callers
never start a process.

### `synck monitor start|status|stop`

Manage the optional per-repository filesystem monitor. `start` is idempotent,
detaches the same installed `synck` executable, and waits at most two seconds
for authenticated readiness. `status` reports `running`, `stopped`, or
`unavailable` without changing VCS state. `stop` asks the authenticated current
process instance to close its watcher and endpoint; clients never kill a PID or
remove a server-owned socket. `stop` succeeds without contacting an endpoint
only when status is definitively `stopped`; an unavailable, unauthenticated, or
protocol-incompatible endpoint is an explicit `MONITOR_UNAVAILABLE` error.
The `status` detail is the category-free diagnostic text; the plain renderer
adds `monitor unavailable:` exactly once and JSON returns the same unprefixed
detail.
All three commands support the ordinary schema-version-5 JSON envelope.
Monitor state never appears in normal successful command output.

### `synck commit [-m <message>] [<path> ...]`

An omitted message records the commit's existing optional message field as
`null`; Synck never invents one. If `-m` is supplied, its value must be
non-empty and at most 16,384 UTF-8 bytes. Message validation precedes the
working-tree scan. Scan and validate before installing objects, then append
exactly one commit or none. There is no fixed event-count, intent-count or
aggregate commit-byte cap. Individual event and field validation still applies.
Success exits 0.
Operational failure exits 1. A failure after authoritative append must name the
accepted commit and pending recovery.

With no paths, commit the complete observed working-tree delta. With paths,
commit only changes selected by those caller-relative file/folder scopes;
unselected changes remain ordinary working-tree changes. A move is selectable
by its old or new path. A file scope is exact; a folder scope includes its
complete changed subtree. Selecting a newly added descendant automatically
includes newly added ancestor folders and the same-path deletion required to
create them.
Reject a selected path that exists in neither accepted nor working state.
Reject an accepted/working identity path pair when either spelling is reused by
another accepted or working item, including an untracked entry; path reuse
requires a broader commit instead of guessed selection. Same-path `D` and `A`
rows are selected together. Other structural dependencies
that cannot form a valid commit are rejected atomically by normal VCS
validation.

Selection is an argument to this one command. There is no persistent index,
`add` command, or staged/unstaged distinction.

### `synck revert <revision> -m <message>`

Require an accepted revision, a non-empty message, at least one sequenced
commit, and a clean working tree. Append one new commit
whose events invert the selected commit. The selected commit and all later
history remain immutable and visible; this command never deletes or rewrites a
commit.

Derive the inverse from the exact state roots immediately before and after the
selected commit, then apply it against current HEAD. Restore prior FILE
bytes, names, parents, and lifecycle while preserving item IDs. Items created by
the selected commit move to trash; items trashed by it return to LIVE. Later
changes to unrelated items never block the inverse. A later change to an
affected item is retained only when it changed an independent field: for
example, reverting old FILE content preserves a later move, and reverting an old
move preserves later FILE content. Reject overlapping content, structure,
lifecycle, descendant, or namespace changes rather than guessing a merge or
discarding later accepted work. Reject a selected commit with no reversible
file-tree change.

Revert is deliberately limited to FILE/FOLDER working-tree effects. The
selected commit may contain only FILE and FOLDER events and the automatic
`FILE_VERSION::CREATE` ref events of files it created; a commit that also
changes any other item family, such as a committed email or a chat message, is
rejected atomically. Every affected path must lie inside the checkout
selection; widen a partial checkout first.
`FILE_VERSION_SET` lifecycle events are not inverted. In particular,
`VERSION_TOMBSTONE` is permanent under the shared contract, and a commit that
only changes named-version state has no reversible working-tree change.

Write and sync one strict `.synck/revert.json` intent containing the shared
materializer's exact canonical events and recursive subtree guards before
append. Recovery rematerializes and compares that proposal against the pinned
publication base before an unaccepted append. After the
inverse commit is accepted, move affected current entries deepest-first into
item-ID-named entries below `.synck/revert-stage/`, then publish the prior state
parents-first without replacement. The staging directory makes path swaps and
same-path delete-and-add publication recoverable. Sync every staging and publication
transition. Remove `revert-stage/` and then `revert.json` only after projection,
working-tree bytes, structure, and filesystem identities match the accepted
inverse commit.

Repository discovery completes an interrupted revert exactly once. Pre-append,
accepted, staged, and partially published states are distinguished using the
persisted operation ID, selected commit, publication-base HEAD, item sequences,
paths, kinds, and FILE object identities. Malformed, stale, mixed, orphaned, or
contradictory intent/staging state fails closed. Reverting a revert commit
reapplies the state that commit reversed; there is no hidden special case.

### `synck mv <source> <destination> -m <message>`

Require a clean working tree, a tracked LIVE source, a nonexistent destination,
and a tracked LIVE destination parent folder or the repository root. Paths are
relative to the caller. Reject moves below the source subtree, portable sibling
name conflicts, unsupported filesystem entries, and empty messages.

Write and sync one strict `.synck/move.json` intent before changing the
filesystem. Rename without replacing an intervening destination and sync both
affected parent directories. Append `SET_PARENT` when the parent changes and
`SET_NAME` when the name changes, in that order in one Core commit, using the
source `item_id` and exact head sequence for both. Then fully recover derived
state and remove the intent durably. Folder moves preserve the identities and
histories of the complete descendant subtree.

Repository discovery resolves an interrupted intent before running the caller's
command. If the filesystem move happened but append did not, discovery appends
the exact persisted operation using its stable operation ID. If append happened,
discovery verifies the item and filesystem at the destination and removes the
intent. A malformed or contradictory intent fails closed. Intent recovery never
guesses an operation and has no overwrite fallback; automatic move recognition
belongs to the ordinary scanner described above.

### `synck deleted`

Print every currently trashed FILE and FOLDER, ordered by its last committed
path and then immutable item ID. Each row contains the item ID, item type,
deletion repository sequence, deletion commit ID, deletion time, and last path.
The command is read-only. It reports the latest trash transition for an item;
an item restored by a later commit is not listed.

### `synck recover <path-or-item-id> -m <message>`

Require a non-empty message, a trashed item, and a clean working tree. The
argument is either a canonical item ID or the last path of exactly one trashed
item, as listed by `synck deleted`; an ambiguous path is rejected and names the
item ID form. Every recovered path must lie inside the checkout selection. The selected item's parent must be LIVE or the repository root. Recover a
FILE alone. Recovering a FOLDER also recovers only descendants trashed by the
same commit as that folder, leaving descendants deleted by an earlier commit in
trash. Emit RESTORE events parent-first in one commit, preserving every item ID
and its complete history.

Before append, verify every required object and reject a live portable-name
conflict or any existing filesystem entry at a recovery path. Write and sync
one strict `.synck/recover.json`
intent containing the exact operation ID, items, paths, base sequences, and
FILE content heads. Rematerialize the typed restore and append through
`Repo::append_file_folder_checked` with its exact subtree guard, fully recover
derived state, then recreate folders and exact committed FILE bytes without
overwriting. Sync each published entry and its parent. Remove the intent
durably only after the complete recovered tree is materialized and its worktree
identities are refreshed.

Repository discovery completes an interrupted recovery before running the
caller's command. It distinguishes the all-TRASH pre-append state from the
all-LIVE accepted state using the exact persisted item sequences. A partially
published directory is resumed; an already published FILE is accepted only if
its size and SHA-256 match the intent. Mixed, malformed, conflicting, or
otherwise contradictory state fails closed. There is no overwrite or guessed
recovery path.

All status, show, log, history, compare, diff selection, and JSON projection
keep two sets separate: item IDs whose own state changed, and unchanged ancestor
IDs loaded only to resolve complete paths. An ancestor used as path context does
not acquire a change row. In particular, adding or deleting a child cannot
report its still-LIVE parent folder as added or deleted. A folder row exists
only when that folder's own kind, location, or lifecycle changed. Moving an item
between two existing folders remains one `MV` row when its name and content are
unchanged; both unchanged parents remain path context only.

The Rust history model carries `item_type` on every semantic change. Canonical
event rows retain their raw owning `item_id` and payload, and also carry a
commit-boundary display subject: visible FILE/FOLDER identity, type, name,
previous name, and parent names. Frozen FILE_VERSION_SET events remain
verifiably sidecar events, while new `FILE_VERSION::*` ref events directly name
their owning FILE.
Library presentation consumers may use `show_event_page` with an exclusive
`commit_event_idx` cursor. It reads and projects only the requested immutable
commit page, verifies each returned event's envelope, canonical payload, and
payload digest, and returns the total event count plus the next cursor. The
complete `show_events` path additionally verifies the aggregate commit event
digest and remains the fsck/plumbing authority.

### `synck log [-n <n>|--limit <n>] [--oneline|--events]`

Print sequenced commits newest-first. Human-readable output starts each entry
with repository sequence, local acceptance time including its numeric UTC
offset, author, and exact semantic change count, separated by ` | `. The
repository sequence is bold when terminal styling is enabled. The optional
message follows on the same line with CR/LF replaced by spaces. Delta rows begin
on the next line without an empty separator. Commits are separated by one empty
line. The delta contains at most the first 100 deterministic knowledge-work
change rows using the same rows as `show`; when truncated, the header reports
the number shown and a final instruction directs the caller to
`synck show <repo-seq>`. Folder relocations remain one folder operation;
unchanged descendants are not expanded. Commit IDs do not clutter the browsing
view; use `show` for one commit or `--json` for complete identities.

`--oneline` prints the same sequence, time, and author fields followed by the
optional message on one physical line and omits all delta rows and counts. It
replaces message CR/LF characters with spaces. The default and maximum limits
are fixed constants, not configuration. The limit counts commits, not
repository sequence values; valid sequence gaps do not reduce the result count.

JSON retains its fixed schema, UTC timestamp, complete commit ID, original
message, at most the first 100 `changes`, and explicit `changes_total`,
`changes_shown`, and `changes_truncated` fields. The array is complete exactly
when `changes_truncated` is false; callers never infer completeness from its
length. `--oneline` affects only human-readable rendering and does not remove
fields from JSON.

`--events` and `--oneline` are mutually exclusive. `--events` replaces semantic
rows with the commit's complete authoritative event list in canonical
`commit_event_idx` order. Its header reports both semantic changes and stored
events. Each human event row prints the zero-based event index, exact event type,
item ID, item sequence, and canonical JSON payload. It proves completeness
against the commit's declared event count and fails with an integrity error on
missing, extra, reordered, noncanonical, or digest-mismatched events. Normal
`log` does not read event payloads.

With `--events --json`, each otherwise ordinary commit object additionally has
exact `event_count`, commit-level `events_digest`, and complete `events`. Every
event contains `repo_id`, `commit_id`, `repo_seq`, `commit_event_idx`, `item_id`,
`item_seq`, `event_type`, canonical `payload`, `event_payload_digest`,
`created_at`, `parent_folder_id`, and `parent_folder_id_before`. There is no
event truncation inside an accepted FILE/FOLDER commit. One `log --events`
selection is bounded to 10,000 aggregate events; larger selections fail before
reading payloads and direct the caller to reduce `--limit` or use
`show --events` for one commit.

### `synck show <revision> [--events]`

Resolve one accepted revision. Print its repository sequence, complete commit
ID, UTC acceptance time, author, and message, followed by its deterministic
knowledge-work delta using the same rows as `status`. Initial FILE content is
part of its `A` row, not a redundant `M` row. A trash/create pair at one path
is one `D` row for the old identity followed by one `A` row for the new
identity. The command is read-only and rejects malformed or unknown revisions.
`--events` replaces the semantic rows with the same complete authoritative
event representation as `log --events`; the human header reports both counts
and retains the complete commit ID. `--events --json` adds `event_count`,
`events_digest`, and `events` to the otherwise ordinary commit object.

### `synck history <path>`

Interpret the path relative to the caller and require one currently LIVE FILE
or FOLDER. Resolve the item once by its immutable `item_id`, then print every
sequenced commit that created, changed, moved, or deleted that item in ascending
repository-sequence order, using the same summary-plus-delta form as `show`.
History follows the selected immutable item across its own renames and moves;
an ancestor-only relocation changes the displayed path but does not invent a
change event for the descendant. Initial FILE content is coalesced into its `A`
row. The command is read-only; V1 does not resolve a historical path that is no
longer LIVE.

`show` reconstructs its commit-boundary paths from the selected historical root.
`history` continues to follow one item's lifecycle-event provenance through its
per-item event stream. Malformed structural history or state fails as an
integrity error rather than producing a guessed path.

### `synck compare <from-revision> <to-revision>`

Resolve both accepted state roots and print their deterministic structural delta
using the same rows as `status`. Match items by immutable identity, so a folder
relocation is one folder relocation row rather than fake rows for every
descendant. A descendant whose bytes also changed receives its own `M` row at
the destination path. Comparison is directional, read-only, and
works in either chronological order. Equal Merkle subtrees are not descended.

### `synck diff [<from-revision> <to-revision>]`

With no revisions, compare current accepted FILE content (empty before the
first commit) with the working tree. With exactly two revisions, compare their
accepted FILE content. One revision is invalid. Emit ordinary unified patches
for changed files only when every
present side is valid UTF-8 without NUL characters. Additions and deletions use
`/dev/null`; moves with content changes retain old and new paths. Identical
content and folder-only changes produce no text patch.

For non-text content, print one deterministic `Binary files ... differ` row
without interpreting bytes. A present side larger than the fixed 8 MiB per-file
limit prints one explicit skipped row instead of running the diff algorithm.
Historical inputs are read from size/digest-verified objects. Working files are
opened without following links and must still match the size and SHA-256 seen
by the scanner after the read. A working-tree diff scan may refresh the derived
identity cache. Apart from the general recovery and projection catch-up above,
the comparison appends no history and writes no objects, repository
configuration, or working-tree files. There is no format-aware DOCX, PDF,
image, email, or line-ending conversion in the VCS.

Two-revision historical `diff` resolves both state roots and skips equal Merkle
subtrees. It does not replay earlier repository history after state projection
has caught up.

Native embedders may call `Repository::diff_worktree_selected(caller, inputs)`
to apply the same path-selection rules used by selected commit before reading
working bytes. Empty `inputs` selects the complete delta. A nonempty selection
returns only FILE diffs within those exact file or recursive folder scopes and
retains the same validation, ordering, binary, size, and concurrent-read
contracts as the complete working-tree diff. This is a bounded embedding API;
it adds no CLI form and grants no caller authority outside the discovered
repository.

### `synck versions <path>`

Interpret the path relative to the caller's current directory, require it to
resolve to one currently LIVE FILE item, and print its blob-bearing
`FILE::SET_CONTENT` events in item-sequence order.

### `synck version ...`

Named-version commands use the FILE's attached ref ledger. They advance
`ref_head_seq` and never advance the FILE's ordinary `head_seq`:

```text
synck version list <path> [--all]
synck version create <path> [--item-seq <n>] [--major]
    [--description <text>] -m <message>
synck version set-description <path> <version-id>
    --description <text> -m <message>
synck version promote <path> <version-id> -m <message>
synck version demote <path> <version-id> -m <message>
synck version tombstone <path> <version-id> -m <message>
synck version restore <path> <version-id>
```

`list` prints LIVE labels in `(order_key, creation sequence, version_id)` order.
`--all` also prints permanently tombstoned identities without assigning them a
current display label. `create` labels the current FILE content head unless an
exact historical content-bearing item sequence is supplied. It refuses a
second LIVE label for the same FILE revision. The default kind is `MINOR`;
`--major` selects `MAJOR`. Descriptions are named-version metadata and are
separate from the required commit context message.

Set-description, promote, demote, and tombstone each append one auditable ref
event. An empty description clears it. Tombstone does not delete FILE history
or objects and cannot be undone by `synck revert`.

Named-version restore is a working-tree operation: it replaces the live FILE's
bytes with the selected LIVE label's exact object, appends no event, and leaves
an ordinary modification for a later `synck commit`. Tombstoned versions cannot
be mutated or restored.

### `synck restore <path> [--item-seq <n>]`

Restore the current committed FILE bytes or the exact historical blob-bearing
`FILE::SET_CONTENT` event. This command is explicitly
destructive: it atomically
replaces an existing regular file or recreates a missing tracked file even when
the existing bytes are dirty. It rejects a directory, link, reparse point, or
other unsupported target. It syncs the file and parent directory and never
creates a commit. Restoring an older revision leaves an ordinary working-tree
modification.

### `synck restore-tree`

Discard the complete canonical working-tree delta and reproduce current accepted
state without appending a commit. This covers uncommitted additions, content
changes, moves and renames, deletions, empty folders, and same-path FILE/FOLDER
delete and add. An initialized repository with no accepted commit has an empty
baseline, so the command removes its non-ignored additions. A clean tree prints
`nothing to restore`.

Synck records the complete source and accepted target in strict
`worktree-restore.json`, stages affected current entries deepest-first below
`worktree-restore-stage/`, and publishes accepted folders and files
parents-first. This handles path swaps and same-path delete and add without overwrite;
discovery resumes the exact operation after a crash. Ignored entries remain
outside Synck authority. The command leaves them untouched and rejects an
affected folder or ignore-file change that would move, expose, overwrite, or
remove ignored bytes. Unsupported entries and namespace collisions fail before
publication.

### `synck export <path> <destination> [--item-seq <n>]`

Export the current committed FILE bytes or the exact historical blob-bearing
`FILE::SET_CONTENT` event selected by `restore`. Resolve the
tracked path and a relative destination from the caller. An absolute
destination is allowed. The selected item must be a currently LIVE FILE.

Export is non-destructive and appends no commit. Require a nonexistent
destination below an existing real directory whose resolved path is outside
`.synck/`. Reject existing entries, links, reparse points, metadata aliases,
and invalid historical item sequences. Verify the object size and SHA-256 while
copying it into a unique sibling temporary file, sync the file, publish without
replacement, and sync the destination parent. Failure leaves the destination
absent or complete and removes an unpublished temporary file.

An export outside the working tree does not change `status`. An unignored
export inside the working tree is an ordinary untracked addition. Export never
writes file bytes to standard output and performs no format-aware conversion or
comparison.

### `synck export-tree <revision> <destination> [--folder <path>]`

Resolve the complete live FILE/FOLDER tree at one accepted commit. Without
`--folder`, export the repository tree. With `--folder`, resolve that folder in
the selected historical tree—even if it was later moved or deleted—and export
its descendants relative to the destination root. The destination must not
exist and its existing real parent must be outside `.synck/`.

Export-tree is a read-only root-backed materialization, not an archive. It contains usable
files and empty folders but no `.synck`, SQLite, objects, intents, or
repository history. Verify every historical object while building a
unique sibling temporary directory, sync the complete tree, publish it with a
no-replace rename, and sync the destination parent. A failure before that rename
leaves the requested destination absent. Export changes neither history nor
the working tree.

### `synck fsck`

Run the complete-repository verification defined in [Portable
repositories](portable-repository.md) on the repository containing the current
directory, then the named-version and working-tree checks below.
`synck repository verify [<path>]` runs the same checks on a repository named
by path. On success print the verification report as one JSON object:

```json
{"repo_id":"<UUIDv7>","terminal_repo_seq":4,"commits":4,"items":3,"blobs":3,"bodies":0}
```

Verification covers SQLite physical integrity and the exact schema; Core
semantics, the local event profile and projection fidelity for every commit and
item family; the checkpoint chain; and the complete retained-history
object closure in the recorded storage mode. `WHOLE_FILE` verifies every
referenced object file's size and SHA-256. `CHUNKED` verifies the exact
object-database format and identity, every stored chunk, and every complete
logical blob by reconstruction. Verification never repairs authoritative rows
or object bytes, and it stops at the first failure, which it reports as an
`INTEGRITY` error with exit status 2.

Named-version verification replays attached ref events in repository order. Each create or content replacement must reference a
FILE revision that already exists at that event position and must agree with its
commit coordinates, object, and size. LIVE source-revision uniqueness is
enforced at every transition, not only in the final projection, and create
operation IDs are unique across the whole repository. Named versions are
checked whenever a repository with a checkout is verified.

`synck fsck --rebuild-state-index` selects explicit derived-index repair. It
removes the head first, deletes roots and nodes in bounded batches, installs the
complete sequence-zero closure, and reprojects accepted history. It changes no
commit, event, item, object, sequence allocation, or working-tree byte.

### `synck storage`

Complete pending recovery and verify the repository, collect every object
referenced by accepted history, and inventory the authoritative physical store.
`WHOLE_FILE` scans the flat `objects/` directory without following links or
reparse points; every name must be a lowercase 64-character SHA-256. `CHUNKED`
inventories complete logical blob rows; incomplete rows are not objects. Print
exactly:

```text
referenced_objects <count>
referenced_bytes <bytes>
unreachable_objects <count>
unreachable_bytes <bytes>
```

Counts and byte totals are base-10 integers. Bytes are logical object lengths;
the report excludes compression, SQLite, configuration, filesystem allocation,
and working-tree files. The command changes nothing beyond ordinary pending
derived-state recovery.

### `synck storage optimize`

Convert a `WHOLE_FILE` repository to `CHUNKED` in place, as defined in
[Portable repositories](portable-repository.md#physical-storage-optimization).
The command works with or without a checkout, does not accept `--json`, and
prints `storage optimized` or, for a repository that is already chunked after
verification, `storage already optimized`.

### `synck gc`

Refuse to run while an operation intent is pending. Verify the repository, then
remove only stored objects that accepted history does not reference.
`WHOLE_FILE` removes unreferenced regular files from `objects/` and syncs the
directory. `CHUNKED` first verifies and removes any whole-file leftovers of an
optimization, then marks at most 1,024 unreachable logical blobs incomplete,
removes each blob's mappings in transactions of at most 1,024 rows, then removes
the mapping-free blob rows. Startup recovery resumes this same bounded deletion
for every incomplete blob. It next deletes chunks referenced by no remaining
blob using transactions of at most 1,024 chunks or 8 MiB of payload,
checkpoints the WAL, runs incremental vacuum, closes every handle, and syncs
the database and metadata directory. Print exactly:

```text
removed_objects <count>
removed_bytes <bytes>
```

Historical, trashed, non-head FILE revisions, message bodies and named-version
content remain referenced and are never removed. Counts and bytes are removed
logical objects, not physical allocation reclaimed from shared chunks. A
stopped run may leave any subset of unreachable objects; rerunning converges
without touching referenced objects. GC is invoked only by this explicit
command. There is no background invocation, age or retention policy, automatic
commit hook, dry-run option, or GC trash; `synck storage` is the non-mutating
preview.

### `synck archive <destination-directory> [--storage-mode WHOLE_FILE|CHUNKED]`

Write a verified complete copy of the repository containing the current
directory into a new destination; `synck repository archive <destination>
--path <repository>` is the same operation for a repository named by path.
Copies are defined in [Portable repositories](portable-repository.md): they
require an absent destination, reject a pending checkout-selection transition
or nonempty `.synck/tmp/`, contain committed state only, keep the source's
storage mode unless `--storage-mode` converts the private copy, strip
filesystem tracking and local activation, and reopen as bare followers.
Uncommitted working-tree bytes are never copied. To continue work from a copy,
enable a checkout with `synck repository checkout`.

For copies, successful no-replace rename is the publication point. If syncing
the parent directory then fails, return `PublishedNeedsDurability` with the
operation and published destination instead of ordinary `Io`. The completed
directory remains visible, but its survival across a crash is not confirmed;
callers must not report that nothing was created or blindly retry the same
destination. Failures before rename leave the destination absent.

## 11. Verification boundary

Core provides a pure incremental semantic verifier in `synck-vcs`, independent
of its storage backend. The included SQLite adapter rejects malformed physical
fields, then feeds one decoded commit and its ordered events at a time, followed
by materialized items. Memory is bounded by one commit's events plus per-item
state; it does not collect complete event history.

Backend adapters retain their own SQL, transaction snapshots, physical type and
schema checks, and error formatting. `synck-local` retains local event-profile,
projection, named-version and object verification. There is no generic
adapter registry or repair framework.
