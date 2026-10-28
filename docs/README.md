# Synck documentation

- [CLI installation](cli-install.md): version selection and verified downloads.
- [CLI builds](cli-builds.md): manual executable builds and downloadable artifacts.
- [CLI releases](cli-releases.md): manual draft releases, platform signing and
  protected signing-environment setup.
- [Core engine](core.md) defines items, attached refs, sequences, structural
  invariants, append/ingest, canonical JSON and commit hashing.
- [Local checkouts and the CLI](local-repository.md) defines `.synck/`,
  SQLite, the working tree, scanning, commits, recovery and CLI commands.
- [Integrating an application](integration.md) defines how authoring
  applications record saved revisions through `repository create-file` and
  `save-file`, including base checks, attribution, replay and error kinds.
- [Format and compatibility scope](formats.md) explains the source contracts and
  the compatibility boundaries of the crate APIs and persisted formats.
- [Local library example](../crates/synck-local/examples/local_repository.rs)
  creates, versions, reads and verifies a file using only this workspace.
- `synck instructions` documents the current CLI workflows.
- `cargo doc --locked --no-deps -p synck-vcs -p synck-file-folder -p synck-items -p synck-local`
  generates the selected Rust API documentation.

Core is storage-adapter-independent. The included `synck-vcs-sqlite` adapter
and `synck-local` distribution use SQLite; the complete local repository layout
is specified here without constraining other engine adapters.

The [standalone format-7 specification](spec/format.md), its SQL schemas and
language-neutral vectors are the authoritative persisted-format contract.
Typed readers cover FILE/FOLDER, CHAT and EMAIL; unsupported families remain
structurally verifiable. The [central synchronization protocol](spec/protocol.md)
and `synck-sync` library define shared requests, receipts and verified history
paging. A generic server executable and CLI clone/pull/push are not yet included.

- [Portable repositories](portable-repository.md): verified copies, thin-history
  verification, editable/sparse checkouts and source-retirement activation.
