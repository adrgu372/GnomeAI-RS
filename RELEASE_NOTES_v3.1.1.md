# GnomeAI-RS 3.1.1 — Numeric reasoning effort, new icon, transport & battery fixes

Version 3.1.1 is a maintenance release on top of the 3.1 photos/mobile
release. The Rust core moves to **3.1.1**, the Debian package to **3.1-2**
and the Android app stays on display version **3.1.1** with versionCode **18**.

## Numeric reasoning effort (DeepSeek-style 1–100 scale)

- The reasoning-effort setting accepts a number in addition to the named
  tiers, following the effort control described in the DeepSeek-V4.1-Flash
  technical report: one `u8` on a 1–100 scale with the public tiers as
  anchors (`low`=50, `medium`=25, `high`=75, `xhigh`=90, `max`=100; values
  below 5 are clamped to 5 and unparsable values fall back to `default`).
- Native structured fields (`reasoning_effort` for OpenAI,
  `output_config.effort` for Anthropic, `--effort` for the Claude CLI,
  `model_reasoning_effort` for the Codex app server) only receive tier names.
- Numeric values travel through a universal system-prompt line
  (`Reasoning effort: N (range 1-100; higher values request more thorough
  reasoning)`) so providers without a structured field (Ollama, vLLM,
  llama.cpp) are conditioned the same way.
- Set it with `/effort 42`, `/effort max`, the desktop Settings dialog or the
  `reasoning_effort` / `subagent_reasoning_effort` config keys.

## New application icon

- New neon G / mesh / speech-bubble icon shipped as hicolor PNGs (16–256) and
  a rebuilt SVG for the Debian package, mipmaps plus an Avalonia window asset
  for the desktop app, and the Android launcher icon
  (`android:icon="@mipmap/icon"` in the manifest).

## Android transport stability

- `NetworkObserver` debounces callback storms and only reconnects on real
  connectivity loss instead of tearing down healthy mesh links.
- Mesh status polling skips the ready-wait when healthy and polls at 3 s
  otherwise, so recovery after an outage is quick without busy-polling.

## Battery life

- Unified 15 s heartbeat across online and pairing states (was 30 s online,
  5 s pairing), snapshot refresh skips identical epochs, workspace autosync
  runs at 5 minutes and the Tor circuit budget drops from 32 to 8.
- `DozeMonitor` parks peer links while the device is idle and resumes them on
  wake, so background devices stop burning battery in Doze.
- `gnomeai_destroy` wakes the event reader blocked in `recv()` before
  unwrapping the handle; previously every destroyed handle leaked a live
  two-thread Tokio runtime and the drain grew with each activity recreation.

## Assets

- `gnomeai-rs_3.1-2_amd64.deb` — Debian 13+ (amd64) desktop package.
- `io.github.adrgu372.gnomeai-Signed.apk` — Android ARM64 (API 26+),
  versionCode 18.
