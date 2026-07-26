# GnomeAI-RS

Rust coding agent with a terminal UI, multiple API providers, sandboxing, and
an optional self-hosted web interface.

## Included

- Rust backend source under `src/`
- `Cargo.toml` and `Cargo.lock`
- `index.html` web UI
- optional WhatsApp bridge under `whatsapp/`
- lazy rootless Firecrawl launcher plus the corresponding upstream source
- clean `config.example.json`

## Requirements

Required:

- Rust stable with edition 2024 support
- an OpenAI-compatible API or `llama-server`

Optional:

- `tesseract` for OCR on images
- `pdftotext` from Poppler for PDF text extraction
- Node.js 18+ and `npm` for the WhatsApp bridge
- Podman for the packaged, lazy local Firecrawl deployment
- the official `claude` CLI for Anthropic account login

## Quick Start

Open the coding-agent TUI in the current directory:

```bash
cargo run -- .
```

This is the default executable. It opens with the gnome banner; run
`/provider` to select an API or account-backed provider.

## Release Build

Build:

```bash
cargo build --release
```

For a source build that supports OpenAI account login, install the pinned
official Codex sidecar next to the release binaries:

```bash
./scripts/install-codex-sidecar.sh
```

Run:

```bash
./target/release/gnomef-rs .
```

## Coding-agent TUI

The rewritten agent components are the primary GnomeAI-RS executable.
`gnomef-agent` remains available as a compatibility alias.

Start it against the current repository:

```bash
cargo run --bin gnomef-agent -- .
```

Or target another workspace and override the model:

```bash
cargo run --bin gnomef-agent -- /path/to/repository --model your-model
```

It reuses `llama_base_url`, `llama_api_key`, and `default_model` from
`config.json`. The values can be overridden without editing the file:

```bash
GNOMEF_BASE_URL=http://127.0.0.1:8090/v1 \
GNOMEF_MODEL=your-model \
GNOMEF_API_KEY=optional-key \
cargo run --bin gnomef-agent -- .
```

Useful TUI commands:

- `/new` starts a new persisted session.
- `/sessions` opens an interactive picker: Enter resumes, `r` renames, `d`
  deletes; `/resume ID` resumes directly and `/fork` branches the current
  session at its tip.
- `/compact` compacts older context.
- `/rollback` restores files changed through `apply_patch`.
- `/diff` shows the active patch log.
- `/workspace PATH` switches the live agent, sandbox, tools, and provider to
  another project; `/cd PATH` is an alias. Bare `/workspace` lists recent
  workspaces and `/workspace N` picks one by number. The last workspace is
  remembered: starting `gnomef-rs` from your home directory reopens it.
- `/provider` opens the provider and authentication picker.
- `/model MODEL` switches the model for subsequent turns.
- `/websearch` toggles web search; `/websearch on|off` sets it explicitly.
- `/sandbox read-only|workspace-write|danger-full-access` changes isolation.
- `/memory` shows the cross-conversation memory shared with WebTool;
  `/memory clear|on|off` manages it. Facts that look like credentials are
  excluded automatically.
- `/mouse on|off` toggles app mouse support. With it on, drag with the left
  button to select transcript text, keep the button down and use the wheel to
  extend the selection beyond the screen, then release to copy automatically.
  `Esc` clears a selection. With capture off, native terminal selection works.
- `/copy` (or `Ctrl+Y`) copies the active selection, or the last assistant
  reply when nothing is selected, through OSC 52.
- `/doctor` checks permissions, configuration, the provider endpoint,
  Firecrawl, the session database, and the working directories.
- Provider, workspace, sandbox, and web-search changes issued while the model
  is generating are queued and applied when the turn ends — never lost.
- `Ctrl+C` interrupts active model calls and commands; while idle it exits.
- The mouse wheel and `PageUp`/`PageDown` scroll the transcript;
  `Ctrl+Home`/`Ctrl+End` jump to its beginning/end. A scrollbar plus a
  percentage indicator appear while scrolled away from the tail, and `Esc`
  jumps back to the latest message.

Explicit Romanian or English requests such as „schimbă folderul în
`/path/to/project`” and “my project is in `/path/to/project`” are interpreted
as real workspace changes. They are not implemented as a temporary shell
`cd`, so subsequent file and shell tools use the new directory.

The TUI opens with a compact true-colour gnome banner showing the current
version, model, sandbox, and workspace. It is part of the transcript and
scrolls away naturally as the conversation grows.

### Providers

Run `/provider`, choose a provider with the arrow keys, and press Enter. Hosted
providers prompt for an API key (masked while typing); the custom/local option
also asks for a base URL. The built-in catalog includes:

- OpenAI and Anthropic
- DeepSeek, Moonshot/Kimi, Qwen, xAI/Grok, Mistral, and Google Gemini
- Groq, OpenRouter, Together AI, Fireworks AI, and Perplexity
- Cerebras, NVIDIA NIM, SambaNova, and Cohere
- any custom or local OpenAI-compatible endpoint

The selected provider and model take effect without restarting. `/model MODEL`
can override the preset's current default.

API keys are kept out of transcripts and diagnostic output. They are stored in
`store/providers.json` with owner-only (`0600`) permissions. Do not commit this
runtime file.

### OpenAI and Anthropic account login

The two account entries use the vendors' official authentication and execution
paths rather than treating subscription credentials as API keys:

- **OpenAI account (Codex)** uses the bundled official Codex app-server.
  GnomeAI shows the ChatGPT device-login URL and one-time code, while Codex
  owns credential persistence, token refresh, model calls, and coding tools.
- **Anthropic account (Claude Code)** runs `claude auth login`, then delegates
  turns to `claude -p`, which reuses the login managed by Claude Code.

The Linux binary archive already contains Codex v0.145.0; no separate Codex
installation is required. Source builds can install the pinned,
checksum-verified sidecar with `scripts/install-codex-sidecar.sh`. Set
`GNOMEF_CODEX_BIN` only if you deliberately want to use another official
Codex or `codex-app-server` executable.

Install and test Claude Code before selecting the Anthropic account entry.
GnomeAI-RS never reads, copies, or persists either vendor's OAuth tokens.
Account mode uses the vendor runtime's own tools and permission engine;
API-key modes continue to use GnomeAI's native tool loop.

The default policy is `workspace-write`: commands have no network access and
can write only to the selected workspace and build-cache directories. Use
`--session ID` to resume a session stored in `store/agent.db`.

## WebTool

The browser interface is available as `gnomef-web`. It shares the selected API
provider, model, API key, Web Search switch, and state directory with the
terminal agent.

```bash
cargo run --bin gnomef-web
```

Open `http://127.0.0.1:8787`, or the host and port configured in
`config.json`.

The interface is a single-page, ChatGPT-style UI: a sidebar with rename and
delete per conversation (the first message names a new chat automatically),
Markdown rendering with copyable code blocks, streaming with a Stop button,
and file uploads — the 📎 button, drag & drop anywhere on the page, or
pasting an image straight from the clipboard. Uploaded images are shown
inline; text and PDF content is extracted (OCR via `tesseract`, PDF via
`pdftotext`) and folded into the conversation.

### Local API security

WebTool binds to a loopback address by default, rejects non-loopback `Host`
headers, does not enable cross-origin access, and protects every `/api/`
request with a random per-process token injected into its own page. The token
is removed from request URIs before tracing, is never written to `config.json`,
and is also shared privately with the optional WhatsApp bridge.

Binding to a non-loopback address is refused unless both
`GNOMEF_ALLOW_REMOTE=1` and a `GNOMEF_WEB_TOKEN` of at least 16 characters are
set explicitly. Put a TLS-authenticating reverse proxy in front of WebTool
when exposing it beyond the local machine.

Uploaded PDF/image parsers and LLM-generated document Python run with a
timeout inside the Linux Landlock/seccomp sandbox, without network access.
Generated code can write only to its private temporary directory. These
untrusted-code paths fail closed and refuse execution if full Landlock
filesystem isolation is unavailable. The general coding sandbox deliberately
permits local `AF_UNIX` and `AF_NETLINK` sockets for build-tool and libc
compatibility, while blocking Internet socket families (`AF_INET`, `AF_INET6`,
and `AF_PACKET`).

### Cross-chat memory

WebTool keeps compact facts and recent-conversation summaries in
`store/memory.json`, then selects relevant items for a new conversation. In
Settings, **Memory between conversations** can be disabled or limited to
items from the last 7, 30, 90, or 365 days. **No age limit** keeps every
stored age eligible. The age filter does not delete old data; it only prevents
old facts and summaries from entering the model context.

### Web Search and local Firecrawl

The Web Search switch is available in both WebTool and the terminal agent.
When it is off, Firecrawl is not started. The first actual search or fetch
while it is on starts the packaged rootless Podman deployment on
`127.0.0.1:3002`. Official images are pinned and downloaded on first use;
their matching AGPL-3.0 source snapshot is included under
`third_party/firecrawl/`.

Useful packaged commands:

```bash
gnomeai-firecrawl status
gnomeai-firecrawl logs
gnomeai-firecrawl stop
```

## Runtime Data

Both applications use one writable, per-user state directory:

```text
${XDG_STATE_HOME:-$HOME/.local/state}/gnomeai-rs
```

Set `GNOMEF_RS_HOME` to override it. The selected coding workspace is
independent from this state directory. Runtime folders include:

- `chats/`
- `uploads/`
- `generated/`
- `store/`

To use an explicit state location:

```bash
mkdir -p ./data
GNOMEF_RS_HOME=./data cargo run --release --bin gnomef-web
```

The UI and optional bridge code can still stay in the project root.

## Third-party notices

The optional bundled OpenAI Codex app-server is distributed under Apache
License 2.0. Its pinned version, checksum, license, and NOTICE are under
`third_party/codex/`. The optional Firecrawl deployment is AGPL-3.0; its
pinned upstream source and license are under `third_party/firecrawl/`.
GnomeAI-RS itself remains licensed under GPL-3.0.

## Optional WhatsApp Bridge

The Rust backend can start the Node bridge automatically, but the Node
dependencies must exist first:

```bash
cd whatsapp
npm install
cd ..
```

Then enable the feature in `config.json` /web interface :

- `whatsapp_enabled: true`
- `whatsapp_bridge_port`
- `whatsapp_assistant_name`

The backend exposes:

- `GET /api/whatsapp/status`
- `POST /api/whatsapp/start`
- `POST /api/whatsapp/stop`
- `GET /api/whatsapp/qr`

## Notes

- If `config.json` is missing, the server falls back to built-in defaults.
- `config.json` is ignored by Git in this export so local secrets do not get
  committed by accident.
- WebTool stores persistent cross-chat memory in `store/memory.json`.
- The browser UI is a single static `index.html` file served by the backend.
