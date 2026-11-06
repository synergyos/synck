# Changelog

Format numbers are listed with each release that changes one. Before 1.0 a release
reads only its own repository format; see [Format and compatibility](docs/formats.md).

## 0.3.0

Repository format 7, commit format 6, object frame V1, history bundle format 5,
Core SQLite schema 7, as in 0.2.0: 0.2.0 repositories open unchanged. Object
store version 2: 0.3.0 upgrades a version 1 store when it opens one, after which
0.2.0 refuses it, as it refuses the stores 0.3.0 creates.

### CLI

- `init` and `commit` rename files and folders whose names break the portable rules,
  NFD names included, and report each rename; they no longer refuse them.
  `--fix-names` is no longer needed, and is accepted and ignored.
- `--json` works with every command. `restore`, `restore-tree`, `export`,
  `export-tree`, `recover`, `mv`, `gc`, `archive`, `init`, `storage optimize`,
  `instructions` and the `version` writes refused it before, although every
  command's help listed it.
- `repository save-file` and `duplicate`, `item show`, `export` and `list`, and
  every `--parent`, `--link` and `--merged-from` accept a repository path such as
  `Matters/brief.docx` as well as an item ID. Names match by their canonical key,
  so case and Unicode normalization do not matter.
- `synck clone <repository> <directory>` makes another working tree of a local
  repository, reading its history and stored content in place; `--include`
  checks out part of the tree. Commits from every working tree go into the
  origin's history. `synck pull` brings in the others' commits and keeps local
  changes; `commit`, `revert`, `mv`, `recover` and `version restore` pull first.
  A file changed on both sides is a `CONFLICT` with its `paths`, and nothing is
  written. `checkout.json` records each working tree's `baseline` commit, and
  `committing`, the operation of a commit made at HEAD, until the commit is
  its baseline: a working tree killed in between is based on its own commit
  at the next command, not left behind it.
- `synck duplicate <source> <destination>` copies a committed file or folder as
  an alternative in the same history, each copy recording `derived_from`.
  `synck adopt <alternative> <original>` makes the original match it in one
  commit, keeping the original's items: changed files record `merged_from`.
- `synck pull` merges files changed both here and in history: text three-way
  with conflict markers, anything else kept as yours with the repository's
  version beside it as `<name>.~theirs.<ext>`. `status` lists conflicts
  (`conflicts` in JSON when present), `commit` refuses until `synck resolve`
  marks each fixed, and `pull` reports them. A commit that meets such a file
  still stops with `CONFLICT`, writing nothing. Non-content changes on both
  sides fail with `CONFLICT` as before.
- Reservations (`synck lock`) live in the origin and hold in every clone: a
  commit touching a reserved path from any working tree needs its token, while
  `pull` and other working-file rewrites answer only to their own tree's
  reservations. `status` lists each holder and expiry (`reservations` in JSON
  when any exist). The reservation database migrates to schema 2.
- `synck clone --hold` keeps commits in the clone until `synck push`, which
  pulls and then sends them in order, each with the commit ID it was given;
  `status` lists them. A held clone keeps its own copy of the history; the
  origin's `gc` keeps the content of unpushed commits. Held commits are
  proposals: the held commits before the first one that changes an item the
  origin changed too stay held with their IDs; `pull` withdraws that one and
  every later one, and their changes go back into the working files, merged
  with the origin's, to be committed again (`unwound` and `withdrawn` in
  `pull --json`); `push` does the same and stops with `CONFLICT`. `log` shows
  `held` in place of a held commit's sequence (`"held": true` in JSON).
- A withdrawn commit is kept as a withdrawn save with its commit ID, labelled
  `w1`, `w2`, …, in a held or HTTPS clone's `.synck/withdrawn.json`.
  `synck withdrawn` lists them, and `show`, `export` and `discard` (or `--all`)
  take a label or a commit ID. They are kept for 90 days, protected from `gc`
  (through an optional `withdrawn` array in the origin's pins, or the HTTPS
  clone's own store) and checked by `fsck`; `gc` then removes them and names
  them (`expired_withdrawn`). `status` counts them and `show` of a withdrawn
  commit ID names its label.
- `info` starts with `origin` or `clone of <origin>`, and a clone's `status`
  with the same line and the commits waiting to be sent. A clone's `info` and
  `status` JSON carry `origin`, and `held_commits` when it can hold commits.
- `synck diff` compares Word, Excel and PowerPoint files by their text:
  paragraphs, slide text and sheet cells with formulas, instead of reporting
  a binary difference. JSON marks these entries `DOCUMENT`.
- `restore-tree <revision>` writes an earlier commit's tree into the working
  tree. `status` shows it as changes against HEAD; `restore-tree` returns, and
  `commit` records a rollback that keeps every moved and edited item's identity.
- The filesystem monitor answers at once on macOS and Linux. Its server checked
  for a connection only every 25 ms and both sides polled the socket every 2 ms;
  `status` takes about 35 ms instead of 70.
- With the filesystem monitor running, `status` and `commit` missed a change
  inside a folder that has a sibling sorting between the folder and its
  contents, such as `Matter` beside `Matter.pdf` or `Matter-old`: a deleted
  file stayed out of `status` and was left out of the commit.
- A commit can move files or folders out of a folder it deletes. It failed
  with `CONFLICTING_INTENT`; now the moved items stay live and only the rest
  goes to the trash.
- `log`, `show`, `history` and `diff` between revisions look up file contents
  by index. SQLite scanned every event of the repository for each batch of 256
  files, so their time grew with history length.
- `fsck` checks metadata objects over one database connection instead of
  opening and validating one per object. `diff`, `export-tree` and
  `restore-tree` read stored files over one connection instead of one per file.
- `--json log --oneline` returns commit metadata without change lists, as
  `--oneline` does without `--json`.
- Commands started together queue for the repository instead of failing: the
  CLI waits up to 30 seconds for the lock before returning `BUSY`. Agents running
  `status`, `diff` or `log` side by side got `BUSY` at once. The library still
  returns `BUSY` without waiting.
- The filesystem monitor runs in checkouts deeper than a Unix socket address
  (about 104 bytes): its socket moves to the user's private temporary directory
  on macOS, or `XDG_RUNTIME_DIR` on Linux. Without it every `status` scanned the
  whole tree. A monitor whose socket is removed now exits, and the next command
  starts another.
- A symbolic link, or another entry that is neither a file nor a folder, no
  longer stops `init`, `status` and `commit` for the whole tree. Untracked ones
  are skipped and listed (`skipped` in JSON); a tracked path that becomes one is
  still refused. The local database records them in schema 16; a version 15
  database gains the table when the repository opens.

### Storage

- The local database (`repository.local.sqlite3`) is about 45% smaller: rows no
  longer repeat the repository ID, keyed tables are `WITHOUT ROWID`, and the
  unused object digest table is gone. Local schema 15; a version 14 database is
  migrated in place when the repository opens, keeping every row.
- A commit flushes the drive about 6 times instead of 39, and importing 200 files
  about 55 times instead of 1063. Repository databases keep their WAL between
  commands instead of checkpointing on every close; object staging and the
  disposable local database no longer flush each transaction, and one checkpoint
  before the history transaction makes a commit's objects durable.
- Importing 906 MB of files takes 2.9 s instead of 24.9 s. A commit captures files
  on two threads, compresses chunks at zstd level 1 instead of 6, and lets the
  object store's WAL reach 1 GiB before an automatic checkpoint. New object stores
  use 32 KiB pages, which 0.2.0 refuses to open; 4 KiB stores still open.
- `export-tree` of 906 MB takes 1.7 s instead of 3.6 s. It hashes each byte while
  copying instead of again from disk afterwards, and on macOS flushes the drive
  twice instead of once per file and folder.
- The historical-state cache uses WAL without a flush per transaction, so catching
  it up after 20 commits flushes the drive about 20 times instead of 280.
- A command keeps an idle connection to the history and local databases, as it
  already did to the object store. A connection opened alone rebuilt the WAL index
  from the whole WAL and forgot what had been checkpointed, so past 1000 pages
  every transaction copied the whole WAL again and the WAL never restarted. A small
  commit flushes the drive 6 times instead of 14.
- On macOS, `repository checkout`, `restore-tree`, `revert` and `recover` order each
  working file and folder they write with a barrier instead of a full flush, and
  the operation's last full flush makes them durable. Checking out 300 files
  flushes the drive 22 times instead of 789. `restore`, `export` and version
  restores end with one full flush.
- A FILE's plaintext digest is its object ID. Checkout and status no longer reread
  and hash every object to learn it, and capture no longer writes it to the local
  database. The table that cached it is unused.
- Office files are chunked per ZIP member, with a large member's local header apart
  from its payload, so an edit stores only the members it changed.
- A commit stores a changed chunk as a Zstd delta against the chunk in the same
  place in the file's previous version, with chains of at most 24 deltas.
  Object store version 2 records each delta's base in `chunk_bases`, and `gc`
  keeps a base while a delta needs it. Replaying git.git's 311 release tags, the
  object store holds 26.7 MB instead of 135.7 MB; a commit takes about 17% longer
  and `fsck` 4.0 s instead of 2.2 s.

### Format

- Encryption scheme 2 is XChaCha20-Poly1305 in 64 KiB segments, so a large
  encrypted file is written and read a segment at a time instead of whole. Its
  frame is checked without a key from the header and length alone;
  `EncryptedFrameCheck` does that as bytes stream past. Vectors:
  `encrypted.json` and `encrypted-segments.json`.

### Sync server

- `synck serve` serves repositories to other devices and agents over protocol 1,
  with named read and write tokens (`synck serve init`, `synck serve token`). It
  listens on loopback; `deploy/Caddyfile` and `deploy/synck-serve.service` put
  Caddy in front for TLS with an automatic certificate. See
  [the sync server](docs/sync-server.md).
- `synck clone https://HOST/v1/repositories/REPO_ID` makes an HTTPS clone with
  the token in `SYNCK_TOKEN`: the server's verified history and content, kept
  locally. Commits are sent at once, or held while the server cannot be
  reached and sent by `push`; `--hold` keeps them until `push`. `lock` uses
  the server's leases. New error kinds `UNREACHABLE` and `SERVER`. See
  [HTTPS clones](docs/local-repository.md).
- Metadata objects travel through the object routes: a proposal needs every
  one it names, and `GET /objects` serves them from history.
  `SqliteStorage` gains `metadata_object` and `put_metadata_objects`.
- Object packs: `PUT /objects` uploads up to 512 objects in one request and
  `POST /objects` downloads them, up to 64 MiB per answer. `Capabilities`
  gains `object_packs` (absent means false); HTTPS clones use packs where it
  is true and one request per object elsewhere. `ObjectRequest` is new, and
  `SqliteStorage` gains `metadata_objects`. A clone of 3,100 small files fell
  from 47 s to 11–16 s and a push of 600 from 15 s to about 7 s (debug
  build, macOS, loopback): objects written for a later checkpoint are ordered
  with a barrier instead of each fully flushed, a pack is read through one
  connection per database, and an object store checks its schema at its
  first connection, not at each.
- `portable::ServedRepository` opens a repository for a sync server without
  taking its lock and reports its protocol-1 `Capabilities`. `SqliteStorage`
  gains `history_identity`, `history_head` and `history_cursor`.
- `ServedRepository` stores and streams objects and accepts a verified
  protocol-1 proposal with the checks a local commit gets, waiting for the
  repository lock as a clone does. A retry returns the original commit; stale
  heads come back as a conflict, absent content as missing objects.
- `ServedRepository` serves packed history pages
  (`SqliteStorage::history_page`) and takes, renews, releases and breaks
  reservations for remote writers in the repository's own reservation table.
  A proposal that writes a reserved path needs the lease's token. Lease wire
  values are in `synck-sync`.

## 0.2.0

Repository format 7, commit format 6, object frame V1, history bundle format 5,
Core SQLite schema 7. Repositories from 0.1 are refused. Copy files across with
`export-tree`; history does not transfer.

### Format

- Items are tree items or records. Tree items have a name and a parent; records have
  neither, and may have a key and a link to a folder. EMAIL and CHAT are records.
- Linear history with no parent links: a checkpoint chain gives the order and binds
  the commit, its position and its acceptance time.
- `create_item.derived_from` and `merged_from` record the item revision an item came
  from or took in.
- Metadata and content objects have PLAIN, SALTED and ENCRYPTED forms, and bytes can
  be erased without changing a hash.
- Names are NFC, case folded and checked against Unicode 18.0.0, whatever the
  language's own tables say. `unicode-tables.json` ships the data.
- Hashed JSON is a strict subset of RFC 8785, with a fixed integer range and depth.
- History bundles end with a record count, tip and digest, so a truncated bundle is
  refused.
- History may hold a commit with no events, which changes nothing and keeps its
  position. Its digest hashes that position, so two of them never share a commit ID.
  No writer adds one.
- A FOLDER never federates. A link set in a commit must target a LIVE folder, even if
  it names the folder the record already had.
- A replica following a federation domain files through the domain: append refuses to
  set a link there, so a local write adds no link that refuses a partner's tombstone.
- Language-neutral vectors now cover names, JSON, objects, encryption, metadata,
  events, replay, state digests, checkpoints, bundles, labels, the sync wire and a
  whole repository, and an independent generator computes all of them.

### Engine

- `item show` prints a ref's payload as `payload_json`, a string, not a byte array.
- `RESERVED_WORKTREE_PATH` is gone. `.synck` is a reserved name, so it reports
  `INVALID_NAME`.
- Every write path goes through one set of admission rules.
- macOS writes flush the drive cache. Every SQLite connection refuses schema tricks
  from a repository it opens.
- Repository text is escaped before it reaches a terminal.
- A monitor exits when its repository disappears.

### Testing

- A crash matrix kills a real `synck` at each filesystem mutation it makes through
  libc, which on macOS is all of them.
- Every format read from untrusted input has a fuzz target and a mutation test.

## 0.1.1

Repository format 6.

## 0.1.0

First release.
