# Chime

Chime sends short messages from a command line or web application to browsers you enroll. Notifications can include an image and a link to open when clicked. Administrators use passkeys to manage receiving devices and revocable send keys. Chime keeps no notification history.

## Build and run

Build the server and CLI from the repository:

```sh
cargo build --workspace --locked --release
mkdir -m 0700 data
target/release/chime-server \
  --origin https://chime.example-tailnet.ts.net \
  --data-dir ./data
```

Put the server behind HTTPS at the exact configured origin and forward requests to its loopback listener on port 8080. For a rootless Podman and Tailscale deployment, use the [operator guide](docs/operator-guide.md).

On first start, the server prints a one-use setup code to its console. Enter it at the HTTPS site and register an administrator passkey. The code expires after 15 minutes; keep the console output private. On each receiving browser, choose **Enable notifications**, allow notifications, and approve enrollment with a passkey. On iPhone or iPad, add the site to the Home Screen and open that app first.

Create a named send key in the browser. Chime shows the key once. Save it in a protected config file such as `~/.config/chime/config.json` and set the file mode to `0600`:

```json
{
  "server_url": "https://chime.example-tailnet.ts.net",
  "api_key": "PASTE_THE_NEW_KEY_HERE"
}
```

Send a text notification or upload an image with a click URL:

```sh
target/release/chime send --title 'Build completed' --body 'All checks passed'
target/release/chime send --title 'Render ready' --image ./render.png --url https://example.com/result
```

The CLI also accepts `CHIME_URL` and `CHIME_API_KEY`; environment values override the config file. Do not put a key in a command argument. Revoke a key in the browser when it is no longer needed.

## Delivery and privacy

The CLI reports push-provider acceptance, not device receipt or display. Chime has no inbox, durable retry queue, or delivery guarantee. A timeout can leave acceptance uncertain.

Cross-device dismissal is a best-effort background push. A browser can delay or miss it, and Chrome may show a generic background-update notification. Safari background dismissal is disabled pending separate verification. An acknowledgement confirms Chime accepted the request; it does not confirm another device removed its notification.

The [Send API and CLI section](docs/proposals/chime-proposal.md#send-api-and-cli) defines request fields, JSON and multipart formats, delivery summaries, and fixed limits. [Cross-origin sending](docs/proposals/chime-proposal.md#cross-origin-sending) defines its CORS boundary. The proposal also documents the [security and resource boundaries](docs/proposals/chime-proposal.md#security-and-resource-boundaries). Uploaded image URLs are temporary capabilities; external image URLs are fetched by the receiving browser, not by Chime.

## More information

- [Operator guide](docs/operator-guide.md): install, setup, update, reset, backup, and troubleshooting.
- [Cross-device dismissal amendment](docs/proposals/chime-cross-device-dismissal.md): behavior, ordering, and failure limits.
- [Development checks](docs/development.md): Rust, browser, and deployment checks, including what local fixtures do not prove.
