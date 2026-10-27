#!/usr/bin/env python3
"""Validate and assemble manual Synck CLI releases."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tomllib
import zipfile


REPOSITORY = "synergyos/synck"
ENVIRONMENT = "synck-release"
ASSET_LIMIT = 2 * 1024**3
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
)


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def check_environment(environment, policies):
    reviewers = [
        rule for rule in environment["protection_rules"]
        if rule["type"] == "required_reviewers" and rule.get("reviewers")
    ]
    if not reviewers or environment.get("can_admins_bypass", True):
        raise ValueError("Release environment requires reviewers and no admin bypass")
    if environment.get("deployment_branch_policy") != {
        "protected_branches": False, "custom_branch_policies": True,
    } or [(rule["name"], rule["type"]) for rule in policies] != [("main", "branch")]:
        raise ValueError("Release environment must allow only the main branch")


def check_release_absent(tag):
    # The tag endpoint lists published releases only; include drafts via the list.
    pages = json.loads(command("gh", "api", "--paginate", "--slurp", f"repos/{REPOSITORY}/releases"))
    if any(release["tag_name"] == tag for page in pages for release in page):
        raise ValueError("Release already exists, including drafts")


def validate_source_tag(tag):
    if not re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
        raise ValueError("Select an existing vX.Y.Z release tag")
    if os.environ["GITHUB_REPOSITORY"] != REPOSITORY or os.environ["GITHUB_REF"] != "refs/heads/main":
        raise ValueError("Dispatch the release workflow from synergyos/synck main")
    commit = command("git", "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}")
    subprocess.run(
        ["git", "merge-base", "--is-ancestor", commit, os.environ["GITHUB_SHA"]],
        check=True,
    )
    manifest = tomllib.loads(command("git", "show", f"{commit}:Cargo.toml"))
    version = manifest["workspace"]["package"]["version"]
    if tag != f"v{version}":
        raise ValueError("Release tag does not match the tagged workspace version")
    return commit, version


def write_release_output(tag, commit, version):
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        output.write(f"commit={commit}\nversion={version}\n")
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as summary:
        summary.write(f"Release **{tag}**, source `{commit}`. Publication remains manual.\n")


def validate_build(tag):
    commit, version = validate_source_tag(tag)
    check_release_absent(tag)
    write_release_output(tag, commit, version)


def validate_tag(tag):
    commit, version = validate_source_tag(tag)
    base = f"repos/{REPOSITORY}/environments/{ENVIRONMENT}"
    environment = json.loads(command("gh", "api", base))
    policies = json.loads(command("gh", "api", f"{base}/deployment-branch-policies"))
    check_environment(environment, policies["branch_policies"])
    check_release_absent(tag)
    write_release_output(tag, commit, version)


def executable_name(version, target):
    suffix = ".exe" if target.endswith("windows-msvc") else ""
    return f"synck-{version}-{target}{suffix}"


def check_asset(path):
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"Expected a regular release file: {path}")
    size = path.stat().st_size
    if not 0 < size < ASSET_LIMIT:
        raise ValueError(f"Release asset must be nonempty and under 2 GiB: {path}")


def package(directory, output, version, target):
    binary = "synck.exe" if target.endswith("windows-msvc") else "synck"
    names = (binary, "LICENSE-APACHE", "NOTICE")
    if {path.name for path in directory.iterdir()} != set(names):
        raise ValueError("Package input must contain only the CLI, license and notice")
    for name in names:
        check_asset(directory / name)
    output.mkdir(parents=True, exist_ok=True)
    executable = output / executable_name(version, target)
    if executable.exists() or executable.is_symlink():
        raise FileExistsError(executable)
    archive = output / f"synck-{version}-{target}.zip"
    with zipfile.ZipFile(archive, "x", compression=zipfile.ZIP_DEFLATED) as bundle:
        for name in names:
            info = zipfile.ZipInfo(name)
            info.create_system = 3
            mode = 0o755 if name == binary else 0o644
            info.external_attr = (stat.S_IFREG | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            with (directory / name).open("rb") as source, bundle.open(info, "w") as destination:
                while chunk := source.read(1024 * 1024):
                    destination.write(chunk)
    check_asset(archive)
    with (directory / binary).open("rb") as source, executable.open("xb") as destination:
        shutil.copyfileobj(source, destination)
    executable.chmod(0o755)
    check_asset(executable)


def checksums(directory, version):
    names = [f"synck-{version}-{target}.zip" for target in TARGETS]
    names.extend(executable_name(version, target) for target in TARGETS)
    write_checksums(directory, names)


def write_checksums(directory, names):
    names = sorted(names)
    if {path.name for path in directory.iterdir()} != set(names):
        raise ValueError("Release must contain exactly the expected assets")
    lines = []
    for name in names:
        asset = directory / name
        check_asset(asset)
        with asset.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        lines.append(f"{digest}  {name}\n")
    manifest = directory / "SHA256SUMS.txt"
    manifest.write_text("".join(lines), encoding="utf-8")
    check_asset(manifest)


def assemble_build(directory, output, version):
    inputs = {
        f"synck-{target}{'.exe' if target.endswith('windows-msvc') else ''}":
        executable_name(version, target) for target in TARGETS
    }
    if {path.name for path in directory.iterdir()} != set(inputs):
        raise ValueError("Build must contain exactly the four expected executables")
    for name in inputs:
        check_asset(directory / name)
    output.mkdir()
    for source, destination in inputs.items():
        shutil.copyfile(directory / source, output / destination)
    root = Path(__file__).resolve().parents[1]
    metadata = ("install.sh", "install.ps1", "LICENSE-APACHE", "NOTICE")
    for name in metadata:
        shutil.copyfile(root / name, output / name)
    write_checksums(output, [*inputs.values(), *metadata])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    validate = commands.add_parser("validate")
    validate.add_argument("tag")
    build = commands.add_parser("validate-build")
    build.add_argument("tag")
    bundle = commands.add_parser("package")
    bundle.add_argument("directory", type=Path)
    bundle.add_argument("output", type=Path)
    bundle.add_argument("version")
    bundle.add_argument("target", choices=TARGETS)
    checksum = commands.add_parser("checksums")
    checksum.add_argument("directory", type=Path)
    checksum.add_argument("version")
    assemble = commands.add_parser("assemble-build")
    assemble.add_argument("directory", type=Path)
    assemble.add_argument("output", type=Path)
    assemble.add_argument("version")
    args = parser.parse_args()
    if args.command == "validate":
        validate_tag(args.tag)
    elif args.command == "validate-build":
        validate_build(args.tag)
    elif args.command == "package":
        package(args.directory, args.output, args.version, args.target)
    elif args.command == "checksums":
        checksums(args.directory, args.version)
    else:
        assemble_build(args.directory, args.output, args.version)


if __name__ == "__main__":
    main()
