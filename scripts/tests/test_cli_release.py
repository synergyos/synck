import contextlib
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile


SPEC = importlib.util.spec_from_file_location(
    "cli_release", Path(__file__).resolve().parents[1] / "cli_release.py",
)
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def environment(self):
        return {
            "protection_rules": [{"type": "required_reviewers", "reviewers": [{"id": 1}]}],
            "can_admins_bypass": False,
            "deployment_branch_policy": {
                "protected_branches": False, "custom_branch_policies": True,
            },
        }

    def test_environment_requires_approval_no_bypass_and_only_main_branch(self):
        policies = [{"name": "main", "type": "branch"}]
        release.check_environment(self.environment(), policies)
        for field, value in (
            ("can_admins_bypass", True),
            ("protection_rules", []),
            ("deployment_branch_policy", None),
        ):
            with self.subTest(field=field), self.assertRaises(ValueError):
                release.check_environment({**self.environment(), field: value}, policies)
        for denied in ([], [{"name": "main", "type": "tag"}],
                       policies + [{"name": "*", "type": "branch"}]):
            with self.subTest(policies=denied), self.assertRaises(ValueError):
                release.check_environment(self.environment(), denied)

    def test_invalid_tags_and_untrusted_dispatches_fail_before_commands(self):
        for tag in ("main", "v1.2.3;echo unsafe", "v01.2.3", "v1.2", "v1.2.3/other"):
            with self.subTest(tag=tag), patch.object(release, "command") as command:
                with self.assertRaises(ValueError):
                    release.validate_tag(tag)
                command.assert_not_called()
        for repository, ref in (("fork/synck", "refs/heads/main"),
                                (release.REPOSITORY, "refs/heads/other"),
                                (release.REPOSITORY, "refs/tags/v1.2.3")):
            with self.subTest(repository=repository, ref=ref), patch.dict(os.environ, {
                "GITHUB_REPOSITORY": repository, "GITHUB_REF": ref,
            }), patch.object(release, "command") as command:
                with self.assertRaises(ValueError):
                    release.validate_tag("v1.2.3")
                command.assert_not_called()

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.root), *args], text=True).strip()

    def test_tagged_version_and_main_ancestry_are_checked(self):
        self.git("init", "-q", "--initial-branch=main")
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "test@example.invalid")
        manifest = self.root / "Cargo.toml"
        manifest.write_text('[workspace.package]\nversion = "1.2.3"\n')
        self.git("add", "Cargo.toml")
        self.git("commit", "-qm", "Initial")
        main = self.git("rev-parse", "HEAD")
        self.git("tag", "v1.2.3")
        self.git("tag", "v9.9.9")
        with patch.dict(os.environ, {
            "GITHUB_REPOSITORY": release.REPOSITORY,
            "GITHUB_REF": "refs/heads/main", "GITHUB_SHA": main,
            "GITHUB_OUTPUT": str(self.root / "output"),
            "GITHUB_STEP_SUMMARY": str(self.root / "summary"),
        }), contextlib.chdir(self.root):
            with self.assertRaisesRegex(ValueError, "workspace version"):
                release.validate_tag("v9.9.9")
            original_command = release.command

            def command(*args):
                if args[0] != "gh":
                    return original_command(*args)
                import json
                response = self.environment()
                if args[2].endswith("deployment-branch-policies"):
                    response = {"branch_policies": [{"name": "main", "type": "branch"}]}
                return json.dumps(response)

            with patch.object(release, "command", side_effect=command), patch.object(
                release, "check_release_absent",
            ):
                release.validate_tag("v1.2.3")
            self.assertEqual((self.root / "output").read_text(), f"commit={main}\nversion=1.2.3\n")
            # This tag points at a descendant, not a commit contained in dispatch main.
            manifest.write_text('[workspace.package]\nversion = "1.2.4"\n')
            self.git("add", "Cargo.toml")
            self.git("commit", "-qm", "Later")
            self.git("tag", "v1.2.4")
            with self.assertRaises(subprocess.CalledProcessError):
                release.validate_tag("v1.2.4")

    def test_release_lookup_includes_drafts_and_fails_on_network_errors(self):
        with patch.object(release, "command") as command:
            command.return_value = '[[]]'
            release.check_release_absent("v1.2.3")
            for draft in (False, True):
                import json
                command.return_value = json.dumps([[], [{"tag_name": "v1.2.3", "draft": draft}]])
                with self.subTest(draft=draft), self.assertRaisesRegex(ValueError, "already exists"):
                    release.check_release_absent("v1.2.3")
            command.side_effect = subprocess.CalledProcessError(1, "gh")
            with self.assertRaises(subprocess.CalledProcessError):
                release.check_release_absent("v1.2.3")

    def test_build_validation_does_not_access_an_environment(self):
        with patch.object(release, "validate_source_tag", return_value=("abc123", "1.2.3")), \
             patch.object(release, "check_release_absent") as absent, \
             patch.object(release, "write_release_output") as output, \
             patch.object(release, "check_environment") as environment:
            release.validate_build("v1.2.3")
            absent.assert_called_once_with("v1.2.3")
            output.assert_called_once_with("v1.2.3", "abc123", "1.2.3")
            environment.assert_not_called()

    def test_build_assembly_requires_all_targets_and_checksums_final_assets(self):
        directory = self.root / "binaries"
        directory.mkdir()
        for target in release.TARGETS:
            suffix = ".exe" if target.endswith("windows-msvc") else ""
            (directory / f"synck-{target}{suffix}").write_bytes(target.encode())
        output = self.root / "release"
        release.assemble_build(directory, output, "1.2.3")
        import hashlib
        for target in release.TARGETS:
            name = release.executable_name("1.2.3", target)
            self.assertEqual((output / name).read_bytes(), target.encode())
        manifest = (output / "SHA256SUMS.txt").read_text()
        for asset in output.iterdir():
            if asset.name != "SHA256SUMS.txt":
                self.assertIn(f"{hashlib.sha256(asset.read_bytes()).hexdigest()}  {asset.name}\n", manifest)
        (directory / "unexpected").write_bytes(b"must not ship")
        with self.assertRaises(ValueError):
            release.assemble_build(directory, self.root / "extra", "1.2.3")
        (directory / "unexpected").unlink()
        for asset in directory.iterdir():
            asset.unlink()
            break
        with self.assertRaises(ValueError):
            release.assemble_build(directory, self.root / "missing", "1.2.3")

    def input_files(self, target):
        directory = self.root / target
        directory.mkdir()
        binary = "synck.exe" if "windows" in target else "synck"
        (directory / binary).write_bytes(b"signed final CLI bytes")
        for name in ("LICENSE-APACHE", "NOTICE"):
            (directory / name).write_text(name)
        return directory

    def test_archives_preserve_executable_mode_and_checksums_use_final_bytes(self):
        output = self.root / "release"
        for target in release.TARGETS:
            release.package(self.input_files(target), output, "1.2.3", target)
        archive = output / "synck-1.2.3-aarch64-apple-darwin.zip"
        with zipfile.ZipFile(archive) as bundle:
            self.assertEqual(bundle.read("synck"), b"signed final CLI bytes")
            self.assertEqual(bundle.getinfo("synck").external_attr >> 16 & 0o777, 0o755)
        executable = output / release.executable_name("1.2.3", "aarch64-apple-darwin")
        self.assertEqual(executable.read_bytes(), b"signed final CLI bytes")
        release.checksums(output, "1.2.3")
        import hashlib
        self.assertIn(
            f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n",
            (output / "SHA256SUMS.txt").read_text(),
        )
        # Extra assets, including an accidentally staged credential, must fail closed.
        with self.assertRaises(ValueError):
            release.checksums(output, "1.2.3")

    def test_packaging_rejects_extra_files_symlinks_and_overwrites(self):
        target = release.TARGETS[0]
        directory = self.input_files(target)
        extra = directory / "certificate.p12"
        extra.write_bytes(b"must not ship")
        with self.assertRaises(ValueError):
            release.package(directory, self.root / "out", "1.2.3", target)
        extra.unlink()
        binary = directory / "synck"
        binary.unlink()
        binary.symlink_to(directory / "NOTICE")
        with self.assertRaises(ValueError):
            release.package(directory, self.root / "out", "1.2.3", target)
        binary.unlink()
        binary.write_bytes(b"CLI")
        release.package(directory, self.root / "out", "1.2.3", target)
        with self.assertRaises(FileExistsError):
            release.package(directory, self.root / "out", "1.2.3", target)

    def test_size_limit_rejects_empty_and_exactly_two_gib(self):
        asset = self.root / "asset.zip"
        asset.touch()
        with self.assertRaises(ValueError):
            release.check_asset(asset)
        with asset.open("wb") as output:
            output.truncate(release.ASSET_LIMIT)
        with self.assertRaises(ValueError):
            release.check_asset(asset)
        with asset.open("wb") as output:
            output.truncate(release.ASSET_LIMIT - 1)
        release.check_asset(asset)


if __name__ == "__main__":
    unittest.main()
