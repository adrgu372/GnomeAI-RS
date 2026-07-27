#!/usr/bin/env bash
set -euo pipefail

version="0.145.0"
destination="${1:-target/release/codex}"
case "$(uname -m)" in
    x86_64 | amd64)
        target="x86_64-unknown-linux-musl"
        sha256="63c3568f800723421ec4a4dec591158dbb1a7e8f353d1f333b080203d96ffa85"
        ;;
    aarch64 | arm64)
        target="aarch64-unknown-linux-musl"
        sha256="803e718bfc108a97f443a23d5203b42771c5afbeb5ff9bd534a755515ccfe3b3"
        ;;
    *)
        printf 'Unsupported Linux architecture: %s\n' "$(uname -m)" >&2
        exit 1
        ;;
esac
archive="codex-app-server-package-${target}.tar.gz"
download_url="https://github.com/openai/codex/releases/download/rust-v${version}/${archive}"
temporary_directory="$(mktemp -d)"
trap 'rm -rf -- "$temporary_directory"' EXIT

curl --fail --location --retry 3 --output "$temporary_directory/$archive" "$download_url"
(
    cd "$temporary_directory"
    printf '%s  %s\n' "$sha256" "$archive" | sha256sum --check -
)

mkdir -p "$destination"
tar --no-same-owner --extract --gzip \
    --file "$temporary_directory/$archive" \
    --directory "$destination"

printf 'Installed Codex app-server v%s in %s\n' "$version" "$destination"
