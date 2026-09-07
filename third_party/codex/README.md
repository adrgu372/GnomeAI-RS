# OpenAI Codex app-server

GnomeAI-RS integrates the official Codex app-server as an unmodified sidecar
for OpenAI/ChatGPT account authentication and account-backed coding turns.

- Upstream: <https://github.com/openai/codex>
- Release: `rust-v0.153.4`
- Commit: `3d2ee51ca2d5db578f328aa75e20aa22c0197c9a`
- Linux amd64 package: `codex-package-x86_64-unknown-linux-musl.tar.gz`
- Linux amd64 package SHA-256:
  `a822187e1a2420c61c5926721bfbd878701ed95547c9bb0d4de4498a16ba1821`
- Linux amd64 app-server package SHA-256:
  `a5d37ff1fa6953ee6d317b7e69bfafd39f5f53350b631d790fa7531159f22420`
- License: Apache License 2.0

The Debian builder downloads the official release package from GitHub and
verifies the pinned SHA-256 before staging its declared resources. The source
distribution also includes `scripts/install-codex-sidecar.sh`, which downloads
the standalone app-server package and verifies the corresponding upstream
checksum before extraction.

GnomeAI-RS communicates with the sidecar over its documented newline-delimited
JSON app-server protocol. It does not read or copy Codex authentication files.
