GnomeAI-RS 2.0.0 — native graphical package

Launch from the desktop menu or with:

    gnomeai-rs /path/to/workspace

The compatibility names `gnomef-rs` and `gnomef-agent` open the same GUI.
The application provides streaming chat, tool/patch output, command approvals,
sessions, providers, models, workspaces, sandbox policies, Web Search, skills,
memory, diagnostics, Markdown export and native WhatsApp pairing. WhatsApp
uses a private authenticated loopback helper with no HTML page.

Never start GnomeAI-RS with sudo. Install-time elevation does not change the
per-user runtime at `${XDG_STATE_HOME:-$HOME/.local/state}/gnomeai-rs`.

Optional provider integrations:

- OpenAI account login uses the bundled official Codex sidecar;
- Anthropic account login uses the official Claude Code executable;
- graphical desktop navigation prefers AT-SPI semantic controls through the
  packaged `gnomeai-desktop` helper, so most clicks and text entry need no
  screenshot or coordinates and survive window resizing; xdotool plus visual
  capture remains the fallback for inaccessible or canvas-based interfaces;
- API providers store keys in protected owner-only settings.

Firecrawl remains available on demand through `gnomeai-firecrawl`; Podman is a
recommended dependency only for the local packaged deployment.
