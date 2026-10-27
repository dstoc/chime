# Proposal: Chime — a small, self-hosted notification service

## Motivation

Send a short message and optional image from a CLI, script or web application to personal devices. Receive it as an ordinary OS notification without keeping the web app open. Manage sending keys and receiving devices in a browser, with passkeys authorizing administration.

Run one Rust service behind HTTPS inside a Tailscale tailnet. Use native browser APIs and static HTML, CSS and JavaScript modules. Keep the system small enough that its authorization paths and handling of secrets can be reviewed directly.

This is an ephemeral delivery service. There is no inbox: if an OS notification disappears, its content need not remain available.

## Problem statement

The agreed design needs concrete boundaries between three kinds of authority:

- A sender can deliver notifications using an independently revocable Bearer key.
- An administrator can issue and revoke those keys, enroll devices and manage passkeys.
- An enrolled receiving device can reconcile notifications and acknowledge a notification it received.

A send key must never acquire administrative authority. Cross-origin callers must be able to send with their key without receiving access to administration. Delivery, images and acknowledgement must have explicit limits rather than silently growing into a message archive or reliable job queue.

No repository or implementation was supplied for this proposal. All paths below describe the proposed initial project structure, not existing files.

## Proposal

### Deployment and components

The project is named **Chime**. Build one Rust server executable, `chime-server`, and a small Rust CLI, `chime`. The server embeds the static browser assets and uses SQLite plus a temporary image directory. Both executables can share the request and response types in one Cargo workspace; do not introduce a general SDK.

The server listens on `127.0.0.1:8080` by default. Tailscale Serve supplies trusted HTTPS and tailnet reachability:

```bash
chime-server --origin https://chime.example-tailnet.ts.net --data-dir /var/lib/chime
tailscale serve --bg 8080
```

The origin is required configuration. Derive the WebAuthn RP ID from its hostname; do not derive either value from request headers. Keep the hostname stable. Tailscale identity headers are not administrative authorization.

The server requires outbound HTTPS to supported browser push services. Devices require their browser vendor's push connectivity. Neither a public server endpoint nor a Firebase project is part of this design. Use standard Web Push and a persistent VAPID key pair rather than a vendor SDK.

Delivery can reach a device when its tailnet connection is unavailable. Fetching a private image, contacting the server to acknowledge a notification, or opening a private click URL still requires the relevant connectivity.

### Container image and systemd deployment

Ship a root-level `Containerfile` suitable for `podman build` and running `chime-server`. Use a multi-stage build with `cargo build --locked --release`, embedding the static assets in the executable. The small runtime image contains the server, its required runtime libraries and trusted CA certificates for outbound Web Push HTTPS. Run as a fixed non-root user, handle termination signals directly and support a read-only root filesystem with writable persistent state at `/var/lib/chime`. Include `.containerignore` to exclude build outputs, local state and secrets. Pin build/runtime base images to reviewed versions or digests; building must not require production credentials.

Provide one documented rootless systemd Quadlet example under `examples/quadlet/`, rather than several deployment variants:

| File | Purpose |
| --- | --- |
| `chime.build` | Build the local checkout using its Containerfile and tag the result; document the checkout path to edit and allow enough startup time for a release build |
| `chime.container` | Run the built image through `Image=chime.build`, set the exact HTTPS origin/data directory, and mount persistent Chime state |
| `chime-tailscale.container` | Run a pinned official Tailscale image, persist its node state and load the supplied Serve configuration |
| `chime.pod` | Give both containers a shared network namespace and loopback interface without publishing host ports |
| `tailscale/serve.json` | Configure tailnet-only HTTPS forwarding to `http://127.0.0.1:8080`; leave Funnel disabled |

Both containers join the pod. Chime keeps its default loopback listener; the sidecar reaches it through the pod's shared loopback. Set `TS_USERSPACE=true` so the sidecar needs neither `/dev/net/tun` nor added network capabilities. Chime uses ordinary container egress for push-provider requests; this example does not route its outbound traffic through a Tailscale proxy. Do not use host networking, privileged containers or a host container-engine socket.

Configure the sidecar with `TS_HOSTNAME=chime`, `TS_STATE_DIR` on its own persistent mount, `TS_AUTH_ONCE=true`, and `TS_SERVE_CONFIG` pointing to the supplied JSON. Mount the Serve configuration's parent directory read-only, as required for configuration updates. Keep Tailscale state separate from Chime state and do not mount the sidecar's LocalAPI socket or credentials into Chime. Persist node identity and document checking the actual assigned tailnet FQDN against Chime's configured origin before passkey setup; requesting a hostname alone does not guarantee that exact name is assigned.

Document initial Tailscale authentication using a separately supplied Podman secret and the official image's supported file-based auth-key input. Never commit an auth key, bake one into an image, or put its literal value in example unit files. After enrollment, remove the bootstrap auth-key injection and use the persisted identity. Tailnet grants and HTTPS prerequisites remain operator configuration; the examples do not enable Funnel or change access policy automatically.

The operator guide must provide the complete copy/edit/start workflow: required Podman/systemd versions, user Quadlet installation directory, persistent-directory ownership and SELinux labeling, auth-key secret creation, `systemctl --user daemon-reload`, generated service names, startup/log commands and optional lingering for unattended boot. Set explicit pod/container dependencies and restart policies, and document update/rebuild and stop procedures. Verify recovery when either container restarts or is recreated, rather than treating startup ordering as readiness. A sidecar outage makes HTTPS unavailable; it must not cause Chime to publish an alternate listener or rotate persistent keys.

Explain how to obtain the sensitive first-use Chime setup code from the service's startup output and how to run local setup/reset commands using the same image and data mount. Offline reset still requires the running Chime service to be stopped and its data lock to be available. Backups preserve Chime state and Tailscale node state separately; losing the latter may change the HTTPS identity and affect existing passkeys.

### Common workflows

1. Start the service and obtain the first-use setup code from its console.
2. Open the HTTPS site, enter that code and register an administrator passkey.
3. On each receiving device, open the site, enable notifications and authorize enrollment with a passkey.
4. Create a named send key in the browser. Copy its value into the intended CLI or application.
5. Send notifications. Revoke an individual key in the browser when it is no longer needed.

The UI has three focused areas: this device's notification status, send keys, and receiving devices/administrator passkeys. It explains successful outcomes and concrete next steps for denied permission, missing connectivity and expired setup. It contains no notification feed.

For iPhone/iPad, document the Home Screen installation requirement for Web Push and provide a small manifest and icons. Request notification permission only from an explicit user action. If Push API support or permission is unavailable, explain that notifications cannot be enabled on that browser; do not silently fall back to polling.

### Authority model

| Operation | Required authority |
| --- | --- |
| Send to all active receiving devices | Valid send key |
| List, create or revoke send keys | Fresh administrator passkey assertion |
| List or remove receiving devices | Fresh administrator passkey assertion |
| Enroll or replace a receiving subscription | Fresh administrator passkey assertion |
| List, add or remove administrator passkeys | Fresh administrator passkey assertion |
| Read acknowledgement state / connect to event stream | Enrolled device token |
| Acknowledge a notification | Enrolled device token and notification-specific acknowledgement capability |
| Fetch a temporary uploaded image | Image capability URL |
| Register the first administrator passkey | Single-use local setup code |

There is one logical administrator and no user-account selection, roles or delegated key creation. All registered administrator passkeys have equal authority. A synced passkey may be usable on several physical devices; the UI must not imply that each passkey record represents a unique device.

### Passkey authorization

Use an established Rust WebAuthn crate, with `webauthn-rs` as the initial candidate. Use its passkey flow, require user verification during registration and authentication, and verify the signed result on the server. Do not implement WebAuthn signature, authenticator-data or attestation parsing in application code.

Authorize each administrative operation independently. Do not introduce a persistent administrator session or cookies.

1. The UI submits a typed operation, such as `create_key { name }` or `revoke_key { id }`, to a same-origin challenge endpoint.
2. The server validates its input and holds the exact operation and library ceremony state under a random request ID for two minutes.
3. The browser calls `navigator.credentials.get()` with `userVerification: "required"`.
4. The completion request contains the request ID and assertion. It does not resubmit mutable operation arguments.
5. The server atomically takes the pending request, verifies the assertion against that state and an administrator credential that is still registered, then executes the stored operation.

A failed or interrupted attempt requires a new challenge. Prevent replay and concurrent completion by consuming the request once. Check credential membership at completion so a pending ceremony cannot preserve authority after passkey removal. Apply database mutations transactionally and update credential metadata through the library's supported APIs. Do not apply a homegrown strictly increasing signature-counter rule that rejects valid synced passkeys.

The verified origin and RP ID prevent use of an assertion from another site; the stored challenge prevents replay; required user verification requires the authenticator's PIN/biometric or equivalent verification. Storing the operation binds server execution to what the UI requested. This is not a trusted transaction display: the authenticator prompt does not independently prove that the administrator saw a particular key name. Protecting the served JavaScript remains essential.

An authenticated list operation returns a browser snapshot. Further mutations each need their own assertion. This keeps the normal workflow to one prompt per action without issuing reusable administration credentials.

Adding a passkey needs two ceremonies. An existing passkey first authorizes a registration intent, including its name; the server then returns registration options bound to that short-lived intent. Completing registration consumes it once and rechecks that the authorizing credential remains registered. Reject duplicate credential IDs. Prevent removal of the last administrator passkey.

### First use and lost passkeys

When no administrator has ever been enrolled, generate a random 256-bit setup code. Print it once in the foreground startup output, never through an HTTP status endpoint. Store only its digest and expiry. Treat captured console output as sensitive. The UI may reveal that setup is required, but not the code.

The code expires after 15 minutes. An expired or lost code can be replaced through a local operator command. Setup binds successful code validation to a short-lived registration ceremony; completing the first registration atomically establishes the administrator and disables bootstrap. Concurrent attempts cannot create additional administrators. Restarting must not reopen setup on an initialized database.

Provide an explicit offline `chime-server reset-admin --data-dir ...` command for total passkey loss. Require the service to be stopped and an exclusive data-directory lock. It clears administrator credentials and prepares a new single-use setup code. It preserves send keys, subscriptions and the VAPID identity; its output states this so the operator can revoke those separately if compromise is suspected. No HTTP reset endpoint exists. Local data-directory access is already part of the trusted server boundary.

### Send keys

Generate a random 256-bit secret for each key. Store its SHA-256 digest, stable ID, display name, creation time and revocation state in SQLite. High-entropy random tokens do not require a password hashing function. Never persist plaintext keys or echo them into logs.

The browser shows a new key once with a copy control. If the response is lost, create another key and revoke the unusable entry; there is no recovery endpoint. List responses contain metadata, not secrets. Names need not be unique: operations use stable IDs.

Keys have equal send-only authority, no expiry by default, and independent revocation. Revocation rejects future requests immediately after the transaction commits. It cannot recall a request already authorized and being delivered. No key can create or inspect another key, enroll a device or fetch subscription endpoints.

### Send API and CLI

`POST /api/notify` accepts only `Authorization: Bearer <key>`. Never accept keys in query strings or cookies.

JSON fields:

| Field | Meaning |
| --- | --- |
| `title` | Required nonempty text |
| `body` | Optional text |
| `image_url` | Optional absolute HTTPS image URL |
| `url` | Optional absolute HTTPS destination opened on click |

Multipart requests use the same text fields and an optional binary `image` part. Reject requests specifying both `image_url` and `image`. Reject unknown or duplicate fields and malformed values at the boundary. Notifications always target all active devices; targeting and topics are outside the initial scope.

```bash
chime send --title 'Build completed' --body 'All checks passed'
chime send --title 'Render ready' --image ./render.png --url https://example.com/result

curl https://chime.example-tailnet.ts.net/api/notify \
  -H "Authorization: Bearer $CHIME_API_KEY" \
  -F 'title=Render ready' -F 'image=@render.png'
```

The CLI reads the server URL and key from environment variables or a protected local configuration file. Do not require a secret on the command line. Support text, image upload and click URL; omit server administration from the sending CLI. Report the returned delivery summary clearly and exit nonzero for partial failure, total failure or zero receiving devices.

Validate and construct the complete push payload before fan-out. Generate a random notification ID and acknowledgement capability. Encrypt separately for each subscription using a focused Web Push crate; `web-push` is the initial candidate. The encrypted payload has an explicit format version and carries title, body, optional image/click URLs, ID, expiry and acknowledgement capability. Images never go inside the push payload.

Fan out with bounded concurrency and timeouts. There is no application retry queue or automatic resubmission. The provider may retain accepted messages up to the requested push TTL. Use a one-hour TTL initially.

Return a synchronous HTTP 200 summary after valid sends: `id`, `attempted`, `accepted`, `failed`, `expired_subscriptions_removed`, and `outcome` (`accepted`, `partial`, `failed`, or `no_devices`). These counts describe push-provider responses, not device receipt or display. Timeouts count as failures with unknown acceptance. Return structured 4xx errors for invalid input/authentication and 503 for local capacity exhaustion. Do not expose endpoints, provider response bodies or secrets in responses.

Delete subscriptions on the provider's documented permanent-expiry responses, normally 404/410. Keep them on transient failures and authentication/configuration failures; report a redacted operator diagnostic. A lost API response can leave delivery uncertain. Retrying may produce another notification; there is no durable idempotency record in this version.

### Cross-origin sending

Allow CORS on `/api/notify` only:

```http
Access-Control-Allow-Origin: *
Access-Control-Allow-Methods: POST, OPTIONS
Access-Control-Allow-Headers: Authorization, Content-Type
```

Answer preflights without authentication and add the CORS headers to send errors as well as success responses. Do not set `Access-Control-Allow-Credentials`.

```javascript
await fetch('https://chime.example-tailnet.ts.net/api/notify', {
  method: 'POST',
  credentials: 'omit',
  headers: {
    Authorization: `Bearer ${apiKey}`,
    'Content-Type': 'application/json'
  },
  body: JSON.stringify({ title: 'Job completed' })
});
```

The key grants send authority regardless of caller origin. A web application given that key can read and use it; recommend a separate key per application. Browser local-network access controls can independently block or prompt for access to a tailnet destination. CORS does not override them. Include a real cross-origin browser check in acceptance testing rather than claiming all calling sites will work automatically.

### Receiving devices

After browser permission and `pushManager.subscribe({ userVisibleOnly: true, applicationServerKey })`, submit the subscription and device name as the operation authorized by a passkey. The service validates its endpoint and key material before storing it.

Issue a separate random device token, returning it once after enrollment and storing only its digest on the server. Store it in same-origin IndexedDB so both the page and service worker can use it. The token authorizes reconciliation and an event stream, not sending or administration. Clearing site data loses it; the user must authorize enrollment again.

Make enrollment an upsert by subscription endpoint. Reenrollment rotates the device token and updates that record, preventing duplicate deliveries after a lost enrollment response. If a browser creates a new endpoint, it becomes a new record; the device list lets the administrator remove the old one. A changed subscription requires fresh passkey authorization; do not silently replace it in the background.

Disable notifications on this device by authorizing server removal and calling browser unsubscribe. If either step fails, show which action remains incomplete. Removing a device also invalidates its token and closes its event streams. Already accepted provider messages may still arrive.

### Clicks and cross-device dismissal

The service worker validates the payload version, field types, lengths and HTTPS URL schemes before using it, then displays each notification with a tag derived from its ID. Malformed payloads follow a bounded generic notification fallback rather than rendering untrusted HTML or attempting navigation. A click closes that local notification and immediately opens the validated `url` when present. Otherwise, when the body is absent or empty and an image URL is present, open that validated image URL. A nonempty body without a `url` closes without opening a page. Do not block navigation on acknowledgement networking. Manual dismissal never navigates. Uploaded image URLs keep their existing expiry and require service or tailnet access; external image URLs open at their original host.

The service worker uses `event.waitUntil()` to submit an acknowledgement with its device token and notification-specific capability. Store only a digest of that capability alongside the ID in a one-hour, bounded in-memory record created at send time. On valid acknowledgement mark it acknowledged and broadcast its ID to connected devices. Repeated acknowledgements are harmless.

Pages receive events over an authenticated fetch-based SSE stream so the token can stay in an Authorization header. Native `EventSource` cannot supply that header; do not place device tokens in stream URLs. Use a small, focused event parser for this server's fixed event format. On an acknowledgement event, call `registration.getNotifications({ tag })` and close matches.

On page opening and stream reconnection, enumerate currently visible notifications and query their IDs in a bounded batch. Close those the server still remembers as acknowledged. Stream events are hints; the reconciliation lookup covers missed events while records exist.

Do not send silent dismissal pushes. Background browsers, especially Safari, cannot be relied upon to process them without displaying a notification. Dismissal on other devices therefore works when their page is active or reconnects, not necessarily while closed. If a device cannot reach the server on click, it closes locally but cross-device acknowledgement is lost. There is no persistent offline acknowledgement queue.

Restart, expiry or capacity eviction loses these short-lived records. Unknown IDs leave local notifications alone. Push events still display their valid notification without a pre-display server lookup; this avoids suppressing user-visible pushes in ways browsers may prohibit. A late push can consequently reappear after acknowledgement. This is an accepted limitation, not a delivery guarantee to conceal.

### Images

Uploaded images are written under random 256-bit capability URLs with a 24-hour expiry, longer than the push TTL. Persist only file metadata needed for serving and expiry; no title, body or click URL accompanies it. A restart preserves the original expiry. Sweep expired files at startup and periodically; clean up abandoned temporary files.

Serve accepted PNG, JPEG or WebP files with a fixed validated Content-Type, `X-Content-Type-Options: nosniff`, and `Cache-Control: no-store`. Check a bounded file signature against the declared type using a focused implementation/library; do not decode or transcode images. Reject SVG, HTML and unsupported formats. Use generated storage names, never client filenames, and write atomically.

The server does not download supplied `image_url` values. The receiving browser fetches them directly, so their host may observe the receiving device and request. An uploaded image capability URL is readable by anyone who obtains it and can reach the service. Neither image form guarantees display on every OS/browser. Failure to fetch or display the image leaves the text notification useful; expiry produces 404. The service does not provide a fallback image viewer or inbox.

## Security and resource boundaries

Keep checks at the boundary and pass typed validated values into operations. Avoid a second authorization mechanism hidden inside generic middleware.

| Boundary | Check and purpose |
| --- | --- |
| HTTP caller → send | Digest lookup for Bearer key; prevents unauthorized delivery |
| Browser → administration | Exact configured Origin, JSON requests, no admin CORS, and verified one-use passkey ceremony; prevents cross-origin invocation and replay |
| Browser → bootstrap | High-entropy expiring setup capability plus one-use registration state; prevents first-visitor takeover |
| Browser → stored subscription | Validate key encodings/lengths and supported HTTPS push endpoint origins; prevents malformed encryption inputs and an outbound request proxy |
| Server → push endpoint | Explicit supported provider origins, standard port, no URL credentials/fragments, no redirects, normal TLS verification; confines outbound destinations |
| Device → acknowledgement | Active device token plus notification capability; prevents a send key or guessed ID from dismissing notifications |
| Untrusted strings → UI | DOM text APIs, never HTML interpolation; prevents notification/key/device names becoming script |
| Untrusted URL → navigation | Parse once, require absolute HTTPS, reject userinfo; prevents script-scheme navigation |

Use a small explicit set of canonical push origins for the browsers tested by the project. Verify those origins from actual subscription samples and provider documentation during implementation. Reject unsupported origins with a clear error; do not add an arbitrary-endpoint option or a generic proxy. A provider change requires a reviewed compatibility update. Private application click/image URLs remain allowed: tailnet URLs are part of the use case, and the server does not fetch them.

Initial fixed limits, expressed in bytes unless stated otherwise:

- Title: 200 UTF-8 bytes; body: 1,500; names: 100; each URL: 2,048.
- Final plaintext push payload: 3,000 bytes including metadata, then verify the crate's encrypted wire-size limit before any delivery.
- JSON requests: 16 KiB; multipart total: 5 MiB plus 16 KiB overhead; one image maximum.
- Stored records: 64 devices, 128 send keys, 16 administrator passkeys.
- Pending ceremonies: 128 globally, two-minute expiry. Setup ceremonies cannot outlive their setup code.
- Sends: four requests in flight, eight provider requests concurrently in total; 10-second provider timeout and 90-second total send deadline.
- Provider responses: consume at most 16 KiB; cancellation stops scheduling unsent recipients.
- Image storage: 256 MiB quota; reject an upload before fan-out when it cannot fit. Expired-file cleanup frees capacity.
- Transient notification records: 4,096; evict oldest records on overflow. Reconciliation: at most 100 IDs per request.
- Event streams: two per device, 128 total, bounded channel buffers; disconnect slow clients and reconcile on reconnect.

Apply coarse request-rate limits to setup/challenge creation and authenticated sending, using bounded limiter state. Unauthorized requests must not allocate image files or trigger fan-out. Stream uploads under a size limit rather than buffering unbounded input. The initial constants can be adjusted in code after actual use; they are not a configuration subsystem.

Serve assets with a restrictive CSP using same-origin scripts, no inline JavaScript or third-party CDNs, and `frame-ancestors 'none'`. Keep the service worker at a fixed same-origin path; do not cache API responses, keys or images. Use `no-store` for all sensitive API responses. Send keys entered into third-party sites are exposed to that site's scripts; a compromised administrator origin can exercise administration despite passkeys.

Keep the data directory private to the service account. SQLite contains subscription capabilities and encryption material, passkey public data, token digests and VAPID private material. Protect its backups accordingly. Log operation outcome, nonsecret IDs and aggregate delivery counts; exclude tokens, endpoints, URL capabilities, request bodies and notification content. The service has no application message history, but browsers and external push services have their own retention behavior.

## Storage and restart behavior

| Storage | Contents | Restart behavior |
| --- | --- | --- |
| SQLite | Stable logical administrator ID, passkey records, send-key digests, device subscriptions/token digests, VAPID pair, bootstrap state, temporary image metadata | Preserved |
| Image directory | Accepted temporary images | Preserved until original expiry |
| Memory | Pending ceremonies, notification capability digests/acknowledgements, streams, in-flight delivery | Lost |

Use one schema with forward migrations and ordinary SQLite transactions. Use an exclusive process lock so two server/reset processes cannot operate on the same directory concurrently. Do not build a storage interface or interchangeable backend.

A restart retains keys and subscriptions and does not rotate VAPID. Active clients reconnect, administrative prompts restart, and uncertain sends are not automatically retried. Recover no message payloads. Back up persistent state and images together if desired; the operator guide explains that restoring a backup can restore keys or subscriptions revoked after that backup.

## Suggested implementation shape

Proposed files:

```text
Cargo.toml
Containerfile
.containerignore
crates/server/src/{main,config,http,admin,devices,notify,push,images,store}.rs
crates/cli/src/main.rs
crates/protocol/src/lib.rs
web/{index.html,styles.css,app.js,api.js,passkeys.js,devices.js,sw.js}
web/{manifest.webmanifest,icons/}
migrations/
tests/
README.md
docs/{operator-guide,security,development}.md
docs/proposals/chime-proposal.md
examples/quadlet/{chime.build,chime.container,chime-tailscale.container,chime.pod}
examples/quadlet/tailscale/serve.json
```

These are focused modules, not separate services. Keep operation types and dispatch in `admin.rs`; network boundaries in HTTP/push modules; SQL and transaction boundaries in `store.rs`. Extract smaller modules when existing behavior benefits, not for hypothetical extension points.

Use Tokio/Axum for HTTP and bounded async work, Serde for boundary types, a focused SQLite crate such as rusqlite, webauthn-rs for passkey ceremonies, and a reviewed Web Push crate. Keep synchronous SQLite work off async executor threads through a small bounded blocking path. Use maintained randomness, digest and constant-time comparison primitives as needed; do not implement cryptographic protocols. Reuse the HTTP stack selected by the push crate where practical rather than adding overlapping clients.

Before selecting versions, inspect dependency trees, supported features and maintenance status. In particular, confirm server-held ceremony state, required user verification, VAPID import/persistence, encryption limits, redirect controls and bounded response handling. A crate is justified by the security-sensitive protocol it implements; incompatible behavior must be resolved before committing to it. Pin the resolved dependency graph in Cargo.lock.

Use native browser modules and APIs, no frontend framework, bundler, vendor SDK or browser crypto implementation. Base64url conversion for WebAuthn byte fields remains a small boundary adapter. Browser automation and protocol fixtures belong in development tooling, never production assets.

The README leads with value, a short setup/send example and the main limits. Link detailed deployment/backup instructions, trust boundaries, and development procedures from focused documents. Keep the API contract authoritative in one place rather than repeating it throughout the documentation.

### Implementation sequence

1. Establish server, embedded static assets, SQLite schema, VAPID persistence and exact origin configuration.
2. Implement bootstrap and per-operation passkey administration. Verify replay, operation binding and concurrent completion before connecting key issuance.
3. Add named send keys, device enrollment, minimal text delivery, API summaries and CLI.
4. Add cross-origin sending, uploaded/external images and bounded fan-out/storage handling.
5. Add click handling, device-authenticated streams and transient acknowledgement reconciliation.
6. Add the Containerfile and working Quadlet/Serve examples; verify build, startup, persistence and restart behavior with Podman.
7. Complete browser verification and concise user/operator documentation.

No existing data migration is required. Each stage should remain within the final authority model; do not introduce temporary unauthenticated administration endpoints.

## Non-goals

- Notification history, inbox, content retrieval or guaranteed recovery after OS dismissal.
- Guaranteed delivery, exactly-once sending, durable retries, scheduling or delivery receipts.
- Reliable silent background dismissal or an offline acknowledgement queue.
- Multiple users, roles, scopes, key delegation or recipient topics.
- Public internet hosting, Cloudflare services or vendor-account integrations.
- Hardware-only passkeys, attestation allowlists, PRF-derived secrets or passkey largeBlob storage.
- Pluggable databases, frontend frameworks, native applications or service extensibility frameworks.

## Verification

Use Rust integration tests for authority transitions, transactional behavior and failures; use real browser checks for browser-controlled behavior. A fake push HTTP server is a test-only transport seam and cannot disable endpoint validation in the production binary.

1. Bootstrap: invalid/expired codes fail; concurrent first registrations produce one administrator; initialized restarts do not reopen setup; offline reset preserves the documented state and refuses concurrent server access.
2. Passkeys: reject wrong origin/RP/challenge, missing UV, replayed assertions, mutated operation attempts, expired requests and revoked credentials. Concurrent completion executes once. Adding a passkey needs existing authority; last-passkey removal fails. Use an actual passkey provider as well as a browser virtual authenticator.
3. Keys: plaintext appears only in the creation response; database/logs contain no plaintext; each key sends but cannot administer; revocation blocks later sends without affecting other keys.
4. Devices: passkey enrollment is required; duplicate endpoint enrollment rotates the token without duplicating delivery; removal invalidates tokens/streams; malformed keys, unsupported endpoint origins and redirects are rejected.
5. Sending: JSON and multipart reach all active devices; payload overflow is rejected before fan-out; partial failure and no-device outcomes are explicit; 404/410 cleanup differs from transient failures; timeouts and cancellation stay within resource limits. Restart does not replay sends.
6. Browser CORS: from a distinct HTTPS origin, test preflight and Bearer send, including invalid-key responses. Confirm administrative reads/mutations remain inaccessible cross-origin. Test tailnet local-network permission behavior on the target Chrome setup.
7. Images: accepted image types render where supported; spoofed types, SVG, oversized uploads, traversal filenames and quota overflow fail. Expiry works across restart, external URLs are never server-fetched, and a failed image fetch leaves text usable.
8. Acknowledgement: a click closes locally and opens its URL without waiting for the server; active peers close matching tags; missed events reconcile; wrong tokens/capabilities fail; repeated acknowledgement is harmless. Demonstrate that restart/offline/closed-page cases have the documented limitations and emit no silent push.
9. Real delivery: test Chrome on the user's Linux desktop, Chrome Android if used, and a Home Screen web app on iPhone/iPad if used. Verify permission/enrollment, closed-page receipt, URL clicks and actual image behavior. Record the supported browser matrix from these results; do not infer it solely from API presence.
10. Boundaries: exercise request/challenge/storage/stream limits and slow clients; inspect redacted logs and sensitive response caching headers. Keep these focused on distinct guarantees rather than mirroring every implementation function.
11. Deployment: build from a clean checkout with Podman and start the rootless Quadlets on the documented minimum versions. Verify tailnet HTTPS, shared-loopback forwarding, SSE and real push delivery; confirm no host ports or Funnel are enabled. Recreate each container and reboot the host: subscriptions, VAPID identity, send keys and Tailscale identity survive, with both services recovering. Check non-root runtime permissions, read-only root operation, SELinux mounts where applicable, secret injection/removal and offline reset locking. Missing authentication, mismatched origin and unavailable sidecar must produce actionable diagnostics without broadening exposure. Keep deployment checks in development tooling.

## Success criteria

- One Rust service behind tailnet HTTPS delivers text and optional images to enrolled browsers using standard Web Push.
- The repository includes a working Podman Containerfile and rootless systemd `.build`/`.container` examples with a Tailscale sidecar, persistent state and documented lifecycle commands.
- Static browser modules provide setup, device enrollment and key/passkey management without a frontend build system.
- A CLI and a separate-origin web application can send using independently revocable send-only keys.
- Administrative mutations require fresh verified passkey authority bound to the intended operation.
- Notification content is not retained as history; temporary files and transient state have enforced bounds and expiry.
- Connected-device dismissal works and background/offline/restart limitations are documented and observable.
- Protocol/security dependencies reduce custom code, and trust boundaries can be traced through focused modules.

## Reference checks

These references ground protocol constraints and candidate libraries; they are not promises of compatibility without the verification above.

- WebAuthn ceremony state and passkey verification: https://docs.rs/webauthn-rs/latest/webauthn_rs/ and https://docs.rs/webauthn-rs/latest/webauthn_rs/struct.Webauthn.html
- Rust Web Push candidate: https://docs.rs/web-push/
- User-visible push requirement and Safari behavior: https://webkit.org/blog/12945/meet-web-push/
- Standard Web Push payload overhead and protocol: https://web.dev/articles/push-notifications-web-push-protocol
- Browser local-network controls: https://developer.chrome.com/blog/local-network-access
- Apple Web Push setup and delivery constraints: https://developer.apple.com/documentation/usernotifications/sending-web-push-notifications-in-web-apps-and-browsers
- Podman Quadlet build/container/pod units: https://docs.podman.io/en/latest/markdown/podman-systemd.unit.5.html
- Tailscale container state, authentication and Serve configuration: https://tailscale.com/docs/features/containers/docker/docker-params
