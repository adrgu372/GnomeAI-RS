#!/usr/bin/env bash
# Re-download the models.dev metadata snapshot vendored under
# third_party/models.dev. Prints the upstream commit and the SHA-256 of the
# new file so the provenance records (README.md / SOURCE.txt) can be updated
# in the same change.
set -euo pipefail

project_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
target="$project_root/third_party/models.dev/api.json"
endpoint="https://models.dev/api.json"

command -v curl >/dev/null || { echo "curl is required" >&2; exit 1; }
command -v sha256sum >/dev/null || { echo "sha256sum is required" >&2; exit 1; }

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

echo "Downloading $endpoint ..." >&2
curl -fsSL --max-time 120 "$endpoint" -o "$tmp"

if ! python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$tmp" 2>/dev/null; then
    echo "Downloaded file is not valid JSON; keeping the existing snapshot" >&2
    exit 1
fi

new_sha="$(sha256sum "$tmp" | awk '{print $1}')"
old_sha="$(sha256sum "$target" 2>/dev/null | awk '{print $1}' || true)"

mv "$tmp" "$target"
trap - EXIT

echo "Updated third_party/models.dev/api.json"
echo "  sha256: $new_sha"
if [ -n "$old_sha" ] && [ "$old_sha" != "$new_sha" ]; then
    echo "  previous: $old_sha"
fi
echo
echo "Record the matching upstream commit:"
echo '  curl -fsSL https://api.github.com/repos/anomalyco/models.dev/commits/dev | python3 -c "import json,sys; print(json.load(sys.stdin)[\"sha\"])"'
echo "then update third_party/models.dev/SOURCE.txt and README.md accordingly."
