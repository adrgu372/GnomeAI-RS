> **Current release: 3.1.1 — numeric reasoning effort, a new app icon and
> Android transport & battery fixes.** The highlights are below;
> [CHANGELOG.md](CHANGELOG.md) has the full history. For the Android and
> multi-device foundation introduced in 3.0, see
> [RELEASE_NOTES_v3.0.md](RELEASE_NOTES_v3.0.md) and
> [ANDROID_PORT.md](ANDROID_PORT.md).

# GnomeAI-RS

<p align="center"><img src="promo/icon.png" width="96" alt="GnomeAI-RS logo"></p>

GnomeAI-RS is a native graphical coding agent written in Rust. The desktop
application keeps the proven agent core and `Op`/`Event` protocol of the former
terminal interface while replacing the TUI and browser page with one native
window. A native Android ARM64 client (`.NET 10` + Avalonia) shares the same
Rust core through `libgnomeai_core.so` and pairs with the desktop over a
Tor/Arti mesh transport.

Current Debian package version: **3.1-2** (Rust core: **3.1.1**, Android app:
**3.1.1**, versionCode **18**). See [CHANGELOG.md](CHANGELOG.md) for the
historical release notes.

## What changed in 3.1

Version 3.1 builds on the 3.0 Android preview and focuses on rich messages,
reasoning visibility and phone-side management.

| Area | Version 3.1 |
| --- | --- |
| Photos | Multi-part `image_url` content round-trips through the core; photo bubbles render on desktop and Android with a Save photo action; remote photo attachments share peer conversations in frames capped at 8 MiB |
| Mobile thinking | Reasoning received by the phone is persisted by `MobileThinkingStore` (keyed by answer digest) and shown in expandable transcript blocks with copy actions |
| On-device skills | Android Settings, Skills and Transcript panels list, inspect, install and activate SKILL.md packages through new core actions |
| Model picker | Per-conversation model selection backed by the new `available_models` core action |
| Handoff safety | Discarded session transfers record a tombstone and roll the session status back, so aborted transfers cannot be silently retried |
| Notifications | WhatsApp reply notifications are owned by the Android foreground service and survive backgrounding; native text selection and theme polish |

## What changed in 3.1.1

### Numeric reasoning effort (DeepSeek-style 1–100 scale)

The reasoning-effort setting now accepts a number in addition to the named
tiers, following the effort control described in the
DeepSeek-V4.1-Flash technical report: one `u8` on a 1–100 scale that a client
can interpolate, with the public tiers as anchors (`low`=50, `medium`=25,
`high`=75, `xhigh`=90, `max`=100; values below 5 are clamped to 5 and anything
unparsable falls back to `default`, so existing configs keep their meaning).
The value travels through two deliberately separate channels:

- **Native structured fields** (`reasoning_effort` for OpenAI,
  `output_config.effort` for Anthropic, `--effort` for the Claude CLI,
  `model_reasoning_effort` for the Codex app server) only ever receive tier
  names — a numeric value is filtered out because those APIs would reject it.
- **A system-prompt line** (`Reasoning effort: N (range 1-100; higher values
  request more thorough reasoning)`) carries numeric values to any model,
  which is the same prompt-conditioning mechanism the report uses to make
  effort control work on providers without a structured field (Ollama, vLLM,
  llama.cpp).

Set it with `/effort 42`, `/effort max`, the desktop Settings dialog (the
effort combo boxes are editable) or the `reasoning_effort` /
`subagent_reasoning_effort` config keys. Both agent and delegated-worker
efforts support the scale.

### New application icon

The old robot mark is replaced by a neon "G" built from a speech bubble,
device-mesh nodes and an assist sparkle on a rounded dark tile
(`promo/icon.png`). It ships everywhere:

- Debian: PNG sizes 16–256 installed into `hicolor` alongside the rebuilt
  scalable SVG, so small app-grid sizes stay crisp instead of downscaling a
  blurry fallback
- Android: launcher mipmaps at every density (`mdpi` 48 through `xxxhdpi`
  192), referenced by `android:icon` in the manifest (the app previously had
  no launcher icon at all)
- Desktop window: taskbar/window icon loaded from the Avalonia asset bundle
  (`Assets/AppIcon.png`)

### Android transport stability

- `NetworkObserver` now counts live networks and debounces callback storms
  for 5 s, reconnecting only on a real loss or return of the last transport.
  USB plugging, Wi-Fi/cell handovers and captive-portal checks no longer tear
  down healthy peer sockets (the old 1 s debounce re-connected on every
  callback, which made phone-to-PC sessions visibly stutter).
- The mesh ready-wait skips polling entirely when `mesh_status` already
  answers, and otherwise polls every 3 s instead of 1 s. Tor bootstrap takes
  tens of seconds regardless; faster polling only burned CPU.

### Android battery work

- Peer heartbeat unified at one liveness exchange per **15 s** (previously
  30 s when online but 5 s while pairing), with the hello deadline reduced to
  three missed beats. Pairing probes no longer fire every 5 s while a human
  is still looking at the confirmation code.
- Snapshot refresh skips re-downloading a full transcript when the epoch and
  revision are unchanged (frames that arrived mid-request are still honored).
- Workspace autosync sweeps every 5 minutes instead of every minute.
- Tor circuit budget reduced 32 → 8: a phone pairs with a handful of devices,
  and every kept-alive circuit costs CPU and cover traffic.
- New `DozeMonitor` parks peer links while the system is in Doze (where
  sockets are torn down anyway and retries would fail deterministically) and
  resumes them the moment the device wakes.
- `gnomeai_destroy` now wakes the event reader blocked in `recv()` before
  unwrapping the handle. Without this the reader kept an extra `Arc`
  reference, `Arc::try_unwrap` silently failed, and every destroyed handle
  leaked a live two-thread Tokio runtime — battery drain that grew with each
  activity recreation.

Android build: versionCode **17**, versionName **3.1.1**.

# GnomeAI-RS 3.0 — Android & multi-device preview

Version 3.0 added the native Android client and the multi-device foundation.
Full details live in [RELEASE_NOTES_v3.0.md](RELEASE_NOTES_v3.0.md); the
short version:

| Area | Version 3.0 |
| --- | --- |
| Android | Full ARM64 client with providers, streaming, web search, vision, files/PDFs, sub-agents, camera, file picker and a foreground service |
| QR scanner | NV21 preview plus sharp JPEG capture, inverted/cropped decoding attempts and `Enlarge QR` for dense invitations |
| Pairing | Persistent device identity, ephemeral ECDH P-256 keys, one-time invitations, SAS confirmation; Android Keystore protection on the phone |
| Mesh | `DeviceHub`/`PeerLink`/`PeerTransport`/`DeviceSessionClient` over Tor/Arti Onion Services, with remote conversations |
| Desktop | Device selector (`This PC` / phone / paired devices) and remote approvals bound to the peer captured at card creation |
| Builds | `scripts/build-deb.sh` and `scripts/build-android.sh` produce the Debian package and the signed APK |

The Android port history (mesh preview 3, camera preview 4, UX previews 5–7)
is documented in [ANDROID_PORT.md](ANDROID_PORT.md),
[ANDROID_PREVIEW4.md](ANDROID_PREVIEW4.md), [MESH_PREVIEW3.md](MESH_PREVIEW3.md)
and [PREVIEW5_UPDATE.md](PREVIEW5_UPDATE.md) /
[PREVIEW6_UPDATE.md](PREVIEW6_UPDATE.md) / [PREVIEW7_UPDATE.md](PREVIEW7_UPDATE.md).

## What changed in 2.4-1

Skill installation accepts a standalone `SKILL.md` without copying neighboring
project files. Directory and Git packages no longer have the previous limits of
512 files or 16 MiB during validation and copying.

## What changed in 2.4

Version 2.4 hardens draft recovery and desktop reliability and ships the
Avalonia desktop interface improvements.

| Area | Version 2.4 |
| --- | --- |
| Drafts | Unsent text, attachments, caret position and queues persist per conversation and restore automatically |
| Recovery | Queued messages resume in a paused state; explicit resume and clear controls; failed UI handlers surface in-window errors |
| Desktop | Transcript stays available when the core connection drops; WhatsApp pairing status refreshes automatically |
| Workers | Delegated-worker defaults stay unset (`inherit`) so every user picks their own provider/model in Settings |

## What changed in 2.3

Version 2.3 adds the Z.ai Coding Plan provider and makes WhatsApp turns robust
against stalled upstream connections.

| Area | Version 2.3 |
| --- | --- |
| Providers | Z.ai Coding Plan subscription endpoint with `glm-5.3-flash` as multimodal default |
| WhatsApp | Clarifying questions answered inline; inbound turns serialize per conversation; provider requests honor `llama_timeout` |
| Responses | Native Markdown layout for headings, lists, quotes, tables and fenced code, with one-click code copying |
| Transcript | Reliable mouse-wheel scrolling over selectable text, including while a selection is active |
| Clipboard | Native keyboard, context-menu and Linux primary-selection copy/paste behavior |
| MCP | Generic Streamable HTTP and stdio MCP servers shared by API and account-backed providers |
| Recovery | Automatic recall after transient, empty or interrupted provider responses |
| Privileges | Dynamic PAM/sudo prompts through a private local askpass channel |

## What changed in 2.0

Version 2.0 is the native-desktop and distributed-execution release. It brings
together the graphical application, account and API providers, WhatsApp,
multimodal files, memory, skills, desktop automation and lightweight remote
nodes in one interface.

| Area | Version 2.0 |
| --- | --- |
| Interface | Native Avalonia UI 11 window driven by the Rust core over the `Op`/`Event` bridge; no browser page |
| Conversations | Streaming, automatic titles, resume, rename, fork and sidebar deletion |
| Input | Full-width multiline composer, diacritics, automatic growth, drag-and-drop, Enter to send and Shift+Enter for a new line |
| Providers | API keys plus OpenAI Account/Codex and Anthropic Account/Claude Code |
| Files | Images, PDF, Office/ODF, text/data and source code |
| Agent | Tools, subagents, memory, skills, Web Search, desktop navigation and guarded sudo |
| Messaging | Persistent WhatsApp pairing with the same providers and tool pipeline |
| Devices | Main-PC Hub plus minimal Linux nodes for Raspberry Pi and weak PCs |

## Native desktop interface

The primary `gnomef-rs` executable opens a native application window on Linux
and macOS. `gnomef-agent` remains a compatibility alias to the same app.

The GUI includes:

- live token streaming without blocking input;
- a full-width, multiline composer that grows to eight rows, wraps long text,
  supports Romanian diacritics and focuses when any empty point in it is
  clicked; Enter sends and Shift+Enter inserts a new line;
- a local message queue while the agent is busy, with explicit resume/clear
  controls; Stop pauses pending messages as well as interrupting the turn;
- automatic per-conversation recovery of unsent drafts, attachment references
  and queued messages; recovered queues remain paused until resumed;
- concurrent saved conversations: start or resume another chat while the first
  keeps running in the background, with independent Stop/queue state and live
  sidebar status;
- photo bubbles for image attachments, with a Save photo action;
- structured reasoning, tool output, patches and verification results;
- Stop/interrupt during model calls and long-running tools;
- command approvals and a separate masked sudo credential dialog;
- persisted sessions with automatic titles, resume, rename, delete and fork;
- compact, resizable provider, model, settings, WhatsApp and device windows;
- native provider, account-login and model selectors;
- native workspace and attachment file pickers plus drag-and-drop;
- `read-only`, `normal` and `full-access` sandbox selection;
- Web Search, memory, skills, diagnostics, diff, rollback and compaction;
- transcript search, token totals, notifications and Markdown export;
- a readable transcript after a core disconnection, so it can still be copied
  or exported before restarting;
- native WhatsApp setup, live status, QR pairing and test messaging;
- a Hub for weak Linux devices, with root policy controlled per device;
- a device selector for working from `This PC`, the phone or a paired device,
  with safe session handoff and explicit discard of failed transfers;
- English Windows Apps styling with persistent System, Light and Dark Fluent
  themes, consistent title surfaces, navigation pane, command bar and
  selectable transcript text. Use the title-bar theme button, Settings, or
  `/theme light|dark|system`.

There is no `index.html`, WebTool page or browser launcher. The Debian desktop
entry uses `Terminal=false`. WhatsApp keeps a token-protected loopback service
because the Node bridge must deliver inbound messages somewhere; it is a
private helper process with no HTML route and is started and stopped by the
native app. Coding-agent logic stays outside the GUI and communicates through
the serializable protocol in `src/protocol.rs`.

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
./scripts/build-android.sh
./scripts/build-node-packages.sh
./scripts/build-macos-arm64.sh
```

The Debian builder pins Microsoft .NET SDK 10.0.400 for Linux x64. It downloads
the official tar.gz directly from Microsoft, verifies the published SHA-512,
uses that SDK to publish Avalonia and includes the complete SDK privately under
`/usr/lib/gnomeai-rs/dotnet` in the generated package. Repeated builds reuse
the verified archive from the user cache. For offline builds, set
`GNOMEAI_DOTNET_SDK_ARCHIVE=/path/to/dotnet-sdk-10.0.400-linux-x64.tar.gz`.

### Android build

The signed APK is produced by `scripts/build-android.sh`. Prerequisites:
.NET 10 SDK with the `android` workload, Android NDK r28+, a full JDK 21,
the Android SDK platform matching the workload, `cargo-ndk`, and
`ANDROID_NDK_HOME` pointing at the NDK.

```bash
dotnet workload install android
rustup target add aarch64-linux-android
cargo install cargo-ndk --locked
export ANDROID_NDK_HOME=/path/to/android-ndk
bash scripts/build-android.sh
```

The APK is published as:

```text
ui/GnomeAI.UI.Android/bin/Release/net10.0-android/android-arm64/publish/io.github.adrgu372.gnomeai-Signed.apk
```

The Rust core is built for `aarch64-linux-android` with 16 KiB page
compatibility (`-C link-arg=-Wl,-z,max-page-size=16384`) and linked into the
App as `libgnomeai_core.so`.

## Providers

The built-in catalog includes:

- OpenAI API and OpenAI account (Codex);
- Anthropic API and Anthropic account (Claude Code);
- Z.ai Coding Plan, DeepSeek, Moonshot/Kimi, Qwen, xAI/Grok, Mistral and Gemini;
- Groq, OpenRouter, Together, Fireworks and Perplexity;
- Cerebras, NVIDIA NIM, SambaNova and Cohere;
- custom/local OpenAI-compatible endpoints.

Provider keys are never placed in transcripts or diagnostic events. They are
stored in owner-only settings files. Account-backed entries delegate login and
token refresh to the official vendor runtime.

The model selector is populated from the provider API (through the
`available_models` core action, also used by the phone) and falls back to the
maintained provider catalog. `/model MODEL` remains available for a direct
override.

`Z.ai Coding Plan` uses the dedicated OpenAI-compatible subscription endpoint
`https://api.z.ai/api/coding/paas/v4`. Enter the API key associated with the
active Coding Plan; `glm-5.3-flash` is selected by default. This entry is
separate from Z.ai's general pay-as-you-go API endpoint so Coding Plan requests
use the intended quota path.

API keys are saved in an owner-only settings file and restored when the same
provider is selected later. OpenAI Account authentication is owned by the
bundled Codex app-server and Anthropic Account authentication by the official
Claude Code runtime; GnomeAI reuses their valid sessions until the provider
reports that login has changed or expired. Both account providers expose model
selection rather than forcing every conversation to use `default`.

The same provider selection is available to allowed WhatsApp chats and to the
paired phone. Account providers run through their vendor runtimes on the main
PC, so no OpenAI or Anthropic credentials are copied to WhatsApp, to a
lightweight node or to the phone.

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

Since 3.1, images sent by the model are also displayed: photo bubbles render
in the transcript on desktop and Android. Photos shared with a paired device
travel through the mesh transport in frames capped at 8 MiB
(`ui/GnomeAI.Client/PhotoMessage.cs`) and support JPEG, PNG, WebP and GIF.

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

## Sessions, workspaces and devices

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

Since 3.0, sessions can also be taken over by a paired device. Execution
handoff stages and commits a snapshot between the desktop and the phone;
discarding a failed transfer records a tombstone and restores the previous
ownership, so a stale snapshot can never be activated later.

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

Use `/skill install ./SKILL.md` to install a standalone instruction file without
copying neighboring project files. For skills with supporting resources, pass
the skill directory or Git URL instead. Packages have no total file-count or
size limit.

Managed packages live under:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/gnomeai-rs/skills
```

Project-local skills under `./skills`, `./.agents/skills` and
`./.gnomeai/skills` are also discovered. Activating a skill never widens the
current execution policy.

Since 3.1, the phone manages skills too: the Android Skills panel lists
installed packages, inspects them, installs from a source path or Git URL and
activates them for a conversation, using the same core actions as the desktop.

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

```bash
./scripts/build-node-release.sh
```

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
On the phone, model reasoning is additionally persisted per conversation by
`MobileThinkingStore` under `store/mobile-thinking/`, so reopening a session
restores the thinking blocks exactly as they were received.

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
memory remain private user data and must not be committed. Desktop device
identities and peers live in `store/devices/devices.json` under the same root;
pairing material on the phone is protected by the Android Keystore.

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
new QR code. On Android, reply notifications are owned by the foreground
service and keep working while the app is in the background.

The bridge requires Node.js 20 or newer and the pinned dependencies in
`whatsapp/`. Distribution packages stage those dependencies automatically.
Source builds should run `cargo build --bins --locked` so the private
`gnomef-whatsapp` helper is available beside `gnomef-rs`. The helper listens
only on loopback, requires a per-process token and serves no web page.

## Verification

The Rust test suite covers the core actions added in 3.1 (skills, model
listing, photo content and transfer aborts). The `tests/DeviceLifecycle` and
`tests/MobileThinking` console harnesses exercise pairing, session transfer
and abort handling, and reasoning persistence, across the Rust core and the
.NET clients. Run them with:

```bash
cargo test --locked
dotnet run --project tests/DeviceLifecycle
dotnet run --project tests/MobileThinking
```

## Third-party notices

GnomeAI-RS is GPL-3.0. The optional OpenAI Codex sidecar is distributed under
Apache-2.0. The optional Firecrawl deployment is AGPL-3.0; its exact upstream
source tag, commit, image pins and license are recorded under
`third_party/firecrawl/`.
