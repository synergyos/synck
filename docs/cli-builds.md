# Build CLI executables

The **Build CLI** workflow runs manually. Select a branch or tag in
**Actions → Build CLI → Run workflow**, or run:

```sh
gh workflow run build-cli.yml --repo synergyos/synck --ref main
```

The workflow builds the selected revision and checks `synck --version` and
`synck --help` on each platform:

| Target | Requirements |
| --- | --- |
| `x86_64-unknown-linux-gnu` | Linux x86_64 with glibc 2.39 or newer |
| `aarch64-apple-darwin` | Apple Silicon Mac with macOS 12 or newer |
| `x86_64-apple-darwin` | Intel Mac with macOS 12 or newer |
| `x86_64-pc-windows-msvc` | Windows x86_64 |

Download the executable for your platform from the completed run's artifacts:
`synck-<target>` (`synck-<target>.exe` on Windows). Each download is a single
executable, uploaded directly without a ZIP archive. Files must be nonempty and
under 2 GiB.

On Linux or macOS, rename the download to `synck`, run `chmod +x synck`, and place
it in a directory on `PATH`. On Windows, rename it to `synck.exe` and place it in
a directory on `PATH`.

The workflow uses read-only repository permission, references no configured
secrets, and does not persist checkout credentials. It creates Actions artifacts
with one-day retention. It does not create a GitHub Release.
