#!/usr/bin/env bash
set -euo pipefail

version="0.153.4"
destination="${1:-target/release/codex}"
case "$(uname -m)" in
    x86_64 | amd64)
        target="x86_64-unknown-linux-musl"
        sha256="a5d37ff1fa6953ee6d317b7e69bfafd39f5f53350b631d790fa7531159f22420"
        ;;
    aarch64 | arm64)
        target="aarch64-unknown-linux-musl"
        sha256="5673c5a8935ff2f85ca67b489e560fdd5e08fb0f0e2f7426f048ec7449aa4fdc"
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
