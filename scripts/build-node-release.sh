#!/usr/bin/env bash
set -euo pipefail

project_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

if command -v dpkg-deb >/dev/null 2>&1; then
    host_formats="deb,tar"
else
    host_formats="tar"
fi

for architecture in amd64 arm64; do
    echo "Building gnomeai-node for $architecture"
    GNOMEAI_NODE_ARCH="$architecture" GNOMEAI_NODE_FORMATS="$host_formats" \
        "$project_root/scripts/build-node-packages.sh"
done

echo "Building gnomeai-node for arm64-musl"
GNOMEAI_NODE_ARCH=arm64-musl GNOMEAI_NODE_FORMATS=tar \
    "$project_root/scripts/build-node-packages.sh"

if [[ "${GNOMEAI_SKIP_XBPS:-0}" != "1" ]]; then
    if command -v xbps-create >/dev/null 2>&1; then
        GNOMEAI_SKIP_BUILD=1 \
            GNOMEAI_BIN_DIR="$project_root/target/release" \
            GNOMEAI_NODE_FORMATS=xbps \
            "$project_root/scripts/build-node-packages.sh" amd64
        GNOMEAI_SKIP_BUILD=1 \
            GNOMEAI_BIN_DIR="$project_root/target/aarch64-unknown-linux-gnu/release" \
            GNOMEAI_NODE_FORMATS=xbps \
            "$project_root/scripts/build-node-packages.sh" arm64
        GNOMEAI_SKIP_BUILD=1 \
            GNOMEAI_BIN_DIR="$project_root/target/aarch64-unknown-linux-musl/release" \
            GNOMEAI_NODE_FORMATS=xbps \
            "$project_root/scripts/build-node-packages.sh" arm64-musl
    else
        "$project_root/scripts/build-node-xbps-container.sh"
    fi
fi

echo "Node release packages are available in $project_root/dist"
