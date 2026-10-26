import hashlib
import json
import os
import re
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
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
    def test_single_workspace_version_and_real_lockfile_selectors(self) -> None:
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

        import tomllib

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
            [(entry["path"], entry["jsonpath"]) for entry in extra_files[:1]],
            [("Cargo.toml", "$.workspace.package.version")],
        )
        lock_selectors = {
            entry["jsonpath"]: entry
            for entry in extra_files
            if entry["path"] == "Cargo.lock"
        }
        self.assertEqual(len(lock_selectors), len(VERSION_NAMES))
        for name in VERSION_NAMES:
            selector = f'$.package[?(@.name=="{name}")].version'
            self.assertIn(selector, lock_selectors)
            matches = [entry for entry in lock["package"] if entry["name"] == name]
            self.assertEqual(len(matches), 1, f"selector must match exactly one {name} record")
            self.assertEqual(matches[0]["version"], version)
            self.assertEqual(lock_selectors[selector]["type"], "toml")

        self.assertEqual(
            {entry["path"] for entry in extra_files}, {"Cargo.toml", "Cargo.lock"}
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
        self.assertIn("branches: [main]", workflow)
        self.assertIn("workflow_dispatch:", workflow)
        self.assertIn("SELECTED_REF", workflow)
        self.assertIn('refs/tags/$TAG', workflow)
        self.assertIn("concurrency:", workflow)
        self.assertIn("group: chime-release", workflow)
        self.assertIn("cancel-in-progress: false", workflow)
        self.assertIn("secrets.GITHUB_TOKEN", workflow)
        self.assertIn("docker/build-push-action@v6", workflow)
        self.assertIn("platforms: linux/amd64", workflow)
        self.assertIn("actions/attest@v4", workflow)
        self.assertIn("gh attestation verify", workflow)
        self.assertIn("--source-digest", workflow)
        self.assertIn("--signer-workflow", workflow)
        self.assertIn("--bundle-from-oci", workflow)
        self.assertIn("draft=false", workflow)
        self.assertNotIn("release:", lowered)
        self.assertNotIn("--clobber", workflow)
        self.assertIn("cmp --silent", workflow)
        self.assertIn("already contains", workflow)

        self.assertLess(workflow.index("releases/tags/$TAG"), workflow.index("docker/build-push-action@v6"))
        self.assertLess(workflow.index("docker/build-push-action@v6"), workflow.index("actions/attest@v4"))
        self.assertLess(workflow.index("gh attestation verify"), workflow.index("draft=false"))

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
    volume=""
    image_found=false
    previous=""
    for argument in "$@"; do
      case "$previous" in
        --origin) origin=$argument ;;
        --data-dir) data_dir=$argument ;;
        --volume) volume=$argument ;;
      esac
      [[ "$argument" == "ghcr.io/dstoc/chime@$IMAGE_DIGEST" ]] && image_found=true
      previous=$argument
    done
    [[ "$origin" == "$EXPECTED_ORIGIN" ]]
    [[ "$data_dir" == /var/lib/chime ]]
    [[ "$volume" == *:/var/lib/chime:rw ]]
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


if __name__ == "__main__":
    unittest.main()
