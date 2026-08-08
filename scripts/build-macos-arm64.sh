#!/usr/bin/env bash
set -euo pipefail

project_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$project_root"

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
    echo "This packager must run on macOS Apple Silicon (arm64)." >&2
    exit 1
fi

# Do not turn resource forks or Finder metadata into `._*` payload files.
export COPYFILE_DISABLE=1

version="$(cargo metadata --no-deps --format-version 1 \
    | sed -n 's/.*"name":"gnomef-rs","version":"\([^"]*\)".*/\1/p' \
    | head -n 1)"
[[ -n "$version" ]] || {
    echo "Could not read the gnomef-rs version from Cargo metadata." >&2
    exit 1
}

output_dir="${GNOMEAI_OUTPUT_DIR:-$project_root/dist}"
binary_dir="${GNOMEAI_BIN_DIR:-$project_root/target/release}"
codex_version="0.145.0"
package_name="GnomeAI-RS-${version}-macos-arm64"
app_identity="${GNOMEAI_APP_SIGN_IDENTITY:--}"
installer_identity="${GNOMEAI_INSTALLER_SIGN_IDENTITY:-}"

if [[ "${GNOMEAI_SKIP_BUILD:-0}" != "1" ]]; then
    cargo build --release --locked
fi

for binary in gnomef-rs gnomef-agent gnomef-web; do
    [[ -x "$binary_dir/$binary" ]] || {
        echo "Missing binary: $binary_dir/$binary" >&2
        exit 1
    }
done

build_root="$(mktemp -d "${TMPDIR:-/tmp}/gnomeai-macos.XXXXXX")"
mounted_image=""
cleanup() {
    if [[ -n "$mounted_image" ]]; then
        hdiutil detach "$mounted_image" >/dev/null 2>&1 || true
    fi
    rm -rf -- "$build_root"
}
trap cleanup EXIT
payload_root="$build_root/payload"
applications_root="$payload_root/Applications"
primary_app="$applications_root/GnomeAI-RS.app"
agent_app="$applications_root/GnomeAI-RS Agent.app"
web_app="$applications_root/GnomeAI-RS Web.app"
primary_macos="$primary_app/Contents/MacOS"
resources_root="$primary_app/Contents/Resources"
npm_cache="${GNOMEAI_NPM_CACHE:-$build_root/npm-cache}"

mkdir -p \
    "$applications_root" \
    "$resources_root/codex" \
    "$resources_root/docs" \
    "$resources_root/whatsapp" \
    "$payload_root/usr/local/bin"

xcrun swiftc \
    -O \
    -target arm64-apple-macos12.0 \
    -framework AppKit \
    packaging/macos/Launcher.swift \
    -o "$build_root/GnomeAILauncher"

create_app() {
    local app_path="$1"
    local identifier="$2"
    local display_name="$3"
    mkdir -p "$app_path/Contents/MacOS" "$app_path/Contents/Resources"
    install -m 0755 "$build_root/GnomeAILauncher" "$app_path/Contents/MacOS/GnomeAILauncher"
    cat > "$app_path/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDisplayName</key>
    <string>${display_name}</string>
    <key>CFBundleExecutable</key>
    <string>GnomeAILauncher</string>
    <key>CFBundleIdentifier</key>
    <string>${identifier}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>${display_name}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${version}</string>
    <key>CFBundleVersion</key>
    <string>${version}</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF
    plutil -lint "$app_path/Contents/Info.plist" >/dev/null
}

create_app "$primary_app" "com.gnomeai.rs" "GnomeAI-RS"
create_app "$agent_app" "com.gnomeai.rs.agent" "GnomeAI-RS Agent"
create_app "$web_app" "com.gnomeai.rs.web" "GnomeAI-RS Web"

install -m 0755 "$binary_dir/gnomef-rs" "$primary_macos/gnomef-rs"
install -m 0755 "$binary_dir/gnomef-agent" "$primary_macos/gnomef-agent"
install -m 0755 "$binary_dir/gnomef-web" "$primary_macos/gnomef-web"
install -m 0644 index.html "$resources_root/index.html"
install -m 0644 config.example.json "$resources_root/config.example.json"
install -m 0644 README.md "$resources_root/docs/README.md"
install -m 0644 LICENSE "$resources_root/docs/LICENSE"

if [[ -d skills ]]; then
    cp -a skills "$resources_root/skills"
fi

install -m 0644 whatsapp/bridge.mjs "$resources_root/whatsapp/bridge.mjs"
install -m 0644 whatsapp/package.json "$resources_root/whatsapp/package.json"
install -m 0644 whatsapp/package-lock.json "$resources_root/whatsapp/package-lock.json"

if [[ -n "${GNOMEAI_WHATSAPP_NODE_MODULES:-}" ]]; then
    [[ -d "$GNOMEAI_WHATSAPP_NODE_MODULES" ]] || {
        echo "GNOMEAI_WHATSAPP_NODE_MODULES is not a directory." >&2
        exit 1
    }
    cp -a "$GNOMEAI_WHATSAPP_NODE_MODULES" "$resources_root/whatsapp/node_modules"
else
    (
        cd "$resources_root/whatsapp"
        npm ci --ignore-scripts --legacy-peer-deps --omit=dev --omit=optional \
            --no-audit --no-fund --cache "$npm_cache"
    )
fi

if [[ -n "${GNOMEAI_CODEX_DIR:-}" ]]; then
    [[ -x "$GNOMEAI_CODEX_DIR/bin/codex" ]] || {
        echo "GNOMEAI_CODEX_DIR does not contain bin/codex." >&2
        exit 1
    }
    cp -a "$GNOMEAI_CODEX_DIR/." "$resources_root/codex/"
else
    (
        cd "$build_root"
        npm --cache "$npm_cache" pack \
            "@openai/codex@${codex_version}-darwin-arm64" --silent >/dev/null
    )
    codex_archive="$build_root/openai-codex-${codex_version}-darwin-arm64.tgz"
    mkdir -p "$build_root/codex-extract"
    tar -xzf "$codex_archive" -C "$build_root/codex-extract"
    codex_vendor="$build_root/codex-extract/package/vendor/aarch64-apple-darwin"
    [[ -x "$codex_vendor/bin/codex" ]] || {
        echo "Official Codex arm64 binary was not found in npm package." >&2
        exit 1
    }
    cp -a "$codex_vendor/." "$resources_root/codex/"
fi

write_cli_wrapper() {
    local output="$1"
    local executable="$2"
    cat > "$output" <<EOF
#!/bin/sh
set -eu
export GNOMEF_RS_HOME="\${GNOMEF_RS_HOME:-\${HOME:?HOME is not set}/Library/Application Support/GnomeAI-RS}"
export GNOMEF_RS_ASSETS="\${GNOMEF_RS_ASSETS:-/Applications/GnomeAI-RS.app/Contents/Resources}"
export GNOMEF_CODEX_BIN="\${GNOMEF_CODEX_BIN:-/Applications/GnomeAI-RS.app/Contents/Resources/codex/bin/codex}"
mkdir -p "\$GNOMEF_RS_HOME"
exec "/Applications/GnomeAI-RS.app/Contents/MacOS/${executable}" "\$@"
EOF
    chmod 0755 "$output"
}

write_cli_wrapper "$payload_root/usr/local/bin/gnomef-rs" "gnomef-rs"
write_cli_wrapper "$payload_root/usr/local/bin/gnomef-agent" "gnomef-agent"
write_cli_wrapper "$payload_root/usr/local/bin/gnomef-web" "gnomef-web"

# Installer payloads must not retain Finder metadata or source quarantine flags.
xattr -crs "$payload_root"

sign_code() {
    local target="$1"
    local args=(--force --options runtime --sign "$app_identity")
    if [[ "$app_identity" != "-" ]]; then
        args+=(--timestamp)
    fi
    codesign "${args[@]}" "$target"
}

while IFS= read -r -d '' candidate; do
    if file -b "$candidate" | grep -q 'Mach-O'; then
        sign_code "$candidate"
    fi
done < <(find "$applications_root" -type f -print0)

sign_code "$primary_app"
sign_code "$agent_app"
sign_code "$web_app"

for app in "$primary_app" "$agent_app" "$web_app"; do
    codesign --verify --deep --strict --verbose=2 "$app"
done

component_pkg="$build_root/GnomeAI-RS-component.pkg"
component_plist="$build_root/components.plist"
pkgbuild --analyze --root "$payload_root" "$component_plist"
for index in 0 1 2; do
    plutil -replace "${index}.BundleIsRelocatable" -bool NO "$component_plist"
done
pkgbuild \
    --root "$payload_root" \
    --component-plist "$component_plist" \
    --identifier "com.gnomeai.rs.pkg" \
    --version "$version" \
    --install-location / \
    --ownership recommended \
    "$component_pkg"

mkdir -p "$output_dir"
pkg="$output_dir/${package_name}.pkg"
product_args=(--package "$component_pkg")
if [[ -n "$installer_identity" ]]; then
    product_args+=(--sign "$installer_identity")
fi
productbuild "${product_args[@]}" "$pkg"
if [[ -n "$installer_identity" ]]; then
    pkgutil --check-signature "$pkg"
fi
payload_list="$build_root/pkg-payload.txt"
pkgutil --payload-files "$pkg" > "$payload_list"
grep -q '^./Applications/GnomeAI-RS.app/' "$payload_list"
grep -q '^./Applications/GnomeAI-RS Agent.app/' "$payload_list"
grep -q '^./Applications/GnomeAI-RS Web.app/' "$payload_list"
grep -q '^./usr/local/bin/gnomef-web$' "$payload_list"
if grep -q '/\._' "$payload_list"; then
    echo "The installer payload contains unexpected AppleDouble files." >&2
    exit 1
fi

notary_args=()
if [[ -n "${GNOMEAI_NOTARY_PROFILE:-}" ]]; then
    notary_args+=(--keychain-profile "$GNOMEAI_NOTARY_PROFILE")
elif [[ -n "${GNOMEAI_NOTARY_KEY:-}" || -n "${GNOMEAI_NOTARY_KEY_ID:-}" || -n "${GNOMEAI_NOTARY_ISSUER_ID:-}" ]]; then
    [[ -f "${GNOMEAI_NOTARY_KEY:-}" \
        && -n "${GNOMEAI_NOTARY_KEY_ID:-}" \
        && -n "${GNOMEAI_NOTARY_ISSUER_ID:-}" ]] || {
        echo "Notarization requires GNOMEAI_NOTARY_KEY, GNOMEAI_NOTARY_KEY_ID, and GNOMEAI_NOTARY_ISSUER_ID." >&2
        exit 1
    }
    notary_args+=(
        --key "$GNOMEAI_NOTARY_KEY"
        --key-id "$GNOMEAI_NOTARY_KEY_ID"
        --issuer "$GNOMEAI_NOTARY_ISSUER_ID"
    )
fi

notarize_and_staple() {
    local target="$1"
    xcrun notarytool submit "$target" "${notary_args[@]}" --wait
    xcrun stapler staple "$target"
    xcrun stapler validate "$target"
}

if (( ${#notary_args[@]} > 0 )); then
    [[ "$app_identity" != "-" && -n "$installer_identity" ]] || {
        echo "Notarization requires Developer ID Application and Developer ID Installer identities." >&2
        exit 1
    }
    notarize_and_staple "$pkg"
fi

dmg_root="$build_root/dmg"
mkdir -p "$dmg_root"
cp -a "$pkg" "$dmg_root/Install GnomeAI-RS.pkg"
cat > "$dmg_root/README.txt" <<EOF
GnomeAI-RS ${version} for macOS Apple Silicon

Double-click "Install GnomeAI-RS.pkg" and follow the macOS Installer steps.

The installer adds these applications to /Applications:
- GnomeAI-RS
- GnomeAI-RS Agent
- GnomeAI-RS Web

It also adds these Terminal commands to /usr/local/bin:
- gnomef-rs
- gnomef-agent
- gnomef-web

WebTool opens at http://127.0.0.1:8787/.
EOF

dmg="$output_dir/${package_name}.dmg"
hdiutil create \
    -volname "GnomeAI-RS ${version}" \
    -fs HFS+ \
    -srcfolder "$dmg_root" \
    -ov \
    -format UDZO \
    "$dmg"
sign_code "$dmg"

if (( ${#notary_args[@]} > 0 )); then
    notarize_and_staple "$dmg"
fi

mount_point="$build_root/mounted"
mkdir -p "$mount_point"
hdiutil attach "$dmg" -nobrowse -readonly -mountpoint "$mount_point" >/dev/null
mounted_image="$mount_point"
[[ -f "$mount_point/Install GnomeAI-RS.pkg" ]]
[[ -f "$mount_point/README.txt" ]]
hdiutil detach "$mount_point" >/dev/null
mounted_image=""

checksums="$output_dir/${package_name}.sha256"
(
    cd "$output_dir"
    shasum -a 256 "$(basename "$pkg")" "$(basename "$dmg")" > "$(basename "$checksums")"
)

echo "$pkg"
echo "$dmg"
echo "$checksums"
