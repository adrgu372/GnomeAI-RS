#!/usr/bin/env bash
# Both real builds must succeed before this script emits a preview archive.
set -euo pipefail
source_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
log_dir="${GNOMEAI_GATE_LOG_DIR:-$source_root/verification}"
mkdir -p "$log_dir"
log_dir="$(cd "$log_dir" && pwd)"
clean_root="$(mktemp -d /tmp/gnomeai-preview-check.XXXXXX)"
trap 'rm -rf -- "$clean_root"' EXIT
python3 - "$source_root" "$clean_root" <<'PY'
from pathlib import Path
import sys, shutil, json, hashlib
source, target = map(Path, sys.argv[1:])
manifest={}
for p in source.rglob('*'):
    if not p.is_file(): continue
    rel=p.relative_to(source)
    if any(part in {'.git','target','obj','dist','verification','__pycache__'} for part in rel.parts): continue
    if 'bin' in rel.parts and rel.parts[:2]!=('src','bin'): continue
    if p.suffix in {'.so','.dll','.pdb','.apk','.deb','.pyc'}: continue
    dest=target/rel;dest.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(p,dest)
    manifest[str(rel)]=hashlib.sha256(dest.read_bytes()).hexdigest()
(target/'.gate-manifest.json').write_text(json.dumps(manifest,sort_keys=True))
PY
android_status=0
desktop_status=0
if (cd "$clean_root" && bash scripts/build-android.sh) >"$log_dir/android.log" 2>&1; then
    echo 'Android build: PASS'
else
    android_status=$?
    echo "Android build: FAILED ($android_status); $log_dir/android.log"
fi
if (cd "$clean_root" && bash scripts/build-deb.sh) >"$log_dir/desktop.log" 2>&1; then
    echo 'Desktop build: PASS'
else
    desktop_status=$?
    echo "Desktop build: FAILED ($desktop_status); $log_dir/desktop.log"
fi
printf 'android=%s\ndesktop=%s\n' "$android_status" "$desktop_status" >"$log_dir/build-status.txt"
if ((android_status != 0 || desktop_status != 0)); then
    echo 'NOT READY: no preview archive produced.' >&2
    exit 1
fi
if ! dotnet run --project "$clean_root/tests/DeviceLifecycle/DeviceLifecycle.csproj" -c Release >"$log_dir/lifecycle.log" 2>&1; then
    echo 'NOT READY: device lifecycle regression failed; no preview archive produced.' >&2
    exit 1
fi
# Optional archive is made from the same clean source that passed both builds.
if [[ $# -gt 0 ]]; then
    python3 - "$clean_root" "$1" <<'PY'
from pathlib import Path
import sys, json, hashlib, zipfile
source, output=map(Path,sys.argv[1:])
manifest=json.loads((source/'.gate-manifest.json').read_text())
for name,digest in manifest.items():
    if hashlib.sha256((source/name).read_bytes()).hexdigest()!=digest:
        raise SystemExit('Source changed during build: '+name)
with zipfile.ZipFile(output,'w',zipfile.ZIP_DEFLATED) as archive:
    for name in manifest: archive.write(source/name,Path('GnomeAI-RS')/name)
print(output)
PY
fi
