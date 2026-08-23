#!/usr/bin/env bash
set -euo pipefail

project_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$project_root"

version="$(sed -n 's/^Version: //p' packaging/debian/control | head -n 1)"
output_dir="${GNOMEAI_OUTPUT_DIR:-$project_root/dist}"
requested_arch="${GNOMEAI_NODE_ARCH:-${1:-$(uname -m)}}"

case "$requested_arch" in
    amd64|x86_64)
        cpu_arch="x86_64"
        target_libc="gnu"
        deb_arch="amd64"
        output_arch="amd64"
        xbps_arch="x86_64"
        rust_target="x86_64-unknown-linux-gnu"
        cross_linker="x86_64-linux-gnu-gcc"
        ;;
    arm64|aarch64)
        cpu_arch="aarch64"
        target_libc="gnu"
        deb_arch="arm64"
        output_arch="arm64"
        xbps_arch="aarch64"
        rust_target="aarch64-unknown-linux-gnu"
        cross_linker="aarch64-linux-gnu-gcc"
        ;;
    arm64-musl|aarch64-musl)
        cpu_arch="aarch64"
        target_libc="musl"
        deb_arch="arm64"
        output_arch="arm64-musl"
        xbps_arch="aarch64-musl"
        rust_target="aarch64-unknown-linux-musl"
        cross_linker="aarch64-linux-musl-gcc"
        ;;
    *)
        echo "Unsupported node architecture: $requested_arch" >&2
        echo "Supported values: amd64, x86_64, arm64, aarch64, arm64-musl, aarch64-musl" >&2
        exit 1
        ;;
esac

host_arch="$(uname -m)"
host_libc="gnu"
if ldd_version="$(ldd --version 2>&1)" && [[ "$ldd_version" == *"musl"* ]]; then
    host_libc="musl"
elif [[ "$ldd_version" == *"musl"* ]]; then
    host_libc="musl"
fi
if [[ "$host_arch" == "$cpu_arch" && "$host_libc" == "$target_libc" ]]; then
    cargo_target_args=()
    default_binary_dir="$project_root/target/release"
else
    cargo_target_args=(--target "$rust_target")
    default_binary_dir="$project_root/target/$rust_target/release"
    linker_var="CARGO_TARGET_$(printf '%s' "$rust_target" | tr '[:lower:]-' '[:upper:]_')_LINKER"
fi
binary_dir="${GNOMEAI_BIN_DIR:-$default_binary_dir}"

if [[ "${GNOMEAI_SKIP_BUILD:-0}" != "1" ]]; then
    if ! command -v cargo >/dev/null 2>&1; then
        echo "cargo is required to build gnomeai-node" >&2
        exit 1
    fi
    if [[ ${#cargo_target_args[@]} -gt 0 && "$target_libc" == "musl" ]]; then
        if ! command -v cross >/dev/null 2>&1; then
            echo "Installing the official cross-rs build frontend for $rust_target"
            cargo install cross --locked
        fi
        if [[ -z "${CROSS_CONTAINER_ENGINE:-}" ]] \
            && command -v podman >/dev/null 2>&1 \
            && ! command -v docker >/dev/null 2>&1
        then
            export CROSS_CONTAINER_ENGINE=podman
        fi
        cross build --release --locked --bin gnomeai-node --target "$rust_target"
    elif [[ ${#cargo_target_args[@]} -gt 0 ]]; then
        if [[ -z "${!linker_var:-}" ]]; then
            if ! command -v "$cross_linker" >/dev/null 2>&1; then
                echo "Missing cross linker: $cross_linker" >&2
                if [[ "$deb_arch" == "arm64" ]]; then
                    echo "On Debian/Ubuntu install it with: sudo apt install gcc-aarch64-linux-gnu" >&2
                fi
                exit 1
            fi
            export "$linker_var=$cross_linker"
        fi
        target_libdir="$(rustc --print target-libdir --target "$rust_target" 2>/dev/null || true)"
        if [[ ! -d "$target_libdir" ]]; then
            if command -v rustup >/dev/null 2>&1; then
                rustup target add "$rust_target"
            else
                echo "Rust standard library for $rust_target is missing." >&2
                echo "Install rustup, then run: rustup target add $rust_target" >&2
                exit 1
            fi
        fi
        cargo build --release --locked --bin gnomeai-node "${cargo_target_args[@]}"
    else
        cargo build --release --locked --bin gnomeai-node
    fi
fi

binary="$binary_dir/gnomeai-node"
if [[ ! -x "$binary" ]]; then
    echo "Missing binary: $binary" >&2
    exit 1
fi

detected_machine="$(file -Lb "$binary" 2>/dev/null || true)"
case "$deb_arch" in
    amd64)
        [[ "$detected_machine" == *"x86-64"* || "$detected_machine" == *"x86_64"* ]] || {
            echo "Refusing to package a non-amd64 binary as amd64: $detected_machine" >&2
            exit 1
        }
        ;;
    arm64)
        [[ "$detected_machine" == *"ARM aarch64"* || "$detected_machine" == *"aarch64"* ]] || {
            echo "Refusing to package a non-arm64 binary as arm64: $detected_machine" >&2
            exit 1
        }
        ;;
esac

if [[ -n "${GNOMEAI_NODE_FORMATS:-}" ]]; then
    formats=",${GNOMEAI_NODE_FORMATS// /},"
else
    formats=",tar,"
    command -v dpkg-deb >/dev/null 2>&1 && formats+="deb,"
    command -v xbps-create >/dev/null 2>&1 && formats+="xbps,"
fi

wants_format() {
    [[ "$formats" == *",$1,"* ]]
}

build_root="$(mktemp -d /tmp/gnomeai-node-package.XXXXXX)"
cleanup() {
    rm -rf -- "$build_root"
}
trap cleanup EXIT

mkdir -p "$output_dir"
outputs=()

if wants_format deb; then
    if [[ "$target_libc" == "musl" ]]; then
        echo "Debian packaging is not emitted for the musl target; use tar or xbps" >&2
        exit 1
    fi
    command -v dpkg-deb >/dev/null 2>&1 || {
        echo "GNOMEAI_NODE_FORMATS requests deb, but dpkg-deb is unavailable" >&2
        exit 1
    }
    package_root="$build_root/deb"
    mkdir -p "$package_root/DEBIAN" "$package_root/usr/bin" \
        "$package_root/usr/share/doc/gnomeai-node"
    install -m 0755 "$binary" "$package_root/usr/bin/gnomeai-node"
    command -v strip >/dev/null 2>&1 \
        && strip --strip-unneeded "$package_root/usr/bin/gnomeai-node" 2>/dev/null \
        || true
    install -m 0644 packaging/debian/README-NODE.md \
        "$package_root/usr/share/doc/gnomeai-node/README.md"
    cat >"$package_root/DEBIAN/control" <<EOF
Package: gnomeai-node
Version: $version
Section: utils
Priority: optional
Architecture: $deb_arch
Maintainer: GnomeAI-RS Project <noreply@example.invalid>
Depends: libc6, ca-certificates
Description: minimal init-agnostic execution client for GnomeAI-RS
 Connects outbound to a GnomeAI Hub and executes centrally approved work on
 weak Linux devices. Runs in the foreground and has no systemd dependency.
EOF
    deb="$output_dir/gnomeai-node_${version}_${deb_arch}.deb"
    dpkg-deb --root-owner-group --build "$package_root" "$deb"
    outputs+=("$deb")
fi

if wants_format tar; then
    tar_root="$build_root/tar/gnomeai-node-$version-$output_arch"
    mkdir -p "$tar_root"
    install -m 0755 "$binary" "$tar_root/gnomeai-node"
    install -m 0644 packaging/debian/README-NODE.md "$tar_root/README.md"
    tarball="$output_dir/gnomeai-node_${version}_${output_arch}.tar.gz"
    tar -C "$build_root/tar" -czf "$tarball" "$(basename "$tar_root")"
    outputs+=("$tarball")
fi

if wants_format xbps; then
    command -v xbps-create >/dev/null 2>&1 || {
        echo "GNOMEAI_NODE_FORMATS requests xbps, but xbps-create is unavailable" >&2
        echo "On Void Linux install it with: sudo xbps-install -S xbps" >&2
        exit 1
    }
    xbps_root="$build_root/xbps"
    mkdir -p "$xbps_root/usr/bin" "$xbps_root/usr/share/doc/gnomeai-node"
    install -m 0755 "$binary" "$xbps_root/usr/bin/gnomeai-node"
    install -m 0644 packaging/debian/README-NODE.md \
        "$xbps_root/usr/share/doc/gnomeai-node/README.md"
    xbps_revision="${version##*-}"
    xbps_version="${version%-*}_${xbps_revision}"
    xbps="$output_dir/gnomeai-node-${xbps_version}.${xbps_arch}.xbps"
    previous_xbps=""
    if [[ -f "$xbps" ]]; then
        previous_xbps="$build_root/previous-${xbps_arch}.xbps"
        mv "$xbps" "$previous_xbps"
    fi
    if ! (
        cd "$output_dir"
        xbps-create \
            -A "$xbps_arch" \
            -n "gnomeai-node-$xbps_version" \
            -s "Minimal init-agnostic execution client for GnomeAI-RS" \
            -S "Connects outbound to the main GnomeAI Hub without systemd." \
            -D "ca-certificates>=0" \
            -H "https://github.com/adrgu372/GnomeAI-RS" \
            -l "GPL-3.0-or-later" \
            -m "GnomeAI-RS Project <noreply@example.invalid>" \
            "$xbps_root"
    ); then
        [[ -z "$previous_xbps" ]] || mv "$previous_xbps" "$xbps"
        exit 1
    fi
    if [[ ! -f "$xbps" ]]; then
        xbps="$(find "$output_dir" -maxdepth 1 -type f \
            -name "gnomeai-node-${xbps_version}*.xbps" -print -quit)"
    fi
    [[ -n "$xbps" && -f "$xbps" ]] || {
        echo "xbps-create completed but its package could not be located" >&2
        exit 1
    }
    outputs+=("$xbps")
fi

if [[ ${#outputs[@]} -eq 0 ]]; then
    echo "No formats selected. Use GNOMEAI_NODE_FORMATS=deb,tar,xbps" >&2
    exit 1
fi

sha256sum "${outputs[@]}"
printf '%s\n' "${outputs[@]}"
