import hashlib
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
SYNC_SCRIPT = ROOT / "scripts" / "sync-release-pr-lockfile.py"
VERSION_NAMES = {"chime-cli", "chime-protocol", "chime-server"}


def workflow_run_script(step_name: str) -> str:
    lines = RELEASE_WORKFLOW.read_text().splitlines()
    marker = f"      - name: {step_name}"
    step_start = lines.index(marker)
    run_start = next(
        (
            index
            for index in range(step_start + 1, len(lines))
            if lines[index] == "        run: |"
            or lines[index].startswith("      - name:")
        ),
        None,
    )
    if run_start is None or lines[run_start] != "        run: |":
        raise AssertionError(f"workflow step has no run block: {step_name}")

    script_lines = []
    for line in lines[run_start + 1 :]:
        if line.strip() and not line.startswith("          "):
            break
        script_lines.append(line[10:] if line.startswith("          ") else "")
    return "\n".join(script_lines) + "\n"


class ReleaseConfigurationTests(unittest.TestCase):
    def test_single_workspace_version_and_no_lockfile_selectors(self) -> None:
        config = json.loads((ROOT / "release-please-config.json").read_text())
        manifest = json.loads((ROOT / ".release-please-manifest.json").read_text())
        workspace = (ROOT / "Cargo.toml").read_text()
        package = config["packages"]["."]

        self.assertEqual(config["bootstrap-sha"], "d4705eb0fee32efab588ff5c5dd7f3b7ca1ce2fe")
        self.assertIs(config["draft"], True)
        self.assertIs(config["force-tag-creation"], True)
        self.assertIs(config["include-component-in-tag"], False)
        self.assertEqual(set(config["packages"]), {"."})
        self.assertEqual(package["release-type"], "simple")
        self.assertEqual(package["version-file"], "release-version.txt")
        self.assertEqual(package["changelog-path"], "CHANGELOG.md")

        cargo_version = re.search(
            r"(?ms)^\[workspace\.package\]\s+version\s*=\s*\"([^\"]+)\"",
            workspace,
        )
        self.assertIsNotNone(cargo_version)
        version = cargo_version.group(1)
        self.assertEqual((ROOT / "release-version.txt").read_text().strip(), version)
        self.assertEqual(manifest, {".": version})

        lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
        local_packages = [
            entry
            for entry in lock["package"]
            if entry["name"] in VERSION_NAMES and "source" not in entry
        ]
        self.assertEqual(len(local_packages), len(VERSION_NAMES))
        self.assertEqual({entry["name"] for entry in local_packages}, VERSION_NAMES)
        self.assertEqual({entry["version"] for entry in local_packages}, {version})

        extra_files = package["extra-files"]
        self.assertEqual(
            [(entry["path"], entry["jsonpath"]) for entry in extra_files],
            [("Cargo.toml", "$.workspace.package.version")],
        )
        for name in VERSION_NAMES:
            member = tomllib.loads(
                (ROOT / "crates" / name / "Cargo.toml").read_text()
            )
            self.assertEqual(member["package"]["version"], {"workspace": True})

    def test_release_workflow_has_one_same_run_draft_and_recovery_path(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text()
        lowered = workflow.lower()

        self.assertIn("googleapis/release-please-action@v4", workflow)
        self.assertIn("steps.release_please.outputs.pr", workflow)
        self.assertIn("scripts/sync-release-pr-lockfile.py", workflow)
        self.assertIn("ref: ${{ github.sha }}", workflow)
        self.assertIn("persist-credentials: true", workflow)
        self.assertIn("branches: [main]", workflow)
        self.assertIn("workflow_dispatch:", workflow)
        self.assertIn("SELECTED_REF", workflow)
        self.assertIn('refs/heads/main', workflow)
        self.assertIn('git rev-parse --verify "$TAG^{commit}"', workflow)
        self.assertIn("concurrency:", workflow)
        self.assertIn("group: chime-release", workflow)
        self.assertIn("cancel-in-progress: false", workflow)
        self.assertIn("secrets.GITHUB_TOKEN", workflow)
        self.assertIn("docker/build-push-action@v6", workflow)
        self.assertIn("platforms: linux/amd64", workflow)
        self.assertIn("actions/attest@v4", workflow)
        self.assertIn("gh attestation verify", workflow)
        self.assertNotIn("--source-digest", workflow)
        self.assertEqual(workflow.count("--source-ref refs/heads/main"), 4)
        self.assertIn('cmp --silent "dist/$CLI_ARCHIVE" "$reuse_dir/$CLI_ARCHIVE"', workflow)
        self.assertIn("--signer-workflow", workflow)
        self.assertIn("--bundle-from-oci", workflow)
        self.assertIn("draft=false", workflow)
        self.assertNotIn("release:", lowered)
        self.assertNotIn("--clobber", workflow)
        self.assertIn("cmp --silent", workflow)
        self.assertIn("already contains", workflow)

        self.assertNotIn("releases/tags/$TAG", workflow)
        self.assertNotIn('gh release download', workflow)
        self.assertNotIn('gh release upload', workflow)
        self.assertNotIn('gh release edit', workflow)
        self.assertIn("releases?per_page=100", workflow)
        self.assertIn("uploads.github.com", workflow)
        self.assertLess(workflow.index("releases?per_page=100"), workflow.index("docker/build-push-action@v6"))
        self.assertLess(workflow.index("docker/build-push-action@v6"), workflow.index("actions/attest@v4"))
        self.assertLess(workflow.index("gh attestation verify"), workflow.index("draft=false"))
        self.assertLess(
            workflow.index("Update the release PR or create a draft release"),
            workflow.index("Synchronize the release PR Cargo.lock"),
        )
        self.assertLess(
            workflow.index("Synchronize the release PR Cargo.lock"),
            workflow.index("Export release outputs"),
        )

    def test_release_asset_upload_uses_github_upload_host(self) -> None:
        script = workflow_run_script("Attach or reuse draft release assets without replacement")
        self.assertNotIn("--hostname uploads.github.com", script)
        self.assertNotIn("api.uploads.github.com", script.split("encoded_name=", 1)[1])
        self.assertIn('"https://uploads.github.com/repos/$GITHUB_REPOSITORY/releases/$RELEASE_ID/assets?name=$encoded_name"', script)
        self.assertIn('--data-binary "@$path"', script)
        self.assertIn('--header "Authorization: Bearer $GH_TOKEN"', script)
        self.assertIn('curl --fail-with-body --silent --show-error', script)
        self.assertIn('.name == $name and .state == "uploaded"', script)

        # Exercise the actual upload snippet with a mock HTTP client. Assert
        # that it carries the full asset bytes, token, and encoded name.
        snippet = script.split('encoded_name=$(jq -nr', 1)[1]
        snippet = 'encoded_name=$(jq -nr' + snippet.split("\n  elif [[", 1)[0]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            asset = root / "asset name.tar.gz"
            asset.write_bytes(bytes([0, 255]) + b"archive" + bytes([0]))
            fake_bin = root / "bin"
            fake_bin.mkdir()
            log = root / "curl-args"
            (fake_bin / "curl").write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "from pathlib import Path\n"
                "args = sys.argv[1:]\n"
                "Path(os.environ['MOCK_CURL_LOG']).write_text(json.dumps(args))\n"
                "path = args[args.index('--data-binary')+1][1:]\n"
                "assert Path(path).read_bytes() == bytes([0, 255]) + b'archive' + bytes([0])\n"
                "print(json.dumps({'name': Path(path).name, 'state': 'uploaded', 'id': 123}))\n"
            )
            (fake_bin / "curl").chmod(0o755)
            env = os.environ.copy()
            env.update(
                PATH=f"{fake_bin}:{env['PATH']}",
                GH_TOKEN="test-token",
                GITHUB_REPOSITORY="dstoc/chime",
                RELEASE_ID="42",
                MOCK_CURL_LOG=str(log),
            )
            result = subprocess.run(
                ["bash", "-euo", "pipefail", "-c", snippet],
                cwd=root,
                env={**env, "path": str(asset), "name": asset.name},
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            args = json.loads(log.read_text())
            self.assertIn(
                "https://uploads.github.com/repos/dstoc/chime/releases/42/assets?name=asset%20name.tar.gz",
                args,
            )
            self.assertIn("Authorization: Bearer test-token", args)

    def test_draft_release_lookup_rejects_missing_or_published_releases(self) -> None:
        script = workflow_run_script("Verify draft, source, and shared workspace version")
        self.assertIn("releases?per_page=100", script)
        self.assertIn("length) == 1", script)
        self.assertIn("$matches[0].draft == true", script)
        self.assertIn("$matches[0].published_at == null", script)
        self.assertIn("releases/$release_id", script)

    def test_recovery_uses_current_main_workflow_and_tagged_source(self) -> None:
        script = workflow_run_script("Validate the selected draft tag")
        self.assertIn('refs/heads/main', script)
        self.assertIn('git rev-parse --verify "$TAG^{commit}"', script)
        self.assertIn('git merge-base --is-ancestor "$source_sha" HEAD', script)
        self.assertIn('echo "source_sha=$source_sha"', script)

    def test_quadlet_and_operator_docs_use_published_images(self) -> None:
        unit = (ROOT / "examples" / "quadlet" / "chime.container").read_text()
        self.assertIn("Image=ghcr.io/dstoc/chime:0.2.0", unit)
        self.assertFalse((ROOT / "examples" / "quadlet" / "chime.build").exists())
        self.assertFalse((ROOT / "examples" / "quadlet" / "chime.pod").exists())

        operator = (ROOT / "docs" / "operator-guide.md").read_text().lower()
        readme = (ROOT / "README.md").read_text().lower()
        releases = (ROOT / "docs" / "releases.md").read_text().lower()
        combined = operator + readme
        for stale in ("chime.build", "chime-build.service", "cargo build"):
            self.assertNotIn(stale, combined)
        self.assertIn("ghcr.io/dstoc/chime", operator)
        self.assertIn("digest", operator)
        self.assertIn("docs/releases.md", readme)
        self.assertIn("gh attestation verify", releases)
        self.assertIn("immutable", releases)
        self.assertIn("public", releases)

    def test_cli_packaging_is_deterministic_and_checksum_covers_archive(self) -> None:
        helper = ROOT / "scripts" / "package-cli.sh"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "chime"
            binary.write_text("#!/bin/sh\nprintf 'chime 0.2.0\\n'\n")
            binary.chmod(0o755)
            first = root / "first"
            second = root / "second"

            for output in (first, second):
                subprocess.run(
                    [str(helper), "0.2.0", "1793006554", str(binary), str(output)],
                    check=True,
                    cwd=ROOT,
                    capture_output=True,
                    text=True,
                )

            archive_name = "chime-v0.2.0-x86_64-unknown-linux-gnu.tar.gz"
            archive = first / archive_name
            self.assertEqual(archive.read_bytes(), (second / archive_name).read_bytes())
            self.assertEqual(
                (first / f"{archive_name}.sha256").read_text(),
                (second / f"{archive_name}.sha256").read_text(),
            )
            expected = hashlib.sha256(archive.read_bytes()).hexdigest()
            self.assertEqual(
                (first / f"{archive_name}.sha256").read_text().split()[0], expected
            )
            with tarfile.open(archive, "r:gz") as bundle:
                self.assertEqual(set(bundle.getnames()), {"chime", "LICENSE"})
                executable = bundle.getmember("chime")
                self.assertTrue(executable.mode & 0o111)
                self.assertEqual(bundle.getmember("LICENSE").mode, 0o644)


    def test_image_smoke_state_is_private_and_writable_by_container_user(self) -> None:
        script = workflow_run_script("Verify anonymous AMD64 pull and server response")

        # The image runs as UID/GID 10001 and chmods its state directory to 0700.
        # A host-owned bind mount (even mode 0777) cannot be chmodded by that user.
        self.assertIn(
            "--tmpfs /var/lib/chime:rw,noexec,nosuid,size=64m,uid=10001,gid=10001,mode=0700",
            script,
        )
        self.assertNotIn('state_dir=$(mktemp -d)', script)
        self.assertNotIn('--volume "$state_dir:/var/lib/chime:rw"', script)
        self.assertIn("--read-only", script)
        self.assertIn("--network host", script)
        self.assertIn('curl --fail --silent http://127.0.0.1:8080/', script)

    def test_cli_runtime_gate_extracts_and_runs_the_archive_binary(self) -> None:
        version = "0.2.0"
        archive_name = f"chime-v{version}-x86_64-unknown-linux-gnu.tar.gz"
        script = workflow_run_script("Run the CLI in a clean Debian runtime")

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            dist = root / "dist"
            runner_temp = root / "runner-temp"
            fake_bin = root / "bin"
            runner_temp.mkdir()
            fake_bin.mkdir()

            binary = root / "chime"
            binary.write_text("#!/bin/sh\nprintf 'chime 0.2.0\\n'\n")
            binary.chmod(0o755)
            subprocess.run(
                [
                    str(ROOT / "scripts" / "package-cli.sh"),
                    version,
                    "1793006554",
                    str(binary),
                    str(dist),
                ],
                check=True,
                cwd=ROOT,
                capture_output=True,
                text=True,
            )

            (fake_bin / "apt-get").write_text("#!/bin/sh\nexit 0\n")
            (fake_bin / "apt-get").chmod(0o755)
            (fake_bin / "docker").write_text(
                """#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == run ]]
shift
mount=""
container_env=""
inner_script=""
while (($#)); do
  case "$1" in
    --volume) mount=$2; shift 2 ;;
    --env) container_env=$2; shift 2 ;;
    sh)
      [[ "$2" == -ec ]]
      inner_script=$3
      shift 3
      ;;
    *) shift ;;
  esac
done
[[ "$mount" == *:/dist:ro ]]
host_dir=${mount%:/dist:ro}
[[ "$container_env" == EXPECTED_VERSION=* ]]
export EXPECTED_VERSION=${container_env#EXPECTED_VERSION=}
[[ "$inner_script" == *"/dist/chime --version"* ]]
inner_script=$(printf '%s' "$inner_script" | sed "s|/dist|$host_dir|g")
sh -ec "$inner_script"
"""
            )
            (fake_bin / "docker").chmod(0o755)

            environment = os.environ.copy()
            environment.update(
                {
                    "PATH": f"{fake_bin}:{environment['PATH']}",
                    "VERSION": version,
                    "CLI_ARCHIVE": archive_name,
                    "RUNNER_TEMP": str(runner_temp),
                }
            )
            result = subprocess.run(
                ["bash", "-euo", "pipefail", "-c", script],
                cwd=root,
                env=environment,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "")
            self.assertTrue((runner_temp / "chime-cli-runtime" / "chime").is_file())

    def test_server_smoke_gate_passes_valid_startup_arguments(self) -> None:
        script = workflow_run_script("Verify anonymous AMD64 pull and server response")
        digest = "sha256:" + "a" * 64
        expected_origin = "https://chime.example-tailnet.ts.net"

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            runner_temp = root / "runner-temp"
            fake_bin = root / "bin"
            runner_temp.mkdir()
            fake_bin.mkdir()
            (fake_bin / "docker").write_text(
                """#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  logout|pull|rm) exit 0 ;;
  image)
    if [[ "$2" == rm ]]; then
      exit 0
    elif [[ "$2" == inspect && "$4" == '{{.Os}}/{{.Architecture}}' ]]; then
      printf 'linux/amd64\\n'
    elif [[ "$2" == inspect ]]; then
      printf 'ghcr.io/dstoc/chime@%s\\n' "$IMAGE_DIGEST"
    else
      exit 90
    fi
    ;;
  run)
    shift
    origin=""
    data_dir=""
    state_tmpfs=""
    image_found=false
    previous=""
    for argument in "$@"; do
      case "$previous" in
        --origin) origin=$argument ;;
        --data-dir) data_dir=$argument ;;
        --tmpfs) [[ "$argument" == /var/lib/chime:* ]] && state_tmpfs=$argument ;;
      esac
      [[ "$argument" == "ghcr.io/dstoc/chime@$IMAGE_DIGEST" ]] && image_found=true
      previous=$argument
    done
    [[ "$origin" == "$EXPECTED_ORIGIN" ]]
    [[ "$data_dir" == /var/lib/chime ]]
    [[ "$state_tmpfs" == /var/lib/chime:rw,noexec,nosuid,size=64m,uid=10001,gid=10001,mode=0700 ]]
    [[ "$image_found" == true ]]
    ;;
  *) exit 90 ;;
esac
"""
            )
            (fake_bin / "docker").chmod(0o755)
            (fake_bin / "curl").write_text(
                """#!/usr/bin/env bash
set -euo pipefail
output=""
while (($#)); do
  if [[ "$1" == --output ]]; then
    output=$2
    shift 2
  else
    shift
  fi
done
printf '<h1>Chime</h1>\\n' > "$output"
"""
            )
            (fake_bin / "curl").chmod(0o755)

            environment = os.environ.copy()
            environment.update(
                {
                    "PATH": f"{fake_bin}:{environment['PATH']}",
                    "IMAGE_DIGEST": digest,
                    "IMAGE_REF": "ghcr.io/dstoc/chime:0.2.0",
                    "EXPECTED_ORIGIN": expected_origin,
                    "RUNNER_TEMP": str(runner_temp),
                    "GITHUB_RUN_ID": "test-11",
                }
            )
            result = subprocess.run(
                ["bash", "-euo", "pipefail", "-c", script],
                cwd=root,
                env=environment,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_workflow_markdown_backticks_are_not_double_quoted_echoes(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text()
        self.assertIsNone(re.search(r'(?m)^\s*echo\s+"[^"\n]*`', workflow))
        self.assertIn("printf -- '- Source: `%s`\\n'", workflow)
        self.assertIn("printf 'GHCR already contains `%s` at `%s`", workflow)


class ReleasePrLockfileSyncTests(unittest.TestCase):
    branch = "release-please--branches--main"
    repository = "dstoc/chime"
    pull_number = 42

    def setUp(self) -> None:
        self.cargo = shutil.which("cargo")
        if not self.cargo:
            self.skipTest("cargo is required for release lockfile sync tests")

        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "checkout"
        self.remote = self.root / "origin.git"
        self.fake_bin = self.root / "bin"
        self.fake_bin.mkdir()
        self.live_pr_file = self.root / "live-pr.json"
        self.gh_call_log = self.root / "gh-calls.txt"
        self.push_call_log = self.root / "push-calls.txt"

        self.git(self.root, "init", "--bare", str(self.remote))
        self.repo.mkdir()
        self.git(self.repo, "init", "--initial-branch=main")
        self.git(self.repo, "config", "user.name", "Release Sync Test")
        self.git(self.repo, "config", "user.email", "release-sync-test@example.invalid")

        members = sorted(VERSION_NAMES)
        member_list = ", ".join(json.dumps(f"crates/{name}") for name in members)
        (self.repo / "Cargo.toml").write_text(
            "[workspace]\n"
            f"members = [{member_list}]\n"
            'exclude = ["support/external-fixture"]\n'
            'resolver = "3"\n\n'
            "[workspace.package]\n"
            'version = "0.1.0"\n'
            'edition = "2021"\n'
        )
        for name in members:
            crate = self.repo / "crates" / name
            (crate / "src").mkdir(parents=True)
            dependencies = (
                "\n[dependencies]\n"
                'external-fixture = { path = "../../support/external-fixture" }\n'
                if name == "chime-cli"
                else ""
            )
            (crate / "Cargo.toml").write_text(
                "[package]\n"
                f'name = "{name}"\n'
                "version.workspace = true\n"
                "edition.workspace = true\n"
                f"{dependencies}"
            )
            (crate / "src" / "lib.rs").write_text("pub fn fixture() {}\n")
        external = self.repo / "support" / "external-fixture"
        (external / "src").mkdir(parents=True)
        (external / "Cargo.toml").write_text(
            "[package]\n"
            'name = "external-fixture"\n'
            'version = "1.0.0"\n'
            'edition = "2021"\n'
        )
        (external / "src" / "lib.rs").write_text("pub fn external_fixture() {}\n")

        self.cargo_run("generate-lockfile")
        self.git(self.repo, "add", "--all")
        self.git(self.repo, "commit", "-m", "chore: seed test workspace")
        self.git(self.repo, "remote", "add", "origin", str(self.remote))
        self.git(self.repo, "push", "-u", "origin", "main")

        self.git(self.repo, "checkout", "-b", self.branch)
        manifest = self.repo / "Cargo.toml"
        manifest.write_text(manifest.read_text().replace('version = "0.1.0"', 'version = "0.2.0"'))
        self.git(self.repo, "add", "Cargo.toml")
        self.git(self.repo, "commit", "-m", "chore(main): release 0.2.0")
        self.git(self.repo, "push", "-u", "origin", self.branch)
        self.git(self.repo, "checkout", "main")

        (self.fake_bin / "gh").write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            "printf '%s\\n' \"$*\" >> \"$GH_CALL_LOG\"\n"
            "cat \"$LIVE_PR_FILE\"\n"
            'if [ -n "${ADVANCE_REMOTE_SHA:-}" ]; then\n'
            '  git --git-dir "$REMOTE_GIT_DIR" update-ref '
            '"refs/heads/release-please--branches--main" "$ADVANCE_REMOTE_SHA"\n'
            "fi\n"
        )
        (self.fake_bin / "gh").chmod(0o755)
        self.environment = os.environ.copy()
        self.environment.update(
            {
                "PATH": f"{self.fake_bin}{os.pathsep}{self.environment['PATH']}",
                "GITHUB_REPOSITORY": self.repository,
                "GH_TOKEN": "test-token",
                "LIVE_PR_FILE": str(self.live_pr_file),
                "GH_CALL_LOG": str(self.gh_call_log),
                "PUSH_CALL_LOG": str(self.push_call_log),
                "REMOTE_GIT_DIR": str(self.remote),
            }
        )

    @staticmethod
    def git(cwd: Path, *arguments: str) -> str:
        result = subprocess.run(
            ["git", *arguments],
            cwd=cwd,
            check=True,
            capture_output=True,
            text=True,
        )
        return result.stdout.strip()

    def cargo_run(self, *arguments: str) -> None:
        subprocess.run(
            [self.cargo, *arguments],
            cwd=self.repo,
            check=True,
            capture_output=True,
            text=True,
        )

    def remote_ref(self, branch: str) -> str:
        return self.git(
            self.root,
            "--git-dir",
            str(self.remote),
            "rev-parse",
            f"refs/heads/{branch}",
        )

    def remote_refs(self) -> dict[str, str]:
        output = self.git(
            self.root,
            "--git-dir",
            str(self.remote),
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
        )
        return dict(line.split() for line in output.splitlines())

    def action_pr(self, **overrides: object) -> str:
        value: dict[str, object] = {
            "number": self.pull_number,
            "headBranchName": self.branch,
            "baseBranchName": "main",
        }
        value.update(overrides)
        return json.dumps(value)

    def live_pr(self, **overrides: object) -> dict[str, object]:
        value: dict[str, object] = {
            "number": self.pull_number,
            "state": "open",
            "head": {
                "ref": self.branch,
                "repo": {"full_name": self.repository},
                "sha": self.remote_ref(self.branch),
            },
            "base": {"ref": "main"},
        }
        value.update(overrides)
        return value

    def run_sync(
        self,
        action_pr: str | None = None,
        live_pr: dict[str, object] | None = None,
        extra_environment: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        environment = self.environment.copy()
        environment["RELEASE_PLEASE_PR"] = action_pr or ""
        if live_pr is not None:
            self.live_pr_file.write_text(json.dumps(live_pr))
        if extra_environment:
            environment.update(extra_environment)
        return subprocess.run(
            [shutil.which("python3") or "python3", str(SYNC_SCRIPT)],
            cwd=self.repo,
            env=environment,
            check=False,
            capture_output=True,
            text=True,
        )

    def install_cargo_wrapper(self, after_update: str) -> None:
        wrapper = self.fake_bin / "cargo"
        wrapper.write_text(
            "#!/usr/bin/env python3\n"
            "import os, subprocess, sys\n"
            f"subprocess.run([{json.dumps(self.cargo)}, *sys.argv[1:]], check=True)\n"
            f"{after_update}\n"
        )
        wrapper.chmod(0o755)

    def install_pre_push_log(self) -> None:
        hook = self.repo / ".git" / "hooks" / "pre-push"
        hook.write_text(
            "#!/bin/sh\n"
            'printf "push\\n" >> "$PUSH_CALL_LOG"\n'
        )
        hook.chmod(0o755)

    def synchronize_remote_branch(self) -> None:
        self.git(self.repo, "checkout", self.branch)
        self.cargo_run("update", "--workspace")
        self.git(self.repo, "add", "Cargo.lock")
        self.git(self.repo, "commit", "-m", "fix: synchronize workspace versions in Cargo.lock")
        self.git(self.repo, "push", "origin", self.branch)
        self.git(self.repo, "checkout", "main")

    def test_open_release_pr_gets_workspace_lockfile_update(self) -> None:
        before = self.remote_refs()
        live = self.live_pr()
        result = self.run_sync(self.action_pr(), live)

        self.assertEqual(result.returncode, 0, result.stderr)
        after = self.remote_refs()
        self.assertEqual(set(after), set(before))
        self.assertEqual(after["refs/heads/main"], before["refs/heads/main"])
        self.assertNotEqual(after[f"refs/heads/{self.branch}"], before[f"refs/heads/{self.branch}"])
        lock_text = self.git(
            self.root,
            "--git-dir",
            str(self.remote),
            "show",
            f"refs/heads/{self.branch}:Cargo.lock",
        )
        lock = tomllib.loads(lock_text)
        local = [
            package
            for package in lock["package"]
            if package["name"] in VERSION_NAMES and "source" not in package
        ]
        self.assertEqual(len(local), len(VERSION_NAMES))
        self.assertEqual({package["version"] for package in local}, {"0.2.0"})
        external = next(
            package for package in lock["package"] if package["name"] == "external-fixture"
        )
        self.assertEqual(external["version"], "1.0.0")
        self.assertEqual(
            self.git(
                self.root,
                "--git-dir",
                str(self.remote),
                "log",
                "-1",
                "--format=%s",
                f"refs/heads/{self.branch}",
            ),
            "fix: synchronize workspace versions in Cargo.lock",
        )

    def test_missing_and_closed_release_prs_are_no_ops(self) -> None:
        before = self.remote_refs()
        missing = self.run_sync()
        self.assertEqual(missing.returncode, 0, missing.stderr)
        self.assertFalse(self.gh_call_log.exists())

        closed_live = self.live_pr(state="closed")
        closed = self.run_sync(self.action_pr(), closed_live)
        self.assertEqual(closed.returncode, 0, closed.stderr)
        self.assertIn("is closed", closed.stdout)
        self.assertEqual(self.remote_refs(), before)

    def test_synchronized_lockfile_creates_no_commit_or_push(self) -> None:
        self.synchronize_remote_branch()
        before = self.remote_ref(self.branch)
        self.install_pre_push_log()

        result = self.run_sync(self.action_pr(), self.live_pr())

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("no commit or push", result.stdout)
        self.assertEqual(self.remote_ref(self.branch), before)
        self.assertEqual(self.git(self.repo, "rev-parse", "HEAD"), before)
        self.assertFalse(self.push_call_log.exists())

    def test_action_and_live_pr_identity_mismatches_fail_without_push(self) -> None:
        before = self.remote_refs()
        malformed = self.run_sync("not-json")
        self.assertNotEqual(malformed.returncode, 0)
        self.assertIn("returned invalid JSON", malformed.stderr)
        self.assertFalse(self.gh_call_log.exists())

        wrong_action = self.run_sync(
            self.action_pr(headBranchName="unrelated"), self.live_pr()
        )
        self.assertNotEqual(wrong_action.returncode, 0)
        self.assertIn("outside the expected release branch", wrong_action.stderr)
        self.assertFalse(self.gh_call_log.exists())

        wrong_live = self.live_pr(head={
            "ref": "unrelated",
            "repo": {"full_name": self.repository},
            "sha": self.remote_ref(self.branch),
        })
        live_result = self.run_sync(self.action_pr(), wrong_live)
        self.assertNotEqual(live_result.returncode, 0)
        self.assertIn("does not match the live", live_result.stderr)
        self.assertEqual(self.remote_refs(), before)

    def test_fetched_head_must_match_the_live_pr_sha(self) -> None:
        reported_sha = self.remote_ref(self.branch)
        concurrent_sha = self.remote_ref("main")

        result = self.run_sync(
            self.action_pr(),
            self.live_pr(),
            {"ADVANCE_REMOTE_SHA": concurrent_sha},
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("head changed while it was being validated", result.stderr)
        self.assertEqual(self.remote_ref(self.branch), concurrent_sha)
        self.assertNotEqual(self.remote_ref(self.branch), reported_sha)

    def test_dependency_drift_fails_without_updating_release_branch(self) -> None:
        self.install_cargo_wrapper(
            'from pathlib import Path\n'
            'lock = Path("Cargo.lock")\n'
            'contents = lock.read_text()\n'
            'contents = contents.replace(\'name = "external-fixture"\\nversion = "1.0.0"\', '
            '\'name = "external-fixture"\\nversion = "1.0.1"\')\n'
            'lock.write_text(contents)'
        )
        before = self.remote_refs()

        result = self.run_sync(self.action_pr(), self.live_pr())

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("changed lockfile data outside workspace versions", result.stderr)
        self.assertEqual(self.remote_refs(), before)

    def test_non_fast_forward_push_conflict_fails_without_overwriting(self) -> None:
        self.git(self.repo, "checkout", self.branch)
        (self.repo / "concurrent.txt").write_text("concurrent release PR edit\n")
        self.git(self.repo, "add", "concurrent.txt")
        self.git(self.repo, "commit", "-m", "docs: concurrent release PR edit")
        competing_sha = self.git(self.repo, "rev-parse", "HEAD")
        self.git(
            self.repo,
            "push",
            "origin",
            f"{competing_sha}:refs/heads/conflict-candidate",
        )
        self.git(self.repo, "reset", "--hard", self.remote_ref(self.branch))
        self.git(self.repo, "checkout", "main")
        self.install_cargo_wrapper(
            'subprocess.run(["git", "--git-dir", '
            'os.environ["REMOTE_GIT_DIR"], "update-ref", '
            '"refs/heads/release-please--branches--main", '
            'os.environ["COMPETING_SHA"]], check=True)'
        )
        before = self.remote_refs()

        result = self.run_sync(
            self.action_pr(),
            self.live_pr(),
            {"COMPETING_SHA": competing_sha},
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("git command failed", result.stderr)
        after = self.remote_refs()
        expected = before.copy()
        expected[f"refs/heads/{self.branch}"] = competing_sha
        self.assertEqual(after, expected)


if __name__ == "__main__":
    unittest.main()
