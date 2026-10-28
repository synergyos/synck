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
characters, and no trailing space/dot. Invalid-name errors list every proposed
repair. `init --fix-names` or `commit --fix-names` explicitly performs and reports
deterministic renames before recording. Without that flag, names are not changed.
Repair journals recover interrupted renames without overwriting a newly occupied
destination. Non-UTF-8 names are refused rather than guessed.

Hidden files are ignored by default except explicit control policy. The chosen
hidden-file setting lives in checkout configuration, not scan cache. Ignore
rules and subtree selection are visibility policy, not erasure of history.
The root `.synck` control directory cannot be recorded as an ordinary item.
Windows path warnings identify paths over 260 UTF-16 units; length is not a
repository namespace rule because checkout locations vary by machine.

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

FILE content is a salted RAW object. Working-tree change detection uses plain
SHA-256 in disposable caches, so a fresh salt does not create a false edit.
Captures stream with bounded memory. CHUNKED is the CLI initialization default;
WHOLE_FILE is available with `--storage-mode`. Stored objects include their frame;
file exports and restores remove it and return exact source bytes.

Local `.eml`/`.msg` sources become EMAIL items named after the source filename
without its email extension. Originals and extracted attachments are retained;
Markdown/HTML rendering is computed when read. EMAIL creation metadata records
the parser version. Exact unchanged import-source bytes are recognized after
cache rebuilding; editing a source makes it visible again. Existing FILE items
cannot silently turn into EMAIL items by rename or content replacement.

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
manages those labels. Tombstoning a label does not delete its FILE history.

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
projections, scan digests and email-source observations; `repository.state.sqlite3`
holds historical indexes. Missing caches are rebuilt under the repository lock,
without accepting working-file edits or changing commit identity. Existing
malformed databases fail explicitly rather than being silently reset. History
reads catch a valid but stale index up before using it, including in a normal
checkout; being behind canonical history is not an integrity failure. `fsck`
checks stored row validity before decoding checkpoints so malformed timestamps
are reported with the affected commit and field, also under `--json`.

`storage` reports physical usage, `storage optimize` changes supported physical
storage, and `gc` removes unreferenced content. Reachability includes all retained
revisions and ref events, not just current heads. These commands do not erase
referenced content or rewrite history.

`archive DEST` creates a verified complete portable copy, excluding working files
and caches. Missing referenced objects prevent a complete archive. Imported
copies and sparse checkout behavior are described in
[portable repositories](portable-repository.md). For direct save APIs, base
conflicts and accepted-write recovery, see [application integration](integration.md).
