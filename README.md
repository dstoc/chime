# Chime

Chime sends short notifications from a command line or web application to browsers that you enroll. Notifications can include an image and an HTTPS link to open when clicked. Administrators use passkeys to manage receiving devices and independently revocable send keys. Chime does not keep a notification history.

Cross-device dismissal uses best-effort background pushes. Browser delivery can delay or miss them, and Chrome may show a generic background-update notification. See the [cross-device dismissal design](docs/proposals/chime-cross-device-dismissal.md).

## Run the server

The server requires its public HTTPS origin and a private data directory. It listens on `127.0.0.1:8080` for a local HTTPS proxy.

```sh
cargo run -p chime-server -- \
  --origin https://chime.example-tailnet.ts.net \
  --data-dir ./data
```

Set `--origin` to the exact HTTPS origin that browsers will open. Do not include a path, query, or fragment. Chime derives the WebAuthn RP ID from the origin's DNS hostname. The server does not use request headers or Tailscale identity headers to select its origin or RP ID.

On the first start, Chime prints a random setup code to the foreground console. Copy it into the setup page at the configured HTTPS origin and register an administrator passkey. The code expires after 15 minutes. Chime stores only its SHA-256 digest and never returns the code over HTTP. Keep the console output private.

Every passkey list, add, or remove action requires a new passkey verification with user verification. Adding a passkey requires one verification to authorize the registration and a second to register it. Chime does not create administrator cookies or sessions. Keep at least one passkey registered.

## Enroll a browser and send a notification

Open Chime in a supported browser over HTTPS. On each receiving browser, choose **Enable notifications**, allow notifications, enter a device name, and approve enrollment with a passkey. On iPhone or iPad, first add Chime to the Home Screen and open that app. Chime does not use polling when push notifications are unavailable.

Create a named send key in the browser. Chime shows its value once. Store it in a protected config file or environment variable. Do not put the key in a command argument.

Create `~/.config/chime/config.json` with mode `0600`:

```json
{
  "server_url": "https://chime.example-tailnet.ts.net",
  "api_key": "PASTE_THE_NEW_KEY_HERE"
}
```

Set the file permissions with `chmod 600 ~/.config/chime/config.json` after you save it.

Send text to all active receiving devices:

```sh
chime send --title 'Build completed' --body 'All checks passed'
```

Upload an image and open a result link when the notification is clicked:

```sh
chime send --title 'Render ready' --image ./render.png --url https://example.com/result
```

Or give receiving browsers an HTTPS image URL. Chime does not fetch it:

```sh
chime send --title 'Build completed' --image-url https://example.com/build.png
```

The send API accepts JSON fields `title`, `body`, `image_url`, and `url`, or multipart fields with an optional `image` file. Uploaded files must be PNG, JPEG, or WebP and no larger than 5 MiB. Chime stores each upload at a random capability URL for 24 hours, with a 256 MiB total image quota. Anyone who obtains that URL and can reach Chime can read the image until it expires. Chime never downloads `image_url`; the receiving browser requests it directly, so that host can observe the receiving device. The notification text remains available if an image cannot load. Image display depends on browser and operating-system support.

Cross-origin browser sends are allowed on `/api/notify` only. The endpoint accepts the send key in an `Authorization` header and does not allow credentialed CORS. Browser local-network permission controls can still block or prompt for access to a tailnet server. Test those controls on the target browser and network.

The CLI also reads `CHIME_URL` and `CHIME_API_KEY`; environment values override the config file. Use `--config PATH` or `CHIME_CONFIG` to select a different file. The CLI reports push-provider acceptance, not device receipt, and exits with a nonzero status for partial failure, total failure, or no devices.

Clicking a notification closes it on that device and opens its validated HTTPS link immediately. The service worker acknowledges the click with its device token and notification capability. Chime then attempts an encrypted dismissal push to other original recipients that remain enrolled. The acknowledgement confirms Chime accepted the request; it does not confirm that another device removed its OS notification.

A matching dismissal closes notifications with the same tag, even when no Chime page is open. A valid dismissal with no matching notification does not show a new notification. The dismissal push TTL is the original notification's remaining lifetime. It limits provider retention; it does not expire a notification already displayed by the operating system. Acknowledgement records contain no notification content and live only in bounded server memory. A restart or eviction can lose an acknowledgement before fan-out, and failed or offline pushes are not retried. Expired or evicted local dismissal memory can allow a delayed notification to reappear. Chime keeps no offline acknowledgement queue.

Chime does not send background-dismissal pushes to Apple push endpoints pending separate Safari verification. Ordinary push delivery and background dismissal have separate browser compatibility results.

The browser provides send-key creation and revocation, device enrollment and removal, and administrator passkey management. Enrollment accepts HTTPS push endpoints from Google FCM, Mozilla Push, and Apple Web Push. Other providers are rejected. Chime limits titles to 200 UTF-8 bytes, bodies to 1,500 bytes, and URLs to 2,048 bytes. JSON requests are limited to 16 KiB; multipart uploads are limited to 5 MiB plus 16 KiB of overhead. Chime supports up to 64 receiving devices, keeps provider messages for one hour, and limits the server to 60 sends per minute, four sends at once, and eight provider requests at once.

If the setup code expires or is lost, or if initial registration was cancelled after setup began, stop Chime and replace the code locally:

```sh
chime-server reset-admin --data-dir /var/lib/chime
```

The command requires the exclusive data-directory lock and refuses to run while Chime is using that directory. It clears administrator passkeys and prints a new 15-minute setup code. It preserves send keys, device subscriptions, and the VAPID identity; revoke send keys separately if compromise is suspected.

Chime creates the data directory and image directory with private permissions on Unix systems. SQLite stores the persistent VAPID key pair, send-key digests, device subscriptions and token digests, and image expiry metadata. The `images` subdirectory holds uploaded files until they expire. Include both SQLite and `images` when backing up Chime, and protect both as sensitive data. SQLite uses forward schema migrations. The server holds an exclusive data-directory lock for its lifetime, and `reset-admin` must acquire the same lock. Chime keeps no notification history.

## Development checks

Run the checks used by the required GitHub `test` workflow:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --bins --locked
```

For the local virtual-authenticator browser check and its limits, see [Development checks](docs/development.md).
