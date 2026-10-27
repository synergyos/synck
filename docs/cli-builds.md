# Build CLI executables

The **Build CLI** workflow runs manually and creates a draft GitHub Release.
In **Actions → Build CLI → Run workflow**, select `main` and enter an existing
`vX.Y.Z` tag, or run:

```sh
gh workflow run build-cli.yml --repo synergyos/synck --ref main -f tag=v0.1.0
```

The tag must be contained in the selected `main` revision, and its workspace
version must match the tag. The workflow builds the tagged source, not the
latest code on `main`. It refuses an existing release, including a draft, and
never creates or moves tags. Use a new version and tag to release newer source.
Release-helper and installer tests run before the platform builds.

The workflow checks `synck --version` and `synck --help`, initializes a temporary
repository, commits a file and runs `synck fsck` on each platform:

| Target | Requirements |
| --- | --- |
| `x86_64-unknown-linux-gnu` | Linux x86_64 with glibc 2.39 or newer |
| `aarch64-apple-darwin` | Apple Silicon Mac with macOS 12 or newer |
| `x86_64-apple-darwin` | Intel Mac with macOS 12 or newer |
| `x86_64-pc-windows-msvc` | Windows x86_64 |

After all four builds succeed, the workflow checks the complete asset set and
creates a **draft** at [GitHub Releases](https://github.com/synergyos/synck/releases).
The draft contains `synck-<version>-<target>` (`.exe` on Windows), `SHA256SUMS.txt`,
`install.sh`, `install.ps1`, `LICENSE-APACHE`, and `NOTICE`. The manifest covers
every attached executable, installer and license file. Executables are uploaded
directly without ZIP archives and must be nonempty and under 2 GiB.

On Linux or macOS, rename the download to `synck`, run `chmod +x synck`, and place
it in a directory on `PATH`. On Windows, rename it to `synck.exe` and place it in
a directory on `PATH`.

Only the final draft job has release write permission. The workflow references
no configured secrets or protected environments and does not persist checkout
credentials. Intermediate Actions artifacts expire after one day.

## Publication

The workflow never publishes. A maintainer must review the draft assets and
explicitly publish through GitHub Releases. Drafts are visible only to users
with repository write access. Until publication, public release downloads and
the quick-install commands cannot use them. Check the exact assets before
publishing; do not replace a published release in place.

If draft creation or asset upload fails partway through, inspect the draft and
explicitly delete that unpublished draft before retrying. The workflow refuses
to overwrite a draft or published release.
