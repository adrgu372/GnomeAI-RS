# GnomeAI minimal node

The node is a foreground process. It does not depend on systemd, D-Bus or a
desktop environment.

1. In the main GnomeAI app, open **Settings → Devices**, enable the Hub and
   restart the app once.
2. Copy the enrollment command from the Devices window and run it on this
   machine.
3. Start the client with `gnomeai-node run`.

For runit, enroll as the unprivileged account that will own the service. Create
`/etc/sv/gnomeai-node/run`, replacing `<user>` with that account:

```sh
#!/bin/sh
exec 2>&1
export HOME=/home/<user>
exec chpst -u <user>:<user> /usr/bin/gnomeai-node run
```

Make it executable and enable the service:

```sh
sudo chmod +x /etc/sv/gnomeai-node/run
sudo ln -s /etc/sv/gnomeai-node /var/service/gnomeai-node
sudo sv up gnomeai-node
sudo sv status gnomeai-node
```

The `HOME` setting is important because enrollment writes
`~/.config/gnomeai-node/config.json`. Optional `svlogd` setup and firewall
examples are documented in the main project README.

The same foreground command works with OpenRC, s6, systemd, another process
supervisor, or manually. Root needs both local enrollment with `--allow-root`
and the per-device policy selected in the main graphical app. Use a trusted
LAN or VPN; do not expose an unencrypted Hub port directly to the internet.

## Building packages

Build both `amd64` and `arm64` node releases from an amd64 Debian/Ubuntu host:

```sh
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu podman
cargo install cross --locked
./scripts/build-node-release.sh
```

The architecture is compiled before it is put in a package; the packaging
script verifies the ELF machine type and refuses a mismatched label. Build one
architecture with `./scripts/build-node-packages.sh amd64` or
`./scripts/build-node-packages.sh arm64`.

The release script always requests XBPS packages. On Void Linux it uses the
local `xbps-create`. On Debian/Ubuntu it uses the official Void glibc OCI image
through Podman or Docker, so installing XBPS tools on the host is unnecessary.
Set `GNOMEAI_SKIP_XBPS=1` only when the XBPS artifacts are intentionally not
required.

To select the format directly on Void Linux:

```sh
GNOMEAI_NODE_FORMATS=xbps,tar ./scripts/build-node-packages.sh arm64
```

The resulting XBPS architectures are `x86_64`, `aarch64` (glibc), and
`aarch64-musl`. Void musl systems must use the `aarch64-musl.xbps` package;
XBPS correctly rejects the glibc package there. Install the packaging utility
with `sudo xbps-install -S xbps`.
