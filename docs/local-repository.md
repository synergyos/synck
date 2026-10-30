# Local repositories and the CLI

The [format specification](spec/format.md) defines canonical storage. This guide
covers working-tree behavior; `synck instructions` and command `--help` provide
the executable's current command/argument reference.

## Initialize and record

```sh
synck init ./work --author "Alex"
synck status
synck commit -m "Record the documents"
```

Run working-tree commands inside the initialized directory. Initialization
records the default `.synckignore`; existing files remain uncommitted. Commit
records all visible changes or the selected paths atomically. There is no staging
area, batch command or fixed event-count limit. Renames/moves preserve item
identity where detected; a case-only rename follows the same namespace rules.

Names follow one portable rule set on every OS: NFC, at most 255 UTF-8 bytes,
Unicode case-folded sibling uniqueness, no reserved device stems or forbidden
characters, no code point unassigned in Unicode 18.0.0, and no trailing space/dot.
A file whose on-disk name is NFD is accepted: the repository stores NFC, the local
index remembers the spelling on disk, and the file is never renamed. New files are
written as NFC. Invalid-name errors list every proposed
repair. `init --fix-names` or `commit --fix-names` explicitly performs and reports
deterministic renames before recording. Without that flag, names are not changed.
Repair journals recover interrupted renames without overwriting a newly occupied
destination. Non-UTF-8 names are refused rather than guessed.

Hidden files are ignored by default except explicit control policy. The chosen
hidden-file setting lives in `.synck/checkout.json`, not scan cache. Ignore
rules and subtree selection are visibility policy, not erasure of history.
The `.synck` control directory and its aliases cannot be recorded as items, in
any folder.

Whole-path length is not a repository rule, because checkout locations vary by
machine. The Windows MSVC CLI embeds a `longPathAware` manifest and uses extended
absolute paths. Windows status and checkout commands warn on stderr for each path
whose ordinary absolute spelling exceeds 260 UTF-16 code units, excluding the
`\\?\` prefix. These warnings do not reject history or change names. Other
applications can have shorter limits.

## Name diagnostics and repair

Filesystem discovery applies ignore rules before name validation. It collects all
violations into one diagnostic, with original and proposed paths quoted so control
characters remain visible. Valid sibling names are reserved before invalid
spellings are repaired; equal valid case-fold keys keep the lexicographically first
spelling. Remaining conflicts receive ` (2)`, ` (3)`, and so on, with UTF-8
truncation before the suffix when needed. Suggestions also avoid existing ignored
entries, and a renamed ancestor is reflected in every reported descendant
destination. Diagnostics rename nothing and publish no repository when
initialization fails.

`init --fix-names` initializes repository metadata and then repairs non-portable
names; as with ordinary initialization, existing files remain uncommitted.
`commit --fix-names` repairs names throughout the observed checkout, remaps any
selected paths, and records the selected changes in one ordinary commit. Both report
every rename, and the JSON commit result includes `renamed`. Ignored entries are
never renamed or overwritten.

Repairs are local filesystem operations, not history rewrites. A local journal
records file and parent identities before mutation. Descendants move before
ancestors, through temporary sibling names, using no-replace renames. Each completed
rename is directory-synced before its progress record is synced. Interrupted repairs
resume on the next locked checkout open, checking identities and refusing an occupied
destination. Initialization publishes its metadata before repair, so an interruption
leaves recoverable state. A complete repository import preserves canonical names and
hashes; it does not repair or rewrite imported history.

## Local agents sharing a checkout

Reserve paths before editing or creating them. No server is required:

```sh
synck --json lock acquire src/existing.rs src/new.rs --owner agent-a --ttl 600
# Save result.token from the JSON response as TOKEN, then edit the reserved files.
synck --lock-token "$TOKEN" commit -m "Implement the change" src/existing.rs src/new.rs
synck lock release "$TOKEN"
```

Reservations cover files or entire folders, including paths that do not exist
and children of folders that do not exist. Names use the VCS's portable case
folding; parent and child reservations conflict. Multiple paths in one acquire
succeed or fail together. Acquisition requires clean tracked paths or absent
paths; existing untracked files, including ignored files under a requested
folder, must be resolved first. The owner is a diagnostic label. Only the random
token proves ownership; keep it private. Listing never reveals tokens.

Pass `--lock-token TOKEN` to every mutation during the edit, including
`repository create-file` and `repository save-file`. Repeat the flag for multiple
tokens. Credentialed writes require coverage for every affected path: a folder
reservation covers its descendants, but a child reservation does not authorize
moving or deleting its parent. Reserve both source and destination before a
move; reservations remain on their original paths until released. Use selected commits so an agent does not collect another
agent's unfinished changes. Agents may independently reserve and commit sibling
new files under the same new folder, including after another agent creates it.

`synck lock renew TOKEN --ttl 600` extends a live lease. TTL is 1–86400 seconds;
expiry is a UTC Unix timestamp. Renew before expiry. Expired reservations still
block writes: another agent can acquire a fresh token only after the path is
clean. Takeover must cover the entire old reservation: acquiring one child
cannot release its expired parent's protection of other children.
Old tokens cannot write, renew, or release a newer reservation. A write
with an expired, released or replaced token returns `LOCK_LOST`; a conflicting
reservation returns `LOCKED`. Neither error discards working files.

`lock list` includes expired reservations. To recover abandoned work, inspect
and preserve its files first, then explicitly `lock break PATH...` or release
its exact token. These commands remove reservations, never working files.
Do not automate breaking another agent's reservation. Revert, item recovery,
restore-tree, checkout selection and name repair conservatively require ownership
of all reservations because they can affect an entire tree.

These are cooperative locks enforced by Synck's checkout mutation APIs, including
exports into the same checkout. Ordinary editors, shell writes, other repositories
exporting into this checkout, and direct database access can bypass them. Every
agent must acquire before reading/editing and stop editing when its lease is lost.
For saves submitted through Synck, retain the `--base-content` check as well.
The short repository OS lock serializes commands; it is not held during editing.
Concurrent commands can return `BUSY`; retry after the other command finishes.

Reservations live in `.synck/agent-locks.sqlite3`, separate from disposable
projection caches and canonical history. Cache rebuilding preserves them;
archives and repository imports omit them. Do not delete this database to clear
locks. There is no canonical format change or server synchronization of leases.

## Content and imported email

FILE content is stored as its exact bytes, and its object ID is their SHA-256, so
`sha256sum` confirms a file and identical bytes are stored once. Captures stream
with bounded memory. CHUNKED is the CLI initialization default; WHOLE_FILE is
available with `--storage-mode`. Chunking uses FastCDC and, for Office files, ZIP
member boundaries; small inputs are stored directly when compression does not help.
File exports and restores return exact source bytes.

An `.eml` or `.msg` file in a working tree is an ordinary FILE. Committing never
converts it. `synck repository import-email` creates an EMAIL record, linked to a
folder with `--link`, and leaves the file alone. `synck item export` writes its
original source to disk. An email has no name: its
title is its subject unless set otherwise, and two emails may share a title. The
stored `title` exists only when the import names one. No provider acquisition client
ships in 0.2. Originals and extracted attachments are retained; Markdown/HTML rendering is
computed when read. EMAIL creation metadata records the parser version.

## Inspect, restore and undo

```sh
synck log --oneline
synck history notes.txt
synck versions notes.txt
synck show HEAD
synck diff
synck export notes.txt ./saved-notes.txt --item-seq 2
synck export-tree HEAD ./exported-tree
```

Commit log order is repository publication order. Item history follows stable
identity through moves/renames. A FILE content revision is distinct from a named
version label: labels point at FILE item sequences and do not store duplicate
content references. `version list/create/promote/demote/set-description/tombstone`
manages those labels. Tombstoning a label does not delete its FILE history. Each
new FILE gets its first label in the commit that creates it.

`restore` and `restore-tree` change working files; recording them requires a
commit. `revert` appends a compensating commit, preserving prior history. Durable
recovery intents retain the metadata and expected plaintext identities needed
to finish interrupted operations. Do not delete intents as if they were caches.
Export commands refuse existing destinations and publish validated output only.
`deleted` lists trash, and `recover` restores retained items through new history.

## Verification, caches and copies

`fsck` checks canonical history/checkpoints and objects. It reports missing
payloads as thin history; present invalid bytes are corruption. A cache cannot
prove that a repository is correct. `repository.local.sqlite3` holds checkout
projections and scan digests; `repository.state.sqlite3`
holds historical indexes. Missing caches are rebuilt under the repository lock,
without accepting working-file edits or changing commit identity. Existing
malformed databases fail explicitly rather than being silently reset. History
reads catch a valid but stale index up before using it, including in a normal
checkout; being behind canonical history is not an integrity failure. `fsck`
checks stored row validity before decoding checkpoints so malformed timestamps
are reported with the affected commit and field, also under `--json`.

`repository.local.sqlite3`, `repository.state.sqlite3`, their SQLite sidecars and
scan caches are disposable and outside the format. `agent-locks.sqlite3`,
`checkout.json` and recovery intents in `tmp/` are not disposable. Never copy only a
live SQLite main file: use SQLite's backup API or a quiescent, checkpointed snapshot.

The state cache holds `vcs_state_nodes`, `vcs_state_roots` and
`vcs_state_projection_heads`. Historical listings create and replay a missing cache
under the repository lock, and explicit index rebuilding repairs damaged contents
without modifying canonical history. `repository.sqlite3` has no `local_*` tables:
FILE-head projections, plaintext digests and filesystem observations live in
`repository.local.sqlite3`, whose schema version is an implementation detail. Local
transactions may read canonical history through a read-only attachment and cannot
write it. Acceptance and projection updates are separate transactions, and recovery
replays accepted history after an interrupted update. Changed working files are
never promoted into history during a rebuild.

`storage` reports physical usage, `storage optimize` changes supported physical
storage, and `gc` removes unreferenced content. Reachability includes all retained
revisions and ref events, not just current heads. These commands do not erase
referenced content or rewrite history.

`archive DEST` creates a verified complete portable copy, excluding working files
and caches. Missing referenced objects prevent a complete archive. Imported
copies and sparse checkout behavior are described in
[portable repositories](portable-repository.md). For direct save APIs, base
conflicts and accepted-write recovery, see [application integration](integration.md).
