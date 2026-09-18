#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"
fail() { echo "$*" >&2; exit 1; }
for tool in cargo rustup dotnet; do command -v "$tool" >/dev/null || fail "Missing prerequisite: $tool"; done
command -v cargo-ndk >/dev/null || fail 'Install cargo-ndk: cargo install cargo-ndk --locked'
: "${ANDROID_NDK_HOME:?Set ANDROID_NDK_HOME to an installed Android NDK r28 or newer}"
[[ -f "$ANDROID_NDK_HOME/source.properties" ]] || fail 'ANDROID_NDK_HOME does not point to an NDK installation.'
ndk_version="$(awk -F ' = ' '/^Pkg.Revision/ {print $2}' "$ANDROID_NDK_HOME/source.properties")"
[[ "${ndk_version%%.*}" =~ ^[0-9]+$ && "${ndk_version%%.*}" -ge 28 ]] || fail 'Use Android NDK r28 or newer.'
if [[ -z "${JAVA_HOME:-}" ]]; then
    javac_path="$(command -v javac || true)"
    [[ -n "$javac_path" ]] || fail 'Install a full JDK 21 (java, javac, jar, keytool, jarsigner) and set JAVA_HOME.'
    export JAVA_HOME="$(dirname -- "$(dirname -- "$(readlink -f -- "$javac_path")")")"
fi
for java_tool in java javac jar keytool jarsigner; do
    [[ -x "$JAVA_HOME/bin/$java_tool" ]] || fail "JAVA_HOME must point to a full JDK; missing bin/$java_tool."
done
java_version="$("$JAVA_HOME/bin/javac" -version 2>&1 | awk '{print $2}')"
[[ "${java_version%%.*}" =~ ^[0-9]+$ && "${java_version%%.*}" -ge 21 ]] || fail 'The Android .NET 10 workload requires JDK 21 or newer.'
export ANDROID_HOME="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}"
[[ -d "$ANDROID_HOME/platforms" ]] || fail 'Set ANDROID_HOME to the Android SDK directory with installed platforms.'
sdk_version="$(dotnet --version)"
if [[ "${sdk_version%%.*}" -lt 10 ]]; then
    echo "Selected .NET SDK is $sdk_version; this project requires .NET 10. Check PATH and global.json." >&2
    exit 1
fi
if ! dotnet workload list | awk '$1 == "android" { found=1 } END {exit !found}'; then
    echo 'Install the Android workload with: dotnet workload install android' >&2
    exit 1
fi
# Query the installed workload's target API without running a build target.
android_api="$(dotnet msbuild ui/GnomeAI.UI.Android/GnomeAI.UI.Android.csproj -nologo -getProperty:TargetPlatformVersion | sed -n '/^[0-9][0-9.]*$/p' | tail -n 1)"
android_api="${android_api%.0}"
[[ -n "$android_api" && -f "$ANDROID_HOME/platforms/android-$android_api/android.jar" ]] || fail "Install Android SDK platform android-$android_api for the selected .NET workload (Android SDK Manager), then retry."
# Resolve SDK packs before the expensive Rust build. Accept Android SDK licences
# separately if the dependency installation target requests them.
dotnet restore ui/GnomeAI.UI.Android/GnomeAI.UI.Android.csproj -r android-arm64
rustup target add aarch64-linux-android
# cargo-ndk selects the NDK linker and sysroot; 16 KiB page compatibility is
# also requested explicitly for contemporary Android devices.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-z,max-page-size=16384"
cargo ndk -t arm64-v8a --platform 26 -o ui/GnomeAI.UI.Android/native build --lib --release --locked
dotnet publish ui/GnomeAI.UI.Android/GnomeAI.UI.Android.csproj -c Release -r android-arm64 -p:JavaSdkDirectory="$JAVA_HOME" -p:AndroidSdkDirectory="$ANDROID_HOME"
echo 'APK output: ui/GnomeAI.UI.Android/bin/Release/net10.0-android/android-arm64/publish/'
