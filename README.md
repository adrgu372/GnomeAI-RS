# GnomeAI-RS

GnomeAI-RS is a native graphical coding agent written in Rust. The desktop
application keeps the proven agent core and `Op`/`Event` protocol of the former
terminal interface while replacing the TUI and browser page with one native
window.

Current package version: **2.0.2**. See [CHANGELOG.md](CHANGELOG.md) for the
historical release notes.

## What changed in 2.0

Version 2.0 is the native-desktop and distributed-execution release. It brings
together the graphical application, account and API providers, WhatsApp,
multimodal files, memory, skills, desktop automation and lightweight remote
nodes in one interface.

| Area | Version 2.0 |
| --- | --- |
| Interface | Native Rust `eframe`/`egui` window; no browser page or terminal launcher |
| Conversations | Streaming, automatic titles, resume, rename, fork and sidebar deletion |
| Input | Full-width multiline composer, diacritics, automatic growth and drag-and-drop |
| Providers | API keys plus OpenAI Account/Codex and Anthropic Account/Claude Code |
| Files | Images, PDF, Office/ODF, text/data and source code |
| Agent | Tools, subagents, memory, skills, Web Search, desktop navigation and guarded sudo |
| Messaging | Persistent WhatsApp pairing with the same providers and tool pipeline |
| Devices | Main-PC Hub plus minimal Linux nodes for Raspberry Pi and weak PCs |

## Native desktop interface

The primary `gnomef-rs` executable now opens a native application window on
Linux and macOS. `gnomef-agent` remains a compatibility alias to the same app.

The GUI includes:

- live token streaming without blocking input;
- a full-width, multiline composer that grows to eight rows, wraps long text,
  supports Romanian diacritics and focuses when any empty point in it is
  clicked;
- a local message queue while the agent is busy;
- expandable reasoning, tool output, patches and verification results;
- Stop/interrupt during model calls and long-running tools;
- command approvals and a separate masked sudo credential dialog;
- persisted sessions with automatic titles, resume, rename, delete and fork;
- conversation deletion directly from the sidebar, without a second window;
- compact, resizable provider, model, settings, WhatsApp and device windows;
- native provider, account-login and model selectors;
- native workspace and attachment file pickers plus drag-and-drop;
- `read-only`, `normal` and `full-access` sandbox selection;
- Web Search, memory, skills, diagnostics, diff, rollback and compaction;
- transcript search, token totals, notifications and Markdown export;
- native WhatsApp setup, live status, QR pairing and test messaging;
- a Hub for weak Linux devices, with root policy controlled per device;
- slash-command suggestions in the composer;
- dark desktop styling, consistent title bars and selectable transcript text.

There is no `index.html`, WebTool page or browser launcher in version 2.0. The
Debian desktop entry uses `Terminal=false`. WhatsApp keeps a
token-protected loopback service because the Node bridge must deliver inbound
messages somewhere; it is a private helper process with no HTML route and is
started and stopped by the native app. Coding-agent logic stays outside the
GUI and communicates through the serializable protocol in `src/protocol.rs`.

## Quick start

Open the current directory as the coding workspace:

```bash
cargo build --bins --locked
cargo run --locked --bin gnomef-rs -- .
```

Open another project or override the model:

```bash
cargo run --locked --bin gnomef-rs -- /path/to/project --model your-model
```

Existing configuration/environment overrides remain supported:

```bash
GNOMEF_BASE_URL=http://127.0.0.1:8090/v1 \
GNOMEF_MODEL=your-model \
GNOMEF_API_KEY=optional-key \
cargo run --locked --bin gnomef-rs -- .
```

Building all binaries once also places the private `gnomef-whatsapp` helper
next to the desktop executable, so QR pairing and inbound messages work in a
development run.

Run `/provider` in the composer or use the Provider button in the sidebar to
select an API or account-backed provider.

## Build

Rust stable with edition 2024 support is required.

```bash
cargo build --release --bins --locked
cargo test --release --locked
```

Linux builds of the native window require the usual X11/Wayland/OpenGL and
D-Bus development packages. On Debian/Ubuntu:

```bash
sudo apt-get install pkg-config libx11-dev libxkbcommon-dev libwayland-dev \
  libgl1-mesa-dev libdbus-1-dev
```

Install the optional pinned Codex sidecar for OpenAI account login in a source
build:

```bash
./scripts/install-codex-sidecar.sh
```

Package builders:

```bash
./scripts/build-deb.sh
./scripts/build-node-packages.sh
./scripts/build-macos-arm64.sh
```

The Debian launcher uses `Terminal=false`; the macOS application launches the
native Rust window directly rather than opening Terminal or a browser.

## Providers

The built-in catalog includes:

- OpenAI API and OpenAI account (Codex);
- Anthropic API and Anthropic account (Claude Code);
- DeepSeek, Moonshot/Kimi, Qwen, xAI/Grok, Mistral and Gemini;
- Groq, OpenRouter, Together, Fireworks and Perplexity;
- Cerebras, NVIDIA NIM, SambaNova and Cohere;
- custom/local OpenAI-compatible endpoints.

Provider keys are never placed in transcripts or diagnostic events. They are
stored in owner-only settings files. Account-backed entries delegate login and
token refresh to the official vendor runtime.

The model selector is populated from the provider API and falls back to the
maintained provider catalog. `/model MODEL` remains available for a direct
override.

API keys are saved in an owner-only settings file and restored when the same
provider is selected later. OpenAI Account authentication is owned by the
bundled Codex app-server and Anthropic Account authentication by the official
Claude Code runtime; GnomeAI reuses their valid sessions until the provider
reports that login has changed or expired. Both account providers expose model
selection rather than forcing every conversation to use `default`.

The same provider selection is available to allowed WhatsApp chats. Account
providers run through their vendor runtimes on the main PC, so no OpenAI or
Anthropic credentials are copied to WhatsApp or to a lightweight node.

## Attachments, documents and vision

Drop files anywhere on the conversation window or use the attachment picker.
GnomeAI stores uploads privately and can process:

- PNG, JPEG, WebP, GIF and BMP images;
- PDF through `pdftotext`;
- DOCX, XLSX and PPTX, including DOCM/XLSM/PPTM and ODT/ODS/ODP containers;
- TXT, Markdown, CSV/TSV, JSON, YAML, TOML, XML, HTML and other text/data files;
- common Rust, Python, JavaScript/TypeScript, C/C++, Go, Java, Kotlin, Swift,
  shell, SQL, web, infrastructure and other source-code formats.

Image-capable requests are sent as native multimodal parts. Capability checks
use provider metadata as well as maintained model knowledge instead of relying
only on names containing `vision`. If a nominally OpenAI-compatible endpoint
returns a schema error for `image_url`, GnomeAI retries safely with a text-only
attachment description instead of failing the whole turn.

## Execution policies

- `read-only` blocks workspace mutations;
- `normal` allows normal user-level access after explicit approval;
- `full-access` skips ordinary user-level approval prompts;
- root commands always use the separate native sudo path and never receive
  implicit root permission from `full-access`.

The sudo credential travels only over the in-process channel. It is never
added to model context, command arguments, environment variables, files or
logs. If a supported desktop keyring is available, saving the credential is an
explicit opt-in.

## Sessions and workspaces

Sessions are stored in SQLite and remain bound to their coding workspace.
Switching workspaces rebuilds path-sensitive providers, tools and sandbox
rules before changing the live session, preventing project context from
leaking between repositories.

Use the GUI controls or these commands:

```text
/new
/sessions
/resume SESSION_ID
/fork
/workspace PATH
/cd PATH
/compact
/rollback
/diff
```

## Skills

GnomeAI-RS supports declarative `SKILL.md` packages with staged validation,
path-traversal protection and atomic activation.

```text
/skills
/skill use NAME
/skill inspect NAME
/skill install PATH_OR_GIT_URL
/skill update NAME
/skill verify NAME
/skill remove NAME
```

Managed packages live under:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/gnomeai-rs/skills
```

Project-local skills under `./skills`, `./.agents/skills` and
`./.gnomeai/skills` are also discovered. Activating a skill never widens the
current execution policy.

When the user explicitly asks the agent to learn a reusable workflow, the
`Learn`/`learn_skill` tool creates a managed skill with an optional POSIX-shell
entrypoint. Saving and running are separate approved operations. An executable
skill can run locally or on a paired node.

## Hub and lightweight nodes

The desktop can keep models, memory, skills and provider credentials on the
main PC while weak Linux machines run only `gnomeai-node`. The client makes an
outbound long-poll connection, so it has no dependency on systemd or a desktop
environment.

1. Open **Settings → Devices**, enable the Hub, bind it to the LAN address
   (`0.0.0.0` when appropriate) and restart GnomeAI once.
2. Copy the enrollment command from the Devices window. It contains the main
   PC address, port `39176` by default, and a one-time enrollment token.
3. Allow that TCP port in the main PC firewall only for the trusted LAN/VPN.
   With UFW, for example: `sudo ufw allow from 192.168.1.0/24 to any port 39176 proto tcp`.
4. Install the minimal package on the weak machine, enroll it and run
   `gnomeai-node run` manually or under runit, OpenRC, s6 or another supervisor.

Root is deliberately two-layered: the node must be enrolled with
`--allow-root`, and the main graphical app must set that device to blocked,
ask, session or always. A session grant is reset when the Hub restarts. Node
credentials cannot change policies or queue commands; local administration
uses a separate private token.

Use a trusted LAN or VPN such as Tailscale. The built-in listener is HTTP and
must not be exposed directly to the public internet.

### Node packages

`./scripts/build-node-release.sh` emits architecture-verified packages rather
than relabelling the host executable:

| Target | Package formats |
| --- | --- |
| Debian/Ubuntu amd64 | `.deb`, `.tar.gz` |
| Debian/Ubuntu arm64 (glibc) | `.deb`, `.tar.gz` |
| Generic arm64 (musl) | `.tar.gz` |
| Void x86_64 (glibc) | `.xbps` |
| Void aarch64 (glibc) | `.xbps` |
| Void aarch64-musl | `.xbps` |

Build node packages for both supported CPU architectures with:

```bash
./scripts/build-node-release.sh
```

This creates real amd64 and arm64 builds rather than relabelling the host
binary. It also creates `.xbps` packages for `x86_64`, glibc `aarch64`, and
`aarch64-musl`: with local `xbps-create` on Void, or inside the official Void
glibc OCI image through Podman/Docker on Debian. Cross-rs builds the actual
static musl executable. On Void Linux that format can also be selected directly
with `GNOMEAI_NODE_FORMATS=xbps,tar ./scripts/build-node-packages.sh arm64-musl`.

### Start a node at boot with runit

The node stays in the foreground and therefore works directly with runit. Run
it as the same unprivileged user that performed enrollment, because the config
is stored in that user's `~/.config/gnomeai-node/config.json`:

```bash
sudo mkdir -p /etc/sv/gnomeai-node/log /var/log/gnomeai-node
sudo chown <user>:<user> /var/log/gnomeai-node
```

Create `/etc/sv/gnomeai-node/run`:

```sh
#!/bin/sh
exec 2>&1
export HOME=/home/<user>
exec chpst -u <user>:<user> /usr/bin/gnomeai-node run
```

Create `/etc/sv/gnomeai-node/log/run`:

```sh
#!/bin/sh
exec chpst -u <user>:<user> svlogd -tt /var/log/gnomeai-node
```

Then enable it:

```bash
sudo chmod +x /etc/sv/gnomeai-node/run /etc/sv/gnomeai-node/log/run
sudo ln -s /etc/sv/gnomeai-node /var/service/gnomeai-node
sudo sv up gnomeai-node
sudo sv status gnomeai-node
```

Replace `<user>` with the enrolled account name. Do not run the node as root;
remote root jobs still use the explicit two-layer policy described above.

## Persistent memory

The native memory engine uses SQLite WAL storage, optional embeddings, hybrid
retrieval, deduplication and background consolidation (“dreaming”). Candidate
facts pass a sanitizer that excludes credentials, injected instructions and
content copied from untrusted uploads or web pages.

```text
/memory show
/memory status
/memory dream
/memory dream --dry-run
/memory reindex
/memory forget FACT_ID
/memory clear
/memory on
/memory off
```

The store is located at `store/memory.db` beneath the per-user state directory.

## Web Search and Firecrawl

The Web Search toggle controls the native WebSearch/WebFetch tools. When
disabled, Firecrawl is not started. The first real search or fetch can start
the packaged rootless Podman deployment on demand.

```bash
gnomeai-firecrawl status
gnomeai-firecrawl logs
gnomeai-firecrawl stop
```

The corresponding pinned AGPL source is included under
`third_party/firecrawl/`.

## Composer commands

Type `/` to show graphical suggestions. Main commands:

```text
/help
/new
/sessions
/fork
/compact
/rollback
/workspace
/provider
/model
/websearch
/whatsapp
/sandbox
/skills
/skill
/memory
/tokens
/doctor
/diff
/export
/clear
/quit
```

Enter sends a message and Shift+Enter inserts a newline. Messages written
while a turn is active are queued. The Stop button or Ctrl+. interrupts the
current turn.

## Runtime data

By default, writable application state lives under:

```text
${XDG_STATE_HOME:-$HOME/.local/state}/gnomeai-rs
```

Set `GNOMEF_RS_HOME` to override it. The selected coding workspace is separate
from the state directory. Provider settings, session storage, tool output and
memory remain private user data and must not be committed.

## WhatsApp bridge

Open **WhatsApp** in the native sidebar or enter `/whatsapp`. The graphical
dialog can enable/disable the bridge, set the assistant name and allowed JIDs,
show connection state, render the pairing QR code and send a test message.
Incoming text, images, documents, audio and video continue through the existing
WhatsApp conversation, memory, skill and tool pipeline.

Allowed chat IDs authorize ordinary user-level tool work for their own inbound
request, so unattended tasks do not wait for a hidden desktop approval dialog.
`read-only` still blocks mutations, and sudo still requires an existing local
ticket or a credential explicitly stored in the desktop keyring. Pairing state
is persisted; transient stream reconnects do not normally require scanning a
new QR code.

The bridge requires Node.js 20 or newer and the pinned dependencies in
`whatsapp/`. Distribution packages stage those dependencies automatically.
Source builds should run `cargo build --bins --locked` so the private
`gnomef-whatsapp` helper is available beside `gnomef-rs`. The helper listens
only on loopback, requires a per-process token and serves no web page.

## Third-party notices

GnomeAI-RS is GPL-3.0. The optional OpenAI Codex sidecar is distributed under
Apache-2.0. The optional Firecrawl deployment is AGPL-3.0, with its matching
source and license included under `third_party/firecrawl/`.
