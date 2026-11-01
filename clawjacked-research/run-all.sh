#!/bin/sh
# Build every PoC, start both lab gateway personalities, and run the whole suite.
#
# Requirements: rustc, python3, node, and (for the TypeScript PoC) npm install.
#
# Usage:
#   ./run-all.sh              # run everything against the lab
#   NO_LAB=1 ./run-all.sh     # only build, do not start the lab or run tests
#
# Exit status is non-zero if any build step fails.

set -eu

cd "$(dirname "$0")"

VULN_PORT=18789
SECURE_PORT=18790
CONFIG=/tmp/lab-gateway.json
SECURE_CONFIG=/tmp/lab-gateway-secure.json
OUT=${OUT:-/tmp/oc-gw-audit-bin}

mkdir -p "$OUT"

echo "== building Rust PoCs =="
for f in F1 F2 F3 F4 F5; do
    rustc "$f.rs" -o "$OUT/$f"
    echo "   built $f"
done

echo "== building the TypeScript PoC =="
if [ -d node_modules ]; then
    npx tsc -p tsconfig.json
    echo "   built dist/web-poc.js"
else
    echo "   skipped (run 'npm install' first)"
fi

if [ "${NO_LAB:-}" = "1" ]; then
    echo "== NO_LAB set, not starting the lab =="
    exit 0
fi

cleanup() {
    [ -n "${VULN_PID:-}" ] && kill "$VULN_PID" 2>/dev/null || true
    [ -n "${SECURE_PID:-}" ] && kill "$SECURE_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

rm -f "$CONFIG" "$SECURE_CONFIG"
python3 lab/gateway.py --mode vulnerable --port "$VULN_PORT" --config "$CONFIG" \
    > /tmp/lab-vulnerable.log 2>&1 &
VULN_PID=$!
python3 lab/gateway.py --mode secure --port "$SECURE_PORT" --config "$SECURE_CONFIG" \
    > /tmp/lab-secure.log 2>&1 &
SECURE_PID=$!
sleep 1.5

echo
echo "############ lab gateway, vulnerable personality (port $VULN_PORT) ############"
echo "   expected: F1 F2 F3(requires --config) F4 F5 CONFIRMED, clawjacked admitted"
echo
for f in F1 F2 F3 F4 F5; do
    rm -f "$CONFIG"
    if [ "$f" = "F3" ]; then
        "$OUT/$f" --port "$VULN_PORT" --config "$CONFIG" || true
    else
        "$OUT/$f" --port "$VULN_PORT" || true
    fi
done
python3 clawjacked_exploit.py --port "$VULN_PORT" || true
python3 prometheus_cve_exploit.py --port "$VULN_PORT" || true

echo
echo "############ lab gateway, secure personality (port $SECURE_PORT) ############"
echo "   expected: every PoC NOT CONFIRMED, nothing admitted without a credential"
echo
for f in F1 F2 F3 F4 F5; do
    rm -f "$SECURE_CONFIG"
    if [ "$f" = "F3" ]; then
        "$OUT/$f" --port "$SECURE_PORT" --config "$SECURE_CONFIG" || true
    else
        "$OUT/$f" --port "$SECURE_PORT" || true
    fi
done
python3 clawjacked_exploit.py --port "$SECURE_PORT" || true
python3 prometheus_cve_exploit.py --port "$SECURE_PORT" || true

echo
echo "== suite finished =="
