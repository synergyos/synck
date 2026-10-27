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

Download the combined `synck-cli-<commit>` artifact from the completed run. It
contains four platform ZIPs, `SHA256SUMS.txt`, and `BUILD.txt` recording the source
commit. Each ZIP contains `synck` (`synck.exe` on Windows), `LICENSE-APACHE`, and
`NOTICE`. Extract your platform's ZIP and place the executable on `PATH`. ZIPs
preserve Unix executable permissions and must be nonempty and under 2 GiB.

The workflow uses read-only repository permission, references no configured
secrets, and does not persist checkout credentials. It creates Actions artifacts
with one-day retention. It does not create a GitHub Release.
