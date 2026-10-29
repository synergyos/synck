# Manual CLI releases

`.github/workflows/release-cli.yml` builds an existing `vX.Y.Z` tag into a **draft**
GitHub Release. A push, pull request or tag push never dispatches it. There is no
publication step: a maintainer must inspect and install the draft assets before
publishing through GitHub Releases separately.

Dispatch the workflow from `main`, with a tag whose commit is already contained
in that workflow's `main` revision. The tagged workspace version must match the
tag. The workflow refuses any existing release, including drafts, and checks the
tag again before upload. It never creates or moves a tag or replaces a release.
To retry after a partial draft upload, first inspect the failure and explicitly
remove that unpublished draft; do not modify a published release in place.

```sh
gh workflow run release-cli.yml --repo synergyos/synck --ref main -f tag=v0.2.0
```

Do not run this command until the workflow is on `main` and an authorized
maintainer has provisioned the protected signing environment. Dispatching
requests real signatures and notarization after approval, even though the
release stays a draft.

## Platform artifacts

Each ZIP contains `synck` (`synck.exe` on Windows), `LICENSE-APACHE`, and `NOTICE`.

| Target | Build host | Distribution |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Ubuntu 24.04 | Unsigned Linux executable; glibc 2.39 or newer |
| `aarch64-apple-darwin` | macOS 15, Apple Silicon | Developer ID signed and notarized; macOS 12 or newer |
| `x86_64-apple-darwin` | macOS 15, Intel | Developer ID signed and notarized; macOS 12 or newer |
| `x86_64-pc-windows-msvc` | Windows 2025 | Azure Authenticode signed and RFC3161 timestamped executable |

Archives are named `synck-<version>-<target>.zip`. `SHA256SUMS.txt` covers the
final archives and the equivalent raw `synck-<version>-<target>` executables
(`.exe` on Windows). The installers use the raw executables. Every uploaded
release asset must be nonempty and strictly smaller than 2 GiB; packaging and
draft assembly both enforce this.
ZIP Unix mode bits preserve the executable permission when using `unzip`.
Place the extracted executable in a directory on `PATH`.

Apple cannot staple a notarization ticket to a standalone executable or ZIP.
Gatekeeper retrieves the ticket online. These ZIPs do not provide an offline
stapled installer. Windows signs the executable itself.

## Protected signing environment

Configure the `synck-release` environment in `synergyos/synck` **before adding
credentials**:

- Require approval by a trusted release maintainer.
- Disable administrator bypass.
- Select deployment branches/tags explicitly: allow only the `main` **branch**.
  The workflow runs from `main`; the release tag is a separately validated input.
- Store all signing credentials as **environment secrets**, never repository
  secrets or organization secrets shared directly with this public repository.

Validation fails if the environment lacks reviewers, permits admin bypass or
allows another branch/tag. Recheck these settings before granting approval.
Anyone who can change approved workflow code or approve signing is trusted with
the signing identity. Review workflow/script changes and the selected source
commit before approval; never approve an untrusted PR or fork build for signing.

Credential provisioning and rotation are handled privately by authorized
maintainers. Secret names referenced in the workflow are configuration labels;
their values belong only in the protected environment. Never commit credential
values or include them in logs, artifacts or release notes.

The bundled `scripts/DeveloperIDG2CA.pem` is Apple's public intermediate
certificate, fingerprint-checked before import. It contains no private key.

Builds receive no signing credentials and smoke-test the compiled executable.
Separate, fresh GitHub-hosted signing jobs download only this run's build
artifacts and never execute those binaries. Signing scripts come from the
workflow's exact `main` revision, not the release tag. macOS uses a temporary
keychain and deletes the certificate and notarization key on exit. Windows uses
the pinned `TrustedSigning` 0.5.8 module and Azure login; the private Windows
signing key remains with the signing service. The draft-upload job has release
write permission and no signing credentials. Third-party actions are pinned to
commit hashes. Intermediate Actions artifacts expire after one day and are
publicly downloadable subject to GitHub's access rules; never place signing
material in them.

Before publication, download the exact draft archives, verify the checksums and
OS signatures, and run the CLI on each supported platform. Local workflow tests
cannot prove live credential validity, notarization, or installed behavior.
