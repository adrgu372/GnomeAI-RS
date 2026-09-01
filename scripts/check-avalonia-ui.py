#!/usr/bin/env python3
"""Fast source-level contract checks for the Avalonia frontend.

The .NET build remains authoritative.  This check catches incomplete UI
migrations before package builds: missing XAML handlers, missing legacy slash
commands, accidentally untranslated labels, and lost core operations.
"""

from __future__ import annotations

import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
UI = ROOT / "ui" / "GnomeAI.UI"
XAML = UI / "MainWindow.axaml"
CODE = UI / "MainWindow.axaml.cs"


def fail(message: str) -> None:
    print(f"Avalonia UI contract failed: {message}", file=sys.stderr)
    raise SystemExit(1)


def main() -> None:
    try:
        ET.parse(XAML)
        ET.parse(UI / "App.axaml")
    except ET.ParseError as error:
        fail(f"invalid XAML/XML: {error}")

    xaml = XAML.read_text(encoding="utf-8")
    code = CODE.read_text(encoding="utf-8")
    models = (UI / "Models.cs").read_text(encoding="utf-8")
    markdown = (UI / "MarkdownView.cs").read_text(encoding="utf-8")
    bridge = (UI / "AgentBridge.cs").read_text(encoding="utf-8")
    recovery = (UI / "WindowGeometryRecovery.cs").read_text(encoding="utf-8")
    preferences = (UI / "UiPreferences.cs").read_text(encoding="utf-8")
    app_xaml = (UI / "App.axaml").read_text(encoding="utf-8")
    protocol = (ROOT / "src" / "protocol.rs").read_text(encoding="utf-8")
    agent = (ROOT / "src" / "agent.rs").read_text(encoding="utf-8")
    desktop_core = (ROOT / "src" / "bin" / "gnomef-agent.rs").read_text(encoding="utf-8")
    background_core = (ROOT / "src" / "main.rs").read_text(encoding="utf-8")
    native_service = (ROOT / "src" / "native_service.rs").read_text(encoding="utf-8")
    deb_service = (ROOT / "packaging" / "debian" / "gnomeai-whatsapp.service").read_text(encoding="utf-8")
    deb_launcher = (ROOT / "packaging" / "debian" / "gnomef-rs").read_text(encoding="utf-8")
    deb_postinst = (ROOT / "packaging" / "debian" / "postinst").read_text(encoding="utf-8")
    deb_desktop = (ROOT / "packaging" / "debian" / "gnomeai-rs-agent.desktop").read_text(encoding="utf-8")
    deb_build = (ROOT / "scripts" / "build-deb.sh").read_text(encoding="utf-8")
    combined = "\n".join((
        xaml, code, models, markdown, bridge, recovery, preferences, app_xaml,
        protocol, agent, desktop_core, background_core, native_service, deb_service,
        deb_launcher, deb_postinst, deb_desktop, deb_build,
    ))

    handlers = set(
        re.findall(
            r'(?:Click|TextChanged|KeyDown|PointerPressed)="([A-Za-z_][A-Za-z0-9_]*)"',
            xaml,
        )
    )
    for handler in sorted(handlers):
        if not re.search(rf"\b{re.escape(handler)}\s*\(", code):
            fail(f"XAML handler {handler} has no code-behind method")

    required_commands = {
        "/help", "/new", "/sessions", "/resume", "/fork", "/compact",
        "/rollback", "/workspace", "/cd", "/provider", "/model",
        "/websearch", "/whatsapp", "/nodes", "/sandbox", "/skills",
        "/skill", "/memory", "/copy", "/theme", "/contrast", "/notify", "/mouse",
        "/tokens", "/doctor", "/diff", "/export", "/clear", "/quit",
    }
    commands = set(re.findall(r'Command\s*=\s*"(/[a-z]+)"', code))
    missing_commands = required_commands - commands
    if missing_commands:
        fail(f"missing slash commands: {', '.join(sorted(missing_commands))}")

    required_ops = {
        "submit", "submit_attachment", "new_session", "set_workspace",
        "interrupt", "approve", "provide_privilege_credential", "compact",
        "rollback", "show_diff", "set_model", "set_provider",
        "login_provider", "set_web_search", "set_mcp_servers", "set_sandbox",
        "set_whatsapp", "set_node_hub", "list_sessions", "resume_session",
        "fork_session", "rename_session", "delete_session", "memory_show",
        "memory_status", "memory_clear", "memory_set", "memory_dream",
        "memory_reindex", "memory_forget", "skills_list", "skill_inspect",
        "skill_activate", "skill_install", "skill_update", "skill_verify",
        "skill_remove", "doctor", "shutdown",
    }
    operations = set(re.findall(r'\["op"\]\s*=\s*"([a-z_]+)"', code))
    operations.update(re.findall(r'=>\s*"((?:skill|memory)_[a-z_]+)"', code))
    missing_ops = required_ops - operations
    if missing_ops:
        fail(f"missing core operations: {', '.join(sorted(missing_ops))}")

    required_surfaces = {
        "SlashPopup", "ActivityPane", "ChangedFiles", "ActivityItems",
        "ShowProviderAsync", "ShowModelAsync", "ShowSessionsAsync",
        "ShowSettingsAsync", "ShowWhatsAppConversationsAsync",
        "ShowWhatsAppSettingsAsync", "ShowNodesAsync", "CreateMcpCard",
        "MarkdownView", "CreateQrBitmap", "ExportConversationAsync",
        "WindowGeometryRecovery", "ResumeGap", "RecoveryRetry",
        "ThemeDictionaries", "UiPreferences", "sessionClose", "DetailsLabel",
        "Padding=\"24,18,24,44\"", "TranscriptBottomAnchor", "Height=\"72\"",
        "HorizontalContentAlignment=\"Stretch\"", "RoutingStrategies.Tunnel",
        "Classes.Add(\"danger\")", "TranscriptContent.SizeChanged",
        "_streamFlushTimer", "_scrollSettleTimer", "QueueTranscriptScroll",
        "ConcurrentQueue<JsonElement>", "QueueRebuild", "maxEventsPerPass",
        "SessionEvent", "HandleSessionEventAsync", "SessionRuntime",
        "FuturesUnordered<TurnFuture>", "HashMap<String, ActiveTurn>",
        "can_run_during_turns", "retired_mcp_runtimes", "Working… ·",
        "TranscriptScroll_PointerWheelChanged", "IsScrollChainingEnabled=\"False\"",
        "BringIntoViewOnFocusChange=\"False\"", "SelectableTextBlock",
        "AppendTokenDelta", "AppendReasoningDelta", "FinishStreamingSegment",
        "native-service.token", "GNOMEF_PERSISTENT_SERVICE",
        "gnomeai-whatsapp.service", "systemctl --global enable",
        "systemctl --user daemon-reload",
        "SignalKind::terminate", "wait_for_shutdown_signal",
    }
    for surface in sorted(required_surfaces):
        if surface not in combined:
            fail(f"required surface is missing: {surface}")

    forbidden_ui_words = {
        "Conversație", "Conversații", "Setări", "Alege proiectul", "Caută",
        "Trimite", "Elimină", "Atașament", "Memorie", "Eroare", "Anulează",
        "Permite", "Refuză", "Se conectează", "Nucleu activ",
    }
    found = sorted(word for word in forbidden_ui_words if word in combined)
    if found:
        fail(f"untranslated UI text remains: {', '.join(found)}")

    if "DispatcherTimer.RunOnce(SettleAtBottom" in code:
        fail("per-token transcript timers can accumulate on the UI thread")
    if "TranscriptBottomAnchor.BringIntoView" in code:
        fail("transcript scrolling must not route nested bring-into-view requests")
    if "var box = new TextBox" in markdown or "var codeBox = new TextBox" in markdown:
        fail("Markdown transcript text must not create nested TextBox scrollers")
    if "AppendTokenDelta(buffered.Text);" in code or "AppendReasoningDelta(buffered.Text);" in code:
        fail("buffered token builders must be converted to strings during replay")
    if "e.Data.GetFiles()" in code:
        fail("drag-and-drop must use the current DataTransfer API")
    if "private IBrush BorderBrush" in markdown:
        fail("Markdown brushes must not hide TemplatedControl.BorderBrush")
    if "public event EventHandler? CanExecuteChanged;" in models:
        fail("the always-enabled command event must use explicit accessors")
    if "card.Actions.Clear();" not in code or "card.Status = completedStatus;" not in code:
        fail("approval buttons must disappear and show the selected decision")
    for label in ("Allow once", "Always allow", "Deny"):
        if f'AddApprovalAction(card, "{label}"' not in code:
            fail(f"approval action is not routed through visible feedback: {label}")
    if "ScreenFromWindow" in recovery:
        fail("idle recovery must not enumerate monitors on the UI thread")
    if "let mut active: Option<ActiveTurn>" in desktop_core:
        fail("desktop core regressed to one global active turn")
    if "payload: Box<Event>" not in protocol or "SessionEventSender" not in agent:
        fail("turn events are no longer routed by session")
    tool_case = code.find('case "tool_call_started":')
    tool_boundary = code.find("FinishStreamingSegment();", tool_case)
    if tool_case < 0 or tool_boundary < tool_case or tool_boundary > tool_case + 180:
        fail("tool steps no longer close the preceding assistant bubble")
    if '"resumed session ' in desktop_core or '"started a new session"' in desktop_core:
        fail("session navigation must not inject status cards into the transcript")
    if "native_service::load_or_create_token(&app_dir)?" in background_core:
        fail("background service must not borrow app_dir after AppPaths takes ownership")
    if "native_service::load_or_create_token(paths.app_dir.as_path())?" not in background_core:
        fail("background service token must use the path retained by AppPaths")

    print(
        f"Avalonia UI contract OK: {len(handlers)} handlers, "
        f"{len(commands)} slash commands, {len(operations)} core operations"
    )


if __name__ == "__main__":
    main()
