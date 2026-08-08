#!/usr/bin/env bash
set -euo pipefail

project_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$project_root"

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
    echo "This packager must run on macOS Apple Silicon (arm64)." >&2
    exit 1
fi

version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
output_dir="${GNOMEAI_OUTPUT_DIR:-$project_root/dist}"
binary_dir="${GNOMEAI_BIN_DIR:-$project_root/target/release}"
codex_version="0.145.0"

if [[ "${GNOMEAI_SKIP_BUILD:-0}" != "1" ]]; then
    cargo build --release --locked
fi

for binary in gnomef-rs gnomef-web; do
    [[ -x "$binary_dir/$binary" ]] || {
        echo "Missing binary: $binary_dir/$binary" >&2
        exit 1
    }
done

build_root="$(mktemp -d "${TMPDIR:-/tmp}/gnomeai-macos.XXXXXX")"
trap 'rm -rf -- "$build_root"' EXIT
package_name="GnomeAI-RS-${version}-macos-arm64"
package_root="$build_root/$package_name"
mkdir -p "$package_root/whatsapp" "$package_root/codex" "$package_root/docs"

install -m 0755 "$binary_dir/gnomef-rs" "$package_root/gnomef-rs"
install -m 0755 "$binary_dir/gnomef-web" "$package_root/gnomef-web"
ln -s gnomef-rs "$package_root/gnomef-agent"
install -m 0644 index.html "$package_root/index.html"
install -m 0644 config.example.json "$package_root/config.example.json"
install -m 0644 README.md "$package_root/docs/README.md"
install -m 0644 LICENSE "$package_root/docs/LICENSE"

if [[ -d skills ]]; then
    cp -a skills "$package_root/skills"
fi

install -m 0644 whatsapp/bridge.mjs "$package_root/whatsapp/bridge.mjs"
install -m 0644 whatsapp/package.json "$package_root/whatsapp/package.json"
install -m 0644 whatsapp/package-lock.json "$package_root/whatsapp/package-lock.json"

npm_cache="${GNOMEAI_NPM_CACHE:-$build_root/npm-cache}"
(
    cd "$package_root/whatsapp"
    npm ci --ignore-scripts --legacy-peer-deps --omit=dev --omit=optional \
        --no-audit --no-fund --cache "$npm_cache"
)

(
    cd "$build_root"
    npm --cache "$npm_cache" pack "@openai/codex@${codex_version}-darwin-arm64" --silent >/dev/null
)
codex_archive="$build_root/openai-codex-${codex_version}-darwin-arm64.tgz"
mkdir -p "$build_root/codex-extract"
tar -xzf "$codex_archive" -C "$build_root/codex-extract"
codex_vendor="$build_root/codex-extract/package/vendor/aarch64-apple-darwin"
[[ -x "$codex_vendor/bin/codex" ]] || {
    echo "Official Codex arm64 binary was not found in npm package." >&2
    exit 1
}
cp -a "$codex_vendor/." "$package_root/codex/"

cat > "$package_root/README-macOS.txt" <<'EOF'
GnomeAI-RS for macOS Apple Silicon

Requirements:
- macOS on Apple Silicon (M1 or newer)
- Node.js 20+ only when using WhatsApp
- optional: Claude Code CLI for Anthropic account login

Run the terminal agent:
  ./gnomef-rs .

Run WebTool:
  ./gnomef-web
  then open http://127.0.0.1:8788

The official Codex 0.145.0 arm64 sidecar is bundled for OpenAI account login.
macOS may quarantine unsigned downloads. If Gatekeeper blocks the extracted
folder you built or downloaded yourself, remove quarantine from that folder:
  xattr -dr com.apple.quarantine GnomeAI-RS-*-macos-arm64

Strict internal/read-only commands use the macOS sandbox-exec facility.
normal and full-access modes use the current macOS user's native permissions.
EOF

mkdir -p "$output_dir"
archive="$output_dir/${package_name}.tar.gz"
dmg="$output_dir/${package_name}.dmg"
tar -C "$build_root" -czf "$archive" "$package_name"
hdiutil create \
    -volname "GnomeAI-RS ${version}" \
    -srcfolder "$package_root" \
    -ov \
    -format UDZO \
    "$dmg"

shasum -a 256 "$archive" "$dmg"
echo "$archive"
echo "$dmg"
