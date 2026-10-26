# Rootless Podman deployment

This guide runs a published Chime image and Tailscale as two rootless Quadlet container services. Tailscale owns the network namespace. Chime joins that namespace and keeps listening on `127.0.0.1:8080`, where Tailscale Serve can reach it. The host publishes no ports. Chime uses ordinary container egress for Web Push; it does not send provider traffic through Tailscale.

The example pins a Chime release and a Tailscale image version. Review and update those versions deliberately. Chime image pulls need GHCR access; the published package is public and does not require registry credentials.

## Requirements and lifecycle behavior

- Linux with rootless Podman and Quadlet 5.4.2 or newer, systemd 252 or newer, and cgroup v2. Podman 5.4.2 documents `Network=<name>.container`; the systemd dependencies used here are available in systemd 252. See the [Podman Quadlet reference](https://docs.podman.io/en/v5.4.2/markdown/podman-systemd.unit.5.html) and [systemd unit reference](https://www.freedesktop.org/software/systemd/man/252/systemd.unit.html).
- A rootless Podman user with subordinate UID/GID mappings configured.
- A Tailscale tailnet with MagicDNS and HTTPS certificates enabled. The operator must configure tailnet access grants; the example does not enable Funnel.
- `curl` to download the two container units and Serve configuration from a Chime release tag.

The Chime service declares `Network=chime-tailscale.container`. Quadlet generates its dependency on `chime-tailscale.service`. `After=`, `BindsTo=`, and `PartOf=` make Chime stop when Tailscale stops or becomes inactive and propagate a Tailscale restart to Chime. Tailscale declares `Wants=chime.service`, so starting Tailscale starts Chime again. Starting Chime directly also starts Tailscale through the generated network dependency.

These dependencies manage service lifecycle. They do not prove that Tailscale authenticated, Serve is ready, or Chime is healthy. A deliberate `systemctl --user stop chime.service` leaves Tailscale running. Starting or restarting Tailscale later starts Chime again.

The version numbers above are the documented feature floor, not the result of a runtime lifecycle test for this example. The development runner used for this change had systemd 257 but no Podman, Quadlet generator, or user systemd manager. Inspect the generated units and run the lifecycle checks below on the target host before relying on namespace recreation behavior.

## Install and edit the units

Choose a published Chime version. The example uses the first planned release, `0.2.0`. Set `VERSION` to the release you want to deploy. Download the two container units and Serve configuration from that tag:

```sh
VERSION=0.2.0
BASE="https://raw.githubusercontent.com/dstoc/chime/v${VERSION}/examples/quadlet"
mkdir -p "$HOME/.config/containers/systemd" "$HOME/.config/chime/tailscale"
chmod 0700 "$HOME/.config/containers/systemd" "$HOME/.config/chime" "$HOME/.config/chime/tailscale"
curl --fail --location --output "$HOME/.config/containers/systemd/chime.container" "$BASE/chime.container"
curl --fail --location --output "$HOME/.config/containers/systemd/chime-tailscale.container" "$BASE/chime-tailscale.container"
curl --fail --location --output "$HOME/.config/chime/tailscale/serve.json" "$BASE/tailscale/serve.json"
chmod 0600 "$HOME/.config/containers/systemd/chime.container" "$HOME/.config/containers/systemd/chime-tailscale.container" "$HOME/.config/chime/tailscale/serve.json"
```

Set the image version in `chime.container` to the release you selected. You can also use the release's SHA-256 digest to pin the exact image. Replace `https://chime.example-tailnet.ts.net` with the HTTPS origin you expect Tailscale to assign. Keep the origin as a scheme and DNS hostname with no path.

```sh
sed -i "s|^Image=.*|Image=ghcr.io/dstoc/chime:${VERSION}|" "$HOME/.config/containers/systemd/chime.container"
# To pin by digest instead, set IMAGE_DIGEST from the release notes or digest asset:
# sed -i "s|^Image=.*|Image=ghcr.io/dstoc/chime@${IMAGE_DIGEST}|" "$HOME/.config/containers/systemd/chime.container"
```

The sidecar requests the hostname `chime`. The actual tailnet FQDN can differ if that name is unavailable or the tailnet assigns a different name. Verify it before registering any passkeys, as described below.

The generated service names are `chime.service` and `chime-tailscale.service`. Keep the two state directories separate. Chime's container remains non-root with a read-only root filesystem and one writable data mount. Tailscale uses userspace networking and receives its state, Serve configuration, and bootstrap secret through separate mounts or a Podman secret.

## Create persistent directories and the bootstrap secret

Chime runs as UID and GID 10001 inside the container. Create separate private state directories. `:Z` on the bind mounts gives each container a private SELinux label where SELinux is enabled.

```sh
install -d -m 0700 "$HOME/.local/share/chime/data" "$HOME/.local/share/chime/tailscale"
podman unshare chown -R 10001:10001 "$HOME/.local/share/chime/data"
podman unshare chmod 0700 "$HOME/.local/share/chime/data"
```

The Tailscale directory stays owned by your host user; the rootless container's UID 0 maps to that user. Chime and Tailscale state use separate directories and are mounted only into their respective containers.

Create a one-use, non-ephemeral Tailscale auth key. Read it without echoing it or putting it in shell history, then send it directly to Podman's secret store:

```sh
read -r -s -p 'Tailscale auth key: ' TS_AUTHKEY
printf '\n'
printf '%s' "$TS_AUTHKEY" | podman secret create tailscale-authkey -
unset TS_AUTHKEY
```

The sidecar receives this secret as a read-only file and passes `file:/run/secrets/tailscale-authkey` to Tailscale's auth-key input. The key is not stored in the unit, image, or environment value. Protect access to the rootless Podman account and its secret store. See [Tailscale container parameters](https://tailscale.com/docs/features/containers/docker/docker-params) and the [Tailscale Serve container example](https://tailscale.com/blog/docker-tailscale-guide) for the supported environment settings and Serve configuration format.

## Pull and start

Reload the user systemd manager and start Tailscale. Its `Wants=` dependency starts Chime and Podman pulls the selected image:

```sh
systemctl --user daemon-reload
systemctl --user start chime-tailscale.service
systemctl --user status chime-tailscale.service chime.service
```

Quadlet applies each unit's `[Install]` section during generation, so `WantedBy=default.target` starts both services when the user manager starts. Do not run `systemctl --user enable` on generated Quadlet services. Both container services use `Restart=always`. Unit ordering does not mean that Chime's HTTP listener, Tailscale authentication, or Serve configuration is ready.

Check the sidecar logs and its assigned name:

```sh
journalctl --user -u chime-tailscale.service -f
podman exec chime-tailscale tailscale status
podman exec chime-tailscale tailscale serve status --json
```

The status output and Tailscale admin console show the actual FQDN. Confirm that it exactly matches the `--origin` value in `chime.container`. Tailscale Serve substitutes `${TS_CERT_DOMAIN}` in `serve.json`; it forwards HTTPS on port 443 to `http://127.0.0.1:8080` and sets Funnel to `false`. If the assigned FQDN differs, edit `Exec=` in `chime.container`, run `systemctl --user daemon-reload`, and restart Chime:

```sh
systemctl --user restart chime.service
```

Only continue with passkey setup after the browser URL, Serve FQDN, and configured origin match exactly. A mismatch can cause browser origin and passkey failures.

For unattended starts after host reboot, enable lingering for your user if your distribution does not already do so:

```sh
sudo loginctl enable-linger "$USER"
```

## Register the first administrator

Chime prints a single-use setup code to its startup log. Read it locally and keep it private. It expires after 15 minutes; do not paste it into a ticket, chat, screenshot, or shared log.

```sh
journalctl --user -u chime.service -f
```

Open the verified HTTPS FQDN in a browser, enter the code, and register an administrator passkey. If the code expires before registration, use the offline reset procedure below to generate another.

After Tailscale has authenticated and `tailscale status` confirms the node is online, remove the bootstrap secret injection. Delete the `Secret=` and `Environment=TS_AUTHKEY=...` lines from `chime-tailscale.container`, then recreate the sidecar from its saved state:

```sh
systemctl --user daemon-reload
systemctl --user restart chime-tailscale.service
podman secret rm tailscale-authkey
```

The Tailscale restart also stops and starts Chime so it joins the new network namespace. Keep `TS_AUTH_ONCE=true` and the persistent Tailscale state mount. They let the node reuse its saved identity without the bootstrap key.

## Operate the services

Follow service logs and inspect state with:

```sh
journalctl --user -u chime.service -f
journalctl --user -u chime-tailscale.service -f
systemctl --user status chime.service chime-tailscale.service
```

Restart Chime without interrupting Tailscale:

```sh
systemctl --user restart chime.service
```

Stop and start Tailscale to stop and start both containers. Restarting Tailscale recreates Chime after Tailscale, so Chime joins the current network namespace:

```sh
systemctl --user stop chime-tailscale.service
systemctl --user start chime-tailscale.service
systemctl --user restart chime-tailscale.service
```

To shut down the deployment, stop Tailscale. `BindsTo=` and `PartOf=` stop Chime with it:

```sh
systemctl --user stop chime-tailscale.service
```

Chime's persistent directory retains its SQLite database, VAPID identity, send keys, and subscriptions. The separate Tailscale state directory retains the node identity. If Tailscale is unavailable, Chime does not publish a host port. Check `tailscale status`, `tailscale serve status --json`, the configured origin, and both services' logs before diagnosing browser failures.

### Update Chime

Select a released version or digest in `chime.container`, then reload Quadlet and restart Chime. Tailscale remains running:

```sh
sed -i 's|^Image=.*|Image=ghcr.io/dstoc/chime:0.2.0|' "$HOME/.config/containers/systemd/chime.container"
systemctl --user daemon-reload
systemctl --user restart chime.service
```

Replace `0.2.0` with the selected version. To pin a digest, use `Image=ghcr.io/dstoc/chime@sha256:<digest>` instead. Podman pulls the new image when it starts the service.

### Update Tailscale

Change the exact version in `chime-tailscale.container`, reload the user manager, and restart Tailscale. Systemd stops Chime before replacing Tailscale and starts Chime again after Tailscale:

```sh
systemctl --user daemon-reload
systemctl --user restart chime-tailscale.service
```

Keep the existing Tailscale state directory. Do not use `podman rm` as an update procedure; use the systemd service so the dependent Chime service is stopped and recreated in order.

### Recover after an out-of-band container removal

If a container was removed directly with Podman, use systemd to recreate it and its dependent service:

```sh
systemctl --user restart chime-tailscale.service
systemctl --user status chime-tailscale.service chime.service
```

If either unit is failed, inspect its journal. After correcting the cause, clear the failed state and start Tailscale; its `Wants=` dependency starts Chime:

```sh
journalctl --user -u chime-tailscale.service -u chime.service --since '15 minutes ago'
systemctl --user reset-failed chime-tailscale.service chime.service
systemctl --user start chime-tailscale.service
```

### Check the generated dependency and namespace

To inspect the generated services, run the Quadlet generator in dry-run mode. The generator path can vary by distribution:

```sh
QUADLET_UNIT_DIRS="$HOME/.config/containers/systemd" /usr/lib/systemd/system-generators/podman-system-generator --user --dryrun
```

Confirm that `chime.service` contains the generated dependency on `chime-tailscale.service` and retains `After=`, `BindsTo=`, and `PartOf=`. Confirm that `chime-tailscale.service` contains `Wants=chime.service`. There must be no ordering cycle.

After a Tailscale restart or recreation, compare the containers' network namespace IDs. They must match:

```sh
for name in chime-tailscale chime-server; do
  pid="$(podman inspect --format '{{.State.Pid}}' "$name")"
  printf '%s: ' "$name"
  readlink "/proc/${pid}/ns/net"
done
```

Run this check after fresh startup, a Chime-only restart, a Tailscale stop/start, a Tailscale restart, and a container recreation through systemd. A generated dependency or active service alone does not prove that both containers use the same network namespace.

## Reset administrator passkeys

The reset command needs the same Chime image and data mount as the server. Stop Chime first so it releases its exclusive data-directory lock. Then run the command as the container's UID and start Chime again:

```sh
IMAGE=ghcr.io/dstoc/chime:0.2.0
systemctl --user stop chime.service
podman run --rm --network=none --read-only --user 10001:10001 \
  --volume "$HOME/.local/share/chime/data:/var/lib/chime:rw,Z" \
  "$IMAGE" reset-admin --data-dir /var/lib/chime
systemctl --user start chime.service
```

Set `IMAGE` to the exact version or digest reference in `chime.container`.

The command prints a new sensitive setup code. It clears administrator passkeys and preserves send keys, device subscriptions, and the VAPID identity. Revoke send keys separately if compromise is suspected. If the lock is unavailable, confirm that `chime.service` and any other process using this data directory have stopped before retrying.

## Back up and restore state

Stop Tailscale before making a consistent backup; this also stops Chime. Store Chime state and Tailscale node state in separate protected archives. Chime's data directory includes the SQLite database and uploaded images. Both archives contain sensitive material; encrypt them before sending them off the host.

```sh
systemctl --user stop chime-tailscale.service
umask 077
podman unshare tar --numeric-owner --xattrs -C "$HOME/.local/share/chime" \
  -czf "$HOME/chime-data-backup.tar.gz" data
podman unshare tar --numeric-owner --xattrs -C "$HOME/.local/share/chime" \
  -czf "$HOME/chime-tailscale-backup.tar.gz" tailscale
systemctl --user start chime-tailscale.service
```

To restore, stop Tailscale and move the current directories aside before extracting the archives. Choose unused snapshot names if these paths already exist:

```sh
systemctl --user stop chime-tailscale.service
mv "$HOME/.local/share/chime/data" "$HOME/.local/share/chime/data.pre-restore"
mv "$HOME/.local/share/chime/tailscale" "$HOME/.local/share/chime/tailscale.pre-restore"
podman unshare tar --numeric-owner --xattrs -C "$HOME/.local/share/chime" \
  -xzf "$HOME/chime-data-backup.tar.gz"
podman unshare tar --numeric-owner --xattrs -C "$HOME/.local/share/chime" \
  -xzf "$HOME/chime-tailscale-backup.tar.gz"
systemctl --user start chime-tailscale.service
```

Keep the pre-restore directories until both services work. Restore Tailscale state only to the host that will own that node identity. Losing Tailscale state may assign a new FQDN or HTTPS identity; verify the origin again before passkey use. Restoring an older Chime backup can restore send keys or subscriptions revoked after that backup. The Podman bootstrap secret is not included in these archives; create a new auth key only if Tailscale must enroll again.

## Diagnose startup and authentication failures

Inspect logs without printing secret values:

```sh
journalctl --user -u chime-tailscale.service --since '15 minutes ago'
journalctl --user -u chime.service --since '15 minutes ago'
podman exec chime-tailscale tailscale status
podman exec chime-tailscale tailscale serve status
podman secret ls
```

If a generated service is missing after `daemon-reload`, inspect Quadlet's parser output with `/usr/lib/systemd/system-generators/podman-system-generator --user --dryrun`. For failed Tailscale authentication, confirm the secret is named `tailscale-authkey`, the bootstrap lines are still present before first enrollment, the key is valid, and the unit manager was reloaded after editing. For a node that was previously enrolled, first verify that its saved Tailscale state directory is mounted and intact. Recreate the bootstrap secret only when the sidecar needs to enroll again.

For browser origin or passkey errors, compare the URL in the browser with the actual tailnet FQDN and the `--origin` in `chime.container`. For a missing HTTPS endpoint, verify that tailnet HTTPS is enabled, the node is online, and Serve status shows the forwarding rule. Neither systemd startup ordering nor a running sidecar proves these conditions.
