#!/usr/bin/env bash
set -euo pipefail

project_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$project_root"

control_file="$project_root/packaging/debian/control"
version="$(sed -n 's/^Version: //p' "$control_file" | head -n 1)"
architecture="$(sed -n 's/^Architecture: //p' "$control_file" | head -n 1)"
output_dir="${GNOMEAI_OUTPUT_DIR:-$project_root/dist}"
binary_dir="${GNOMEAI_BIN_DIR:-$project_root/target/release}"
codex_version="0.145.0"
codex_target="x86_64-unknown-linux-musl"
codex_tar_sha256="11239480f8e3efd1430f23bbe91c1a397856b8bbe6185ccbaee2382d25e03df2"
codex_bin_sha256="a2a05dafaa1acb002a45eaec0a462de5b13694fcfcd7bc43305f14781ce7be14"
dotnet_sdk_version="8.0.424"
dotnet_sdk_filename="dotnet-sdk-${dotnet_sdk_version}-linux-x64.tar.gz"
dotnet_sdk_url="https://builds.dotnet.microsoft.com/dotnet/Sdk/${dotnet_sdk_version}/${dotnet_sdk_filename}"
dotnet_sdk_sha512="6503fd9f464d5e3a4f43a881d2b74afc6a2c46ceda74d027f1565b7239f4b3ec884857c03c0dcd49eb52f384d5ae1fa5aaf135f0a6aabc5518103aceed643c74"

download_file() {
    local url="$1"
    local destination="$2"
    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --retry 3 --output "$destination" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget --tries=3 --output-document="$destination" "$url"
    else
        echo "curl or wget is required to download the Microsoft .NET SDK." >&2
        return 1
    fi
}

verify_dotnet_archive() {
    local archive="$1"
    [[ -f "$archive" ]] || return 1
    printf '%s  %s\n' "$dotnet_sdk_sha512" "$archive" | sha512sum --check --status
}

if [[ "${GNOMEAI_SKIP_BUILD:-0}" != "1" ]]; then
    cargo build --release --bins --locked
fi

for binary in gnomef-rs gnomef-whatsapp gnomeai-desktop-a11y gnomeai-node gnomeai-hubctl; do
    if [[ ! -x "$binary_dir/$binary" ]]; then
        echo "Missing binary: $binary_dir/$binary" >&2
        exit 1
    fi
done

build_root="$(mktemp -d /tmp/gnomeai-deb.XXXXXX)"
npm_cache="${GNOMEAI_NPM_CACHE:-$build_root/npm-cache}"
cleanup() {
    rm -rf -- "$build_root"
}
trap cleanup EXIT

if [[ "$architecture" != "amd64" ]]; then
    echo "The pinned Microsoft .NET SDK archive is linux-x64 and requires Architecture: amd64." >&2
    exit 1
fi

# Always stage the pinned Microsoft SDK. It is used to publish Avalonia and is
# copied into the .deb, so installation never depends on a Microsoft APT feed.
dotnet_sdk_root="${GNOMEAI_DOTNET_SDK_DIR:-}"
if [[ -z "$dotnet_sdk_root" ]]; then
    dotnet_sdk_archive="${GNOMEAI_DOTNET_SDK_ARCHIVE:-}"
    if [[ -z "$dotnet_sdk_archive" ]]; then
        if [[ -n "${GNOMEAI_DOWNLOAD_CACHE:-}" ]]; then
            download_cache="$GNOMEAI_DOWNLOAD_CACHE"
        elif [[ -n "${XDG_CACHE_HOME:-}" ]]; then
            download_cache="$XDG_CACHE_HOME/gnomeai-rs/downloads"
        elif [[ -n "${HOME:-}" ]]; then
            download_cache="$HOME/.cache/gnomeai-rs/downloads"
        else
            download_cache="$project_root/.cache/gnomeai-rs/downloads"
        fi
        mkdir -p "$download_cache"
        dotnet_sdk_archive="$download_cache/$dotnet_sdk_filename"
        if ! verify_dotnet_archive "$dotnet_sdk_archive"; then
            downloaded_sdk="$build_root/$dotnet_sdk_filename"
            echo "Downloading Microsoft .NET SDK $dotnet_sdk_version (linux-x64)..." >&2
            download_file "${GNOMEAI_DOTNET_SDK_URL:-$dotnet_sdk_url}" "$downloaded_sdk"
            if ! verify_dotnet_archive "$downloaded_sdk"; then
                echo "Microsoft .NET SDK SHA-512 verification failed." >&2
                exit 1
            fi
            install -m 0644 "$downloaded_sdk" "$dotnet_sdk_archive"
        fi
    elif ! verify_dotnet_archive "$dotnet_sdk_archive"; then
        echo "GNOMEAI_DOTNET_SDK_ARCHIVE failed the pinned SHA-512 verification." >&2
        exit 1
    fi

    dotnet_sdk_root="$build_root/dotnet-sdk"
    mkdir -p "$dotnet_sdk_root"
    tar --no-same-owner -xzf "$dotnet_sdk_archive" -C "$dotnet_sdk_root"
fi

dotnet_sdk_root="$(CDPATH= cd -- "$dotnet_sdk_root" && pwd)"
dotnet_cmd="$dotnet_sdk_root/dotnet"
dotnet_cli_home="$build_root/dotnet-home"
mkdir -p "$dotnet_cli_home"
if [[ ! -x "$dotnet_cmd" \
      || ! -f "$dotnet_sdk_root/LICENSE.txt" \
      || ! -f "$dotnet_sdk_root/ThirdPartyNotices.txt" ]]; then
    echo "Invalid Microsoft .NET SDK directory: $dotnet_sdk_root" >&2
    exit 1
fi
if ! DOTNET_ROOT="$dotnet_sdk_root" DOTNET_MULTILEVEL_LOOKUP=0 \
    DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1 \
    DOTNET_NOLOGO=1 DOTNET_CLI_HOME="$dotnet_cli_home" \
    "$dotnet_cmd" --list-sdks | grep -Fq "$dotnet_sdk_version ["; then
    echo "The staged SDK does not contain Microsoft .NET SDK $dotnet_sdk_version." >&2
    exit 1
fi

avalonia_publish_dir="${GNOMEAI_AVALONIA_PUBLISH_DIR:-}"
if [[ -z "$avalonia_publish_dir" ]]; then
    avalonia_publish_dir="$build_root/avalonia-publish"
    DOTNET_ROOT="$dotnet_sdk_root" \
    DOTNET_MULTILEVEL_LOOKUP=0 \
    DOTNET_CLI_TELEMETRY_OPTOUT=1 \
    DOTNET_NOLOGO=1 \
    DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1 \
    DOTNET_CLI_HOME="$dotnet_cli_home" \
    "$dotnet_cmd" publish ui/GnomeAI.UI/GnomeAI.UI.csproj \
        -c Release -r linux-x64 --self-contained false \
        -p:SelfContained=false -p:UseAppHost=true \
        -p:PublishTrimmed=false -p:DebugType=None \
        -o "$avalonia_publish_dir"
fi
if [[ ! -x "$avalonia_publish_dir/GnomeAI.UI" ]]; then
    echo "Invalid Avalonia publish directory: missing GnomeAI.UI executable in $avalonia_publish_dir" >&2
    exit 1
fi

package_root="$build_root/package"
mkdir -p \
    "$package_root/DEBIAN" \
    "$package_root/usr/bin" \
    "$package_root/usr/lib/gnomeai-rs" \
    "$package_root/usr/lib/gnomeai-rs/dotnet" \
    "$package_root/usr/lib/systemd/user" \
    "$package_root/usr/share/applications" \
    "$package_root/usr/share/doc/gnomeai-rs/firecrawl" \
    "$package_root/usr/share/icons/hicolor/scalable/apps" \
    "$package_root/usr/share/gnomeai-rs/skills" \
    "$package_root/usr/share/gnomeai-rs/whatsapp"

if [[ -d skills ]]; then
    cp -a skills/. "$package_root/usr/share/gnomeai-rs/skills/"
fi

install -m 0755 "$binary_dir/gnomef-rs" "$package_root/usr/lib/gnomeai-rs/gnomef-rs"
install -m 0755 "$binary_dir/gnomef-whatsapp" \
    "$package_root/usr/lib/gnomeai-rs/gnomef-whatsapp"
install -m 0755 "$binary_dir/gnomeai-desktop-a11y" \
    "$package_root/usr/lib/gnomeai-rs/gnomeai-desktop-a11y"
install -m 0755 "$binary_dir/gnomeai-node" "$package_root/usr/bin/gnomeai-node"
install -m 0755 "$binary_dir/gnomeai-hubctl" "$package_root/usr/bin/gnomeai-hubctl"
cp -a "$dotnet_sdk_root/." "$package_root/usr/lib/gnomeai-rs/dotnet/"
find "$package_root/usr/lib/gnomeai-rs/dotnet" -type d -exec chmod 0755 {} +
chmod 0755 "$package_root/usr/lib/gnomeai-rs/dotnet/dotnet"
install -d "$package_root/usr/lib/gnomeai-rs/ui"
cp -a "$avalonia_publish_dir/." "$package_root/usr/lib/gnomeai-rs/ui/"
find "$package_root/usr/lib/gnomeai-rs/ui" -type d -exec chmod 0755 {} +
find "$package_root/usr/lib/gnomeai-rs/ui" -type f -exec chmod 0644 {} +
chmod 0755 "$package_root/usr/lib/gnomeai-rs/ui/GnomeAI.UI"
if command -v strip >/dev/null 2>&1; then
    strip --strip-unneeded "$package_root/usr/lib/gnomeai-rs/gnomef-rs"
    strip --strip-unneeded "$package_root/usr/lib/gnomeai-rs/gnomef-whatsapp"
    strip --strip-unneeded "$package_root/usr/lib/gnomeai-rs/gnomeai-desktop-a11y"
    strip --strip-unneeded "$package_root/usr/bin/gnomeai-node"
    strip --strip-unneeded "$package_root/usr/bin/gnomeai-hubctl"
fi
ln -s gnomef-rs "$package_root/usr/lib/gnomeai-rs/gnomef-agent"

install -m 0755 packaging/debian/gnomef-rs "$package_root/usr/bin/gnomeai-rs"
ln -s gnomeai-rs "$package_root/usr/bin/gnomef-rs"
ln -s gnomeai-rs "$package_root/usr/bin/gnomef-agent"
install -m 0755 scripts/gnomeai-firecrawl "$package_root/usr/bin/gnomeai-firecrawl"
install -m 0755 scripts/gnomeai-desktop "$package_root/usr/bin/gnomeai-desktop"
install -m 0644 packaging/debian/gnomeai-whatsapp.service \
    "$package_root/usr/lib/systemd/user/gnomeai-whatsapp.service"

install -m 0644 config.example.json "$package_root/usr/share/gnomeai-rs/config.example.json"
install -m 0644 whatsapp/bridge.mjs "$package_root/usr/share/gnomeai-rs/whatsapp/bridge.mjs"
install -m 0644 whatsapp/package.json "$package_root/usr/share/gnomeai-rs/whatsapp/package.json"
install -m 0644 whatsapp/package-lock.json \
    "$package_root/usr/share/gnomeai-rs/whatsapp/package-lock.json"

whatsapp_node_modules="${GNOMEAI_WHATSAPP_NODE_MODULES:-}"
if [[ -z "$whatsapp_node_modules" ]]; then
    if ! command -v npm >/dev/null 2>&1; then
        echo "npm is required at build time to stage the pinned WhatsApp dependencies" >&2
        exit 1
    fi
    whatsapp_build_root="$build_root/whatsapp-deps"
    mkdir -p "$whatsapp_build_root"
    install -m 0644 whatsapp/package.json "$whatsapp_build_root/package.json"
    install -m 0644 whatsapp/package-lock.json "$whatsapp_build_root/package-lock.json"
    npm_args=(
        ci
        --ignore-scripts
        --legacy-peer-deps
        --omit=dev
        --omit=optional
        --no-audit
        --no-fund
        --cache
        "$npm_cache"
    )
    (
        cd "$whatsapp_build_root"
        npm "${npm_args[@]}"
    )
    whatsapp_node_modules="$whatsapp_build_root/node_modules"
fi

for dependency in \
    "@whiskeysockets/baileys/package.json" \
    "libsignal/package.json" \
    "pino/package.json"
do
    if [[ ! -f "$whatsapp_node_modules/$dependency" ]]; then
        echo "Invalid WhatsApp node_modules directory; missing $dependency" >&2
        exit 1
    fi
done

mkdir -p "$package_root/usr/share/gnomeai-rs/whatsapp/node_modules"
cp -a "$whatsapp_node_modules/." \
    "$package_root/usr/share/gnomeai-rs/whatsapp/node_modules/"
find "$package_root/usr/share/gnomeai-rs/whatsapp/node_modules" \
    -type d -exec chmod 0755 {} +
find "$package_root/usr/share/gnomeai-rs/whatsapp/node_modules" \
    -type f -exec chmod 0644 {} +

if ! command -v node >/dev/null 2>&1; then
    echo "Node.js 20 or newer is required to verify the staged WhatsApp bridge" >&2
    exit 1
fi
node_major="$(node --version | sed -E 's/^v([0-9]+).*/\1/')"
if [[ ! "$node_major" =~ ^[0-9]+$ || "$node_major" -lt 20 ]]; then
    echo "Node.js 20 or newer is required; found $(node --version)" >&2
    exit 1
fi
(
    cd "$package_root/usr/share/gnomeai-rs/whatsapp"
    node --input-type=module -e \
        "await import('@whiskeysockets/baileys'); await import('pino')"
)

install -m 0644 packaging/debian/gnomeai-rs-agent.desktop \
    "$package_root/usr/share/applications/gnomeai-rs-agent.desktop"
install -m 0644 packaging/icons/gnomeai-rs-agent.svg \
    "$package_root/usr/share/icons/hicolor/scalable/apps/gnomeai-rs-agent.svg"

install -m 0644 LICENSE "$package_root/usr/share/doc/gnomeai-rs/LICENSE"
install -m 0644 README.md "$package_root/usr/share/doc/gnomeai-rs/README.md"
install -m 0644 SECURITY-AUDIT-REMEDIATION.md \
    "$package_root/usr/share/doc/gnomeai-rs/SECURITY-AUDIT-REMEDIATION.md"
install -m 0644 packaging/debian/README.Debian \
    "$package_root/usr/share/doc/gnomeai-rs/README.Debian"
install -m 0644 packaging/debian/README-BINARY.txt \
    "$package_root/usr/share/doc/gnomeai-rs/README-BINARY.txt"
install -m 0644 packaging/debian/BUILD-INFO.txt \
    "$package_root/usr/share/doc/gnomeai-rs/BUILD-INFO.txt"
install -m 0644 third_party/dotnet/README.md \
    "$package_root/usr/share/doc/gnomeai-rs/DOTNET-SDK.txt"
install -m 0644 packaging/debian/copyright \
    "$package_root/usr/share/doc/gnomeai-rs/copyright"

install -m 0644 third_party/firecrawl/LICENSE \
    "$package_root/usr/share/doc/gnomeai-rs/firecrawl/LICENSE"
install -m 0644 third_party/firecrawl/README.md \
    "$package_root/usr/share/doc/gnomeai-rs/firecrawl/README.md"
install -m 0644 third_party/firecrawl/IMAGE-DIGESTS \
    "$package_root/usr/share/doc/gnomeai-rs/firecrawl/IMAGE-DIGESTS"
install -m 0644 third_party/firecrawl/firecrawl-v2.11.134.tar.gz \
    "$package_root/usr/share/doc/gnomeai-rs/firecrawl/firecrawl-v2.11.134.tar.gz"

codex_vendor_root="${GNOMEAI_CODEX_VENDOR_ROOT:-}"
if [[ -z "$codex_vendor_root" ]]; then
    if ! command -v npm >/dev/null 2>&1; then
        echo "npm is required to fetch the pinned official Codex platform package" >&2
        exit 1
    fi
    (
        cd "$build_root"
        npm --cache "$npm_cache" \
            pack "@openai/codex@${codex_version}-linux-x64" --silent >/dev/null
    )
    codex_archive="$build_root/openai-codex-${codex_version}-linux-x64.tgz"
    printf '%s  %s\n' "$codex_tar_sha256" "$codex_archive" | sha256sum --check -
    mkdir -p "$build_root/codex-extract"
    tar --no-same-owner -xzf "$codex_archive" -C "$build_root/codex-extract"
    codex_vendor_root="$build_root/codex-extract/package/vendor/$codex_target"
fi

if [[ ! -f "$codex_vendor_root/codex-package.json" || ! -x "$codex_vendor_root/bin/codex" ]]; then
    echo "Invalid Codex vendor root: $codex_vendor_root" >&2
    exit 1
fi
if ! grep -Fq "\"version\": \"$codex_version\"" "$codex_vendor_root/codex-package.json"; then
    echo "Codex vendor metadata does not match version $codex_version" >&2
    exit 1
fi
if ! grep -Fq "\"target\": \"$codex_target\"" "$codex_vendor_root/codex-package.json"; then
    echo "Codex vendor metadata does not match target $codex_target" >&2
    exit 1
fi
printf '%s  %s\n' "$codex_bin_sha256" "$codex_vendor_root/bin/codex" | sha256sum --check -

mkdir -p "$package_root/usr/lib/gnomeai-rs/codex"
cp -a "$codex_vendor_root/." "$package_root/usr/lib/gnomeai-rs/codex/"
install -m 0644 third_party/codex/LICENSE "$package_root/usr/lib/gnomeai-rs/codex/LICENSE"
install -m 0644 third_party/codex/NOTICE "$package_root/usr/lib/gnomeai-rs/codex/NOTICE"
install -m 0644 third_party/codex/README.md "$package_root/usr/lib/gnomeai-rs/codex/README.md"
install -m 0644 third_party/codex/codex-package_SHA256SUMS \
    "$package_root/usr/lib/gnomeai-rs/codex/SHA256SUMS"

chmod 0755 "$package_root/usr/lib/gnomeai-rs/codex/bin/codex"
find "$package_root/usr/lib/gnomeai-rs/codex" -type d -exec chmod 0755 {} +
find "$package_root/usr/lib/gnomeai-rs/codex" -type f \
    ! -path '*/bin/codex' -exec chmod 0644 {} +
for helper in \
    bin/codex-code-mode-host \
    codex-path/rg \
    codex-resources/bwrap \
    codex-resources/zsh/bin/zsh
do
    if [[ -f "$package_root/usr/lib/gnomeai-rs/codex/$helper" ]]; then
        chmod 0755 "$package_root/usr/lib/gnomeai-rs/codex/$helper"
    fi
done

cp "$control_file" "$package_root/DEBIAN/control"
install -m 0755 packaging/debian/postinst "$package_root/DEBIAN/postinst"
install -m 0755 packaging/debian/postrm "$package_root/DEBIAN/postrm"
installed_size="$(du -sk "$package_root/usr" | awk '{print $1}')"
sed -i "s/^Installed-Size:.*/Installed-Size: $installed_size/" \
    "$package_root/DEBIAN/control"

mkdir -p "$output_dir"
output_file="$output_dir/gnomeai-rs_${version}_${architecture}.deb"
dpkg-deb --root-owner-group --build "$package_root" "$output_file"
sha256sum "$output_file"
echo "$output_file"
