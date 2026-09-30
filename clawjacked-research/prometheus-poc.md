# CVE-2026-100525 - Prometheus Diagnostics Scope Bypass

## Summary

The diagnostics-prometheus plugin fails to enforce `operator.read` scope on authenticated endpoints, allowing non-read-scoped identities to retrieve diagnostic metrics.

## Reproduction

```bash
# 1. Deploy OpenClaw with trusted-proxy gateway mode
# 2. Create a user WITHOUT operator.read scope
# 3. Call /tools/prometheus/metrics with that identity
# Result: Metrics exposed despite intended access control
```

## Impact

- Operational metrics leakage to lower-privilege identities
- Potential for reconnaissance of cluster topology
- Fixed in v2026.9.3

## References

- [GitHub Advisory GHSA-rx8p-qcpv-c7vr](https://github.com/openclaw/openclaw/security/advisories/GHSA-rx8p-qcpv-c7vr)
