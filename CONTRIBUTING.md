# Contributing

Keep changes small, explicit and focused on a concrete requirement. Explain the
problem, resulting behavior, compatibility impact and the exact verification
commands you ran. Add tests when they demonstrate a meaningful behavior or
regression. Use the checked-in Rust toolchain and `--locked` with Cargo.

Persisted events, identifiers, canonical bytes, database/storage formats and
versioned wire protocols are compatibility contracts. Discuss a change to those
contracts before implementing it. Follow the [pre-1.0 single-format policy](docs/formats.md), preserve accepted
history within that format, and preserve error categories. Product policy, hosted services and GUI applications are outside
this workspace's scope.

## Getting started

Install Rust through rustup and run commands from the checkout root.
`rust-toolchain.toml` selects the project's toolchain. Its minimal profile does
not include rustfmt; install it before running formatting checks:

```sh
rustup component add rustfmt
```

Build the local CLI package with:

```sh
cargo build --locked -p synck-local
```

See the [README](README.md#install) for CLI installation and a local
repository walkthrough, and the [documentation index](docs/README.md) for the
engine and persisted-format contracts.

## Verification

The [CI workflow](.github/workflows/public-ci.yml) lists the public package checks
and platform matrix. Locally, run only the package and test targets affected by
your change, for example:

```sh
cargo test --locked -p synck-file-folder --test profile_v1
cargo fmt -p synck-file-folder -- --check
```

Two longer checks are worth running when a change touches what they cover:

- A write path: the crash matrix kills a real `synck` at every filesystem
  mutation, then checks that the repository verifies and the operation finishes.
  It runs on macOS and Linux and takes minutes.

  ```sh
  cargo test --locked -p synck-local --test crash_points -- --ignored
  ```

- A parser of untrusted input: `cargo test -p synck-fuzz` runs a short fixed
  mutation pass, `SYNCK_MUTATION_ROUNDS=30000` a long one, and
  [fuzz/](fuzz/README.md) a coverage-guided search.

A change to the format changes its [vectors](docs/spec/vectors/README.md), and a
change to anything else must not. `python3 -I scripts/generate_vectors.py --check`
says which. `cargo deny check` covers advisories, licenses and sources.

Include the command output or result and any untested platforms in your pull
request. For documentation-only changes, reviewing the diff is usually enough;
say when no tests or builds were run. Report security vulnerabilities through
[SECURITY.md](SECURITY.md).

## CLI builds

Run the manual **Build CLI** workflow with an existing version tag to create a
draft release with executable downloads and installers. See the
[build instructions](docs/cli-builds.md) for review and manual publication.

## CLI releases

Signed CLI releases use a separate manual workflow and a protected signing
environment. The workflow creates a draft only. See the
[release procedure](docs/cli-releases.md) before provisioning credentials or
dispatching a release.

## Developer Certificate of Origin

Sign off each contributed commit to attest to the
[Developer Certificate of Origin 1.1](https://developercertificate.org/).
Use your own name and an email address you are willing to publish:

```text
Signed-off-by: Your Name <your-address>
```

`git commit -s` adds this line. Signing off certifies that you have the right to
submit the contribution under the project's Apache-2.0 license. No separate CLA
is requested by this contribution policy. Identify any copied or generated
material and preserve its required attribution and license notices.
