# Desktop recovery improvements

Based on `adrgu372/GnomeAI-RS` commit
`2d9132f358f37a4ad12f3640f8e1837a04981f45`.

This is a complete source snapshot. The package version is 2.4.0.
No dependencies were added to the application.

## What to try

1. Write a message with Romanian diacritics and attach a file. Switch to
   another conversation and back: the text, attachment and caret return.
2. Close the application with an unsent draft, reopen it and resume that saved
   conversation. The draft returns. Files are referenced by their original
   paths, so an attachment must still exist when it is sent.
3. While a response runs, press Enter to queue two messages. Press Stop: the
   queue stays paused. Use Resume queue to continue or Clear queue to remove
   pending messages. Queues recovered after reopening are always paused.
4. With the WhatsApp service unavailable, try Restart bridge or New QR code.
   The settings dialog reports the failure and remains usable. When the
   service is available, its QR/status refresh automatically every two seconds
   while the dialog is open. Closing the dialog stops this polling.
5. If the Rust core disconnects, the window stays open with an error. Use
   Settings → Export Markdown or select and copy transcript text, then restart
   GnomeAI to reconnect.
6. Try sending an attachment after moving the original file. The editor keeps
   the message and explains why the attachment cannot be sent.
7. Choose the current workspace again. The core confirms this no-op navigation
   so the composer becomes available again.

## Implementation notes

Drafts are saved after a 750 ms editing pause, on conversation navigation and
on normal window close, below the existing application-data directory in
`gnomeai-rs/drafts`. Unix files are owner-only. Each snapshot is written to a
unique sibling file and atomically replaces the previous snapshot. An abrupt
process termination can lose edits made since the most recent save.

Queued items are removed after their operation is written to the Rust pipe.
The existing protocol has no durable delivery acknowledgement; this change
does not claim exactly-once message delivery if the core crashes at that point.
The transcript remains the place to check whether a turn was actually started.

The UI checks now include all `MainWindow*.cs` partial files. The Rust core also
emits its existing Ready event when a workspace request resolves to the current
workspace. The provider protocol, Cargo manifests and packaging interfaces are
unchanged, so the existing build instructions and package builder apply.

## Validation for this snapshot

- The existing `python3 scripts/check-avalonia-ui.py` check passed: 32 XAML
  handlers, 29 slash commands and 39 core operations.
- All 10 C# source files parsed without syntax errors using a C# grammar.
- The modified Rust entry point also parsed without syntax errors.
- `git diff --check` passed.
- No compilation, package build, runtime tests or desktop launch were run,
  as requested. Type checking and the manual scenarios above remain for the
  local build. A syntax parse does not replace a .NET build.
