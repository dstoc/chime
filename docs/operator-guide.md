# Rootless Podman deployment

This guide runs Chime in a rootless Podman pod. The Tailscale sidecar provides HTTPS access inside your tailnet. Chime keeps its default listener on the pod's loopback interface, so the host publishes no ports. Chime uses ordinary container egress for Web Push; it does not send provider traffic through Tailscale.

The example pins the Rust, Debian, and Tailscale image versions. Review and update those versions deliberately. Image builds need registry, Debian package, and Cargo registry access, but no production credentials.

## Requirements

- Linux with rootless Podman 5.4 or newer, systemd 252 or newer, and cgroup v2. See the [Podman Quadlet reference](https://docs.podman.io/en/v5.4.2/markdown/podman-systemd.unit.5.html) for unit support and the [build unit reference](https://docs.podman.io/en/v5.4.2/markdown/podman-build.unit.5.html) for `.build` fields.
- A rootless Podman user with subordinate UID/GID mappings configured.
- A Tailscale tailnet with MagicDNS and HTTPS certificates enabled. The operator must configure tailnet access grants; the example does not enable Funnel.
- A checkout of Chime on the host.

Quadlet generates user systemd services from `.build`, `.pod`, and `.container` files. The example uses `Pod=` to associate both containers with the pod and `StartWithPod=true` to start them with it. The generated service names are `chime-build.service`, `chime-pod.service`, `chime.service`, and `chime-tailscale.service`.

## Install and edit the units

From the Chime checkout, copy the units and the Serve configuration:

```sh
mkdir -p "$HOME/.config/containers/systemd" "$HOME/.config/chime/tailscale"
chmod 0700 "$HOME/.config/containers/systemd" "$HOME/.config/chime" "$HOME/.config/chime/tailscale"
install -m 0600 examples/quadlet/chime.build "$HOME/.config/containers/systemd/chime.build"
install -m 0600 examples/quadlet/chime.container "$HOME/.config/containers/systemd/chime.container"
install -m 0600 examples/quadlet/chime-tailscale.container "$HOME/.config/containers/systemd/chime-tailscale.container"
install -m 0600 examples/quadlet/chime.pod "$HOME/.config/containers/systemd/chime.pod"
install -m 0600 examples/quadlet/tailscale/serve.json "$HOME/.config/chime/tailscale/serve.json"
```

Edit `chime.build`. Replace both `/src/chime` paths with the absolute path to your checkout. Quadlet uses this directory as the build context and reads its `Containerfile`.

Edit `chime.container`. Replace `https://chime.example-tailnet.ts.net` with the HTTPS origin you expect Tailscale to assign. Keep the origin as a scheme and DNS hostname with no path.

The sidecar requests the hostname `chime`. The actual tailnet FQDN can differ if that name is unavailable or the tailnet assigns a different name. Verify it before registering any passkeys, as described below.

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

## Build and start

Reload the user systemd manager, build the image, then start the pod:

```sh
systemctl --user daemon-reload
systemctl --user start chime-build.service
systemctl --user start chime-pod.service
systemctl --user status chime-pod.service chime.service chime-tailscale.service
```

The build unit allows 15 minutes for the locked release build. `Pod=` provides the pod dependency. Both containers and the pod use `Restart=always`; Quadlet ordering does not mean that Chime's HTTP listener, Tailscale authentication, or Serve configuration is ready.

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

Keep `TS_AUTH_ONCE=true` and the persistent Tailscale state mount. They let the node reuse its saved identity without the bootstrap key.

## Operate the deployment

Follow service logs and inspect state with:

```sh
journalctl --user -u chime.service -f
journalctl --user -u chime-tailscale.service -f
systemctl --user status chime-pod.service chime.service chime-tailscale.service
```

Restart either container to confirm recovery. Chime's persistent directory retains its SQLite database, VAPID identity, send keys, and subscriptions. The separate Tailscale state directory retains the node identity.

```sh
systemctl --user restart chime.service
systemctl --user restart chime-tailscale.service
```

If Tailscale is unavailable, Chime remains bound to pod loopback and no host port becomes available. Unit ordering does not guarantee tailnet or application readiness. Check `tailscale status`, `tailscale serve status --json`, the configured origin, and both services' logs before diagnosing browser failures.

To build an updated Chime checkout and recreate its container:

```sh
systemctl --user stop chime-pod.service
systemctl --user restart chime-build.service
systemctl --user start chime-pod.service
```

When reviewing a Tailscale image update, change its exact version in `chime-tailscale.container`, reload the user manager, and restart `chime-tailscale.service`. Keep the existing Tailscale state directory.

Stop the deployment with:

```sh
systemctl --user stop chime-pod.service
```

For unattended starts after host reboot, enable lingering for your user if your distribution does not already do so:

```sh
sudo loginctl enable-linger "$USER"
```

## Reset administrator passkeys

The reset command needs the same Chime image and data mount as the server. Stop the pod first so Chime releases its exclusive data-directory lock. Then run the command as the container's UID and restart the pod:

```sh
systemctl --user stop chime-pod.service
podman run --rm --network=none --read-only --user 10001:10001 \
  --volume "$HOME/.local/share/chime/data:/var/lib/chime:rw,Z" \
  localhost/chime:0.1.0 reset-admin --data-dir /var/lib/chime
systemctl --user start chime-pod.service
```

The command prints a new sensitive setup code. It clears administrator passkeys and preserves send keys, device subscriptions, and the VAPID identity. Revoke send keys separately if compromise is suspected. If the lock is unavailable, confirm that `chime.service` and any other process using this data directory have stopped before retrying.

## Back up and restore state

Stop the pod before making a consistent backup. Store Chime state and Tailscale node state in separate protected archives. Chime's data directory includes the SQLite database and uploaded images. Both archives contain sensitive material; encrypt them before sending them off the host.

```sh
systemctl --user stop chime-pod.service
umask 077
podman unshare tar --numeric-owner --xattrs -C "$HOME/.local/share/chime" \
  -czf "$HOME/chime-data-backup.tar.gz" data
podman unshare tar --numeric-owner --xattrs -C "$HOME/.local/share/chime" \
  -czf "$HOME/chime-tailscale-backup.tar.gz" tailscale
systemctl --user start chime-pod.service
```

Restore each archive to its matching directory with the services stopped. Restore the Tailscale state only to the host that will own that node identity. Losing Tailscale state may assign a new FQDN or HTTPS identity; verify the origin again before passkey use. Restoring an older Chime backup can restore send keys or subscriptions revoked after that backup.

## Diagnose startup and authentication failures

Inspect logs without printing secret values:

```sh
journalctl --user -u chime-tailscale.service --since '15 minutes ago'
journalctl --user -u chime.service --since '15 minutes ago'
podman exec chime-tailscale tailscale status
podman exec chime-tailscale tailscale serve status
podman secret ls
```

If a generated service is missing after `daemon-reload`, inspect Quadlet's parser output with `/usr/lib/systemd/system-generators/podman-system-generator --user --dryrun`.

For failed Tailscale authentication, confirm the secret is named `tailscale-authkey`, the bootstrap lines are still present in the sidecar unit, the key is valid, and the unit manager was reloaded after editing. For a node that was previously enrolled, first verify that its saved Tailscale state directory is mounted and intact. Recreate the bootstrap secret only when the sidecar needs to enroll again.

For browser origin or passkey errors, compare the URL in the browser with the actual tailnet FQDN and the `--origin` in `chime.container`. For a missing HTTPS endpoint, verify that tailnet HTTPS is enabled, the node is online, and Serve status shows the forwarding rule. Neither systemd startup ordering nor a running sidecar proves these conditions.
