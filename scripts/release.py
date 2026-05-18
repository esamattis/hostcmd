#!/usr/bin/env python3
"""Release script for creating new versioned releases."""

import re
import subprocess
import sys


def run(*cmd: str, check: bool = True) -> subprocess.CompletedProcess:
    """Run a shell command and return the result."""
    return subprocess.run(cmd, capture_output=True, text=True, check=check)


def run_shell(cmd: str, check: bool = True) -> subprocess.CompletedProcess:
    """Run a shell command through bash and return the result."""
    return subprocess.run(
        ["bash", "-c", cmd], capture_output=True, text=True, check=check
    )


def is_valid_semver(version: str) -> bool:
    """Validate a version string against SemVer 2.0.0."""
    # SemVer regex pattern from semver.org
    pattern = (
        r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
        r"(?:-((?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)"
        r"(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))"
        r"?(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$"
    )
    return bool(re.match(pattern, version))


def semver_compare(a: str, b: str) -> int:
    """Compare two SemVer version strings.

    Returns:
        -1 if a < b, 0 if a == b, 1 if a > b.
    """
    def parse(v: str) -> tuple:
        # Extract main version and prerelease
        match = re.match(
            r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
            r"(?:-(.+))?(?:\+.+)?$",
            v,
        )
        if not match:
            return (0, 0, 0, None)
        major, minor, patch = int(match.group(1)), int(match.group(2)), int(match.group(3))
        pre = match.group(4)
        return (major, minor, patch, pre)

    def split_pre(pre: str | None) -> list:
        if pre is None:
            return []
        parts = []
        for part in pre.split("."):
            if re.match(r"^\d+$", part):
                parts.append((0, int(part)))
            else:
                parts.append((1, part))
        return parts

    a_parsed = parse(a)
    b_parsed = parse(b)

    # Compare major, minor, patch
    for i in range(3):
        if a_parsed[i] < b_parsed[i]:
            return -1
        if a_parsed[i] > b_parsed[i]:
            return 1

    # A version without prerelease has higher precedence
    a_pre = split_pre(a_parsed[3])
    b_pre = split_pre(b_parsed[3])

    if not a_pre and b_pre:
        return 1
    if a_pre and not b_pre:
        return -1

    # Compare prerelease identifiers
    for a_id, b_id in zip(a_pre, b_pre):
        if a_id < b_id:
            return -1
        if a_id > b_id:
            return 1

    if len(a_pre) < len(b_pre):
        return -1
    if len(a_pre) > len(b_pre):
        return 1

    return 0


def main() -> int:
    """Run the release workflow."""
    # Check for uncommitted changes
    status_result = run("git", "status", "--porcelain", check=False)
    if status_result.stdout.strip():
        print(
            "Error: There are uncommitted changes or untracked files. "
            "Please commit or stash them before releasing.",
            file=sys.stderr,
        )
        return 1

    # Check current branch
    branch_result = run("git", "rev-parse", "--abbrev-ref", "HEAD")
    current_branch = branch_result.stdout.strip()
    if current_branch != "main":
        print(
            f"Error: You must be on the 'main' branch to release. "
            f"Current branch: '{current_branch}'.",
            file=sys.stderr,
        )
        return 1

    # Find the latest git tag prefixed with "v"
    tag_result = run_shell("git tag -l 'v*' | sort -V | tail -n 1", check=False)
    latest_tag = tag_result.stdout.strip()

    if latest_tag:
        print(f"Latest release tag: {latest_tag}")
    else:
        print("No release tags found.")

    # Ask for new version
    new_version = input("Enter new version (without v prefix): ").strip()

    if not is_valid_semver(new_version):
        print(f"Invalid semver: {new_version}", file=sys.stderr)
        return 1

    new_tag = f"v{new_version}"

    if latest_tag:
        latest_version = latest_tag.lstrip("v")
        if semver_compare(new_version, latest_version) < 0:
            print(
                f"New version {new_version} must be greater than latest tag {latest_tag}",
                file=sys.stderr,
            )
            return 1

    # Update version in Cargo.toml
    cargo_toml_path = "Cargo.toml"
    try:
        with open(cargo_toml_path, "r", encoding="utf-8") as f:
            cargo_toml_content = f.read()
    except FileNotFoundError:
        print(f"Error: {cargo_toml_path} not found.", file=sys.stderr)
        return 1

    updated_cargo_toml = re.sub(
        r'^version = ".*"$',
        f'version = "{new_version}"',
        cargo_toml_content,
        flags=re.MULTILINE,
    )

    with open(cargo_toml_path, "w", encoding="utf-8") as f:
        f.write(updated_cargo_toml)

    print("Running cargo build to update Cargo.lock...")
    run("cargo", "build")
    run("git", "add", cargo_toml_path, "Cargo.lock")
    run("git", "commit", "-m", f"Bump version to {new_version}")

    # Check if the tag already exists locally or remotely
    local_tag_result = run("git", "tag", "-l", new_tag, check=False)
    local_tag_exists = local_tag_result.stdout.strip() == new_tag

    remote_tag_result = run(
        "git", "ls-remote", "--tags", "origin", new_tag, check=False
    )
    remote_tag_exists = f"refs/tags/{new_tag}" in remote_tag_result.stdout.strip()

    if local_tag_exists or remote_tag_exists:
        print("")
        print("╔══════════════════════════════════════════════════════════════════╗")
        print("║                     ⚠️  DESTRUCTIVE WARNING  ⚠️                  ║")
        print("╠══════════════════════════════════════════════════════════════════╣")
        if local_tag_exists:
            print(f"║  Local tag '{new_tag}' already exists and will be deleted.       ║")
        if remote_tag_exists:
            print(f"║  Remote tag '{new_tag}' already exists and will be deleted.      ║")
        print("╚══════════════════════════════════════════════════════════════════╝")
        print("")

        confirmation = input(
            "Are you sure you want to delete and recreate this tag? (yes/no): "
        ).strip()

        if confirmation.lower() != "yes":
            print("Release aborted.")
            return 1

    if local_tag_exists:
        print(f"Deleting existing local tag {new_tag}...")
        run("git", "tag", "-d", new_tag)

    if remote_tag_exists:
        print(f"Deleting existing remote tag {new_tag}...")
        run("git", "push", "origin", "--delete", new_tag)

    run("git", "tag", "-a", new_tag, "-m", f"Release {new_tag}")
    run("git", "push", "origin", "HEAD")
    run("git", "push", "origin", new_tag)
    print(f"Released {new_tag}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
