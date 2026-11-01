# oc-gw-audit

Security research artifacts for the **OpenClaw Gateway**: technical writeups and
working proof-of-concept tooling covering two publicly disclosed upstream issues
(ClawJacked and CVE-2026-100525) and a suite of five gateway-specific findings of
our own (F1-F5).

Everything here is **runnable and verifiable**. Each PoC reports a verdict, sets
its exit status from that verdict, and includes a negative control - so a PoC
cannot claim success against a target that merely ignores authentication. A
self-contained lab gateway is included so the whole suite can be exercised
without installing OpenClaw.

Before running anything, read [Scope and fair use](#scope-and-fair-use).

## Quick start

```bash
# 1. Build the PoCs
./run-all.sh            # builds everything, starts the lab, runs the full suite

# Or step by step:
for f in F1 F2 F3 F4 F5; do rustc "$f.rs" -o "$f"; done
npm install && npm run build      # for the TypeScript PoC

# 2. Start the lab gateway (two personalities)
python3 lab/gateway.py --mode vulnerable --config /tmp/lab-gateway.json &
python3 lab/gateway.py --mode secure --port 18790 &

# 3. Run a PoC
./F1                     # CONFIRMED against the vulnerable lab, exit 0
./F1 --port 18790        # not confirmed against the secure lab, exit non-zero
```

Against a **real** gateway, drop `--port` (the default is the documented gateway
port, `18789`) and point it at your own host.

## Contents

| Path | Kind | Subject |
| --- | --- | --- |
| `clawjacked-poc.md` | Writeup | ClawJacked - localhost WebSocket hijacking |
| `clawjacked_exploit.py` | PoC (Python) | Loopback WebSocket accessibility check, no dependencies |
| `web-poc.ts` | PoC (TypeScript) | Same check from a browser-like vantage point |
| `web-poc-test.js` | Tests (Node) | Unit tests for the WebSocket frame codec |
| `prometheus-poc.md` | Writeup | CVE-2026-100525 - metrics scope bypass |
| `prometheus_cve_exploit.py` | PoC (Python) | Scope-bypass check using a restricted identity |
| `README-F1-F5.md` | Writeup | The F1-F5 suite, with honest scope per finding |
| `F1.rs` … `F5.rs` | PoCs (Rust) | The five findings |
| `poc_support.rs` | Library | Shared helpers, included by each PoC |
| `lab/gateway.py` | Lab | Dependency-free gateway stand-in, two personalities |
| `run-all.sh` | Harness | Builds and runs the entire suite against the lab |
| `LICENSE` | Legal | The Unlicense (public domain) |

## Scope and fair use

- The two documented issues (ClawJacked, CVE-2026-100525) are **publicly
  disclosed and fixed upstream**. Nothing here is a zero-day.
- The F1-F5 findings are **original, unverified research**. They have not been
  reported to or confirmed by the OpenClaw maintainers, and they carry no CVE or
  advisory. Each has a "what this establishes / what it does not" note.
- Every PoC targets `127.0.0.1` by default. Do not point any of this at systems
  you do not own or have explicit written permission to test.
- Provided as-is, with no warranty, for education and defensive verification.

## Requirements

- **Rust** (`rustc`) for F1-F5. Each is a single file with no dependencies; no
  `Cargo.toml` is needed.
- **Python 3.8+** for the `.py` PoCs and the lab gateway. No third-party
  packages: an earlier version depended on `websocket-client` and silently
  degraded without it, and that dependency has been removed.
- **Node.js 18+** for `web-poc.ts`. Run `npm install` once for the TypeScript
  compiler; the PoC itself only uses node built-ins, so there is no runtime
  dependency.

## 1. ClawJacked - localhost WebSocket hijacking

**Publicly disclosed by Oasis Security on 2026-02-26 and fixed within 24 hours in
OpenClaw `2026.2.25`.**

Browsers do not apply the same-origin policy to WebSocket handshakes, so
JavaScript on any website a developer visits can open a WebSocket to a gateway
bound to loopback. The published chain: connect to `localhost`, brute-force the
gateway password (no rate limiting at the time), get silently registered as a
trusted device because localhost connections were auto-approved, and end up with
full agent control.

### Artifacts

- `clawjacked-poc.md` - structured writeup of the attack flow and mitigations.
- `clawjacked_exploit.py` - completes the documented handshake
  (`connect.challenge` → `req/connect` → `hello-ok`) with no credential and
  reports whether the gateway admits the caller. That is the ClawJacked
  precondition: a website's JavaScript is likewise unauthenticated.
- `web-poc.ts` - the same check written in TypeScript, using only node built-ins.

```bash
python3 clawjacked_exploit.py                       # against a real gateway
python3 clawjacked_exploit.py --port 18790          # against the secure lab
python3 clawjacked_exploit.py --token <operator>    # with a credential
python3 clawjacked_exploit.py --origin https://evil.example   # test origin gating
node dist/web-poc.js --origin https://evil.example
```

| Verdict | Meaning |
| --- | --- |
| Unauthenticated caller admitted | The ClawJacked precondition is present: no credential was needed to reach the control plane. |
| Handshake refused | Credentials or origin are required - consistent with a patched gateway. |
| Upgrade refused | Refused before the handshake, consistent with origin/auth gating. |
| No listener | Gateway not running, or bound elsewhere. |

The PoC does **not** brute-force any password and does **not** execute commands.
Reproducing the full published chain would require implementing the operator
session-token handshake and a browser context, which is out of scope here.

## 2. CVE-2026-100525 - Prometheus diagnostics scope bypass

**CVSS 4.0: 5.3 (Medium). Published 2026-09-26. Fixed in
`@openclaw/diagnostics-prometheus` `2026.9.3`.**

In deployments using an identity-bearing gateway authentication mode such as
`trusted-proxy`, the diagnostics plugin did not enforce the `operator.read`
scope on its authenticated metrics endpoint. A caller whose effective role has
no read scope could therefore retrieve the diagnostics document even though
ordinary read methods reject the same identity. Shared-secret gateway callers
already hold full operator scope and are not affected.

Workaround, per the advisory: disable the Prometheus endpoint, or ensure every
identity that can reach it is intended to hold `operator.read`.

### Artifact

`prometheus_cve_exploit.py` - the earlier version sent no credentials and could
not test the bug at all. This version uses the identity the advisory is about:

```bash
python3 prometheus_cve_exploit.py
python3 prometheus_cve_exploit.py --restricted-token <jwt> --operator-token <token>
```

It makes three requests and compares them:

1. an ordinary read method (`/tools/list`) with a **restricted** credential
2. the metrics endpoint with that **same restricted** credential
3. the metrics endpoint with a **full operator** credential, as a control

The bypass is **CONFIRMED** when (1) is refused but (2) is served a valid
Prometheus exposition document. That combination is what separates a
scope-enforcement bug from a merely public endpoint. 404s, HTML error pages and
empty bodies are explicit failures.

## 3. F1-F5 - original gateway PoC suite

> **Our own research, unverified by upstream.** No CVE, no advisory. See
> `README-F1-F5.md` for the full per-finding scope.

| ID | Finding | Confirmed when |
| --- | --- | --- |
| F1 | Dashboard fragment token leak | A credential in the dashboard URL fragment replays as a working bearer credential. |
| F2 | JS bridge token forgery | A credential exposed to page context (`window.__OPENCLAW_NATIVE_BROWSER_TOKEN__`) authenticates via the API. |
| F3 | Hot-reload config evasion | An unsigned config write alone switches authentication off. |
| F4 | Ephemeral port hijacking | A control port the gateway advertises is free and can be bound first. |
| F5 | Cross-tab label leakage | A bare-key label resolver lets one route supply labels another route trusts. |

### Build and run

```bash
for f in F1 F2 F3 F4 F5; do rustc "$f.rs" -o "$f"; done

./F1
./F2
./F3 --config /path/to/gateway/config.json
./F4
./F5
```

Every PoC accepts `--host`, `--port` and `--json`. F3 also takes `--config` and
`--sign`; F4 takes `--low`/`--high`. Each exits `0` only when the finding is
confirmed, and each includes a negative control.

## The lab gateway

`lab/gateway.py` is **not OpenClaw**. It is a dependency-free stand-in that
reproduces the observable behaviours the PoCs test, in two personalities, so the
whole suite can be run and verified without installing the real gateway.

- `--mode vulnerable` - unauthenticated WebSocket handshake with no rate
  limiting, the shared operator token handed out in a URL fragment, a native
  bridge token injected into page context, tokens in `localStorage`, metrics
  served without the `operator.read` scope, and unsigned config hot-reload.
- `--mode secure` - auth required at the handshake plus origin and rate gating,
  a single-use bootstrap token that is not an operator credential, no injected
  bridge token, `sessionStorage`, scope enforced on metrics, and config changes
  rejected unless signed.

Protocol details follow the public OpenClaw documentation: gateway port `18789`
with HTTP and WebSocket multiplexed on one port, `/tools/invoke`, and a
handshake of `connect.challenge` → `req/connect` → `hello-ok` carrying a role and
scopes.

### Verify the whole suite

```bash
./run-all.sh
```

This builds every PoC, starts both personalities, and runs the Rust, Python and
TypeScript PoCs against each. Expected outcome: every behavioural PoC confirms
against the vulnerable personality and is refused by the secure one.

## Repository layout

```
.
├── README.md                    # this file
├── README-F1-F5.md              # the F1-F5 suite in detail
├── clawjacked-poc.md            # ClawJacked writeup
├── clawjacked_exploit.py        # WebSocket accessibility check (no deps)
├── web-poc.ts                   # the same check in TypeScript
├── web-poc-test.js              # unit tests for the frame codec
├── prometheus-poc.md            # CVE-2026-100525 writeup
├── prometheus_cve_exploit.py    # scope-bypass check
├── F1.rs … F5.rs                # F1-F5 PoCs (standalone Rust)
├── poc_support.rs               # shared PoC helpers
├── lab/gateway.py               # lab gateway, vulnerable + secure
├── run-all.sh                   # build and test everything
├── package.json, tsconfig.json  # TypeScript build
└── LICENSE                      # The Unlicense
```

## References

- Oasis Security - *OpenClaw Vulnerability: Website-to-Local Agent Takeover*
  (ClawJacked, 2026-02-26): <https://www.oasis.security/blog/openclaw-vulnerability>
- GitHub Security Advisory for the Prometheus issue:
  <https://github.com/openclaw/openclaw/security/advisories/GHSA-rx8p-qcpv-c7vr>
- OpenClaw gateway documentation (protocol, default port, dashboard auth):
  <https://docs.openclaw.ai/gateway/protocol> and
  <https://docs.openclaw.ai/web/dashboard>

## License

Released into the public domain under [The Unlicense](LICENSE). You are free to
use, modify and distribute this material without restriction.
