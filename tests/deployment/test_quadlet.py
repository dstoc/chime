import configparser
import json
import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
QUADLET = ROOT / "examples" / "quadlet"


def read_unit(name: str) -> configparser.ConfigParser:
    parser = configparser.ConfigParser(interpolation=None, strict=False)
    parser.optionxform = str
    with (QUADLET / name).open(encoding="utf-8") as unit_file:
        parser.read_file(unit_file)
    return parser


class DeploymentConfigTests(unittest.TestCase):
    def test_container_build_is_locked_and_uses_reviewed_base_tags(self) -> None:
        containerfile = (ROOT / "Containerfile").read_text(encoding="utf-8")

        self.assertRegex(containerfile, r"(?m)^FROM docker\.io/library/rust:1\.99\.1-bookworm AS build$")
        self.assertRegex(containerfile, r"(?m)^FROM docker\.io/library/debian:12\.15-slim AS runtime$")
        self.assertIn("cargo build --locked --release --package chime-server", containerfile)
        self.assertIn("ca-certificates libgcc-s1 libssl3 libzstd1 zlib1g", containerfile)
        self.assertIn("USER 10001:10001", containerfile)
        self.assertIn("STOPSIGNAL SIGTERM", containerfile)
        self.assertIn("RUN /usr/local/bin/chime-server --help >/dev/null", containerfile)
        self.assertIn('ENTRYPOINT ["/usr/local/bin/chime-server"]', containerfile)

    def test_build_quadlet_uses_checkout_and_allows_release_build_time(self) -> None:
        unit = read_unit("chime.build")

        self.assertEqual(unit["Build"]["ImageTag"], "localhost/chime:0.1.0")
        self.assertEqual(unit["Build"]["File"], "%h/src/chime/Containerfile")
        self.assertEqual(unit["Build"]["SetWorkingDirectory"], "%h/src/chime")
        self.assertEqual(unit["Service"]["TimeoutStartSec"], "900")

    def test_chime_joins_tailscale_namespace_and_restarts_with_it(self) -> None:
        unit = read_unit("chime.container")
        container = unit["Container"]

        self.assertEqual(container["Image"], "chime.build")
        self.assertEqual(container["Network"], "chime-tailscale.container")
        self.assertEqual(
            set(unit["Unit"]["After"].split()), {"chime-tailscale.service"}
        )
        self.assertEqual(
            set(unit["Unit"]["BindsTo"].split()), {"chime-tailscale.service"}
        )
        self.assertEqual(
            set(unit["Unit"]["PartOf"].split()), {"chime-tailscale.service"}
        )
        self.assertEqual(container["User"], "10001:10001")
        self.assertEqual(container["ReadOnly"], "true")
        self.assertEqual(container["ReadOnlyTmpfs"], "true")
        self.assertIn("--data-dir /var/lib/chime", container["Exec"])
        self.assertIn("--origin https://", container["Exec"])
        self.assertIn("/var/lib/chime:rw,Z", container["Volume"])
        self.assertNotIn("/var/run/tailscale", container["Volume"])
        self.assertNotIn("Secret=", (QUADLET / "chime.container").read_text(encoding="utf-8"))
        self.assertEqual(unit["Service"]["Restart"], "always")
        self.assertFalse((QUADLET / "chime.pod").exists())

    def test_tailscale_uses_file_secret_persistent_state_and_starts_chime(self) -> None:
        unit = read_unit("chime-tailscale.container")
        container = unit["Container"]

        self.assertRegex(
            container["Image"],
            r"^docker\.io/tailscale/tailscale:v\d+\.\d+\.\d+$",
        )
        self.assertEqual(set(unit["Unit"]["Wants"].split()), {"chime.service"})
        self.assertEqual(container["Environment"], "TS_SERVE_CONFIG=/config/serve.json")
        lines = (QUADLET / "chime-tailscale.container").read_text(encoding="utf-8").splitlines()
        for required in (
            "Environment=TS_USERSPACE=true",
            "Environment=TS_STATE_DIR=/var/lib/tailscale",
            "Environment=TS_AUTH_ONCE=true",
            "Environment=TS_AUTHKEY=file:/run/secrets/tailscale-authkey",
            "Secret=tailscale-authkey,type=mount,target=tailscale-authkey",
            "Environment=TS_SERVE_CONFIG=/config/serve.json",
        ):
            self.assertIn(required, lines)
        self.assertTrue(any("/var/lib/tailscale:rw,Z" in line for line in lines))
        self.assertTrue(any("/config:ro,Z" in line for line in lines))
        self.assertEqual(unit["Service"]["Restart"], "always")

    def test_two_containers_do_not_publish_or_gain_host_privileges(self) -> None:
        unit_files = [
            QUADLET / "chime.build",
            QUADLET / "chime.container",
            QUADLET / "chime-tailscale.container",
        ]
        combined = "\n".join(path.read_text(encoding="utf-8") for path in unit_files)
        for forbidden in (
            "PublishPort=",
            "Network=host",
            "Privileged=",
            "AddCapability=",
            "Device=",
            "SecurityLabelDisable=",
            "/run/podman/podman.sock",
        ):
            self.assertNotIn(forbidden, combined)
        self.assertNotIn("Pod=", combined)
        self.assertNotIn("StartWithPod=", combined)
        self.assertNotIn("PodName=", combined)
        self.assertNotRegex(combined, r"tskey-(?:auth|client)-[A-Za-z0-9-]+")

    def test_serve_config_forwards_loopback_https_without_funnel(self) -> None:
        config = json.loads(
            (QUADLET / "tailscale" / "serve.json").read_text(encoding="utf-8")
        )

        self.assertIs(config["TCP"]["443"]["HTTPS"], True)
        self.assertEqual(
            config["Web"]["${TS_CERT_DOMAIN}:443"]["Handlers"]["/"]["Proxy"],
            "http://127.0.0.1:8080",
        )
        self.assertIs(config["AllowFunnel"]["${TS_CERT_DOMAIN}:443"], False)

    def test_containerignore_excludes_local_state_and_secrets(self) -> None:
        patterns = (ROOT / ".containerignore").read_text(encoding="utf-8").splitlines()

        for expected in (
            ".git",
            "target",
            "data",
            ".env",
            ".env.*",
            ".secrets",
            "secrets",
            "*.key",
            "*.pem",
            "*.secret",
            "*.db",
            "*.sqlite3",
        ):
            self.assertIn(expected, patterns)


if __name__ == "__main__":
    unittest.main()
