# GnomeAI-RS 0.1.0-8 — complete description of the changes

Version **0.1.0-8** is a major update to GnomeAI-RS. It extends the agent with a
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
GnomeAI-RS 0.1.0-8
```

---

## Summary

GnomeAI-RS 0.1.0-8 brings:

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
