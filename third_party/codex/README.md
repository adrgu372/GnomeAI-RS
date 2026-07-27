# OpenAI Codex app-server

GnomeAI-RS integrates the official Codex app-server as an unmodified sidecar
for OpenAI/ChatGPT account authentication and account-backed coding turns.

- Upstream: <https://github.com/openai/codex>
- Release: `rust-v0.145.0`
- Commit: `25af12f7e61572b0bc18ddb1008be543b91519b0`
- Packaged Linux artifact: official npm platform package
  `@openai/codex@0.145.0-linux-x64`
- npm tarball SHA-256:
  `11239480f8e3efd1430f23bbe91c1a397856b8bbe6185ccbaee2382d25e03df2`
- bundled `bin/codex` SHA-256:
  `a2a05dafaa1acb002a45eaec0a462de5b13694fcfcd7bc43305f14781ce7be14`
- License: Apache License 2.0

The Debian binary distribution preserves the official platform-package layout
under `/usr/lib/gnomeai-rs/codex/`, including its declared resources and
code-mode host. The source distribution includes
`scripts/install-codex-sidecar.sh`, which can alternatively download the
pinned standalone app-server package and verifies the upstream checksum before
extracting it.

GnomeAI-RS communicates with the sidecar over its documented, newline-delimited
JSON app-server protocol. It does not read or copy Codex authentication files.
