# Changelog

Format numbers are listed with each release that changes one. Before 1.0 a release
reads only its own repository format; see [Format and compatibility](docs/formats.md).

## Unreleased

### CLI

- `--json` works with every command. `restore`, `restore-tree`, `export`,
  `export-tree`, `recover`, `mv`, `gc`, `archive`, `init`, `storage optimize`,
  `instructions` and the `version` writes refused it before, although every
  command's help listed it.

### Storage

- A commit flushes the drive about 6 times instead of 39, and importing 200 files
  about 55 times instead of 1063. Repository databases keep their WAL between
  commands instead of checkpointing on every close; object staging and the
  disposable local database no longer flush each transaction, and one checkpoint
  before the history transaction makes a commit's objects durable.
- Importing 906 MB of files takes 3.0 s instead of 24.9 s. A commit captures files
  on two threads, compresses chunks at zstd level 1 instead of 6, and lets the
  object store's WAL reach 1 GiB before an automatic checkpoint. New object stores
  use 64 KiB pages, which 0.2.0 refuses to open; 4 KiB stores still open.
- Office files are chunked per ZIP member, with a large member's local header apart
  from its payload, so an edit stores only the members it changed.

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
