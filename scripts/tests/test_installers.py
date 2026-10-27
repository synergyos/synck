"""Exercise installers with local release fixtures and mocked HTTP commands."""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[2]
POWERSHELL = os.environ.get("SYNCK_TEST_PWSH") or shutil.which("pwsh")


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.tools = self.root / "tools"
        self.tools.mkdir()
        self.install = self.root / "install with spaces"
        self.install.mkdir()
        self.requests = self.root / "requests"
        self.env = {
            **os.environ,
            "PATH": f"{self.tools}{os.pathsep}{os.environ['PATH']}",
            "FIXTURE": str(self.root),
            "TEST_INSTALL_DIR": str(self.install),
            "TEST_VERSION": "1.2.3",
            "TEST_OS": "Linux",
            "TEST_ARCH": "x86_64",
            "TEST_TAG": "v1.2.3",
            "LOCALAPPDATA": str(self.root),
            "OS": "Windows_NT",
            "PROCESSOR_ARCHITECTURE": "AMD64",
            "PROCESSOR_ARCHITEW6432": "",
        }
        self.binary = b"fixture CLI: installers must never execute this file\n"
        lines = []
        for target in (
            "x86_64-unknown-linux-gnu", "aarch64-apple-darwin",
            "x86_64-apple-darwin", "x86_64-pc-windows-msvc",
        ):
            asset = self.root / f"synck-1.2.3-{target}.zip"
            name = "synck.exe" if "windows" in target else "synck"
            with zipfile.ZipFile(asset, "w") as archive:
                archive.writestr(name, self.binary)
                # Unrelated entries must never be extracted into the filesystem.
                archive.writestr("../escaped", b"must not extract")
            lines.append(f"{hashlib.sha256(asset.read_bytes()).hexdigest()}  {asset.name}\n")
        self.manifest = self.root / "SHA256SUMS.txt"
        self.manifest.write_text("".join(lines))
        self.tool("uname", """
import os
import sys
print(os.environ['TEST_OS' if sys.argv[1] == '-s' else 'TEST_ARCH'])
""")
        self.tool("curl", """
import os
from pathlib import Path
import shutil
import sys
args = sys.argv[1:]
root = Path(os.environ['FIXTURE'])
url = args[-1]
with (root / 'requests').open('a') as log:
    log.write(url + '\\n')
if url == 'https://github.com/synergyos/synck/releases/latest':
    print('https://github.com/synergyos/synck/releases/tag/' + os.environ['TEST_TAG'], end='')
else:
    assert url.startswith('https://github.com/synergyos/synck/releases/download/v1.2.3/')
    shutil.copyfile(root / url.rsplit('/', 1)[1], args[args.index('--output') + 1])
""")
        self.wrapper = self.root / "run.ps1"
        self.wrapper.write_text("""
$ErrorActionPreference = 'Stop'
function Invoke-RestMethod {
    param($Uri)
    Add-Content -LiteralPath (Join-Path $env:FIXTURE 'requests') -Value $Uri
    return @{ tag_name = $env:TEST_TAG }
}
function Invoke-WebRequest {
    param([switch]$UseBasicParsing, $Uri, $OutFile)
    if (-not $UseBasicParsing) { throw 'UseBasicParsing is required' }
    if (-not $Uri.StartsWith('https://github.com/synergyos/synck/releases/download/v1.2.3/')) {
        throw "Unexpected release URL: $Uri"
    }
    Add-Content -LiteralPath (Join-Path $env:FIXTURE 'requests') -Value $Uri
    Copy-Item -LiteralPath (Join-Path $env:FIXTURE ($Uri.Split('/')[-1])) -Destination $OutFile
}
try {
    if ($env:TEST_PIPE -eq '1') {
        Get-Content -LiteralPath $env:TEST_INSTALLER -Raw | Invoke-Expression
    } else {
        & $env:TEST_INSTALLER -Version $env:TEST_VERSION -InstallDir $env:TEST_INSTALL_DIR
    }
} catch {
    Write-Host $_
    exit 1
}
""")

    def tool(self, name, script):
        path = self.tools / name
        path.write_text(f"#!{sys.executable}\n" + script)
        path.chmod(0o755)

    def run_installer(self, platform, version=None, piped=False):
        env = {**self.env}
        env["TEST_PIPE"] = "1" if piped else "0"
        if version is not None:
            env["TEST_VERSION"] = version
        if platform == "windows":
            env["TEST_INSTALLER"] = str(ROOT / "install.ps1")
            command = [POWERSHELL, "-NoProfile", "-File", str(self.wrapper)]
        else:
            command = ["sh", str(ROOT / "install.sh"), "--version", env["TEST_VERSION"],
                       "--install-dir", str(self.install)]
            if piped:
                command = ["sh", "-s", "--", "--install-dir", str(self.install)]
        return subprocess.run(
            command, env=env, capture_output=True, text=True,
            input=(ROOT / "install.sh").read_text() if piped and platform == "unix" else None,
        )

    def test_quick_install_pipeline_entry_points(self):
        for platform in self.platforms():
            with self.subTest(platform=platform):
                result = self.run_installer(platform, piped=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                destination = (self.root / "Synck" / "bin" / "synck.exe"
                               if platform == "windows" else self.install / "synck")
                self.assertEqual(destination.read_bytes(), self.binary)

    def platforms(self):
        return ("unix", "windows") if POWERSHELL else ("unix",)

    def destination(self, platform):
        return self.install / ("synck.exe" if platform == "windows" else "synck")

    def test_explicit_and_latest_versions_install_verified_bytes(self):
        for platform in self.platforms():
            for version in ("1.2.3", "v1.2.3", "latest"):
                with self.subTest(platform=platform, version=version):
                    destination = self.destination(platform)
                    if version != "1.2.3":
                        destination.write_bytes(b"previous installation")
                    result = self.run_installer(platform, version)
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertEqual(destination.read_bytes(), self.binary)
                    self.assertFalse((self.root / "escaped").exists())
                    if platform == "unix":
                        self.assertEqual(destination.stat().st_mode & 0o777, 0o755)
        urls = self.requests.read_text().splitlines()
        self.assertIn("https://github.com/synergyos/synck/releases/latest", urls)
        self.assertTrue(all("/download/v1.2.3/" in url for url in urls if "/download/" in url))

    def test_checksum_failures_preserve_existing_installation(self):
        original = self.manifest.read_text()
        for content in (
            "".join("0" * 64 + line[64:] + "\n" for line in original.splitlines()),
            "", original + original,
            "".join("not-a-hash" + line[64:] + "\n" for line in original.splitlines()),
        ):
            self.manifest.write_text(content)
            for platform in self.platforms():
                with self.subTest(platform=platform, manifest=content):
                    destination = self.destination(platform)
                    destination.write_bytes(b"keep existing")
                    result = self.run_installer(platform)
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertEqual(destination.read_bytes(), b"keep existing")
                    self.assertFalse(list(self.install.glob(".synck*")))

    def test_invalid_versions_fail_before_network_or_installation(self):
        for version in ("../main", "v01.2.3", "1.2", "1.2.3;echo unsafe", "v1.2.3\nextra", "v1.2.3\n"):
            for platform in self.platforms():
                with self.subTest(platform=platform, version=version):
                    self.assertNotEqual(self.run_installer(platform, version).returncode, 0)
        self.assertFalse(self.requests.exists())

    def test_unavailable_download_preserves_existing_installation(self):
        for asset in self.root.glob("synck-*.zip"):
            asset.unlink()
        for platform in self.platforms():
            with self.subTest(platform=platform):
                destination = self.destination(platform)
                destination.write_bytes(b"keep existing")
                self.assertNotEqual(self.run_installer(platform).returncode, 0)
                self.assertEqual(destination.read_bytes(), b"keep existing")

    def test_unix_platform_detection(self):
        for machine in ("arm64", "x86_64"):
            with self.subTest(machine=machine):
                self.env.update(TEST_OS="Darwin", TEST_ARCH=machine)
                result = self.run_installer("unix")
                self.assertEqual(result.returncode, 0, result.stderr)
        self.env.update(TEST_OS="Linux", TEST_ARCH="aarch64")
        self.assertNotEqual(self.run_installer("unix").returncode, 0)

    def test_invalid_archive_preserves_existing_installation(self):
        for empty in (False, True):
            lines = []
            for asset in self.root.glob("synck-*.zip"):
                binary = "synck.exe" if "windows" in asset.name else "synck"
                with zipfile.ZipFile(asset, "w") as archive:
                    archive.writestr(binary if empty else "wrong-entry", b"")
                lines.append(f"{hashlib.sha256(asset.read_bytes()).hexdigest()}  {asset.name}\n")
            self.manifest.write_text("".join(lines))
            for platform in self.platforms():
                with self.subTest(platform=platform, empty=empty):
                    destination = self.destination(platform)
                    destination.write_bytes(b"keep existing")
                    result = self.run_installer(platform)
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertEqual(destination.read_bytes(), b"keep existing")

    def test_latest_tag_is_validated_before_asset_downloads(self):
        self.env["TEST_TAG"] = "v1.2.3-rc.1"
        for platform in self.platforms():
            with self.subTest(platform=platform):
                self.assertNotEqual(self.run_installer(platform, "latest").returncode, 0)
        self.assertNotIn("/download/", self.requests.read_text())

    @unittest.skipUnless(POWERSHELL, "Set SYNCK_TEST_PWSH or install pwsh to test PowerShell")
    def test_windows_rejects_unsupported_architecture(self):
        self.env["PROCESSOR_ARCHITECTURE"] = "ARM64"
        self.assertNotEqual(self.run_installer("windows").returncode, 0)
        self.assertFalse(self.requests.exists())


if __name__ == "__main__":
    unittest.main()
