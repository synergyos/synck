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
malformed databases fail explicitly rather than being silently reset.

`storage` reports physical usage, `storage optimize` changes supported physical
storage, and `gc` removes unreferenced content. Reachability includes all retained
revisions and ref events, not just current heads. These commands do not erase
referenced content or rewrite history.

`archive DEST` creates a verified complete portable copy, excluding working files
and caches. Missing referenced objects prevent a complete archive. Imported
copies and sparse checkout behavior are described in
[portable repositories](portable-repository.md). For direct save APIs, base
conflicts and accepted-write recovery, see [application integration](integration.md).
