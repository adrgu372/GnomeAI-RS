# Unreleased

## Avalonia desktop frontend

- Make command-approval feedback explicit: choosing Allow once, Always allow
  or Deny immediately removes all decision buttons, records the selected state
  on the card and prevents duplicate/conflicting submissions.
- Install the WhatsApp backend as an auto-enabled systemd user service. It now
  keeps receiving and answering messages after the Avalonia window closes,
  while the UI and service share a private stable per-user loopback token.
- Reuse the application path retained by `AppPaths` when creating that token,
  avoiding a moved-`PathBuf` compilation failure in `gnomef-whatsapp`.
- Download the pinned Microsoft .NET SDK 8.0.424 Linux x64 archive directly
  during Debian packaging, verify its official SHA-512 and bundle the complete
  SDK privately in the `.deb`. The installed package no longer depends on a
  Microsoft APT repository or a separately installed `dotnet-sdk-8.0` package.
- Route transcript wheel input through the outer ScrollViewer and render chat
  text with SelectableTextBlock instead of nested read-only TextBox scrollers,
  preventing mouse-wheel input from terminating the Avalonia process.
- Close the current assistant/reasoning segment before tool, approval, patch or
  verification cards, so the final answer starts in a new chronological bubble.
- Keep new/resumed session navigation silent in the transcript while background
  chats continue independently.
- Convert buffered multi-chat token builders back to strings during replay and
  update the Avalonia drag-data API, eliminating the reported C# build errors
  and obsolete-member warning. Rename the Markdown border brush and implement
  the always-enabled command event explicitly to keep the UI build warning-free.
- Restored every Rust binary entry point under `src/bin` in the complete source
  archive so `scripts/build-deb.sh` can resolve all targets from `Cargo.toml`.
- Replaced the `eframe`/`egui` window with a native Avalonia UI 11 frontend
  (`ui/GnomeAI.UI`) spawned and supervised by the Rust core as a private child
  process.
- Conversations, providers, attachments, approvals, sudo prompts, WhatsApp,
  memory, skills, sandbox modes and the node hub now flow through the extended
  newline-delimited JSON `Op`/`Event` bridge in `src/avalonia_bridge.rs`.
- Run multiple saved conversations concurrently. Turn events are tagged by
  session, background streaming is buffered independently, each chat has its
  own queue and Stop state, and the sidebar shows working/attention status.
- Keep path-bound providers, registries and MCP transports alive for background
  turns when the foreground conversation changes workspace. Sensitive approval
  and sudo interactions remain serialized so replies cannot cross sessions.
- Removed the `eframe` and `egui` dependencies; the Rust executable stays the
  sole core and packaging builds the Avalonia .NET 8 UI with its private SDK.
- Restored feature parity with the previous native window: every slash command,
  session rename/delete/fork flow, provider and model selection, MCP editor,
  memory and skill tools, transcript search/export, activity/diff pane,
  WhatsApp conversations/QR/test messaging and per-node root policy is exposed
  in Avalonia.
- Reworked the complete desktop shell in an English Windows Apps style with
  persistent System, Light and Dark Fluent themes, a navigation pane, command
  bar, content cards and native resizable dialogs.
- Replaced oversized conversation close controls and the stock reasoning
  expander with compact Fluent controls, fixed sidebar icon spacing and
  composer alignment, and kept the last transcript line clear of the composer.
- Stabilized chat-card width from the first message, centered the Send label,
  and made Enter send while Shift+Enter inserts a new line.
- Anchored transcript auto-scroll after Markdown layout settles so the final
  lines remain fully visible above the composer.
- Batch streamed tokens and throttle Markdown reconstruction with reusable UI
  timers, then follow the transcript's real content-size changes. This avoids
  dispatcher backlogs, UI freezes and stale bottom-scroll positions.
- Increase the live transcript follow space to roughly three text lines so new
  output moves clearly above the composer instead of touching its top edge.
- Stop polling monitor enumeration from the UI thread during idle/standby
  recovery; window geometry alone now drives bounded restoration.
- Refined the delete-conversation confirmation into a compact, symmetrical
  Fluent dialog.
- Added native Markdown presentation for streamed headings, lists, quotes,
  tables and fenced code, including one-click code copying.
- Added a source-level UI contract check to CI so XAML handlers, legacy slash
  commands, core operations and English-only UI labels cannot silently regress.

## Display resume and responsive layout

- Preserve the last stable Avalonia window size, position, display size and
  window state, then restore them after suspend, screen lock, monitor power-off
  or a transient fallback resolution.
- Wait for geometry to settle, retry compositor recovery a bounded number of
  times, and accept a deliberate resize or permanent display change as the new
  baseline.
- Keep the transcript and composer inside bounded Grid/ScrollViewer layouts;
  fenced code and tables receive their own horizontal scrollers so long output
  cannot widen or shift the conversation.
- Bound every conversation card to the transcript width and collapse detailed
  reasoning/tool output without hiding approvals or administrator prompts.

---

# GnomeAI-RS 2.3.0 - 2026-08-27

## Z.ai Coding Plan

- Added `Z.ai Coding Plan` as a native OpenAI-compatible provider using the
  dedicated `https://api.z.ai/api/coding/paas/v4` subscription endpoint.
- Added `glm-5.3-flash` as the multimodal default and `glm-5.3` as a selectable
  fallback, with provider-scoped API-key persistence shared by the desktop,
  WebTool, WhatsApp and subagent paths.

## WhatsApp provider recovery

- Fixed GLM WhatsApp turns stalling after the first answer when the model
  selected the browser-only `AskUserQuestion` tool. WhatsApp now asks such
  clarifications as an ordinary reply instead of waiting up to one hour for a
  widget that does not exist on that channel.
- Serialized inbound turns per WhatsApp conversation and prevented dedicated
  assistant numbers from feeding their own outbound replies back to the model.
- Applied the configured `llama_timeout` to streamed and buffered provider
  requests, so a silent upstream connection can no longer hold the WhatsApp
  flow indefinitely.

---

# GnomeAI-RS 2.2.0 - 2026-08-25

## Structured native responses

- Added native Markdown rendering for assistant responses in the desktop and
  WhatsApp conversation views: headings, paragraphs, nested ordered and
  unordered lists, task markers, quotes, separators, tables and fenced code.
- Added language labels and one-click copying to code blocks while preserving
  selectable text and the existing copy-selection/context-menu behavior.
- Fixed transcript mouse-wheel scrolling over read-only selectable text,
  including while a text selection is active. The main transcript and the
  separate WhatsApp transcript both use the corrected wheel path.
- Added native keyboard, context-menu and Linux primary-selection clipboard
  behavior for transcript and composer text.

## Generic MCP and provider recovery

- Added configurable MCP servers over Streamable HTTP and stdio. Native tools
  are registered in one shared approval-aware registry and work with API-key
  providers as well as delegated OpenAI/Codex and Anthropic/Claude sessions.
- Added MCP resource/content handling and native settings for server command,
  URL, headers, environment variables and enablement.
- Added up to two automatic recalls after transient provider failures, empty
  responses, interrupted streams or silent streams before the interface marks
  the provider unavailable.

## Privileges and WhatsApp

- Reworked elevated-command authentication around dynamic PAM/sudo prompts and
  a private local askpass socket, with cancellation, timeouts and diagnostics.
  Only actual password prompts may be saved to the keyring.
- Added a separate WhatsApp conversations window with chat selection, message
  history and structured assistant output.
- Added regression coverage for structured Markdown parsing, streaming code,
  links, Unicode selection, WhatsApp filtering and conversation display.

---

# GnomeAI-RS 2.0.2 - 2026-08-23

## English native interface

- Translated the complete native `eframe`/`egui` interface to English,
  including conversations, settings, provider and account login, model
  selection, attachments, approvals, activity, WhatsApp and paired devices.
- Kept Romanian natural-language workspace detection intact, so Romanian
  requests that change the active project continue to work.
- Preserved Unicode and Romanian-diacritic regression coverage while leaving
  command names, provider IDs, protocol values and internal keys unchanged.

## Persistent context compaction

- Improved the SQLite-backed compactor used by the actual native `Agent`
  instead of maintaining a second compaction path in the WhatsApp helper.
- Automatic compaction now starts at 80% of the configured context window,
  keeps the newest eight turns and preserves assistant/tool-result boundaries.
- Added `GNOMEF_CONTEXT_WINDOW_TOKENS` for local and custom models, with a
  128k-token fallback when no override is configured.
- Compaction checkpoints now preserve decisions, file paths, key code, tool
  results, open tasks and explicit user preferences in a structured format.
- Checkpoints are persisted and reused, so older history is summarized once
  instead of being sent through another compaction call on every later turn.
  Long conversations can recursively compact earlier checkpoints without
  deleting the original SQLite turns.
- Compacted material is treated as untrusted data and credentials, tokens and
  private keys are explicitly excluded from generated checkpoints.

---

# GnomeAI-RS 2.0.1 - 2026-08-23

- Fixed OpenAI Account requests inheriting a custom provider such as
  OpenRouter from `~/.codex/config.toml` and incorrectly asking for
  `OPENROUTER_API_KEY`. The embedded Codex app-server now explicitly selects
  the built-in OpenAI provider and ChatGPT authentication while continuing to
  reuse the persistent account session in `CODEX_HOME`.
- Added regression coverage for both supported sidecar layouts: the complete
  `codex app-server` CLI and the standalone `codex-app-server` executable.

---

# GnomeAI-RS 2.0.0 - 2026-08-23

GnomeAI-RS 2.0 replaces the terminal/browser-first product with a polished
native graphical application while preserving the Rust agent core and adding
distributed execution for weak Linux computers.

## Native graphical application

- Replaced the former HTML page as the primary interface with a native
  `eframe`/`egui` desktop application. The packaged launcher opens no terminal
  and no browser window.
- Added a Codex-inspired workspace: conversation sidebar, searchable
  transcript, streaming messages, expandable reasoning/tool cards, diff and
  verification views, and a full-width bottom composer.
- The composer accepts focus from any empty point inside the bar, renders
  Romanian diacritics, wraps long text and grows automatically to eight rows.
- Added drag-and-drop and native file pickers. Attachments support images,
  PDF, DOCX/XLSX/PPTX and their macro/ODF variants, plain text/data files, and
  a broad set of source-code formats.
- Conversations now receive automatic titles and can be renamed, forked or
  deleted directly from the sidebar.
- Settings, provider, model, WhatsApp, skills, memory, diagnostics and devices
  open as compact, resizable native windows with consistent close controls.

## Providers, accounts and multimodal input

- Added native provider and model selectors for OpenAI-compatible and
  Anthropic APIs, plus subscription-backed OpenAI Account (Codex app-server)
  and Anthropic Account (Claude Code) sessions.
- API keys and valid account sessions are reused across launches; users only
  need to authenticate again when credentials change or expire.
- Account providers can select explicit supported models instead of being
  restricted to `default`.
- Relaxed model-name heuristics for vision and added safe multipart-to-text
  fallback when an OpenAI-compatible endpoint rejects `image_url` content.

## WhatsApp and agent capabilities

- Added native WhatsApp configuration, QR pairing, status, reconnect and test
  messaging. Allowed chats share providers, memory, skills, file processing,
  tools and paired nodes with the desktop app, including OpenAI and Anthropic
  account-backed providers.
- Added safe recovery for stale WhatsApp cryptographic sessions and clearer
  handling of transient stream/USync errors.
- Added installable `SKILL.md` packages and restored explicit workflow learning
  through `learn_skill`. Learning and execution remain separate operations;
  learned entrypoints can run locally or on a paired node.
- Added autonomous Linux desktop navigation. Semantic AT-SPI inspection and
  actions are preferred, with screenshot/coordinate input as a fallback.
- Added persistent SQLite memory, Web Search/Firecrawl on demand, subagents,
  task planning, sandbox modes, native approvals and a dedicated sudo path.

## Hub and lightweight nodes

- Added the native Hub/devices window and the init-agnostic `gnomeai-node`
  client for Raspberry Pi, Void Linux and other low-power computers. Models,
  credentials, memory and reasoning remain on the main PC; nodes connect
  outbound and execute approved jobs.
- Added separate enrollment/admin tokens and per-device root policies. Remote
  root requires both local `--allow-root` enrollment and permission from the
  main graphical app.
- Added foreground/manual, runit, OpenRC and s6 deployment documentation with
  no systemd dependency.
- Added real cross-compiled node release packages: amd64 and arm64 `.deb` and
  `.tar.gz`, plus Void `x86_64`, glibc `aarch64` and musl `aarch64-musl`
  `.xbps` packages. Package builders validate ELF architecture before naming an
  artifact and use `cross-rs` for the musl target.
- Fixed the XBPS CA certificate dependency to use the valid
  `ca-certificates>=0` expression.

---

# GnomeAI-RS 1.2.4-15 - 2026-08-23

- Added an init-agnostic `gnomeai-node` client for Raspberry Pi and weak Linux
  PCs, plus a native Hub/devices window in the main application.
- Added separate node/admin credentials and per-device root policies; root
  also requires local `--allow-root` opt-in.
- Restored explicit workflow learning as managed SKILL.md packages with an
  optional executable entrypoint that can run locally or on a paired node.
- Added standalone `.deb` and `.tar.gz` node packaging for manual, runit,
  OpenRC, s6 or other supervisors.
- Node packaging now cross-compiles and verifies both amd64 and arm64 ELF
  binaries, and emits native Void Linux `.xbps` packages locally or through
  the official Void OCI image on Debian hosts.
- Added a distinct `aarch64-unknown-linux-musl` build and
  `aarch64-musl.xbps` package for Void musl installations.
- Fixed the XBPS CA certificate dependency to use a valid package expression
  (`ca-certificates>=0`) so local repository installation can resolve it.

# GnomeAI-RS 1.2.4 - 2026-08-09

- Allowed WhatsApp chats now treat the inbound request itself as authorization
  for standard user-level tools. Writes, edits, Bash commands, and delegated
  subagent work no longer block on an unattended WebTool approval dialog.
- `read-only` remains enforced. WhatsApp sudo can reuse an active ticket or a
  valid credential saved in the desktop keyring; otherwise it returns a clear
  error without opening a local password prompt.

---

# GnomeAI-RS 1.2.3 - 2026-08-08

- Replaced the raw macOS DMG folder with a real `.pkg` installer contained in
  the DMG.
- Added three Spotlight/Launchpad applications and three `/usr/local/bin`
  commands for Agent, compatibility Agent, and WebTool launches.
- Added Developer ID Application/Installer signing and Apple notarization
  support, including stapled tickets for both the package and DMG.
- Removed the hard-coded release name that relabeled a 1.2.1 build as 1.2.2;
  release tags must now match the Cargo package version.
- Corrected the macOS WebTool URL from port 8788 to 8787.

---

# GnomeAI-RS 1.2.1 - 2026-08-06

- Replaced simulated WebTool response chunking with real token-by-token
  streaming for OpenAI-compatible and Anthropic providers, including reliable
  assembly of fragmented and parallel tool calls.
- Added live reasoning and tool lifecycle events to WebTool. The final saved
  answer remains the authoritative post-consistency-check response.
- Added server-side turn tracking and a functional interrupt endpoint. Stopping
  a WebTool response now cancels the running model/tool turn instead of merely
  closing the browser connection.
- Removed the default tool-round ceiling. Agent loops now continue until the
  model finishes, the context budget stops them, or the user interrupts.
- Added context compaction that preserves the system prompt, original request,
  and complete assistant-tool-result groups.
- Isolated approval scopes for parallel subagents while keeping task identity
  attached to the parent conversation, preventing unrelated approval dialogs
  from rejecting one another.
- Propagated real cancellation into Bash and Sudo execution and report
  cancelled commands as failures rather than successful runs.
- Fixed the terminal composer to wrap long lines, keep byte-accurate cursor
  placement, grow to eight rows, and scroll while keeping the cursor visible.
- Made WebTool startup resilient so a failed workspace request no longer
  prevents chats and other independent sections from loading.
- Updated package metadata to 1.2.1 and restored the corresponding Firecrawl
  AGPL source archive required by the Debian bundle.

---

# GnomeAI-RS 1.2.0 - 2026-08-02

- Added native Rust, Claude Code-style subagent orchestration: every subagent
  receives an isolated context and durable ID, parent/child metadata, bounded
  nesting/concurrency, profile-specific tools, and a result returned to the
  parent. Multiple `Agent` calls emitted in one model response run concurrently.
- Each subagent can independently use `inherit` or a chosen API provider and
  model without changing the main chat provider. Saved provider-scoped API keys
  are reused, while account-backed terminal providers remain excluded from the
  WebTool/WhatsApp tool loop.
- WebTool has a shared subagent panel for manual per-instance provider/model
  selection, live output, history, and stopping workers. WhatsApp exposes the
  same registry through `/agents`, `/agent ID`, and `/stopagent ID`.
- WebTool now has a safe live workspace selector with persistent recent
  folders; WebTool skills, file tools, Bash, delegated agents, and WhatsApp
  turns all resolve against the same selected workspace.
- WebTool and WhatsApp now share `read-only`, `normal`, and `full-access`
  execution modes. Normal-mode writes and commands produce local approval
  dialogs labeled with their originating interface.
- The native `Sudo` tool is available to WebTool/WhatsApp through a masked
  local credential dialog. Remote WebTool bindings cannot browse folders or
  request sudo, and ordinary Bash is launched with `no_new_privs`.
- Added an original native Rust `sudo` tool with per-command root approval,
  masked TUI authentication, zeroized secrets, optional Secret Service keyring
  persistence, and no plaintext credential fallback.
- Ordinary `shell` children now run with Linux `no_new_privs`, including in
  full-access mode, so a cached sudo ticket cannot bypass the dedicated root
  approval path.
- Native tool schemas now carry explicit side effects, concurrency and approval
  requirements in one deterministic registry.
- Large tool results are retained for seven days in private owner-only files;
  the model receives a bounded preview and can retrieve the complete result in
  line ranges with `read_tool_output`.
- API keys are now persisted per provider and automatically restored when the
  user switches back; secret maps are never returned by WebTool APIs.
- OpenRouter `402 Payment Required` responses now retry against a live,
  capability-filtered list of zero-cost models ordered by current agentic or
  intelligence ranking, with `openrouter/free` as the last fallback.
- The terminal composer now shows inline slash-command autosuggestions and
  accepts them with `Tab` or `Enter` without executing them; `/provider` and
  `/model` remain dedicated searchable pickers.
- Provider model lists are loaded from live OpenAI-compatible `/models`
  endpoints with an eight-second timeout and maintained per-provider fallback.
- The terminal `/model` command opens a searchable model picker populated by
  the active provider; `/model MODEL` remains available for manual overrides.
- Agent `ProviderChanged` and `Ready` snapshots now carry the same normalized
  model list instead of replacing live results with the hardcoded catalog.
- WebTool refreshes model suggestions for the provider currently being edited,
  ignores stale concurrent responses, and refreshes again after saving.

---

# GnomeAI-RS 1.1.1 - 2026-08-02

- Fixed agent turns that stopped after displaying an intermediate tool status:
  textual tool calls from local model templates are now recognized and run.
- Tool loops now always synthesize a final response when the execution round
  limit is reached instead of leaving the user with a progress message.
- The WhatsApp bridge watches its WebTool parent process and exits cleanly when
  that process disappears, preventing stale listeners on port `8788`.
- Project skills, including the eToro app skill, are copied into Debian
  packages under `/usr/share/gnomeai-rs/skills`.
- WebTool now recognizes `omni` vision models and reads OpenRouter
  `architecture.input_modalities`, so image-capable models are no longer
  rejected when their identifier does not contain `vision` or `vl`.

- Terminal `Ctrl+V` now sends clipboard images as real multimodal request
  parts to compatible OpenAI and Anthropic API providers. Only images captured
  by the clipboard handler are trusted, uploads are limited to 20 MiB, and
  temporary files are removed after submission.
- Persisted image turns are rendered compactly in the terminal, delegated CLI
  prompts, token estimates, and compaction; base64 payloads are never printed.
- WhatsApp status changes are delivered to WebTool over SSE. QR regeneration
  is explicit, confirmed in the UI, clears stale pairing state, and reports
  conflicts and startup failures with the correct HTTP status.
- The terminal commands `/contrast`, `/notify`, `/tokens`, and `/export` are
  documented and covered by tests.

---

# GnomeAI-RS 1.0 - complete description of the changes

Version **1.0** is a major update to GnomeAI-RS. It extends the agent with a
native skills system, advanced persistent memory, provider administration,
improved WhatsApp integration, clearly defined access modes, and a more complete
and reliable Debian package.

The issues identified in previous versions have been fixed, including WebTool
startup, OpenAI and Claude authentication, WhatsApp QR code generation, loading
resources installed in `/usr/share`, interface navigation, and shipping the
required dependencies inside the `.deb` package.

---

## 1. Native skills system in Rust

GnomeAI-RS now includes a skills system implemented natively in Rust and
compatible with the standard **`SKILL.md`** format.

A skill can contain:

* instructions for the agent;
* a description and metadata;
* associated commands and tools;
* auxiliary files;
* usage rules;
* version and source information.

Skills can be installed either from a local folder or directly from a Git
repository.

The following commands were implemented:

```text
/skills
/skill use
/skill inspect
/skill install
/skill update
/skill verify
/skill remove
```

### Skill administration

`/skills` lists the available skills and their state.

`skill use` activates a skill for the current conversation.

`skill inspect` shows the skill's metadata, instructions, files and source.

`skill install` installs a skill from a local folder or from a Git repository.

`skill update` updates an installed skill from its original source.

`skill verify` validates the structure, the `SKILL.md` file, the metadata and the
integrity of the files.

`skill remove` uninstalls the skill and removes its local resources.

The system validates names, paths, directory structure and declared files, so a
skill cannot write outside the space reserved for its installation.

---

## 2. Skill manager in WebTool and WhatsApp

WebTool now includes a dedicated interface for administering skills.

From the web interface you can:

* view installed skills;
* inspect instructions and metadata;
* install local or Git-based skills;
* update existing skills;
* verify files and structure;
* enable or disable skills;
* remove skills.

Skills can also be activated from WhatsApp conversations, without having to open
the terminal or WebTool directly.

Commands sent over WhatsApp are processed through the same native manager, so
skill state stays synchronized across the Agent, WebTool and the WhatsApp bridge.

---

## 3. The `normal` and `full-access` modes

The execution modes have been renamed and clarified.

### `normal`

`normal` grants full system access after user approval.

The agent may propose commands that require access outside the workspace. Once
the user approves a command, it is actually executed with the permissions it
needs.

The situation where commands appeared to be approved but stayed blocked by the
old sandbox, or were not executed at all, has been fixed.

### `full-access`

`full-access` grants full access without further approval prompts.

In this mode the agent can reach the filesystem and run the commands permitted to
the Linux user it runs as, without intermediate confirmation dialogs.

The mode does not automatically grant `root` privileges and does not replace the
operating system's normal permission mechanism.

---

## 4. Terminal interface

The TUI received several fixes related to navigation and mouse handling:

* conversation scrolling was repaired;
* the mouse wheel can scroll the conversation;
* mouse text selection was corrected;
* navigating long menus keeps the selected entry visible;
* the up and down arrows navigate the command menu;
* `Tab` and `Shift+Tab` can be used for navigation;
* the menu scrolls automatically;
* the selection has a visible marker;
* `Esc` closes the menu first;
* command history stays available while the menu is closed.

A new command was added:

```text
/help
```

The following aliases are also accepted:

```text
/?
/commands
```

It lists the available commands and the keys used for navigation.

### Copy/paste in the TUI

The internal mechanisms that blocked selection and mouse interaction were fixed.
Even so, the final copy/paste behaviour can still depend on the terminal
emulator, the multiplexer in use, and how the application enables mouse capture.

During the K3 audit, copying directly from the TUI could not be fully confirmed.
The functionality should therefore be considered fixed at the application level,
but it still needs validation in the specific terminal it is run in.

---

## 5. WebTool: scrolling and static resources

The WebTool bug that prevented scrolling through long conversations has been
fixed.

The cause was the structure of the main grid, whose implicit row grew with the
content while the page had:

```css
body {
    overflow: hidden;
}
```

The following constraint was introduced:

```css
grid-template-rows: minmax(0, 1fr);
```

It keeps the main area inside the viewport and allows correct scrolling both in
the conversation and in the sidebar conversation list.

Resources installed in:

```text
/usr/share/gnomeai-rs/
```

now take precedence over older copies or leftovers from previous installations.

As a result, WebTool no longer accidentally serves an outdated `index.html`,
scripts or other static resources.

---

## 6. Providers and banner

The provider system has been extended and made consistent.

The Agent and WebTool can administer the configured providers, including
OpenAI-compatible providers and dedicated integrations.

Configurations are included for services such as:

* OpenAI;
* Anthropic;
* DeepSeek;
* Moonshot;
* Qwen;
* Grok;
* Mistral;
* Ollama;
* other OpenAI-compatible endpoints.

The commands and the interface allow selecting the active provider and model.

The Agent and WebTool banner was updated to show more clearly:

* which application started;
* the version;
* the active provider;
* the selected model;
* the access mode;
* the state of memory and auxiliary services.

---

## 7. OpenAI authentication

OpenAI authentication now uses the official Codex flow:

```bash
codex login --device-auth
```

This change removes the improvised flows that were incompatible with recent
versions of the Codex CLI.

The official Codex CLI, version **0.145.0**, is included in the distribution and
can be used without a separate manual installation.

The login process displays the code and the instructions required for device
authorization.

---

## 8. Claude detection

Claude CLI detection has been extended.

Besides the locations available in `PATH`, the application explicitly checks:

```text
~/.local/bin
~/.claude/bin
```

Claude is therefore detected even when it was installed only for the current
user, without being copied into `/usr/bin` or `/usr/local/bin`.

---

## 9. Native memory engine

The JSON file previously used for memory has been replaced with a native Rust
engine backed by SQLite.

### Main files

```text
src/memory_engine.rs
src/embeddings.rs
```

Storage moved from:

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

The old file is kept as:

```text
memory.json.bak
```

and also receives restrictive permissions.

### Structure of a memory

Each memory can contain:

* an identifier;
* the original text;
* the normalized text;
* a category;
* confidence;
* importance;
* the source conversation;
* the source channel;
* the creation date;
* the update date;
* the last access;
* the access count;
* the memory state;
* a reference to the memory that replaced it;
* the content hash;
* the embedding model;
* the vector dimension;
* the embedding vector;
* the list of sources.

The available states are:

```text
active
superseded
forgotten
```

---

## 10. Embeddings and semantic search

An `EmbeddingProvider` trait was implemented, with support for:

* OpenAI-compatible `/v1/embeddings` endpoints;
* Ollama `/api/embed`;
* a lexical fallback when no embedding provider is configured.

Vectors are validated before being stored:

* dimension between 8 and 8192;
* finite values;
* a non-zero vector;
* consistent dimensions.

Cosine similarity is computed directly in Rust, without an external vector
server.

When the embedding model changes, memories can be reindexed.

Existing API keys are reused from the configuration and are never written into
the memory database.

---

## 11. Memory deduplication

Deduplication works in four stages:

1. comparing the content hash;
2. comparing a lexical key that is independent of word order;
3. computing the Jaccard score within the same category;
4. semantic comparison through cosine similarity.

The semantic thresholds are:

```text
>= 0.92      duplicate
0.82-0.92    ambiguous case
< 0.82       separate fact
```

Ambiguous cases go through a single LLM call, which must return exactly one of
the operations:

```text
ADD
MERGE
UPDATE
SUPERSEDE
IGNORE
FORGET
```

The operations are applied transactionally.

Contradictions never physically delete the old information. The previous memory
is marked `superseded` and keeps a reference to its replacement.

---

## 12. Hybrid retrieval

Relevant memories are selected through a combined score that includes:

* semantic similarity;
* lexical overlap;
* confidence;
* importance;
* age;
* access count;
* proximity to the source conversation;
* the configured maximum-age filter.

Duplicate memories are no longer injected into the prompt repeatedly.

The memory block is delimited and explicitly marked as data rather than
instructions, to reduce the risk of prompt injection.

---

## 13. Dreaming

An asynchronous worker for memory consolidation was implemented.

Dreaming can be started:

* manually;
* automatically after at least five minutes of inactivity;
* automatically if it has not run in the last 24 hours;
* in dry-run mode.

The worker uses:

* a bounded queue;
* a cross-process lease in SQLite;
* clean cancellation;
* time limits;
* limits on the number of calls;
* limits on the number of facts processed.

The process can:

* group semantically close memories;
* merge duplicates;
* detect contradictions;
* lower the confidence of old facts;
* regenerate embeddings;
* produce a consolidation report.

Every consolidated fact must keep verifiable sources. Operations that cannot
point to their sources are rejected.

---

## 14. Memory security

The memory sanitizer filters out or refuses:

* API keys;
* tokens;
* passwords;
* secrets;
* data coming directly from uploads without validation;
* instructions taken from web pages;
* content that tries to change the agent's behaviour;
* phrasing characteristic of prompt-injection attacks.

---

## 15. The memory API

The following endpoints were added:

```text
/api/memory/status
/api/memory/dream
/api/memory/reindex
/api/memory/forget
/api/memory/clear
```

The TUI includes the commands:

```text
/memory status
/memory dream
/memory dream --dry-run
/memory reindex
/memory forget ID
```

WebTool includes a memory maintenance section with:

* the current status;
* the list of facts;
* starting a dreaming cycle;
* dry-run;
* reindexing;
* deleting a single memory;
* clearing the whole memory.

The memory extractor integration in the TUI was also fixed. It previously ran
only at initialization; it now runs after every finished turn.

---

## 16. WhatsApp: dependencies and bridge startup

All dependencies required by the WhatsApp integration are included in the `.deb`
package.

Installing the bridge modules manually after installing GnomeAI-RS is no longer
necessary.

The startup process was changed so the application no longer treats the bridge as
functional immediately after launching the process.

The bridge:

* waits for the actual startup;
* checks that the process stays alive;
* reports initialization errors;
* propagates the relevant messages to the Agent and WebTool;
* no longer hides early errors.

---

## 17. WhatsApp QR code

QR code generation has been fixed.

After the bridge starts, the application waits up to **30 seconds** for the QR
code to appear.

When the QR code becomes available:

* it is detected automatically;
* it is displayed without an additional command;
* it can be scanned directly to link the WhatsApp account.

If the QR code is not generated, the interface shows the bridge's real error
instead of hanging or reporting only that the process was started.

---

## 18. Files received over WhatsApp

The bridge previously downloaded images only. It now also processes:

* `documentMessage`;
* `documentWithCaptionMessage`;
* audio;
* video;
* stickers;
* video thumbnails.

Native extraction was added for:

* DOCX;
* XLSX;
* PPTX;
* DOCM;
* XLSM;
* PPTM;
* ODT;
* ODS;
* ODP.

Office archives are processed directly in memory from Rust, without executing any
file and without an external converter.

Limits were introduced to protect against zip-bomb archives.

Around 120 text and code extensions are accepted, as well as files without an
extension, including:

```text
Makefile
Dockerfile
```

Legacy binary Office formats such as `DOC`, `XLS` and `PPT` receive an explicit
message stating that they cannot be extracted natively, instead of being
interpreted as corrupted text.

The flow was verified end to end with:

* PDF;
* DOCX;
* XLSX;
* PPTX;
* Rust files;
* text files.

---

## 19. Audio, video and stickers

The following module was added:

```text
src/transcribe.rs
```

It defines a `TranscriptionProvider` together with an OpenAI-compatible
implementation for:

```text
/v1/audio/transcriptions
```

Audio and video files can be sent to the provider for transcription.

The following are not required:

* Python;
* a bundled local model;
* a local FFmpeg installation.

Video containers are sent directly to the endpoint, which can extract the audio
track.

If transcription is not configured, the file is still stored and the conversation
explains why no transcript was produced.

The video thumbnail is stored separately as an image and can be analysed by
vision-capable models.

Stickers are processed through the image pipeline, OCR and vision.

---

## 20. OCR fix

A pre-existing problem was identified that made OCR non-functional even for
ordinary images.

Tesseract was starting OpenMP threads, and the `RLIMIT_NPROC` limit could prevent
their initialization. The error was hidden by an empty fallback.

The following variable was introduced:

```text
OMP_THREAD_LIMIT=1
```

and explicit error messages were added.

OCR was verified with images containing text such as:

```text
TEST OCR
SALUT
```

---

## 21. Conversation numbering

Two problems were fixed:

* deleting a conversation always created a new one;
* numbering used only the maximum value plus one.

Now:

* the first free number is reused;
* after a deletion an existing conversation is selected;
* the memory associated with the deleted conversation is removed;
* a reused identifier does not inherit the summary or the memory of the old
  conversation.

---

## 22. Application icons

Separate icons were added for:

* **GnomeAI Agent**;
* **GnomeAI WebTool**.

The icons are included in the Debian package and wired into the desktop files, so
the two applications can be told apart in the desktop environment's menu.

The following were updated:

* the installed icons;
* the `.desktop` files;
* the paths under `/usr/share`;
* the package metadata.

---

## 23. Codex and Firecrawl included

The distribution includes the official Codex CLI, version:

```text
0.145.0
```

Codex is also used for OpenAI authentication through device auth.

The Firecrawl source required by the local integration is included in the
package.

Firecrawl can be started on demand and used by the web search system without the
user having to clone the repository separately.

Firecrawl resources are installed together with the application and are looked up
first in the official directories under `/usr/share`.

---

## 24. The Debian package

The `.deb` package was rebuilt to include:

* the Agent and WebTool binaries;
* the WebTool resources;
* the WhatsApp bridge;
* the WhatsApp dependencies;
* the Agent and WebTool icons;
* the desktop files;
* Codex CLI 0.145.0;
* the Firecrawl source;
* the default configurations;
* the skill manager;
* the files required by the memory engine.

The applications are meant to run as a normal user, without launching the whole
agent through `sudo`.

Only the operations that genuinely require additional privileges should be
approved or run separately.

---

## 25. Checks and tests

The final version passed the following checks:

```text
242 tests passed
cargo fmt clean
release build successful
bridge.mjs syntax valid
index.html syntax valid
```

The following were tested:

* the WebTool API;
* the entire skill lifecycle;
* installing a local skill;
* installing a skill from Git;
* inspection;
* activation;
* verification;
* updating;
* removal;
* skill integration in the Agent;
* integration with WebTool;
* activation from WhatsApp;
* memory and deduplication;
* dreaming and dry-run;
* embedding reindexing;
* WhatsApp uploads;
* bridge startup;
* waiting for and displaying the QR code;
* OpenAI authentication;
* Claude detection;
* loading resources from `/usr/share`.

These fixes produced version:

```text
GnomeAI-RS 1.0
```

---

## Summary

GnomeAI-RS 1.0 brings:

* native Rust skills compatible with `SKILL.md`;
* skill installation from local folders and from Git;
* a skill manager in the Agent, WebTool and WhatsApp;
* the `normal` and `full-access` modes;
* SQLite memory with embeddings, deduplication and dreaming;
* improved providers and banner;
* fixed scrolling and navigation;
* extended support for documents, audio, video and stickers;
* working OCR;
* official OpenAI authentication through Codex;
* improved Claude detection;
* the WhatsApp bridge fully included in the `.deb`;
* automatic QR code display;
* separate icons for the Agent and WebTool;
* official Codex 0.145.0 and the Firecrawl source included;
* 242 passing tests and the full skill lifecycle verified.
