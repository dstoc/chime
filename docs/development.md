# Development checks

Run the Rust checks used by the required GitHub `test` workflow from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --bins --locked
```

## Container and Quadlet checks

The required `test` workflow checks the deployment files with Python's standard library and builds the root `Containerfile` from a clean checkout. Run the configuration check locally with:

```sh
python3 -m unittest discover --start-directory tests/deployment --pattern 'test_*.py'
```

The same workflow checks the Release Please version selectors, shared Cargo versions, deterministic CLI archive, and release/deployment documentation:

```sh
python3 -m unittest discover --start-directory tests/release --pattern 'test_*.py'
```

Where rootless Podman is available, build the image with `podman build --tag localhost/chime:0.1.0 .` and follow the [operator guide](operator-guide.md) to validate the Quadlet units. The static check does not validate Podman Quadlet parsing, rootless runtime permissions, SELinux labels, a reboot, or a real tailnet connection.

## Local passkey browser check

The browser check exercises setup, passkey administration, send-key controls, device enrollment and removal, browser IndexedDB token storage, and service-worker payload validation and text display against a temporary Chime data directory. It starts the server, a local HTTPS proxy, Chromium, and a WebAuthn virtual authenticator. It uses no npm packages.

Prerequisites are Node.js 24 or newer, Python 3 with its standard `sqlite3` module, Chromium, OpenSSL, and an available local port 8080. Build the server, then run:

```sh
cargo build -p chime-server --locked
node tests/browser/passkeys.mjs
```

The script checks invalid and valid setup codes, registration and authentication with required user verification, passkey list/add/remove flows, cancellation, last-passkey protection, wrong challenge/origin/RP ID/user-verification assertions, and one-use completion. It loads the standalone display mode and icon metadata from the manifest under the production CSP. It creates and revokes independent send keys through the browser UI, checks the no-device and revoked-key API responses, and verifies that SQLite and server logs contain only send-key digests. It enrolls through the Enable notifications form with a local push fixture, checks the submitted device name and IndexedDB token, then rotates an enrollment token for the same endpoint and verifies its SQLite digest and IndexedDB value before removal. It injects valid and malformed local push events and checks the notification text and safe fallback. It submits valid assertion and registration completions after expiry, removes authorizers before both completion stages, and races duplicate valid mutation completions. Expiry coverage waits just over two minutes. The script keeps the generated setup code in memory and removes its temporary data when it exits. Set `CHROMIUM_PATH` if Chromium is not at `/usr/bin/chromium`.

The dismissal checks also run in Chromium. They cover versioned notify/dismiss dispatch, unmatched dismissal no-op behavior, serialized push ordering, local click memory, expiry and the bounded IndexedDB set. They verify that URL opening starts before acknowledgement work completes, a pending acknowledgement does not block later pushes, and a click closes a notification after an in-flight display finishes. They also verify that the device token stays in an Authorization header. Rust tests cover token and capability checks, first-acknowledgement fan-out, duplicate requests, recipient removal, provider failure, remaining push TTL and transient-state expiry, eviction and restart behavior. The router tests confirm that the old event and reconciliation routes are absent.

These fixtures do not test a real push provider or a physical device. Verify Chrome desktop and Android where available. Record browser version, OS and any generic replacement notification when dismissing one notification, multiple notifications and notifications under low site engagement. Chime withholds dismissal pushes from Apple push endpoints until Safari background dismissal is verified. Ordinary push delivery is a separate compatibility result.

This check uses a virtual authenticator and a temporary self-signed certificate. Its push endpoint is a local fixture; it does not send through a provider. The service-worker checks cover image options, click URL validation, text display, and a text fallback when image display fails. Rust notification tests use a fake transport after the same production endpoint and key validation. These checks do not verify physical passkeys, production TLS, tailnet access, real push-provider delivery, iOS Home Screen behavior, or another browser's behavior. Those checks still require acceptance on target devices and deployment.

## Cross-origin send check

Run the distinct-origin browser check with Chromium:

```sh
node tests/browser/notify-cors.mjs
```

The script starts Chime behind a local HTTPS proxy and opens a page at a different HTTPS origin. It exercises the browser-generated preflight and Bearer send, reads an invalid-key 401 response, confirms that setup and admin requests remain unreadable cross-origin, then verifies that the service worker requests a valid PNG from the second HTTPS origin under its served CSP. It also checks that notification text remains available when an external image returns 404. Both test origins use loopback, so this does not exercise Chrome's local-network permission behavior for a tailnet destination. Verify that permission flow on the target Chrome setup. The virtual browser test does not establish physical-device or push-provider delivery.
