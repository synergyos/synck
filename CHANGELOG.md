# Changelog

Format numbers are listed with each release that changes one. Before 1.0 a release
reads only its own repository format; see [Format and compatibility](docs/formats.md).

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
- Language-neutral vectors now cover names, JSON, objects, encryption, metadata,
  events, replay, state digests, checkpoints, bundles, labels, the sync wire and a
  whole repository, and an independent generator computes all of them.

### Engine

- Every write path goes through one set of admission rules.
- macOS writes flush the drive cache. Every SQLite connection refuses schema tricks
  from a repository it opens.
- Repository text is escaped before it reaches a terminal.
- A monitor exits when its repository disappears.

### Testing

- A crash matrix kills a real `synck` at every filesystem mutation.
- Every format read from untrusted input has a fuzz target and a mutation test.

## 0.1.1

Repository format 6.

## 0.1.0

First release.
