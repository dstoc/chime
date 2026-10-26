# Chime

Chime is a small, self-hosted notification service. The current server foundation serves an embedded landing page and initializes private SQLite state. Sending notifications, device enrollment, and passkey administration are not available yet.

## Run the server

The server requires its public HTTPS origin and a private data directory. It listens on `127.0.0.1:8080` for a local HTTPS proxy.

```sh
cargo run -p chime-server -- \
  --origin https://chime.example-tailnet.ts.net \
  --data-dir ./data
```

Set `--origin` to the exact HTTPS origin that browsers will open. Do not include a path, query, or fragment. Chime derives the WebAuthn RP ID from the origin's DNS hostname. The server does not use request headers or Tailscale identity headers to select its origin or RP ID.

Chime creates the data directory with private permissions on Unix systems. SQLite stores the persistent VAPID key pair and uses forward schema migrations. The server holds an exclusive data-directory lock for its lifetime; any future local reset command must acquire the same lock. The current database has no notification history.

## Development checks

Run the checks used by the required GitHub `test` workflow:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --bins --locked
```
