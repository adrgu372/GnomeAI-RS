# PoC Suite: F1-F5 Findings in the OpenClaw Gateway

Five gateway findings, each with a runnable proof-of-concept. Every PoC is a
single-file Rust program that compiles with a bare `rustc F1.rs` and exits
non-zero unless it actually confirms the finding.

> **Status: our own research, not disclosed upstream.** These findings have no
> CVE and no advisory. Two of the five describe a mechanism that is
> reproduced end-to-end below; one is a client-side storage-scoping defect
> demonstrated against a model; the remaining two are narrower than their titles
> suggest, and the "Honest scope" note on each says exactly what is and is not
> shown. Treat them as hypotheses with test code attached.

## What each PoC actually establishes

Each PoC prints a verdict and sets its exit status from it: `0` when the finding
is confirmed against the target, non-zero otherwise. Every one includes a
negative control, so a PoC cannot "confirm" a target that simply ignores
authentication.

| ID | Finding | Confirmed when |
| --- | --- | --- |
| F1 | Dashboard fragment token leak | The dashboard redirect puts a credential in the URL fragment, that value is refused when absent, and the same value replays as a working bearer credential. |
| F2 | JS bridge token forgery | The script the dashboard serves assigns a credential to `window.__OPENCLAW_NATIVE_BROWSER_TOKEN__`, and replaying the recovered value authenticates. |
| F3 | Hot-reload config evasion | An unsigned config write alone switches authentication off, with no access to the gateway process. |
| F4 | Ephemeral port hijacking | A control port the gateway advertises is free, so a local process can bind it before the gateway does. |
| F5 | Cross-tab label leakage | A bare-key label resolver lets one route supply labels another route trusts (model-level test). |

## Honest scope

The suite is deliberately explicit about its limits, because the earlier
versions of these files overstated what they tested.

- **F1** proves the leak by reading the redirect the gateway issues; it does not
  follow the redirect, run JavaScript, or read browser history. The step from
  "credential in the URL" to "anyone who sees the URL has access" is stated as
  reasoning, not measured.
- **F2** does not execute JavaScript. It fetches the served bundle and recovers
  the credential from the source the way injected script would recover it from
  `window`. It does not demonstrate the XSS that would be needed to run that
  script.
- **F3** requires write access to the config file, which is itself a privileged
  position on most hosts. The finding is that the reload path does not validate
  content, not that unprivileged code can reach the file.
- **F4** is about the control port, and states its result in both directions: a
  held port means no hijack, a free advertised port means a window. It does not
  demonstrate interception of a live SSH session.
- **F5** is a model-level test of a client-side resolver. `window.opener` +
  `localStorage` is not a cross-origin primitive, and this PoC does not claim it
  is; what it tests is key collision inside one origin, and whether that yields
  operator access depends on what the labels authorise.

## Findings

### F1 - Dashboard fragment token leak

The gateway hands the dashboard its credential inside a URL fragment
(`#/dash?token=...`). Fragments are not sent to the server, but they do persist
in history, get copied with the link, and appear in screenshots. F1 checks that
the value is a real credential by replaying it.

### F2 - JS bridge token forgery

The dashboard page ships a bearer credential into page context under a
well-known global. Any script running on that origin - an injected one, for
instance - can read it and use it directly against the API.

### F3 - Hot-reload config evasion

The gateway reloads its configuration when the file changes and applies the new
rules without validating their origin. F3 writes a payload that turns
authentication off and observes the change, without touching the gateway
process. `--sign` tests the validating path, which should reject it.

### F4 - Ephemeral port hijacking

The control plane is advertised on a port. If nothing holds that port while the
gateway claims to expose it there, a local process can bind it first and own the
control channel.

### F5 - Cross-tab label leakage

Labels used for authorisation decisions are kept in shared storage. A resolver
that looks labels up by bare key lets a sibling route supply the value another
route trusts; a route-scoped key does not.

## Compile and run

```bash
for f in F1 F2 F3 F4 F5; do rustc "$f.rs" -o "$f"; done
```

Against a real gateway on its default port:

```bash
./F1
./F2
./F3 --config /path/to/gateway/config.json
./F4
./F5
```

Options accepted by every PoC: `--host`, `--port`, `--json`. F3 additionally
takes `--config` and `--sign`; F4 takes `--low`/`--high` for the port range.
`--json` prints a single machine-readable result object.

## Run against the lab gateway

`lab/gateway.py` is a dependency-free stand-in for the gateway that reproduces
the observed behaviours, in two personalities: `vulnerable` and `secure`. It is
not OpenClaw, but it lets every PoC be run and verified without installing one.

```bash
python3 lab/gateway.py --mode vulnerable --config /tmp/lab-gateway.json &
python3 lab/gateway.py --mode secure --port 18790 --config /tmp/lab-secure.json &

./F1                                  # CONFIRMED against the vulnerable lab
./F1 --port 18790                     # NOT CONFIRMED against the secure lab
```

`./run-all.sh` builds everything, starts both personalities, and runs the whole
suite (Rust, Python and TypeScript PoCs) against both.

### Expected results against the lab

| PoC | vulnerable (18789) | secure (18790) |
| --- | --- | --- |
| F1 | CONFIRMED | not confirmed |
| F2 | CONFIRMED | not confirmed |
| F3 | CONFIRMED (needs `--config`) | not confirmed (unsigned payload rejected) |
| F4 | CONFIRMED when the control port is free | not confirmed (port held) |
| F5 | CONFIRMED | CONFIRMED (model-level: the defect is in the resolver, not the gateway) |
| `clawjacked_exploit.py` | unauthenticated caller admitted | refused |
| `prometheus_cve_exploit.py` | CONFIRMED scope bypass | not vulnerable |

---

All content for educational and research purposes only.
