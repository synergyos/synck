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
# Reports history/checkpoints, objects, metadata and absent payloads.
```

Synck is the open-source engine behind [SynckHub](https://synckhub.com). It is a
Rust library you can embed and a command-line tool, `synck`, for people and
AI agents.

**Documentation: [synckhub.com/synck](https://synckhub.com/synck)**: tutorials,
everyday use, internals and a full command reference.

`synck init` turns any folder into a repository. Existing files stay
uncommitted until you commit them, and a default `.synckignore` keeps
operating-system and editor clutter out. Email is imported as records filed in
your folders, chats live alongside your files, and content is stored in
compressed, deduplicated chunks.

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
  chats and email. The 0.2 typed readers cover FILE/FOLDER, CHAT and EMAIL. Every item has a permanent
  ID and its own ordered history, so a renamed or moved file is still the same
  file.
- **Append-only ordinary writes.** Undo adds a new change that reverses an old
  one. A future explicit administrative rewrite must change the history generation
  and require client reconciliation; 0.2 does not implement that command.
- **No staging area.** Edit files with any application. `synck commit` records
  what changed: everything, or only the paths you name.
- **One writer, linear history.** Each item has one authority that accepts its
  changes in order. There are no branches or merges, and independent content edits to different
  items do not conflict. Structural changes still share namespace constraints.
- **Integrity throughout.** Commits, file versions and message bodies are
  identified by SHA-256, and a file's ID is the SHA-256 of its bytes. Small
private text and metadata include a random salt. `synck fsck`
  checks history/checkpoints and available objects, reporting absent payloads
  separately from corruption.
- **Complete, self-contained archives.** A complete archive holds its history
  and every byte that history refers to. Thin repositories may lack payloads. `synck archive` writes a verified copy
  that opens with this tool alone.

[Items, events and commits](https://synckhub.com/synck/internals) and the
[system design](https://synckhub.com/synck/internals/system-design) explain the
model in depth, including its [trade-offs against Git, Lore, Perforce and plain
databases](https://synckhub.com/synck/internals/trade-offs).

## Install

### Quick install

These commands install the latest published binary release. They become usable
when the first binary release is published. Until then, install from source or
download a [manual CLI build](docs/cli-builds.md).

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/synergyos/synck/main/install.ps1 | iex
```

macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/synergyos/synck/main/install.sh | sh
```

See [CLI installation](docs/cli-install.md) for version selection, installation
directories, and adding the executable to `PATH`.

### From source

You need Git,
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

A repository in the matching release format can be checked and read directly.
SynckHub currently remains pinned to 0.1; its format-6 exports require that reader:

```sh
synck repository verify ./exported-repository
synck item list --path ./exported-repository
synck item show ITEM_ID --path ./exported-repository
synck item export FILE_ID ./report.pdf --path ./exported-repository
synck repository log ./exported-repository
```

Files, chats and email are readable at any point in their history with
`--through`. To edit the files
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
| `synck-file-folder` | File and folder operations, validation and file-version semantics |
| `synck-items` | Shared vocabulary, readers and body encoding for stored item types: files, chats and email |
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

Synck 0.2.0 uses repository format 7. Before 1.0, each release reads only its
own format and refuses other formats without modifying them. There are no
compatibility readers or automatic migrations. The Rust API can also change;
repositories made by untagged development builds are disposable.

The [standalone format specification](docs/spec/format.md), SQL schemas and
language-neutral vectors define the format independently of the Rust source.
Complete exports preserve the history generation, hashes and exact object salts.
Regulated retention requires a separate archival system; a verified copy alone
is not a claim of WORM or regulatory compliance.

To carry working files from 0.1 to 0.2, keep both executables available:

```sh
# Use the 0.1 executable inside the old repository:
synck-0.1 export-tree HEAD /path/to/new-work
# Then use 0.2:
synck-0.2 init /path/to/new-work --author "Alex"
cd /path/to/new-work
synck-0.2 commit --fix-names -m "Import working files from 0.1"
```

These executable names denote separately installed versions. FILE bytes transfer;
old history, IDs, labels and typed chat/email data do not. Keep the original
repository with its 0.1 reader when those records matter. A history converter
is not included. See [format policy](docs/formats.md).

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
