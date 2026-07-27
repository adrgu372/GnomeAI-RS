# K3 security-audit remediation

This release addresses the findings supplied with the K3 audit.

| Finding | Status | Remediation |
| --- | --- | --- |
| Unauthenticated Web API and permissive CORS | Fixed | Every `/api/` request requires a per-process token, loopback `Host` validation is enforced, and CORS is not enabled. Remote binding requires explicit opt-in and a caller-supplied token; a remote `/` request must already authenticate and cannot bootstrap/disclose that token. |
| Unsandboxed generated Python | Fixed | Generated programs run through the sandbox with no network, resource and output limits, a private writable directory, and fail-closed Landlock enforcement. |
| Unprotected configuration writes | Fixed | Configuration routes are covered by Web API authentication; the process token is never serialized. |
| Key/config metadata exposure | Fixed | API keys remain omitted from responses and all configuration access is authenticated. |
| Unauthenticated WhatsApp inbound bridge | Fixed | A shared process token is required on both the Rust and Node sides, and `own_jid` is read from trusted local credentials rather than the inbound payload. |
| Local socket/DNS sandbox caveat | Documented | Internet socket families remain blocked; local `AF_UNIX`/`AF_NETLINK` are intentionally retained for build-tool and libc compatibility. Strict untrusted-code paths also require full Landlock isolation. |
| Patch path TOCTOU | Fixed | Mutations use directory file descriptors opened component-by-component with `O_NOFOLLOW`, followed by descriptor-relative atomic operations. |
| Case variants of `.git` | Fixed | Protected component matching is ASCII case-insensitive. |
| Unsandboxed PDF/OCR tools | Fixed | Parsers are sandboxed, timed out, output-capped, and require full Landlock isolation. |
| Upload buffered before size check | Fixed | Axum applies a request-body limit before multipart buffering, the per-file limit remains, and a serialized cumulative quota prevents unbounded upload storage. |
| Unsafe served-file headers | Fixed | MIME type, safe attachment policy, and `X-Content-Type-Options: nosniff` are set. Only safe raster images are served inline. |
| Firecrawl launcher resolved through inherited `PATH` | Fixed | The configured launcher is canonicalized to an absolute path before execution. |
| Unbounded WhatsApp seen-ID set | Fixed | Seen IDs are retained in a bounded 10,000-entry FIFO set. |
| Model-configurable shell timeout | Retained with bounds | Timeout and output size remain hard-clamped by the local policy. |

Additional hardening covers owner-only state permissions, secret filtering in
cross-chat memory, Content Security Policy and frame/referrer headers, and
redaction of the API token from traced request URIs.

Installable skills are declarative data, not trusted plugins. Installation is
staged and atomic, validates package/file limits, rejects symlinks and path
escapes, never runs package hooks or scripts automatically, and cannot expand
the active Agent permission policy. Resource reads remain inside the
canonicalized package root and reject binary or oversized content.

The strict document-generation and parser sandboxes deliberately refuse to run
on systems where full Landlock filesystem isolation cannot be established.
