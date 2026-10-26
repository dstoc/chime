#!/usr/bin/env python3
"""Synchronize workspace package versions on the open Release Please PR."""

from __future__ import annotations

import copy
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
from typing import Any


RELEASE_BRANCH = "release-please--branches--main"
BASE_BRANCH = "main"
WORKSPACE_PACKAGES = {"chime-cli", "chime-protocol", "chime-server"}


class SyncError(Exception):
    """An invalid release PR or unsafe lockfile update."""


def run(command: list[str], *, cwd: Path | None = None) -> str:
    result = subprocess.run(
        command,
        cwd=cwd,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip()
        raise SyncError(f"{command[0]} command failed: {detail}")
    return result.stdout


def read_json_object(value: str, source: str) -> dict[str, Any]:
    try:
        result = json.loads(value)
    except json.JSONDecodeError as error:
        raise SyncError(f"{source} returned invalid JSON: {error}") from error
    if not isinstance(result, dict):
        raise SyncError(f"{source} must be a JSON object")
    return result


def release_pr_identity(value: str) -> tuple[int, str, str]:
    action_pr = read_json_object(value, "Release Please")
    number = action_pr.get("number")
    head = action_pr.get("headBranchName")
    base = action_pr.get("baseBranchName")
    if type(number) is not int or number < 1:
        raise SyncError("Release Please returned an invalid PR number")
    if not isinstance(head, str) or not isinstance(base, str):
        raise SyncError("Release Please returned an invalid PR branch identity")
    if head != RELEASE_BRANCH or base != BASE_BRANCH:
        raise SyncError("Release Please returned a PR outside the expected release branch")
    return number, head, base


def live_pr_identity(value: str) -> tuple[str, int, str, str, str, str]:
    live_pr = read_json_object(value, "GitHub")
    try:
        number = live_pr["number"]
        state = live_pr["state"]
        head = live_pr["head"]
        base = live_pr["base"]
        head_branch = head["ref"]
        head_repo = head["repo"]["full_name"]
        head_sha = head["sha"]
        base_branch = base["ref"]
    except (KeyError, TypeError) as error:
        raise SyncError("GitHub returned an incomplete PR identity") from error

    if type(number) is not int or not isinstance(state, str):
        raise SyncError("GitHub returned an invalid PR number or state")
    if not all(
        isinstance(value, str)
        for value in (head_branch, head_repo, head_sha, base_branch)
    ):
        raise SyncError("GitHub returned an invalid PR branch identity")
    if not re.fullmatch(r"[0-9a-f]{40}", head_sha):
        raise SyncError("GitHub returned an invalid PR head commit")
    return state, number, head_branch, base_branch, head_repo, head_sha


def local_workspace_packages(lock: dict[str, Any]) -> list[dict[str, Any]]:
    return [
        package
        for package in lock.get("package", [])
        if package.get("name") in WORKSPACE_PACKAGES and "source" not in package
    ]


def check_lockfile(before_path: Path, lock_path: Path, manifest_path: Path) -> None:
    try:
        before = tomllib.loads(before_path.read_text())
        after = tomllib.loads(lock_path.read_text())
        manifest = tomllib.loads(manifest_path.read_text())
        expected_version = manifest["workspace"]["package"]["version"]
    except (OSError, tomllib.TOMLDecodeError, KeyError, TypeError) as error:
        raise SyncError(f"could not validate Cargo.lock and Cargo.toml: {error}") from error

    before_local = local_workspace_packages(before)
    after_local = local_workspace_packages(after)
    expected_names = WORKSPACE_PACKAGES
    if {package["name"] for package in before_local} != expected_names or len(
        before_local
    ) != len(expected_names):
        raise SyncError("Cargo.lock does not contain exactly the three local workspace packages")
    if {package["name"] for package in after_local} != expected_names or len(
        after_local
    ) != len(expected_names):
        raise SyncError("updated Cargo.lock does not contain exactly the three local workspace packages")

    if any(package.get("version") != expected_version for package in after_local):
        raise SyncError("Cargo.lock workspace versions do not match Cargo.toml")

    normalized_before = copy.deepcopy(before)
    normalized_after = copy.deepcopy(after)
    for document in (normalized_before, normalized_after):
        for package in document["package"]:
            if package.get("name") in WORKSPACE_PACKAGES and "source" not in package:
                package["version"] = "<workspace-version>"
    if normalized_before != normalized_after:
        raise SyncError("cargo update --workspace changed lockfile data outside workspace versions")


def synchronize() -> None:
    action_output = os.environ.get("RELEASE_PLEASE_PR", "").strip()
    if not action_output:
        print("Release Please returned no open PR; lockfile synchronization is a no-op")
        return

    repository = os.environ.get("GITHUB_REPOSITORY", "")
    if not re.fullmatch(r"[^/]+/[^/]+", repository):
        raise SyncError("GITHUB_REPOSITORY is missing or invalid")
    if not os.environ.get("GH_TOKEN"):
        raise SyncError("GH_TOKEN is required to validate the Release Please PR")

    number, branch, base = release_pr_identity(action_output)
    response = run(["gh", "api", f"repos/{repository}/pulls/{number}"])
    state, live_number, live_head, live_base, live_repo, head_sha = live_pr_identity(
        response
    )
    if (
        live_number != number
        or live_head != branch
        or live_base != base
        or live_repo.lower() != repository.lower()
    ):
        raise SyncError("Release Please PR does not match the live pull request identity")
    if state == "closed":
        print(f"Release Please PR #{number} is closed; lockfile synchronization is a no-op")
        return
    if state != "open":
        raise SyncError(f"GitHub returned an unsupported PR state: {state}")

    root = Path(run(["git", "rev-parse", "--show-toplevel"]).strip())
    if run(["git", "status", "--porcelain", "--untracked-files=all"], cwd=root).strip():
        raise SyncError("checkout must be clean before release PR synchronization")

    run(["git", "fetch", "--no-tags", "origin", f"refs/heads/{branch}"], cwd=root)
    fetched_sha = run(["git", "rev-parse", "--verify", "FETCH_HEAD^{commit}"], cwd=root).strip()
    if fetched_sha != head_sha:
        raise SyncError("Release Please PR head changed while it was being validated")
    run(["git", "checkout", "--detach", fetched_sha], cwd=root)

    lock_path = root / "Cargo.lock"
    with tempfile.TemporaryDirectory(prefix="chime-release-lock-") as temporary:
        before_path = Path(temporary) / "Cargo.lock"
        before_path.write_bytes(lock_path.read_bytes())
        run(["cargo", "update", "--workspace"], cwd=root)
        check_lockfile(before_path, lock_path, root / "Cargo.toml")

        status = run(
            ["git", "status", "--porcelain", "--untracked-files=all"], cwd=root
        ).strip()
        if not status:
            print("Cargo.lock is already synchronized; no commit or push is needed")
            return
        if status != "M Cargo.lock":
            raise SyncError(f"cargo update changed files outside Cargo.lock: {status}")

        run(["git", "add", "--", "Cargo.lock"], cwd=root)
        run(
            [
                "git",
                "-c",
                "user.name=github-actions[bot]",
                "-c",
                "user.email=41898282+github-actions[bot]@users.noreply.github.com",
                "commit",
                "-m",
                "fix: synchronize workspace versions in Cargo.lock",
            ],
            cwd=root,
        )
        run(["git", "push", "origin", f"HEAD:refs/heads/{branch}"], cwd=root)
        print(f"Synchronized Cargo.lock on Release Please PR #{number}")


def main() -> int:
    try:
        synchronize()
    except SyncError as error:
        print(f"release PR lockfile synchronization failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
