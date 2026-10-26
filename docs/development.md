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

The script checks invalid and valid setup codes, registration and authentication with required user verification, passkey list/add/remove flows, cancellation, last-passkey protection, wrong challenge/origin/RP ID/user-verification assertions, and one-use completion. It creates and revokes independent send keys through the browser UI, checks the no-device and revoked-key API responses, and verifies that SQLite and server logs contain only send-key digests. It rotates an enrollment token for a duplicate endpoint, verifies its SQLite digest and IndexedDB value, then removes the device. It injects valid and malformed local push events and checks the notification text and safe fallback. It submits valid assertion and registration completions after expiry, removes authorizers before both completion stages, and races duplicate valid mutation completions. Expiry coverage waits just over two minutes. The script keeps the generated setup code in memory and removes its temporary data when it exits. Set `CHROMIUM_PATH` if Chromium is not at `/usr/bin/chromium`.

This check uses a virtual authenticator and a temporary self-signed certificate. Its push endpoint is a local fixture; it does not send through a provider. Rust notification tests use a fake transport after the same production endpoint and key validation. These checks do not verify physical passkeys, production TLS, tailnet access, real push-provider delivery, iOS Home Screen behavior, or another browser's behavior. Those checks still require acceptance on target devices and deployment.
