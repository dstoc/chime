# Development checks

Run the Rust checks used by the required GitHub `test` workflow from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --bins --locked
```

## Local passkey browser check

The browser check exercises the setup and passkey flow against a temporary Chime data directory. It starts the server, a local HTTPS proxy, Chromium, and a WebAuthn virtual authenticator. It uses no npm packages.

Prerequisites are Node.js 24 or newer, Chromium, OpenSSL, and an available local port 8080. Build the server, then run:

```sh
cargo build -p chime-server --locked
node tests/browser/passkeys.mjs
```

The script checks invalid and valid setup codes, registration and authentication with required user verification, passkey list/add/remove flows, cancellation, last-passkey protection, wrong challenge/origin/RP ID/user-verification assertions, and the absence of cookies or browser storage. It also submits valid assertion and registration completions after expiry, removes authorizers before both completion stages, and races duplicate valid mutation completions while checking the stored passkey list. Expiry coverage waits just over two minutes. The script keeps the generated setup code in memory and removes its temporary data when it exits. Set `CHROMIUM_PATH` if Chromium is not at `/usr/bin/chromium`.

This check uses a virtual authenticator and a temporary self-signed certificate. It does not verify physical passkeys, production TLS, tailnet access, push-provider delivery, or another browser's behavior. Those checks still require manual acceptance on the target devices and deployment.
