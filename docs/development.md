# Development checks

Run the Rust checks used by the required GitHub `test` workflow from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --bins --locked
```

## Local passkey browser check

The browser check exercises setup, passkey administration, send-key controls, device enrollment and removal, browser IndexedDB token storage, and service-worker payload validation and text display against a temporary Chime data directory. It starts the server, a local HTTPS proxy, Chromium, and a WebAuthn virtual authenticator. It uses no npm packages.

Prerequisites are Node.js 24 or newer, Python 3 with its standard `sqlite3` module, Chromium, OpenSSL, and an available local port 8080. Build the server, then run:

```sh
cargo build -p chime-server --locked
node tests/browser/passkeys.mjs
```

The script checks invalid and valid setup codes, registration and authentication with required user verification, passkey list/add/remove flows, cancellation, last-passkey protection, wrong challenge/origin/RP ID/user-verification assertions, and one-use completion. It loads the standalone display mode and icon metadata from the manifest under the production CSP. It creates and revokes independent send keys through the browser UI, checks the no-device and revoked-key API responses, and verifies that SQLite and server logs contain only send-key digests. It enrolls through the Enable notifications form with a local push fixture, checks the submitted device name and IndexedDB token, then rotates an enrollment token for the same endpoint and verifies its SQLite digest and IndexedDB value before removal. It injects valid and malformed local push events and checks the notification text and safe fallback. It submits valid assertion and registration completions after expiry, removes authorizers before both completion stages, and races duplicate valid mutation completions. Expiry coverage waits just over two minutes. The script keeps the generated setup code in memory and removes its temporary data when it exits. Set `CHROMIUM_PATH` if Chromium is not at `/usr/bin/chromium`.

The acknowledgement checks also run in Chromium. They verify that click handling starts URL opening before acknowledgement completion, closes without navigation when no valid URL exists, and sends the device token in an Authorization header. Controlled local responses exercise 100-ID reconciliation batches, split SSE frames, unknown event IDs and the browser parser buffer limit. Rust tests exercise the authenticated endpoints, capability checks, stream limits, slow-client disconnect, revocation and transient-state expiry/eviction/restart behavior.

This check uses a virtual authenticator and a temporary self-signed certificate. Its push endpoint is a local fixture; it does not send through a provider. The service-worker checks cover image options, click URL validation, text display, and a text fallback when image display fails. Rust notification tests use a fake transport after the same production endpoint and key validation. These checks do not verify physical passkeys, production TLS, tailnet access, real push-provider delivery, iOS Home Screen behavior, or another browser's behavior. Those checks still require acceptance on target devices and deployment.

## Cross-origin send check

Run the distinct-origin browser check with Chromium:

```sh
node tests/browser/notify-cors.mjs
```

The script starts Chime behind a local HTTPS proxy and opens a page at a different HTTPS origin. It exercises the browser-generated preflight and Bearer send, reads an invalid-key 401 response, confirms that setup and admin requests remain unreadable cross-origin, then verifies that the service worker requests a valid PNG from the second HTTPS origin under its served CSP. It also checks that notification text remains available when an external image returns 404. Both test origins use loopback, so this does not exercise Chrome's local-network permission behavior for a tailnet destination. Verify that permission flow on the target Chrome setup. The virtual browser test does not establish physical-device or push-provider delivery.
