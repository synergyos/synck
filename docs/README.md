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

These contracts cover the engine, format-6 repositories and their checkouts. Typed readers and mutation support vary by
stored item family. The source and canonical vectors define exact encodings;
remote, replica, historical-index and shared-item formats also require their
versioned implementations and fixtures. A crate's presence does not imply
typed mutation support for every stored family.

- [Portable repositories](portable-repository.md): format-6 data storage, verified copying, complete verification, independent editable/sparse checkout and explicit source-retirement activation.
