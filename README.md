# Chime

Chime is a small, self-hosted notification service. This milestone provides first-use administrator setup and passkey administration. Sending notifications and enrolling receiving devices are not available yet.

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

If the setup code expires or is lost, or if initial registration was cancelled after setup began, stop Chime and replace the code locally:

```sh
chime-server reset-admin --data-dir /var/lib/chime
```

The command requires the exclusive data-directory lock and refuses to run while Chime is using that directory. It clears administrator passkeys and prints a new 15-minute setup code. It preserves send keys, device subscriptions, and the VAPID identity; revoke send keys separately if compromise is suspected.

Chime creates the data directory with private permissions on Unix systems. SQLite stores the persistent VAPID key pair and uses forward schema migrations. The server holds an exclusive data-directory lock for its lifetime, and `reset-admin` must acquire the same lock. The current database has no notification history.

## Development checks

Run the checks used by the required GitHub `test` workflow:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --bins --locked
```

For the local virtual-authenticator browser check and its limits, see [Development checks](docs/development.md).
