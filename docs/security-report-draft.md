# Security report draft — gateway credential strength & local rate-limit exemption

Repo audited: https://github.com/openclaw/openclaw
Commit: `main` @ `18d41b7cde8fc3363926fe8b630786bb47f09842` (v2026.9.7 snapshot; re-verify against latest main before sending)
Disclosure path: private GitHub Security Advisory → https://github.com/openclaw/openclaw/security/advisories/new (per SECURITY.md; do NOT open a public issue/PR)

---

## Finding 1 — Gateway accepts arbitrarily short shared secrets (no minimum length / entropy floor)

**Component:** gateway auth
**Severity proposal:** Low–Moderate (configuration-dependent; requires a non-loopback or misconfigured operator setup to be remotely reachable)

### Summary
`gateway.auth.token` and `gateway.auth.password` are accepted at gateway startup with any non-placeholder
value, including single characters. Startup validation (`assertGatewayAuthNotKnownWeak`,
`isInvalidGatewaySecret`) rejects only blank values, the literals `undefined`/`null`, and three published
example placeholders. There is no minimum-length or entropy floor.

### Affected code (latest main)
- `src/gateway/known-weak-gateway-secrets.ts` — `KNOWN_WEAK_GATEWAY_TOKENS` / `KNOWN_WEAK_GATEWAY_PASSWORDS`
  contain only 2–3 hardcoded example strings; `isInvalidGatewaySecret()` checks only
  `["", "undefined", "null"]`.
- `src/gateway/auth.ts` — `assertGatewayAuthConfigured()` performs no length validation for either credential kind.

### Attack scenario
1. Operator sets a short credential, e.g. `gateway.auth.token: "abc"` (no UI/CLI/config validation stops this).
2. The gateway binds beyond loopback when `gateway.bind` is `lan`, `tailnet`, or `auto`
   (`auto` → `0.0.0.0` inside Docker/Podman/K8s, `src/gateway/net.ts:261-284`). Remote guessability then
   depends only on the credential itself.
3. Guessing budget: shared-secret auth rate limit allows **10 attempts / 60 s** with a 300 s lockout
   (`src/gateway/auth-rate-limit.ts:100-102`), so a 2–3 character credential is brute-forceable in minutes,
   a 4–6 character one in hours.

### Why this crosses a boundary
The advertised trust model requires "a long random token" (error text in this same file recommends
`openssl rand -hex 32`), but the enforcement accepts the opposite of that guidance with no warning,
unless `doctor` is run. The gap between documented guidance and enforced policy is the finding.

### Suggested remediation
- Enforce at credential-resolution time (`assertGatewayAuthConfigured` / `assertGatewayAuthNotKnownWeak`):
  token ≥ 16 chars, password ≥ 10 chars, reject otherwise with actionable error
  (reuse the existing `doctor --fix --generate-gateway-token` hint).
- Optionally: warn (not fail) at < 32 chars for tokens; provide `OPENCLAW_ALLOW_WEAK_GATEWAY_SECRET=1`
  escape hatch for legacy setups, mirroring the existing redaction-sentinel hard-fail pattern.

---

## Finding 2 — Shared-secret auth rate limiter is fully exempt for loopback clients

**Component:** gateway auth rate limiting
**Severity proposal:** Low (by-design per threat model; local actor is in-scope trusted, but the exemption is silent)

### Summary
`createGatewayAuthRateLimiter` defaults to `exemptLoopback: true` (`src/gateway/auth-rate-limit.ts:145`),
and the gateway-auth limiter path does not override it. Any local process can therefore perform unlimited
online guessing against `gateway.auth.token`/`password` with zero throttling and zero log noise, including
against the optional local password fallback in `trusted-proxy` mode (`auth.ts`:
`localDirect && auth.password && connectAuth?.password`).

### Attack scenario
Unprivileged malware or another tenant on a shared workstation learns the operator's gateway credential by
brute force within the loopback exemption, then obtains full operator authority (control-plane writes,
`/tools/invoke`, session access). This compounds Finding 1: short credential + unlimited local attempts.

### Suggested remediation
- Keep default exemption, but log a warning every N (e.g. 100) consecutive local failures
  (currently silent), and expose `gateway.auth.rateLimit.exemptLoopback: false` for hardening.
- In `trusted-proxy` + local password fallback, consider a separate, smaller local budget.

---

## Out-of-scope notes observed (no report action needed)
- `bind: auto` → `0.0.0.0` in containers with `auth.mode: "none"` is a footgun; consider refusing that
  combination at startup (may be more of a docs/doctor item than an advisory).
- Otherwise: SSRF guard (pinned-DNS, per-hop redirect revalidation), origin checks, pairing join codes,
  board/share token crypto, WhatsApp LID fail-closed, config.apply guard, Prometheus scope check —
  all verified sound on this snapshot; worth mentioning as audited-clean context in the report body.

## Template fields for the advisory form
- **Title:** Gateway accepts arbitrarily short shared-secret credentials; loopback brute force is unlimited
- **Affected component:** openclaw gateway (npm `openclaw`), auth + auth-rate-limit
- **Affected version:** current `main` @ 18d41b7 (verify latest); likely all released versions (introduced with known-weak-secrets guard)
- **Impact:** operator impersonation where short credentials are configured and the port is reachable beyond loopback (Finding 1); unlimited local online guessing (Finding 2)
- **Repro:** config snippet + one-liner curl loop (below) — no exploit tooling required
- **Remediation:** minimum-length enforcement + documented override flag

```bash
# Repro (safe, single-host, demonstrates acceptance only — not a cracking tool):
# 1) write a config with a 3-char token, start gateway bound to LAN
# 2) from another host:
for t in a b c ab abc 123 xyz 000 111 222; do
  code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $t" http://<gateway-host>:<port>/tools/invoke -X POST -d '{}')
  echo "$t -> $code"
done
# 401 for wrong guesses, non-401 (or scope error) for the accepted token proves acceptance.
```
