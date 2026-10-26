# Chime

Chime sends short messages from a command line or web application to enrolled browsers. Notifications can include an image and a link. Administrators use passkeys to manage receiving devices and revocable send keys. Chime keeps no notification history.

## Run Chime

For a rootless Podman deployment, install the published Chime image with the two-container Tailscale example in the [operator guide](docs/operator-guide.md). The deployment does not build source on the host.

Download the Linux x86-64 CLI from a [GitHub release](https://github.com/dstoc/chime/releases). Replace `0.2.0` below with the version you selected:

```sh
VERSION=0.2.0
ARCHIVE="chime-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
BASE="https://github.com/dstoc/chime/releases/download/v${VERSION}"
mkdir -p "$HOME/.local/bin" "$HOME/.cache/chime"
curl --fail --location --output "$HOME/.cache/chime/$ARCHIVE" "$BASE/$ARCHIVE"
curl --fail --location --output "$HOME/.cache/chime/$ARCHIVE.sha256" "$BASE/$ARCHIVE.sha256"
(cd "$HOME/.cache/chime" && sha256sum --check "$ARCHIVE.sha256")
tar -xzf "$HOME/.cache/chime/$ARCHIVE" -C "$HOME/.local/bin" chime
"$HOME/.local/bin/chime" --version
```

Verify the archive's GitHub provenance and the corresponding image digest before installation. The [release guide](docs/releases.md) gives copyable checksum, digest, and `gh attestation verify` commands.

Create a send key in the browser and save it in a protected config file such as `~/.config/chime/config.json` with mode `0600`:

```json
{
  "server_url": "https://chime.example-tailnet.ts.net",
  "api_key": "PASTE_THE_NEW_KEY_HERE"
}
```

Send a text notification or upload an image with a click URL:

```sh
chime send --title 'Build completed' --body 'All checks passed'
chime send --title 'Render ready' --image ./render.png --url https://example.com/result
```

The CLI also accepts `CHIME_URL` and `CHIME_API_KEY`; environment values override the config file. Do not put a key in a command argument. Revoke a key in the browser when it is no longer needed.

## Delivery and privacy

The CLI reports push-provider acceptance, not device receipt or display. Chime has no inbox, durable retry queue, or delivery guarantee. A timeout can leave acceptance uncertain.

Cross-device dismissal is a best-effort background push. A browser can delay or miss it, and Chrome may show a generic background-update notification. Safari background dismissal is disabled pending separate verification. An acknowledgement confirms Chime accepted the request; it does not confirm another device removed its notification.

The [Send API and CLI section](docs/proposals/chime-proposal.md#send-api-and-cli) defines request fields, JSON and multipart formats, delivery summaries, and fixed limits. [Cross-origin sending](docs/proposals/chime-proposal.md#cross-origin-sending) defines its CORS boundary. The proposal also documents the [security and resource boundaries](docs/proposals/chime-proposal.md#security-and-resource-boundaries). Uploaded image URLs are temporary capabilities; external image URLs are fetched by the receiving browser, not by Chime.

## More information

- [Operator guide](docs/operator-guide.md): install the published image, set up Tailscale, update, reset, and back up the deployment.
- [Release guide](docs/releases.md): release operations, checksums, image digests, and provenance verification.
- [Development checks](docs/development.md): Rust, browser, and deployment checks, including what local fixtures do not prove.
- [Cross-device dismissal amendment](docs/proposals/chime-cross-device-dismissal.md): behavior, ordering, and failure limits.
