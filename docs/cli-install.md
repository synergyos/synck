# Install a published CLI release

The installers download a published GitHub Release, verify the platform asset
against that release's `SHA256SUMS.txt`, and install the CLI in a user-owned
directory. No Rust toolchain or administrator access is needed. A missing asset,
download failure, or checksum failure stops installation before replacing an
existing executable. Draft releases and temporary Actions artifacts are not
installation sources.

The scripts are ready for the first published binary release. Until a maintainer
publishes one, use the source installation in the repository README or download
a manual build as described in [CLI builds](cli-builds.md).

## macOS and Linux

Quick install from the repository:

```sh
curl -fsSL https://raw.githubusercontent.com/synergyos/synck/main/install.sh | sh
```

Or download and inspect the release's copy of the script before running it:

```sh
curl --proto '=https' --proto-redir '=https' -fLsS \
  https://github.com/synergyos/synck/releases/latest/download/install.sh -o install.sh
sh install.sh
```

By default it selects the latest published stable release and installs to
`~/.local/bin/synck`. To choose a version and destination:

```sh
sh install.sh --version 0.1.0 --install-dir "$HOME/bin"
```

Both `0.1.0` and `v0.1.0` select tag `v0.1.0`. Linux x86_64 and macOS Apple
Silicon/Intel are supported; see [platform requirements](cli-builds.md).
The script requires `curl`, `unzip`, and either `sha256sum` or `shasum`.

Ensure the chosen directory is on `PATH`. For the default destination, add this
line to your shell's startup file if needed:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

## Windows

Quick install in PowerShell 5.1 or newer:

```powershell
irm https://raw.githubusercontent.com/synergyos/synck/main/install.ps1 | iex
```

Or download and inspect the release's copy of the script before running it:

```powershell
Invoke-WebRequest -UseBasicParsing `
  https://github.com/synergyos/synck/releases/latest/download/install.ps1 `
  -OutFile install.ps1
.\install.ps1
```

The default destination is `%LOCALAPPDATA%\Synck\bin\synck.exe`. To choose a
version and destination:

```powershell
.\install.ps1 -Version 0.1.0 -InstallDir "$env:USERPROFILE\bin"
```

Windows x86_64 is supported. Add the installation directory to your user `Path`
through **Environment Variables**, then open a new terminal.

## Updates and rollback

Rerun the installer for the latest release, or select a specific version to
upgrade or roll back. The scripts do not run the downloaded CLI or change shell
profiles or persistent `PATH` settings. Run `synck --version` after installation.
To uninstall, remove the installed executable.

Checksums detect downloads that differ from the published manifest. The release
assets and manifest are both obtained over HTTPS from the same repository.

## Release asset contract

Both installers use the existing release assets:

- `synck-<version>-<target>.zip`, containing `synck` or `synck.exe` at the root.
- `SHA256SUMS.txt`, containing exactly one SHA-256 entry for the selected archive.

The manual draft workflow also attaches `install.sh` and `install.ps1` and adds
their checksums to the manifest. Release creation and publication remain manual.
Review the exact assets before publishing.
