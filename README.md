# Synck

[![CI](https://github.com/synergyos/synck/actions/workflows/public-ci.yml/badge.svg)](https://github.com/synergyos/synck/actions/workflows/public-ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE-APACHE)

**Version control for knowledge work.** Synck keeps documents, conversations,
email and structured records with their complete history, in a folder you own,
readable and verifiable without the service that produced it.

```console
$ synck init . --author "Alex"
initialized Synck repository 01a178e7-f433-7720-9811-70994ded0458
$ printf 'Meet at 10.\n' > notes.txt
$ synck commit -m "Record the meeting time"
committed sha256:712ee05929553b1c780640cf844212ed3f50443eb4d00dd9485774b5ebbbf7de
$ printf 'Meet at 11.\n' > notes.txt
$ synck commit -m "Move the meeting to 11"
committed sha256:eece1811e5309fb2d38c92650136c6457fb35c84828d862957c87b23e51fd713
$ synck log --oneline
3 | 2026-10-26 16:50 +01:00 | Alex | Move the meeting to 11
2 | 2026-10-26 16:50 +01:00 | Alex | Record the meeting time
1 | 2026-10-26 16:50 +01:00 | Alex | Initialize Synck repository
$ synck fsck
{"repo_id":"01a178e7-f433-7720-9811-70994ded0458","terminal_repo_seq":3,"commits":3,"items":2,"blobs":3,"bodies":0}
```

Synck is the open-source engine behind [SynckHub](https://synckhub.com). It is a
Rust library you can embed and a command-line tool, `synck`, for people and
AI agents.

**Documentation: [synckhub.com/synck](https://synckhub.com/synck)**: tutorials,
everyday use, internals and a full command reference.

`synck init` turns any folder into a repository. Existing files stay
uncommitted until you commit them, and a default `.synckignore` keeps
operating-system and editor clutter out. Saved `.eml` and `.msg` files are
committed as email items, chats live alongside your files, and content is stored
in compressed, deduplicated chunks.

## Why Synck

A project's value is not only in its final files. It is also in the earlier
drafts, the conversation where a figure was challenged, the task that recorded
an approval and the email that went to the client. Most tools keep that context
inside a hosted service: you can see it, but you do not really hold it.

Synck is built around one promise: **a complete copy of your data, with its
history, that you can read, verify and keep using on your own.** A complete
repository is one directory containing every retained version of every item and
every byte that history refers to. It opens with this tool alone: no account, no
server and no network.

## How it works

If you know Git, some of this will feel familiar and some will not. Each
difference is deliberate.

- **Items, not snapshots.** Synck versions *items*: files and folders, but also
  chats, email, tasks, calendars and other records. Every item has a permanent
  ID and its own ordered history, so a renamed or moved file is still the same
  file.
- **Append-only history.** Accepted changes are never rewritten. Undo adds a
  new change that reverses an old one.
- **No staging area.** Edit files with any application. `synck commit` records
  what changed: everything, or only the paths you name.
- **One writer, linear history.** Each item has one authority that accepts its
  changes in order. There are no branches or merges, and changes to different
  items never conflict.
- **Integrity throughout.** Commits, file versions and message bodies are
  identified by SHA-256. `synck fsck` checks the history and every stored byte.
- **Complete, self-contained copies.** Every repository holds its whole history
  and every byte that history refers to. `synck archive` writes a verified copy
  that opens with this tool alone.

[Items, events and commits](https://synckhub.com/synck/internals) and the
[system design](https://synckhub.com/synck/internals/system-design) explain the
model in depth, including its [trade-offs against Git, Lore, Perforce and plain
databases](https://synckhub.com/synck/internals/trade-offs).

## Install

Synck 0.1 is installed from source. You need Git,
[Rust through rustup](https://rustup.rs) (this repository pins the toolchain in
`rust-toolchain.toml`) and a C compiler: the Xcode Command Line Tools on macOS,
your distribution's build tools on Linux, or the Microsoft C++ Build Tools on
Windows.

```sh
git clone https://github.com/synergyos/synck.git
cd synck
cargo install --locked --path crates/synck-local
synck --version
```

Then follow [Installing Synck](https://synckhub.com/synck/getting-started/install)
and the [getting-started tutorials](https://synckhub.com/synck/getting-started).
`synck --help` lists every command, and `synck instructions` prints a compact
operating guide that also works well for AI agents.

## Open a repository someone gave you

A complete repository exported from SynckHub, or copied from someone else, can be
checked and read directly:

```sh
synck repository verify ./exported-repository
synck item list --path ./exported-repository
synck item show ITEM_ID --path ./exported-repository
synck item export FILE_ID ./report.pdf --path ./exported-repository
synck repository log ./exported-repository
```

Files, chats, email, posts, tasks, records, calendars, calls and time tracking are
all readable, at any point in their history with `--through`. To edit the files
with ordinary applications, make your own copy and check it out:

```sh
synck repository import ./exported-repository ./my-copy
synck repository checkout ./my-copy --author "Alex"
```

[Opening a complete repository](https://synckhub.com/synck/getting-started/first-complete-repository)
walks through all of this with a downloadable sample.

## What is in this repository

| Package | Responsibility |
| --- | --- |
| `synck-vcs` | The engine: items, events, commits, canonical encoding, invariants, verification and historical state. Independent of any database |
| `synck-vcs-sqlite` | SQLite storage adapter |
| `synck-vcs-replica` | Bounded transfer of sealed history, with optional SQLite installation |
| `synck-file-folder` | File and folder operations, validation and file-version semantics |
| `synck-items` | Shared vocabulary, readers and body encoding for stored item types: files, chats, email, tasks, calendars and more |
| `synck-remote-protocol` | Unstable internal protocol; unpublished |
| `synck-local` | Local repositories, verified copying and the `synck` CLI |

The [`docs/`](docs/README.md) directory holds the normative contracts for the
[engine](docs/core.md), [checkouts and the CLI](docs/local-repository.md),
[repositories and copies](docs/portable-repository.md) and
[format boundaries](docs/formats.md). Read those before embedding the libraries.
The [library example](crates/synck-local/examples/local_repository.rs) creates a
repository, records two versions of a file, reads the first one back and verifies
the result:

```sh
cargo run --locked -p synck-local --example local_repository -- /absolute/path/to/new-repository
```

SynckHub's applications, its PostgreSQL integration and its hosted platform are
not part of this repository. Synck preserves the stored data; permissions, live
services and the user interface belong to the application that embeds it.

## Status and compatibility

This is the first public release, 0.1.0. The Rust API is pre-1.0 and may change.
Stored data is held to a stricter standard: releases do not rewrite accepted
history, change identifiers or silently reinterpret stored data, and the canonical
encodings are pinned by test vectors in this repository. See
[Formats and compatibility](https://synckhub.com/synck/reference/formats).

CI builds and tests every package on Linux, macOS and Windows.

## Contributing

Issues and pull requests are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers
building, testing and the compatibility rules for stored formats. Commits are
signed off under the [Developer Certificate of Origin](https://developercertificate.org/)
(`git commit -s`).

Please report security vulnerabilities privately, as described in
[SECURITY.md](SECURITY.md), not in a public issue.

## License

Synck is licensed under the [Apache License 2.0](LICENSE-APACHE). Copyright and
third-party attribution are recorded in [NOTICE](NOTICE). Dependencies keep
their own licenses. Synck is created and maintained by
[Jaime Rivas](AUTHORS).
