# Vendored models.dev snapshot

`api.json` is a verbatim copy of the public models.dev provider/model
metadata feed. The Rust core embeds it at build time
(`src/model_catalog.rs`) to resolve per-model context and output token
limits for the auto-compaction budget, so local OpenAI-compatible endpoints
(Ollama, llama.cpp, vLLM) and hosted providers both get accurate budgets
without maintaining a catalog by hand.

Upstream source:

- Project: <https://models.dev> (source: <https://github.com/anomalyco/models.dev>)
- Commit: `a25d952334e2331e6649b1e054b9a7b4579dd978` (`dev` branch, 2026-09-19)
- Endpoint: `https://models.dev/api.json`
- License: MIT (see `LICENSE`)

GnomeAI-RS does not modify the snapshot. Refresh it with
`scripts/fetch-models-dev.sh`, which re-downloads the feed, records the
upstream commit and prints the SHA-256 of the new file; update the commit
line above (and `SOURCE.txt`) in the same change.

Only a small, permissive subset of the data is used at runtime: provider
ids matched against the built-in presets, model ids and their
`limit.context` / `limit.output` numbers. Unknown providers, unknown models
and a missing or malformed snapshot all fall back to the historical
128k-token default; `GNOMEF_CONTEXT_WINDOW_TOKENS` keeps priority over the
snapshot.
