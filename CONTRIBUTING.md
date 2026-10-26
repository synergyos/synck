# Contributing

Keep changes small, explicit and focused on a concrete requirement. Explain the
problem, resulting behavior, compatibility impact and the exact verification
commands you ran. Add tests when they demonstrate a meaningful behavior or
regression. Use the checked-in Rust toolchain and `--locked` with Cargo.

Persisted events, identifiers, canonical bytes, database/storage formats and
versioned wire protocols are compatibility contracts. Discuss a change to those
contracts before implementing it. Preserve existing accepted history and error
categories. Product policy, hosted services and GUI applications are outside
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

Include the command output or result and any untested platforms in your pull
request. For documentation-only changes, reviewing the diff is usually enough;
say when no tests or builds were run. Report security vulnerabilities through
[SECURITY.md](SECURITY.md).

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
