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
A repository and every checkout of it use one spelling for each name. `init` and
`commit` rename a file or folder whose name breaks these rules, NFD names included,
to its deterministic portable spelling, and report every rename. Repair journals
recover interrupted renames without overwriting a newly occupied destination.
Non-UTF-8 names are refused rather than guessed.

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
destination.

`init` initializes repository metadata and then repairs non-portable names; as with
ordinary initialization, existing files remain uncommitted. A `commit` whose scan
finds non-portable names repairs them throughout the observed checkout, remaps any
selected paths, and records the selected changes in one ordinary commit. Both report
every rename on stderr, and their JSON results include `renamed`. Ignored entries
are never renamed or overwritten. `--fix-names`, which 0.2.0 required, is still
accepted and changes nothing.

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

Reservations live in the origin's `.synck/agent-locks.sqlite3`, separate from
disposable projection caches and canonical history, so they hold in every clone:
a commit, move, revert, recovery, version restore or push from any working tree
that touches a reserved path needs its token. Each reservation records the
working tree that took it, and commands that only rewrite working files —
`pull`, `restore`, `restore-tree`, checkout selection, name repair and exports
into the tree — answer only to their own tree's reservations, so one agent's
lease never stops another clone from pulling the holder's commits. `status`
lists every reservation with its holder and expiry. Cache rebuilding preserves
them; archives and repository imports omit them. Do not delete this database to
clear locks. Leases are not history and are not synchronized to any server.

## Clones: one working tree per agent or person

`synck clone <repository> <directory>` makes another working tree of a
repository on this machine. It reads the origin's history and stored content in
place, so it costs only its working files; `--include PATH` (repeatable) checks
out only those files and folders, and `--author NAME` names its commits.

```sh
synck clone /work/matter /work/matter-agent-a --author agent-a --include Drafts
cd /work/matter-agent-a
synck commit -m "Revise the brief"   # lands in the origin's history at once
synck pull                           # bring in everyone else's commits
```

Every working tree, the origin's included, is based on one commit, recorded in
`checkout.json` as its baseline. `status` and `diff` compare with the baseline,
so commits made from other working trees never look like local edits. `pull`
writes what changed since the baseline to every file without a local change,
keeps local changes, and then bases the tree on HEAD. `commit`, `revert`, `mv`,
`recover` and `version restore` pull first, so commits that touch different
files land from every working tree with no extra step; one that meets a file
changed both here and in history stops with `CONFLICT` and the paths, having
written nothing. `pull --include PATH` then changes the checked-out paths.

### Changed on both sides

`synck pull` merges a file whose content changed both here and in history:

- Text is merged three-way from the baseline's version. Where both sides
  changed the same lines, the file gets markers:

  ```text
  <<<<<<< ours
  this working tree's lines
  =======
  the repository's lines
  >>>>>>> theirs
  ```

- Anything else keeps this tree's version and writes the repository's beside
  it as `<name>.~theirs.<ext>`, for example `plan.~theirs.docx`.

A clean merge is an ordinary change; review it and commit. The others are
conflicts: `status` lists them and `commit` refuses until each is resolved.
Edit the file (remove every marker, or keep the version you want), delete the
`~theirs` copy or keep it as a new file, then run `synck resolve PATH...`.
`restore` and `restore-tree` drop a conflict along with the merged file.
Changes that are not content, such as a move against an edit or a deletion
against an edit, are not merged: `pull` fails with `CONFLICT`, writes nothing,
and the change has to be made the same way here or undone with `restore`.
Prevent conflicts in files that cannot merge with a reservation.

A clone holds the origin's repository lock for each command, waiting for the
other working trees' commands rather than returning `BUSY`. The origin must be on
a local disk: `clone` refuses SMB, NFS, AFP and WebDAV, where SQLite cannot lock
safely. Item and `repository` commands take the origin's path; `fsck` in a clone
verifies the origin's history, its stored content and the clone's files, and
`storage optimize` runs only in the origin. Moving or deleting the origin breaks
its clones.

### Held clones: private until pushed

`synck clone --hold` makes a clone whose commits stay in it until `synck push`:
a draft of the real files, for instance one folder with `--include`, that
nobody else sees until it is ready. The clone keeps its own copy of the
origin's history (about 1.5 MB for a 900 MB repository) and stores content in
the origin, so `status`, `log`, `diff`, `restore` and `revert` work as in any
clone. `status` lists the held commits.

```sh
synck clone /work/matter /work/matter-draft --hold --include Drafts
cd /work/matter-draft
synck commit -m "First pass"       # held here
synck commit -m "Second pass"      # held here
synck push                         # both land in the origin, in order
```

`pull` takes in the origin's newer commits. `push` pulls, then appends the
held commits to the origin in order. A commit's ID does not depend on where it
lands, so each keeps the ID it was given when committed. Nothing changes on
either side when an item was changed both in a held commit and in the origin:
the push or pull fails with `CONFLICT` and the paths. Held commits are not
backed up and nobody else sees them until pushed. `gc` in the origin keeps
their content; delete a held clone only after pushing it.

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
commit. `restore-tree <revision>` discards every working change and writes that
commit's tree, leaving ignored entries alone. `status` then shows the earlier
tree as changes against HEAD, with moved and edited items keeping their
identities: plain `restore-tree` returns to HEAD, and `commit` records the
rollback. `revert` appends a compensating commit, preserving prior history. Durable
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
`checkout.json` and the recovery journals beside it are not disposable. A database's
`-wal` sidecar holds committed transactions between commands, so never copy a SQLite
main file without it: use SQLite's backup API or copy the quiescent `.synck` whole.
Delete a disposable database with its sidecars; a local database whose main file is
gone has its stale sidecars removed when it is rebuilt.

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
