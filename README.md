# GnomeAI-RS

Open-source personal AI agent in Rust: coding-agent TUI, self-hosted WebTool,
optional WhatsApp assistant, multiple model providers, persistent memory,
installable skills, and explicit execution permissions.

Current release: **0.1.0-8**. See the release notes below for the complete
list of changes.

## Release Notes — 0.1.0-8

Version **0.1.0-8** represents a major update to GnomeAI-RS, extending the agent with a native skill system, advanced persistent memory, provider management, improved WhatsApp integration, clearly defined access modes, and a more complete and reliable Debian package.

Issues identified in previous versions have been remediated, including WebTool startup, OpenAI and Claude authentication, WhatsApp QR code generation, loading installed resources under `/usr/share`, interface navigation, and dependency distribution in the `.deb` package.

---

## 1. Native Rust Skill System

GnomeAI-RS now includes a skill system implemented natively in Rust and compatible with the standard **`SKILL.md`** format.

Skills can contain:

* agent instructions;
* description and metadata;
* associated commands and tools;
* auxiliary files;
* usage rules;
* version and source information.

Skills can be installed from a local folder or directly from a Git repository.

The following commands have been implemented:

```text
/skills
/skill use
/skill inspect
/skill install
/skill update
/skill verify
/skill remove
```

### Skill Management

The `/skills` command displays available skills and their status.

`skill use` activates a skill for the current conversation.

`skill inspect` displays a skill's metadata, instructions, files, and source.

`skill install` installs a skill from a local folder or a Git repository.

`skill update` updates an installed skill from its original source.

`skill verify` validates the structure, `SKILL.md` file, metadata, and file integrity.

`skill remove` uninstalls a skill and removes its local resources.

The system verifies names, paths, directory structures, and declared files so that a skill cannot write outside its designated installation area.

---

## 2. Skill Manager in WebTool and WhatsApp

WebTool now includes a dedicated skill management interface.

From the web interface, you can:

* view installed skills;
* inspect instructions and metadata;
* install local or Git skills;
* update existing skills;
* verify files and structure;
* activate or deactivate skills;
* remove skills.

Skills can also be activated from WhatsApp conversations, without needing direct access to the terminal or WebTool.

Commands sent through WhatsApp are processed by the same native manager, so skill state remains synchronized between Agent, WebTool, and the WhatsApp bridge.

---

## 3. Access Modes `normal` and `full-access`

Execution modes have been renamed and clarified.

### `normal`

The `normal` mode provides full system access after user approval.

The agent can propose commands that require access outside the workspace. After the user approves a command, it is executed with the necessary permissions.

A situation has been fixed where commands appeared approved but remained blocked by the old sandbox or were not executed.

### `full-access`

The `full-access` mode provides complete access without additional approval prompts.

In this mode, the agent can access the file system and execute commands permitted by the Linux user under which it runs, without intermediate confirmation dialogs.

The mode does not automatically grant `root` privileges and does not replace the OS normal permission mechanism.

---

## 4. Terminal Interface

The TUI has received several fixes related to navigation and mouse usage:

* conversation scrolling has been fixed;
* the mouse wheel can scroll the conversation;
* text selection with the mouse has been corrected;
* navigation through long menus keeps the selected element visible;
* up and down arrows navigate the command menu;
* `Tab` and `Shift+Tab` can be used for navigation;
* the menu has auto-scrolling;
* selection has visible marking;
* `Esc` closes the menu first;
* the command history remains available when the menu is closed.

A new command has also been added:

```text
/help
```

The following aliases are also accepted:

```text
/?
/commands
```

This displays available commands and the keys used for navigation.

### Copy/paste in TUI

Internal mechanisms that blocked selection and mouse interaction have been fixed. However, final copy/paste may also depend on the terminal emulator, the multiplexer used, and how the application activates mouse capture.

In the K3 audit, direct copying from TUI could not be fully confirmed. Therefore, the functionality should be considered fixed at the application level, but requires additional validation in the specific terminal in which it is run.

---

## 5. WebTool: Scrolling and Static Resources

The WebTool issue that prevented scrolling of long conversations has been fixed.

The cause was the main grid structure, whose default row expanded based on content, while the page had:

```css
body {
    overflow: hidden;
}
```

A constraint was introduced:

```css
grid-template-rows: minmax(0, 1fr);
```

This keeps the main area within the viewport and allows proper scrolling in the conversation and the conversation list in the sidebar.

Resources installed in:

```text
/usr/share/gnomeai-rs/
```

now take priority over old copies or resources left over from previous installations.

Thus, WebTool no longer accidentally serves an old version of `index.html`, scripts, or other static resources.

---

## 6. Providers and Banner

The provider system has been extended and standardized.

Both the Agent and WebTool can manage configured providers, including OpenAI-compatible providers and dedicated integrations.

The following service configurations are included:

* OpenAI;
* Anthropic;
* DeepSeek;
* Moonshot;
* Qwen;
* Grok;
* Mistral;
* Ollama;
* other OpenAI-compatible endpoints.

Commands and the interface allow selecting the active provider and model.

The Agent and WebTool banners have been updated to display more clearly:

* the running application;
* the version;
* the active provider;
* the selected model;
* the access mode;
* the status of memory and auxiliary services.

---

## 7. OpenAI Authentication

OpenAI authentication now uses the official Codex flow:

```bash
codex login --device-auth
```

This change eliminates ad-hoc or incompatible flows with recent versions of the Codex CLI.

The official Codex CLI, version **0.145.0**, is included in the distribution and can be used without a separate manual installation.

The login process displays the code and instructions needed for device authorization authentication.

---

## 8. Claude Detection

Claude CLI detection has been extended.

In addition to locations available in `PATH`, the application explicitly checks:

```text
~/.local/bin
~/.claude/bin
```

Thus, Claude is detected even when installed only for the current user, without copying to `/usr/bin` or `/usr/local/bin`.

---

## 9. Native Memory Engine

The JSON file previously used for memory has been replaced with a native Rust engine based on SQLite.

### Key Files

```text
src/memory_engine.rs
src/embeddings.rs
```

Storage has been moved from:

```text
store/memory.json
```

to:

```text
store/memory.db
```

The database uses:

* SQLite;
* WAL mode;
* restrictive `0600` permissions;
* automatic and idempotent migration;
* preservation of metadata and history;
* deduplication on import.

The old file is kept in the form:

```text
memory.json.bak
```

and also receives restrictive permissions.

### Memory Structure

Each memory entry can contain:

* identifier;
* original text;
* normalized text;
* category;
* confidence;
* importance;
* source conversation;
* source channel;
* creation date;
* update date;
* last access;
* access count;
* memory status;
* reference to the memory that replaced it;
* content hash;
* embedding model;
* vector dimension;
* embedding vector;
* source list.

Available statuses include:

```text
active
superseded
forgotten
```

---

## 10. Embeddings and Semantic Search

An `EmbeddingProvider` trait has been implemented with support for:

* OpenAI-compatible `/v1/embeddings` endpoints;
* Ollama `/api/embed`;
* lexical fallback when no embedding provider is configured.

Vectors are validated before saving:

* dimension between 8 and 8192;
* finite values;
* non-zero vector;
* consistent dimension.

Cosine similarity is computed directly in Rust, without an external vector server.

When the embedding model is changed, memories can be reindexed.

Existing API keys are reused from configuration and are not saved in the memory database.

---

## 11. Memory Deduplication

Deduplication operates in four stages:

1. content hash comparison;
2. word-order-independent lexical key comparison;
3. Jaccard score calculation in the same category;
4. semantic comparison through cosine similarity.

The semantic thresholds used are:

```text
>= 0.92       duplicate
0.82–0.92     ambiguous case
< 0.82        separate fact
```

For ambiguous cases, a single LLM call is made, which must return one of the following operations:

```text
ADD
MERGE
UPDATE
SUPERSEDE
IGNORE
FORGET
```

Operations are applied transactionally.

Contradictions do not physically delete old information. The previous memory is marked `superseded` and retains a reference to its replacement.

---

## 12. Hybrid Retrieval

Relevant memories are selected through a combined score that includes:

* semantic similarity;
* lexical overlap;
* confidence;
* importance;
* age;
* access count;
* proximity to the source conversation;
* configurable maximum age filter.

Duplicate memories are no longer injected repeatedly into the prompt.

The memory block is delimited and explicitly marked as data, not as instructions, to reduce the risk of prompt injection.

---

## 13. Dreaming

An asynchronous worker for memory consolidation has been implemented.

Dreaming can be started:

* manually;
* automatically after a minimum of five minutes of inactivity;
* automatically if it has not run in the last 24 hours;
* in dry-run mode.

The worker uses:

* a bounded queue;
* a cross-process lease in SQLite;
* clean cancellation;
* time limits;
* call limits;
* processed fact limits.

The process can:

* group semantically close memories;
* merge duplicates;
* detect contradictions;
* reduce the confidence of old facts;
* regenerate embeddings;
* produce a consolidation report.

Every consolidated fact must retain verifiable sources. Operations that cannot cite sources are rejected.

---

## 14. Memory Security

The memory sanitizer filters or rejects:

* API keys;
* tokens;
* passwords;
* secrets;
* data arriving directly from uploads without validation;
* instructions taken from web pages;
* content attempting to alter the agent behavior;
* phrasing specific to prompt injection attacks.

---

## 15. Memory API

The following endpoints have been added:

```text
/api/memory/status
/api/memory/dream
/api/memory/reindex
/api/memory/forget
/api/memory/clear
```

The TUI includes commands:

```text
/memory status
/memory dream
/memory dream --dry-run
/memory reindex
/memory forget ID
```

WebTool includes a memory maintenance section with:

* status display;
* fact list;
* starting dreaming;
* dry-run;
* reindexing;
* deleting a memory;
* clearing memory.

The memory extractor integration in TUI has also been fixed. It previously ran only at initialization; it now runs after every completed turn.

---

## 16. WhatsApp: Dependencies and Bridge Startup

All dependencies required for WhatsApp integration are included in the `.deb` package.

Manual separate installation of bridge modules after installing GnomeAI-RS is no longer necessary.

The startup process has been modified so that the application no longer considers the bridge functional immediately after launching the process.

The bridge now:

* waits for real startup;
* checks if the process remains active;
* reports initialization errors;
* propagates relevant messages to Agent and WebTool;
* no longer hides early errors.

---

## 17. WhatsApp QR Code

QR code generation has been fixed.

After starting the bridge, the application waits up to **30 seconds** for the QR code to appear.

When the QR code becomes available:

* it is detected automatically;
* it is displayed without an additional command;
* it can be scanned directly to associate the WhatsApp account.

If the QR code is not generated, the interface displays the bridge's real error instead of remaining stuck or only reporting that the process was started.

---

## 18. WhatsApp Received Files

The bridge previously only downloaded images. Now the following are also processed:

* `documentMessage`;
* `documentWithCaptionMessage`;
* audio;
* video;
* stickers;
* video thumbnails.

Native extraction has been added for:

* DOCX;
* XLSX;
* PPTX;
* DOCM;
* XLSM;
* PPTM;
* ODT;
* ODS;
* ODP.

Office archives are processed directly in memory through Rust, without executing files or using an external converter.

Limits have been introduced for protection against zip bomb archives.

Approximately 120 text and code extensions are also accepted, as well as files without an extension, including:

```text
Makefile
Dockerfile
```

Old binary Office formats such as `DOC`, `XLS`, and `PPT` receive an explicit message that they cannot be natively extracted, instead of being interpreted as garbled text.

The flow has been verified end-to-end with:

* PDF;
* DOCX;
* XLSX;
* PPTX;
* Rust files;
* text files.

---

## 19. Audio, Video, and Stickers

The following module has been added:

```text
src/transcribe.rs
```

This defines a `TranscriptionProvider` and an OpenAI-compatible implementation for:

```text
/v1/audio/transcriptions
```

Audio and video files can be sent to the provider for transcription.

The following are not required:

* Python;
* a locally packaged model;
* FFmpeg installed locally.

Video containers are sent directly to the endpoint, which can extract the audio track.

If transcription is not configured, the file is saved and the conversation explains why a transcript was not generated.

The video thumbnail is saved separately as an image and can be analyzed by vision models.

Stickers are processed through the image processing, OCR, and vision pipeline.

---

## 20. OCR Fix

A pre-existing issue was identified that made OCR non-functional even for ordinary images.

Tesseract spawned OpenMP threads, and the `RLIMIT_NPROC` limit could prevent their initialization. The error was hidden by the use of an empty fallback.

A variable was introduced:

```text
OMP_THREAD_LIMIT=1
```

and explicit error messages were added.

OCR has been verified with images containing text such as:

```text
TEST OCR
SALUT
```

---

## 21. Conversation Numbering

Two issues have been fixed:

* deleting a conversation always created a new one;
* numbering used only the maximum value plus one.

Now:

* the first free number is reused;
* after deletion an existing conversation is selected;
* the memory associated with the deleted conversation is removed;
* a reused identifier does not inherit the summary or memory of the old conversation.

---

## 22. Application Icons

Separate icons have been added for:

* **GnomeAI Agent**;
* **GnomeAI WebTool**.

The icons are included in the Debian package and integrated into desktop files, so the two applications can be identified separately in the desktop environment menu.

The following have been updated:

* installed icons;
* `.desktop` files;
* paths under `/usr/share`;
* package metadata.

---

## 23. Codex and Firecrawl Included

The distribution includes the official Codex CLI, version:

```text
0.145.0
```

Codex is used even for OpenAI authentication through device auth.

The Firecrawl source needed for local integration is included in the package.

Firecrawl can be started on demand and used by the web search system without the user needing to clone the repository separately.

Firecrawl resources are installed together with the application and are searched for first in the official directories under `/usr/share`.

---

## 24. Debian Package

The `.deb` package has been rebuilt to include:

* Agent and WebTool binaries;
* WebTool resources;
* WhatsApp bridge;
* WhatsApp dependencies;
* Agent and WebTool icons;
* desktop files;
* Codex CLI 0.145.0;
* Firecrawl source;
* default configurations;
* skill manager;
* files needed by the memory engine.

Applications must run with the normal user, without launching the entire agent through `sudo`.

Only operations that genuinely require additional privileges should be approved or run separately.

---

## 25. Verification and Tests

The final version has passed the following checks:

```text
235 tests passed
cargo fmt valid
release build successful
bridge.mjs syntax valid
index.html syntax valid
```

The following have been tested:

* WebTool API;
* the full skill lifecycle;
* installing a local skill;
* installing a skill from Git;
* inspection;
* activation;
* verification;
* update;
* removal;
* skill integration in Agent;
* integration with WebTool;
* activation from WhatsApp;
* memory and deduplication;
* dreaming and dry-run;
* embedding reindexing;
* WhatsApp uploads;
* bridge startup;
* QR code waiting and display;
* OpenAI authentication;
* Claude detection;
* loading resources from `/usr/share`.

The following version has been generated as a result of these fixes:

```text
GnomeAI-RS 0.1.0-8
```

---

## Summary

GnomeAI-RS 0.1.0-8 brings:

* native Rust skills compatible with `SKILL.md`;
* skill installation from local folders and Git;
* skill manager in Agent, WebTool, and WhatsApp;
* `normal` and `full-access` modes;
* SQLite memory with embeddings, deduplication, and dreaming;
* improved providers and banner;
* fixed scrolling and navigation;
* extended support for documents, audio, video, and stickers;
* functional OCR;
* official OpenAI authentication through Codex;
* improved Claude detection;
* WhatsApp bridge fully included in `.deb`;
* automatic QR code display;
* separate icons for Agent and WebTool;
* official Codex 0.145.0 and Firecrawl source included;
* 235 tests passed and the full skill lifecycle verified.

## Included

- Rust backend source under `src/`
- `Cargo.toml` and `Cargo.lock`
- `index.html` web UI
- optional WhatsApp bridge under `whatsapp/`
- native, declarative `SKILL.md` package manager shared by every interface
- lazy rootless Firecrawl launcher plus the corresponding upstream source
- clean `config.example.json`

## Requirements

Required:

- Rust stable with edition 2024 support
- an OpenAI-compatible API or `llama-server`

Optional:

- `tesseract` for OCR on images
- `pdftotext` from Poppler for PDF text extraction
- Node.js 20+ and `npm` for source-tree WhatsApp bridge setup
- `git` to install skills directly from repositories
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

Typing `/` opens an interactive command menu listing every command with what
it does. `↑`/`↓` (or `Tab`/`Shift+Tab`) move through it, wrapping at both ends
and scrolling when the list is longer than the popup; typing more characters
narrows it; `Enter` picks the highlighted command and a second `Enter` runs it;
`Esc` closes the menu. With the menu closed, `↑`/`↓` still walk the input
history as before.

Useful TUI commands:

- `/help` (aliases `/?`, `/commands`) prints every command with its
  description, plus the keyboard reference.
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
- `/sandbox read-only|normal|full-access` changes the execution policy.
- `/skills` lists installed skills. `/skill use NAME` activates one for the
  current session; `/skill inspect NAME` shows its metadata and path;
  `/skill install PATH_OR_GIT_URL`, `/skill update NAME`, `/skill verify NAME`,
  and `/skill remove NAME` manage user-installed packages.
- `/memory` shows the cross-conversation memory shared with WebTool and
  WhatsApp. Subcommands: `/memory status` (engine health, embeddings, last
  dream cycle), `/memory show`, `/memory dream` (consolidation cycle now),
  `/memory dream --dry-run` (report only, no writes), `/memory reindex`
  (re-embed all facts with the current embedding model), `/memory forget ID`
  (soft-forget one fact), `/memory clear`, `/memory on|off`. Facts that look
  like credentials are excluded automatically, and the extractor runs after
  every finished agent turn.
- `/mouse on|off` toggles app mouse support. With it on, drag with the left
  button to select transcript text, keep the button down and use the wheel to
  extend the selection beyond the screen, then release to copy automatically.
  `Esc` clears a selection. With capture off, native terminal selection works.
- `/copy` (or `Ctrl+Y`/`Ctrl+Shift+C`) copies the active selection, or the last
  assistant reply when nothing is selected. The TUI uses `wl-copy`, `xclip`,
  or `xsel` when available and falls back to OSC 52.
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

- **OpenAI account (Codex)** runs the bundled official
  `codex login --device-auth` flow, then delegates execution to Codex
  app-server. Codex owns credential persistence, token refresh, model calls,
  and coding tools.
- **Anthropic account (Claude Code)** runs `claude auth login`, then delegates
  turns to `claude -p`, which reuses the login managed by Claude Code.

The Linux binary archive already contains Codex v0.145.0; no separate Codex
installation is required. Source builds can install the pinned,
checksum-verified sidecar with `scripts/install-codex-sidecar.sh`. Set
`GNOMEF_CODEX_BIN` only if you deliberately want to use another official
Codex or `codex-app-server` executable.

Install and test Claude Code before selecting the Anthropic account entry.
GnomeAI also discovers native installs under `~/.local/bin/claude` and
`~/.claude/bin/claude`, which are commonly absent from a desktop launcher's
`PATH`. Set `GNOMEF_CLAUDE_BIN` to override discovery. Codex account state is
kept in `CODEX_HOME` (normally `~/.codex`); GnomeAI checks that directory is
writable and reports a clear ownership error after an accidental `sudo` run.
GnomeAI-RS never reads, copies, or persists either vendor's OAuth tokens.
Account mode delegates execution to the vendor runtime, but GnomeAI still
applies the selected outer execution policy. API-key modes use GnomeAI's
native tool loop.

The default policy is `normal`: read-only inspection is immediate, while
commands that execute programs or change state can access the normal
user-level operating system only after an approval prompt. It is not confined
to the selected workspace. `full-access` grants the same user-level access
without approval prompts; it never elevates to root. `read-only` blocks
mutations. Strict internal jobs that process untrusted uploads or
model-generated Python continue to use a separate fail-closed
Landlock/seccomp sandbox regardless of this user-facing setting. Use
`--session ID` to resume a session stored in `store/agent.db`.

### Installable skills

GnomeAI-RS supports OpenClaw/Hermes-style instruction packages natively in
Rust, using the common `SKILL.md` convention. A skill is declarative: YAML
frontmatter provides `name` and `description`, while the Markdown body
contains the workflow. Optional files under `references/`, `scripts/`, or
`assets/` are loaded only when the active task needs them. Installing or
activating a skill never grants extra command permissions and package scripts
are never executed automatically.

Skills can be installed from a local directory or a Git repository. The
installer clones/copies into a staging directory, rejects traversal,
symlinks, binary or oversized instruction resources, validates package
limits, and activates the result atomically. Managed packages live in:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/gnomeai-rs/skills
```

Discovery also supports shared and project-local packages in
`~/.agents/skills`, `./skills`, `./.agents/skills`, and
`./.gnomeai/skills`; project-local definitions have precedence. At startup
the model sees only the compact catalog (name, description, and path). The
full instructions and referenced resources enter context only after the
matching skill is activated, which keeps prompts small.

Agent exposes `/skills` and `/skill ...`; WebTool exposes the same install,
inspect, verify, activate, update, and remove operations in Settings. On
WhatsApp, `/skills`, `/skill inspect NAME`, and `/skill use NAME` are
available, while remote install/update/delete is deliberately refused.

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
pasting an image straight from the clipboard. Deleting a conversation switches
to another existing one instead of creating a new chat, and new chats reuse the
lowest free number, so the list does not drift upwards after deletions.
Settings also contains the shared skill manager and lets a validated skill be
activated for the current conversation. Skill activation messages are stored
as system context and never rendered as fake assistant replies.

### Supported attachments

The same extraction pipeline serves WebTool uploads and WhatsApp attachments:

- **Images** (`png`, `jpg`, `jpeg`, `bmp`, `gif`, `webp`) — shown inline, text
  read with `tesseract` OCR, and sent to the model directly when it has vision.
  WhatsApp **stickers** are WebP images and follow exactly this path.
- **Voice notes, audio and video** (`ogg`, `opus`, `mp3`, `m4a`, `wav`, `flac`,
  `amr`, `mp4`, `mov`, `webm`, `mkv`, `3gp`, …) — transcribed to text, see
  below. A video also stores its WhatsApp still frame as a separate image, so a
  vision-capable model sees the scene while the transcript carries the speech.
- **PDF** — text via `pdftotext`.
- **OOXML and OpenDocument** (`docx`, `xlsx`, `pptx`, `docm`, `xlsm`, `pptm`,
  `odt`, `ods`, `odp`) — extracted in-process by unzipping the container and
  reading its XML parts. No external converter is involved, nothing inside the
  archive is executed, and archive size and entry count are bounded.
- **Plain text, data and source code** — `txt`, `md`, `csv`, `tsv`, `json`,
  `yaml`, `toml`, `xml`, `html`, `sql`, plus the usual code extensions (`rs`,
  `py`, `js`, `ts`, `go`, `java`, `c`, `cpp`, `cs`, `rb`, `php`, `sh`, `lua`,
  `swift`, `kt`, and more) and extensionless names such as `Makefile` and
  `Dockerfile`.

Legacy binary Office formats (`doc`, `xls`, `ppt`, `rtf`) and unrecognized
binary files are stored but reported as unreadable rather than dumped into the
prompt as garbage; resend them as PDF or OOXML. Extracted text is capped per
file, and attachments are limited to 50 MB (15 MB over WhatsApp by default —
raise `GNOME_WA_MAX_MEDIA_BYTES` to change it).

### Speech-to-text configuration

Transcription is optional and, like embeddings, native Rust talking to an
OpenAI-compatible endpoint — `/v1/audio/transcriptions`, which whisper.cpp's
HTTP server, faster-whisper, LocalAI, vLLM and OpenAI all expose. No Python, no
bundled model, no local `ffmpeg`: video containers are uploaded as-is and the
endpoint pulls out the audio track. Configure in `config.json`:

- `transcription_model` — e.g. `whisper-1`. Empty (the default) disables
  transcription.
- `transcription_provider` — `auto` (default) or `off`.
- `transcription_base_url` — empty reuses `llama_base_url`.
- `transcription_language` — optional ISO-639-1 hint such as `ro`, which
  measurably improves Romanian accuracy. Empty lets the model detect it.

The provider API key is reused and never copied into stored state. Recordings
above 25 MB are rejected before upload. When transcription is unconfigured or
fails, the file is still stored and the conversation says plainly that the
spoken content is unavailable, rather than silently dropping the message.

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
filesystem isolation is unavailable. The strict internal sandbox deliberately
permits local `AF_UNIX` and `AF_NETLINK` sockets for build-tool and libc
compatibility, while blocking Internet socket families (`AF_INET`, `AF_INET6`,
and `AF_PACKET`). The Agent's user-facing `normal` and `full-access` modes are
explicit host-execution policies and do not claim to be this strict sandbox.

### Cross-chat memory

All three interfaces — WebTool, the terminal agent, and WhatsApp — share one
memory engine backed by SQLite at `store/memory.db` (WAL mode, `0600`
permissions, safe for two processes at once). A pre-existing
`store/memory.json` is migrated automatically on first start — facts keep
their ids, confidence, categories, and access statistics — and the old file
stays next to the database as a private `memory.json.bak` backup.

Each memory records its text, normalized text, category, confidence,
importance, source conversation and channel (`webtool`/`tui`/`whatsapp`),
timestamps, access count, status (`active`/`superseded`/`forgotten`), content
hash, its embedding (model, dimension, and `f32` vector), and the list of
sources it was derived from. Contradicted facts are never deleted: the old
version is marked `superseded` and points at its replacement, so provenance
survives.

**Extraction** runs after every finished answer in every interface. New
candidates pass a sanitizer (no credentials, no content lifted from uploads
or web pages, no injected instructions), then four deduplication stages:
exact content hash, order-independent lexical key, Jaccard word overlap
within the same category, and embedding cosine similarity. Cosine ≥ 0.92 is a
duplicate; 0.82–0.92 is ambiguous and goes — in a single batched model call —
to a reconciler that must answer with exactly one of `ADD`, `MERGE`,
`UPDATE`, `SUPERSEDE`, `IGNORE`, or `FORGET` per candidate; below 0.82 the
candidate is stored as a separate fact. All resulting operations are applied
in one transaction.

**Retrieval** is hybrid: the final score combines cosine similarity, lexical
overlap, confidence, importance, recency, access count, and a
same-conversation bonus. Only the top-ranked, deduplicated facts inside the
age filter are injected into the prompt, explicitly framed as stored data,
never instructions. In Settings, **Memory between conversations** can be
disabled or limited to items from the last 7, 30, 90, or 365 days; the age
filter does not delete old data, it only keeps it out of the model context.

### Embeddings configuration

Embeddings are optional — without them the engine falls back to purely
lexical retrieval and deduplication. Configure in `config.json`:

- `embeddings_model` — e.g. `nomic-embed-text` or `text-embedding-3-small`.
  Empty (the default) disables embeddings.
- `embeddings_provider` — `auto` (default), `openai`, `ollama`, or `off`.
  `auto` picks Ollama when the base URL points at `:11434`, otherwise the
  OpenAI-compatible `/v1/embeddings` endpoint.
- `embeddings_base_url` — empty reuses `llama_base_url` for OpenAI-compatible
  providers and `http://127.0.0.1:11434` for Ollama.

The existing provider API key is reused for the embeddings endpoint and is
never copied into the memory database. Vectors are stored as `f32` BLOBs in
SQLite and compared with an exact cosine scan in Rust — at the configured cap
of 5 000 facts an approximate index (HNSW) would not pay for itself. Every
vector is validated (dimension 8–8192, finite, non-zero) before storage.
Changing the embedding model invalidates stored vectors safely: retrieval
only compares vectors produced by the active model, the background worker
re-embeds gradually, and `/memory reindex` (or **Reindexează** in WebTool)
re-embeds everything at once.

### Dreaming (memory consolidation)

A background worker consolidates memory when the system is idle: manually via
`/memory dream` (or the WebTool button), automatically after at least
`memory_dream_idle_minutes` (default 5) of inactivity when no cycle ran in
the last `memory_dream_interval_hours` (default 24), or as a `--dry-run` that
reports every planned operation without writing. A cycle groups semantically
similar facts, folds duplicates, detects contradictions, gradually decays the
confidence of old unused memories, marks outdated information as superseded,
re-embeds changed facts, and produces a report of every operation. Model
proposals are validated before being applied: every consolidated or
generalized fact must cite the ids of its source memories, and operations
referencing unknown ids — or inventing new information — are rejected. The
worker uses a bounded queue, a cross-process lease (so WebTool and the agent
never dream simultaneously), clean cancellation, and time/call/fact limits
(`memory_dream_max_seconds`, `memory_dream_max_llm_calls`,
`memory_dream_max_facts`). It never touches the active conversation.

WebTool exposes the same controls under Settings (**Întreținerea memoriei**):
show facts with per-fact forget, dream, dream dry-run, reindex, and clear,
plus the HTTP endpoints `GET /api/memory`, `GET /api/memory/status`,
`POST /api/memory/dream`, `POST /api/memory/reindex`,
`POST /api/memory/forget`, and `POST /api/memory/clear`.

Known limits: semantic deduplication and dreaming consolidation need an
LLM-capable HTTP provider (OpenAI-compatible or Anthropic); with
account-backed CLI providers the terminal agent still reads and serves
memory, but skips extraction. Without an embeddings model, similarity is
lexical only, which is weaker across languages. The memory store is capped at
`memory_max_facts_stored` (≤ 5 000); overflow is soft-forgotten, lowest
confidence first.

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

Managed skills are user data rather than chat state and live under
`${XDG_DATA_HOME:-$HOME/.local/share}/gnomeai-rs/skills`.

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

The Debian package already contains the pinned Node dependencies and starts
the bridge on demand. A source checkout installs the exact lockfile versions
once with:

```bash
cd whatsapp
npm ci --ignore-scripts --legacy-peer-deps --omit=dev --omit=optional
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

Starting WhatsApp from WebTool waits for the local bridge, then waits up to
30 seconds for Baileys to publish a QR code and displays it automatically.
Bridge import, Node-version, port, and network failures are surfaced in the
settings panel instead of being reported as a running bridge.

Once connected, the assistant also accepts `/skills`,
`/skill inspect NAME`, and `/skill use NAME`. Skills activated from WhatsApp
are persisted as system context for that WhatsApp conversation and use the
same memory and model-provider configuration as WebTool.

## Notes

- If `config.json` is missing, the server falls back to built-in defaults.
- `config.json` is ignored by Git in this export so local secrets do not get
  committed by accident.
- Persistent cross-chat memory lives in `store/memory.db` (SQLite, WAL); a
  legacy `store/memory.json` is migrated automatically and kept as
  `store/memory.json.bak`.
- The browser UI is a single static `index.html` file served by the backend.
