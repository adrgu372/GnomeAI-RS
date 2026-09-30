# PoC Suite: F1–F5 Vulnerabilities in OpenClaw Gateway

Standalone Rust proof-of-concepts for five critical findings.

## Findings

### F1: Dashboard Fragment Token Leak
URL fragments with tokens leak to history/clipboard. Leaked URL = full gateway access.

### F2: JS Bridge Token Forgery
XSS reads `window.__OPENCLAW_NATIVE_BROWSER_TOKEN__` and replays auth.

### F3: Hot-Reload Config Evasion
Config hot-reload applies changes mid-flight without cryptographic validation.

### F4: Ephemeral SSH Port Hijacking
Local malware binds to ephemeral SSH ports and intercepts control channel.

### F5: Cross-Tab Label Leakage
Route-scoped labels in shared localStorage enable cross-tab impersonation.

## Compile & Run

```bash
cd clawjacked-research
for f in F1.rs F2.rs F3.rs F4.rs F5.rs; do rustc $f && ./$f; done
```

Requires OpenClaw gateway at `127.0.0.1:18789`.

---
All content for educational and research purposes only.
