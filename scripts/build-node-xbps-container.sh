#!/usr/bin/env bash
set -euo pipefail

project_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
image="${GNOMEAI_VOID_IMAGE:-ghcr.io/void-linux/void-glibc:latest}"

if command -v podman >/dev/null 2>&1; then
    container_runtime="podman"
elif command -v docker >/dev/null 2>&1; then
    container_runtime="docker"
else
    echo "xbps-create is unavailable and neither Podman nor Docker is installed." >&2
    echo "Install Podman with: sudo apt install podman" >&2
    echo "Then rerun: ./scripts/build-node-release.sh" >&2
    echo "To intentionally omit XBPS, set GNOMEAI_SKIP_XBPS=1." >&2
    exit 1
fi

for binary in \
    "$project_root/target/release/gnomeai-node" \
    "$project_root/target/aarch64-unknown-linux-gnu/release/gnomeai-node" \
    "$project_root/target/aarch64-unknown-linux-musl/release/gnomeai-node"
do
    if [[ ! -x "$binary" ]]; then
        echo "Missing prebuilt node binary: $binary" >&2
        echo "Run ./scripts/build-node-release.sh to compile both architectures first." >&2
        exit 1
    fi
done

mkdir -p "$project_root/dist"
host_uid="$(id -u)"
host_gid="$(id -g)"

"$container_runtime" run --rm \
    -e XBPS_ALLOW_CHROOT_BREAKOUT=1 \
    -e HOST_UID="$host_uid" \
    -e HOST_GID="$host_gid" \
    -v "$project_root:/src" \
    -w /src \
    "$image" \
    /bin/sh -lc '
        set -eu
        xbps-install -S -y xbps bash file ca-certificates
        GNOMEAI_SKIP_BUILD=1 \
            GNOMEAI_BIN_DIR=/src/target/release \
            GNOMEAI_OUTPUT_DIR=/src/dist \
            GNOMEAI_NODE_FORMATS=xbps \
            /src/scripts/build-node-packages.sh amd64
        GNOMEAI_SKIP_BUILD=1 \
            GNOMEAI_BIN_DIR=/src/target/aarch64-unknown-linux-gnu/release \
            GNOMEAI_OUTPUT_DIR=/src/dist \
            GNOMEAI_NODE_FORMATS=xbps \
            /src/scripts/build-node-packages.sh arm64
        GNOMEAI_SKIP_BUILD=1 \
            GNOMEAI_BIN_DIR=/src/target/aarch64-unknown-linux-musl/release \
            GNOMEAI_OUTPUT_DIR=/src/dist \
            GNOMEAI_NODE_FORMATS=xbps \
            /src/scripts/build-node-packages.sh arm64-musl
        chown "$HOST_UID:$HOST_GID" /src/dist/*.xbps 2>/dev/null || true
    '
