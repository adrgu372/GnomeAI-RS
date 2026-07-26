# OpenAI Codex app-server

GnomeAI-RS integrates the official Codex app-server as an unmodified sidecar
for OpenAI/ChatGPT account authentication and account-backed coding turns.

- Upstream: <https://github.com/openai/codex>
- Release: `rust-v0.145.0`
- Commit: `25af12f7e61572b0bc18ddb1008be543b91519b0`
- Linux package: `codex-app-server-package-x86_64-unknown-linux-musl.tar.gz`
- SHA-256: `63c3568f800723421ec4a4dec591158dbb1a7e8f353d1f333b080203d96ffa85`
- License: Apache License 2.0

The binary distribution includes the release package under `codex/`. The
source distribution includes `scripts/install-codex-sidecar.sh`, which
downloads the pinned package and verifies the upstream checksum before
extracting it.

GnomeAI-RS communicates with the sidecar over its documented, newline-delimited
JSON app-server protocol. It does not read or copy Codex authentication files.
