using System.Collections.ObjectModel;
using System.Diagnostics;
using System.Net.Http.Json;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Templates;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Platform.Storage;
using Avalonia.Styling;
using Avalonia.Threading;
using QRCoder;

namespace GnomeAI.UI;

public sealed partial class MainWindow : Window
{
    private IBrush ThemeBrush(string light, string dark) => Brush.Parse(UseDarkPalette ? dark : light);
    private bool UseDarkPalette => _themeMode == "dark"
        || (_themeMode == "system" && ActualThemeVariant == ThemeVariant.Dark);
    private IBrush UserBrush => ThemeBrush("#E8F3FF", "#123A56");
    private IBrush AssistantBrush => ThemeBrush("#FFFFFF", "#2B2B2B");
    private IBrush ToolBrush => ThemeBrush("#F5F7FA", "#252525");
    private IBrush NoticeBrush => ThemeBrush("#EEF5FB", "#183549");
    private IBrush ErrorBrush => ThemeBrush("#FDECEE", "#44272B");
    private IBrush ReasoningBrush => ThemeBrush("#F6F4FB", "#302B3A");
    private IBrush SuccessBrush => ThemeBrush("#137333", "#6CCB8E");
    private IBrush PendingBrush => ThemeBrush("#A05A00", "#FFB65C");
    private IBrush FailureBrush => ThemeBrush("#B3261E", "#FF8A80");

    private static readonly SlashCommandEntry[] Commands =
    [
        new() { Command = "/help", Description = "Show commands and keyboard shortcuts" },
        new() { Command = "/new", Description = "Start a fresh session" },
        new() { Command = "/sessions", Description = "Open saved sessions" },
        new() { Command = "/resume", Description = "Resume a session by ID" },
        new() { Command = "/fork", Description = "Branch the current session" },
        new() { Command = "/compact", Description = "Compact context now" },
        new() { Command = "/rollback", Description = "Undo patches in this session" },
        new() { Command = "/workspace", Description = "Choose a project folder" },
        new() { Command = "/cd", Description = "Alias for /workspace" },
        new() { Command = "/provider", Description = "Choose a provider or account login" },
        new() { Command = "/model", Description = "Choose the active model" },
        new() { Command = "/websearch", Description = "Toggle web search" },
        new() { Command = "/whatsapp", Description = "Open separate WhatsApp conversations" },
        new() { Command = "/nodes", Description = "Manage paired lightweight devices" },
        new() { Command = "/sandbox", Description = "Set read-only, normal, or full-access" },
        new() { Command = "/skills", Description = "List installed skills" },
        new() { Command = "/skill", Description = "Use, inspect, install, update, verify, or remove a skill" },
        new() { Command = "/memory", Description = "Show or manage shared memory" },
        new() { Command = "/copy", Description = "Copy the last assistant reply" },
        new() { Command = "/theme", Description = "Use the system, light, or dark theme" },
        new() { Command = "/contrast", Description = "Legacy alias for toggling light and dark" },
        new() { Command = "/notify", Description = "Toggle desktop notifications" },
        new() { Command = "/mouse", Description = "Show mouse-input information" },
        new() { Command = "/tokens", Description = "Show session token use" },
        new() { Command = "/doctor", Description = "Run diagnostics" },
        new() { Command = "/diff", Description = "Show the accumulated diff" },
        new() { Command = "/export", Description = "Export the transcript to Markdown" },
        new() { Command = "/clear", Description = "Clear only the visible transcript" },
        new() { Command = "/quit", Description = "Close GnomeAI-RS" },
    ];

    private readonly AgentBridge _bridge = new();
    private readonly HttpClient _http = new() { Timeout = TimeSpan.FromSeconds(12) };
    private readonly WindowGeometryRecovery _windowRecovery;
    private readonly List<ProviderInfo> _providers = [];
    private readonly List<SessionItem> _sessionRows = [];
    private readonly List<MessageItem> _allMessages = [];
    private readonly List<string> _recentWorkspaces = [];
    private readonly List<string> _models = [];
    private readonly List<McpServerEntry> _mcpServers = [];
    private readonly List<AttachedFile> _attachments = [];
    private readonly List<string> _commandHistory = [];
    private readonly List<(long Input, long Output, ulong Duration)> _tokenHistory = [];
    private readonly Dictionary<string, SessionRuntime> _sessionRuntimes = [];
    private readonly Dictionary<string, MessageItem> _toolMessages = [];
    private readonly StringBuilder _streamingBuffer = new();
    private readonly StringBuilder _reasoningBuffer = new();
    private readonly DispatcherTimer _streamFlushTimer = new() { Interval = TimeSpan.FromMilliseconds(50) };
    private readonly DispatcherTimer _scrollSettleTimer = new() { Interval = TimeSpan.FromMilliseconds(120) };

    private MessageItem? _streamingMessage;
    private MessageItem? _reasoningMessage;
    private string _currentSessionId = "";
    private string _workspace = "";
    private string? _gitBranch;
    private string _providerName = "";
    private string _model = "";
    private string _sandbox = "normal";
    private string _transcriptSearch = "";
    private bool _webSearchEnabled;
    private bool _busy;
    private bool _sessionTransitioning;
    private bool _notifications = true;
    private string _themeMode = "system";
    private long _totalInputTokens;
    private long _totalOutputTokens;
    private bool _followTranscriptLayout;
    private bool _scrollPostQueued;
    private int? _historyPosition;
    private WhatsAppConfig? _whatsapp;
    private JsonElement? _whatsappStatus;

    public ObservableCollection<MessageItem> Messages { get; } = [];
    public ObservableCollection<SessionItem> Sessions { get; } = [];
    public ObservableCollection<SlashCommandEntry> SlashSuggestions { get; } = [];
    public ObservableCollection<string> ChangedFiles { get; } = [];
    public ObservableCollection<ActivityItem> ActivityItems { get; } = [];

    public MainWindow()
    {
        InitializeComponent();
        DataContext = this;
        _themeMode = UiPreferences.LoadThemeMode();
        ApplyTheme(_themeMode, persist: false);
        ActualThemeVariantChanged += (_, _) =>
        {
            if (_themeMode == "system") RefreshThemePalette();
        };
        _windowRecovery = new WindowGeometryRecovery(this);
        DragDrop.SetAllowDrop(this, true);
        AddHandler(DragDrop.DropEvent, Files_Drop);
        Composer.AddHandler(InputElement.KeyDownEvent, Composer_KeyDown, RoutingStrategies.Tunnel);
        TranscriptScroll.AddHandler(
            InputElement.PointerWheelChangedEvent,
            TranscriptScroll_PointerWheelChanged,
            RoutingStrategies.Tunnel);
        TranscriptContent.SizeChanged += TranscriptContent_SizeChanged;
        _streamFlushTimer.Tick += StreamFlushTimer_Tick;
        _scrollSettleTimer.Tick += ScrollSettleTimer_Tick;

        if (Environment.GetCommandLineArgs().Contains("--ipc"))
        {
            _bridge.EventReceived += OnBridgeEvent;
            _bridge.Disconnected += OnBridgeDisconnected;
            _bridge.Start();
        }
        else
        {
            ShowNotice("The UI was started without --ipc, so no Rust core is connected.");
        }

        Opened += (_, _) => Composer.Focus();
    }

    protected override void OnClosed(EventArgs e)
    {
        _streamFlushTimer.Stop();
        _scrollSettleTimer.Stop();
        _ = _bridge.DisposeAsync();
        _windowRecovery.Dispose();
        _http.Dispose();
        base.OnClosed(e);
    }

    private Task OnBridgeEvent(JsonElement node) => RunUiAsync(() => HandleEventAsync(node));

    private async void OnBridgeDisconnected(string reason) => await RunUiAsync(async () =>
    {
        ConnectionText.Text = "Disconnected from the core";
        ConnectionText.Foreground = FailureBrush;
        ShowNotice(reason);
        await Task.Delay(250);
        Close();
    });

    private async Task RunUiAsync(Func<Task> action)
    {
        try { await action(); }
        catch (Exception error) { ShowError($"UI error: {error.Message}"); }
    }

    private async Task HandleEventAsync(JsonElement node)
    {
        var kind = node.GetProperty("event").GetString();
        if (kind == "session_event")
        {
            await HandleSessionEventAsync(node);
            return;
        }
        switch (kind)
        {
            case "ui_config":
                VersionText.Text = "v" + node.GetProperty("version").GetString();
                _providers.Clear();
                foreach (var provider in node.GetProperty("providers").EnumerateArray())
                    _providers.Add(new ProviderInfo
                    {
                        Id = String(provider, "id"),
                        Name = String(provider, "name"),
                        Auth = String(provider, "auth"),
                        BaseUrl = String(provider, "base_url"),
                        DefaultModel = String(provider, "default_model"),
                        Description = String(provider, "description"),
                    });
                if (node.TryGetProperty("whatsapp", out var whatsapp) && whatsapp.ValueKind == JsonValueKind.Object)
                    _whatsapp = Deserialize<WhatsAppConfig>(whatsapp);
                break;

            case "ready":
                _currentSessionId = String(node, "session_id");
                _sessionTransitioning = false;
                _providerName = String(node, "provider");
                _model = String(node, "model");
                _workspace = String(node, "workspace");
                _sandbox = String(node, "sandbox", "normal");
                _webSearchEnabled = Bool(node, "web_search_enabled");
                _gitBranch = node.TryGetProperty("git_branch", out var branch) && branch.ValueKind == JsonValueKind.String
                    ? branch.GetString()
                    : null;
                LoadStrings(node, "recent_workspaces", _recentWorkspaces);
                LoadStrings(node, "models", _models);
                LoadMcpServers(node, "mcp_servers");
                RefreshHeader();
                await SendAsync(new() { ["op"] = "list_sessions" });
                await ReplayLiveSessionAsync(_currentSessionId);
                break;

            case "session_reset":
                _sessionTransitioning = true;
                ResetTranscript();
                RefreshHeader();
                await SendAsync(new() { ["op"] = "list_sessions" });
                break;

            case "session_list":
                _sessionRows.Clear();
                if (node.TryGetProperty("sessions", out var sessions) && sessions.ValueKind == JsonValueKind.Array)
                    foreach (var entry in sessions.EnumerateArray())
                    {
                        var session = Deserialize<SessionSummaryEntry>(entry);
                        _sessionRows.Add(new SessionItem
                        {
                            Id = session.Id,
                            Title = session.Title ?? "Untitled conversation",
                            Project = session.Workspace,
                            Model = session.Model,
                            Turns = session.Turns,
                            UpdatedAt = session.UpdatedAt,
                            IsCurrent = session.IsCurrent,
                            IsBusy = RuntimeFor(session.Id).Busy,
                            NeedsAttention = RuntimeFor(session.Id).NeedsAttention,
                        });
                    }
                RefreshSessionFilter();
                RefreshHeader();
                break;

            case "history_replay":
                foreach (var turn in node.GetProperty("turns").EnumerateArray())
                {
                    var role = String(turn, "role", "user");
                    var text = String(turn, "text");
                    AppendMessage(role == "user" ? "user" : "assistant", role == "user" ? "You" : "GnomeAI", text,
                        role == "user" ? UserBrush : AssistantBrush,
                        role == "user" ? HorizontalAlignment.Right : HorizontalAlignment.Stretch);
                }
                ScrollDown();
                break;

            case "provider_changed":
                _providerName = String(node, "provider");
                _model = String(node, "model");
                LoadStrings(node, "models", _models);
                RefreshHeader();
                break;

            case "provider_login_device_code":
            {
                var url = String(node, "verification_url");
                var code = String(node, "user_code");
                var card = AppendMessage("notice", "Connect OpenAI Codex",
                    $"Open {url} and enter this one-time code:\n\n{code}", NoticeBrush);
                card.Actions.Add(new MessageAction { Label = "Copy code", Handler = () => CopyTextAsync(code) });
                card.Actions.Add(new MessageAction { Label = "Open browser", Handler = () => OpenUrlAsync(url) });
                ScrollDown();
                break;
            }

            case "provider_login_finished":
            {
                var success = Bool(node, "success");
                AppendMessage(success ? "notice" : "error", "Authentication", String(node, "message"),
                    success ? NoticeBrush : ErrorBrush);
                ScrollDown();
                break;
            }

            case "turn_started":
                FinishStreamingSegment();
                _busy = true;
                _followTranscriptLayout = true;
                _streamingMessage = null;
                _reasoningMessage = null;
                _streamingBuffer.Clear();
                _reasoningBuffer.Clear();
                RefreshComposer();
                RefreshConnectionState("Working…", PendingBrush);
                break;

            case "token":
                AppendTokenDelta(String(node, "text"));
                break;

            case "reasoning":
                AppendReasoningDelta(String(node, "text"));
                break;

            case "tool_call_started":
            {
                FinishStreamingSegment();
                var callId = String(node, "call_id");
                var name = String(node, "name");
                var summary = String(node, "summary");
                var message = AppendMessage("tool", name, summary, ToolBrush);
                message.Status = "running…";
                _toolMessages[callId] = message;
                AddActivity($"● {name}", summary, PendingBrush);
                ScrollDown();
                break;
            }

            case "tool_output":
            {
                var callId = String(node, "call_id");
                if (_toolMessages.TryGetValue(callId, out var tool))
                {
                    tool.Text += String(node, "chunk");
                    EnsureMessageVisible(tool);
                    ScrollDown();
                }
                break;
            }

            case "tool_call_ended":
            {
                var callId = String(node, "call_id");
                var success = Bool(node, "ok");
                var duration = Long(node, "duration_ms");
                if (_toolMessages.Remove(callId, out var tool))
                {
                    tool.Status = success ? "completed" : "failed";
                    AddActivity(success ? $"✓ {tool.Title}" : $"✕ {tool.Title}", $"{duration} ms",
                        success ? SuccessBrush : FailureBrush);
                }
                break;
            }

            case "approval_request":
                FinishStreamingSegment();
                AddApprovalCard(node);
                break;

            case "privilege_credential_request":
                FinishStreamingSegment();
                AddPrivilegeCard(node);
                break;

            case "patch_applied":
            {
                FinishStreamingSegment();
                var files = node.GetProperty("files").EnumerateArray().Select(file => file.GetString() ?? "").Where(file => file.Length > 0).ToList();
                var diff = String(node, "diff");
                if (files.Count == 0) files = ExtractDiffFiles(diff);
                foreach (var file in files)
                    if (!ChangedFiles.Contains(file)) ChangedFiles.Add(file);
                DiffPreview.Text = diff.Length == 0 ? "A patch was applied; no preview was supplied." : diff;
                ChangesTitle.Text = $"Changes ({ChangedFiles.Count})";
                AppendMessage("tool", "Patch applied", string.Join('\n', files), ToolBrush);
                AddActivity("✓ Patch applied", string.Join(", ", files), SuccessBrush);
                ScrollDown();
                break;
            }

            case "verification":
            {
                FinishStreamingSegment();
                var passed = Bool(node, "passed");
                var stage = String(node, "stage", "Verification");
                var summary = String(node, "summary");
                AppendMessage(passed ? "notice" : "error", stage, summary, passed ? NoticeBrush : ErrorBrush);
                AddActivity(passed ? $"✓ {stage}" : $"✕ {stage}", summary, passed ? SuccessBrush : FailureBrush);
                ScrollDown();
                break;
            }

            case "compacted":
                FinishStreamingSegment();
                AppendMessage("notice", "Context compacted", $"{Long(node, "freed_tokens")} tokens were freed.", NoticeBrush);
                ScrollDown();
                break;

            case "turn_completed":
            {
                FinishStreamingSegment();
                _busy = false;
                var input = Long(node, "input_tokens");
                var output = Long(node, "output_tokens");
                var duration = (ulong)Math.Max(0, Long(node, "duration_ms"));
                _totalInputTokens += input;
                _totalOutputTokens += output;
                _tokenHistory.Add((input, output, duration));
                TokenText.Text = $"{input} input · {output} output";
                RefreshComposer();
                RefreshConnectionState("Ready", SuccessBrush);
                ScrollDown();
                if (_notifications) SendDesktopNotification("GnomeAI-RS", "The response is ready.");
                await SendNextQueuedAsync();
                break;
            }

            case "interrupted":
                FinishStreamingSegment();
                _busy = false;
                RefreshComposer();
                RefreshConnectionState("Ready", SuccessBrush);
                await SendNextQueuedAsync();
                break;

            case "web_search_changed":
                _webSearchEnabled = Bool(node, "enabled");
                RefreshHeader();
                break;

            case "mcp_config_changed":
                LoadMcpServers(node, "servers");
                break;

            case "whatsapp_config_changed":
                if (_whatsapp is not null)
                {
                    _whatsapp.Enabled = Bool(node, "enabled");
                    _whatsapp.AssistantName = String(node, "assistant_name", "GnomeAI");
                    _whatsapp.HasOwnNumber = Bool(node, "has_own_number");
                    _whatsapp.AllowedJids = node.TryGetProperty("allowed_jids", out var jids)
                        ? jids.EnumerateArray().Select(jid => jid.GetString() ?? "").Where(jid => jid.Length > 0).ToList()
                        : [];
                    await PostWhatsAppAsync("/api/whatsapp/reload");
                }
                break;

            case "node_hub_config_changed":
                if (_whatsapp is not null)
                {
                    _whatsapp.NodeEnabled = Bool(node, "enabled");
                    _whatsapp.NodeBind = String(node, "bind", "0.0.0.0");
                    _whatsapp.NodePort = (int)Long(node, "port");
                }
                break;

            case "notice":
                ShowNotice(String(node, "message"));
                ScrollDown();
                break;

            case "error":
                FlushStreamingText();
                ShowError(String(node, "message"), Bool(node, "fatal"));
                if (!Bool(node, "fatal")) _busy = false;
                RefreshComposer();
                ScrollDown();
                break;
        }
    }

    private async Task HandleSessionEventAsync(JsonElement envelope)
    {
        var sessionId = String(envelope, "session_id");
        if (sessionId.Length == 0 || !envelope.TryGetProperty("payload", out var payload)) return;
        payload = payload.Clone();
        var kind = String(payload, "event");
        var runtime = RuntimeFor(sessionId);

        if (kind == "turn_started")
        {
            runtime.Busy = true;
            runtime.NeedsAttention = false;
            runtime.LiveEvents.Clear();
        }
        if (runtime.Busy) BufferLiveEvent(runtime, payload, kind);

        var isCurrent = !_sessionTransitioning && sessionId == _currentSessionId;
        if (kind is "approval_request" or "privilege_credential_request")
            runtime.NeedsAttention = !isCurrent;

        if (isCurrent) await HandleEventAsync(payload);

        var terminal = kind is "turn_completed" or "interrupted" or "error";
        if (terminal)
        {
            runtime.Busy = false;
            runtime.LiveEvents.Clear();
            runtime.NeedsAttention = !isCurrent;
            if (!isCurrent && _notifications)
                SendDesktopNotification("GnomeAI-RS", $"A background conversation {(kind == "turn_completed" ? "is ready" : "stopped")}.");
        }

        RefreshSessionRuntimeIndicators();
    }

    private SessionRuntime RuntimeFor(string sessionId)
    {
        var key = sessionId.Length == 0 ? "__pending__" : sessionId;
        if (!_sessionRuntimes.TryGetValue(key, out var runtime))
        {
            runtime = new SessionRuntime();
            _sessionRuntimes[key] = runtime;
        }
        return runtime;
    }

    private Queue<QueuedSubmission> CurrentQueue => RuntimeFor(_currentSessionId).Queue;

    private static void BufferLiveEvent(SessionRuntime runtime, JsonElement payload, string kind)
    {
        if (kind is "token" or "reasoning"
            && runtime.LiveEvents.LastOrDefault() is { } tail
            && tail.Kind == kind)
        {
            tail.Text.Append(String(payload, "text"));
            return;
        }
        runtime.LiveEvents.Add(new BufferedSessionEvent(kind, payload.Clone(),
            kind is "token" or "reasoning" ? String(payload, "text") : ""));
    }

    private async Task ReplayLiveSessionAsync(string sessionId)
    {
        var runtime = RuntimeFor(sessionId);
        runtime.NeedsAttention = false;
        _busy = false;
        if (runtime.Busy)
        {
            foreach (var buffered in runtime.LiveEvents.ToList())
            {
                if (buffered.Kind == "token")
                {
                    AppendTokenDelta(buffered.Text.ToString());
                }
                else if (buffered.Kind == "reasoning")
                {
                    AppendReasoningDelta(buffered.Text.ToString());
                }
                else
                {
                    await HandleEventAsync(buffered.Node);
                }
            }
            _busy = runtime.Busy;
        }
        RefreshComposer();
        RefreshSessionRuntimeIndicators();
        if (_busy) RefreshConnectionState("Working…", PendingBrush);
        else if (CurrentQueue.Count > 0) await SendNextQueuedAsync();
    }

    private void RefreshSessionRuntimeIndicators()
    {
        foreach (var session in _sessionRows)
        {
            var runtime = RuntimeFor(session.Id);
            session.IsBusy = runtime.Busy;
            session.NeedsAttention = runtime.NeedsAttention;
        }
    }

    private void AddApprovalCard(JsonElement node)
    {
        var callId = String(node, "call_id");
        var command = String(node, "command");
        var reason = String(node, "reason");
        var card = AppendMessage("tool", "Approval required", $"{command}\n\nReason: {reason}", ToolBrush);
        AddApprovalAction(card, "Allow once", callId, "allow", "Allowed once");
        if (Bool(node, "allow_always"))
            AddApprovalAction(card, "Always allow", callId, "always_allow", "Always allowed");
        AddApprovalAction(card, "Deny", callId, "deny", "Denied");
        ScrollDown();
    }

    private void AddApprovalAction(
        MessageItem card,
        string label,
        string callId,
        string decision,
        string completedStatus)
    {
        card.Actions.Add(new MessageAction
        {
            Label = label,
            Handler = async () =>
            {
                // Make the answer visible immediately and prevent a second,
                // conflicting decision from being sent for the same request.
                card.Actions.Clear();
                card.Status = completedStatus;
                try
                {
                    await DecideAsync(callId, decision);
                }
                catch (Exception error)
                {
                    card.Status = "Failed";
                    ShowError($"Could not send the approval decision: {error.Message}");
                }
            },
        });
    }

    private void AddPrivilegeCard(JsonElement node)
    {
        var requestId = String(node, "request_id");
        var command = String(node, "command");
        var prompt = OptionalString(node, "prompt") ?? OptionalString(node, "message") ?? "Enter the administrator credential.";
        var dynamic = Bool(node, "dynamic");
        var attempt = Long(node, "attempt");
        var card = AppendMessage("notice", "Administrator credential",
            $"{command}\n\n{(dynamic ? "Authentication step" : "Attempt")} {attempt}", NoticeBrush,
            hasInput: true, inputHint: prompt, rememberVisible: Bool(node, "keyring_available"));
        card.Actions.Add(new MessageAction { Label = "Continue", Handler = () => SendCredentialAsync(requestId, card, true) });
        card.Actions.Add(new MessageAction { Label = "Cancel", Handler = () => SendCredentialAsync(requestId, card, false) });
        ScrollDown();
    }

    private static string String(JsonElement node, string property, string fallback = "") =>
        node.TryGetProperty(property, out var value) && value.ValueKind == JsonValueKind.String
            ? value.GetString() ?? fallback
            : fallback;

    private static string? OptionalString(JsonElement node, string property) =>
        node.TryGetProperty(property, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;

    private static bool Bool(JsonElement node, string property) =>
        node.TryGetProperty(property, out var value) && value.ValueKind == JsonValueKind.True;

    private static long Long(JsonElement node, string property) =>
        node.TryGetProperty(property, out var value) && value.TryGetInt64(out var number) ? number : 0;

    private static T Deserialize<T>(JsonElement node) =>
        JsonSerializer.Deserialize<T>(node.GetRawText()) ?? throw new InvalidOperationException("Deserialization returned null.");

    private static void LoadStrings(JsonElement node, string property, List<string> target)
    {
        target.Clear();
        if (!node.TryGetProperty(property, out var values) || values.ValueKind != JsonValueKind.Array) return;
        foreach (var value in values.EnumerateArray())
            if (value.GetString() is { Length: > 0 } text) target.Add(text);
    }

    private void LoadMcpServers(JsonElement node, string property)
    {
        _mcpServers.Clear();
        if (!node.TryGetProperty(property, out var values) || values.ValueKind != JsonValueKind.Array) return;
        foreach (var value in values.EnumerateArray()) _mcpServers.Add(Deserialize<McpServerEntry>(value));
    }

    private MessageItem AppendMessage(string kind, string title, string text, IBrush brush,
        HorizontalAlignment alignment = HorizontalAlignment.Stretch, bool hasInput = false,
        string inputHint = "", bool rememberVisible = false)
    {
        var message = new MessageItem
        {
            Kind = kind,
            Title = title,
            Text = text,
            CardBrush = brush,
            Alignment = alignment,
            HasInput = hasInput,
            InputHint = inputHint,
            InputRememberVisible = rememberVisible,
            IsCollapsible = (kind is "reasoning" or "tool") && title != "Approval required",
            IsExpanded = false,
        };
        _allMessages.Add(message);
        if (MessageMatchesFilter(message)) Messages.Add(message);
        WelcomePanel.IsVisible = false;
        return message;
    }

    private void EnsureMessageVisible(MessageItem message)
    {
        if (MessageMatchesFilter(message) && !Messages.Contains(message)) Messages.Add(message);
    }

    private bool MessageMatchesFilter(MessageItem message) => string.IsNullOrWhiteSpace(_transcriptSearch)
        || message.Title.Contains(_transcriptSearch, StringComparison.OrdinalIgnoreCase)
        || message.Text.Contains(_transcriptSearch, StringComparison.OrdinalIgnoreCase);

    private void RefreshMessageFilter()
    {
        Messages.Clear();
        foreach (var message in _allMessages.Where(MessageMatchesFilter)) Messages.Add(message);
        WelcomePanel.IsVisible = _allMessages.Count == 0;
    }

    private void ShowNotice(string text)
    {
        FinishStreamingSegment();
        AppendMessage("notice", "Information", text, NoticeBrush);
    }

    private void ShowError(string text, bool fatal = false)
    {
        FinishStreamingSegment();
        AppendMessage("error", fatal ? "Fatal error" : "Error", text, ErrorBrush);
    }

    private void ResetTranscript()
    {
        _streamFlushTimer.Stop();
        _scrollSettleTimer.Stop();
        _streamingBuffer.Clear();
        _reasoningBuffer.Clear();
        _followTranscriptLayout = false;
        _scrollPostQueued = false;
        _allMessages.Clear();
        Messages.Clear();
        _busy = false;
        _attachments.Clear();
        _totalInputTokens = 0;
        _totalOutputTokens = 0;
        _tokenHistory.Clear();
        TokenText.Text = "0 input · 0 output";
        _streamingMessage = null;
        _reasoningMessage = null;
        _toolMessages.Clear();
        ChangedFiles.Clear();
        ActivityItems.Clear();
        DiffPreview.Text = "There are no changes in this session yet.";
        ChangesTitle.Text = "Changes (0)";
        WelcomePanel.IsVisible = true;
        RefreshAttachmentBar();
        RefreshComposer();
    }

    private void AddActivity(string title, string details, IBrush brush)
    {
        ActivityItems.Insert(0, new ActivityItem { Title = title, Details = details, StatusBrush = brush });
        while (ActivityItems.Count > 20) ActivityItems.RemoveAt(ActivityItems.Count - 1);
    }

    private void ScheduleStreamingFlush()
    {
        if (!_streamFlushTimer.IsEnabled) _streamFlushTimer.Start();
    }

    private void AppendTokenDelta(string text)
    {
        if (_streamingMessage is null)
        {
            FlushStreamingText();
            _reasoningMessage = null;
            _streamingMessage = AppendMessage("assistant", "GnomeAI", "", AssistantBrush);
        }
        _streamingBuffer.Append(text);
        ScheduleStreamingFlush();
    }

    private void AppendReasoningDelta(string text)
    {
        if (_reasoningMessage is null)
        {
            FlushStreamingText();
            _streamingMessage = null;
            _reasoningMessage = AppendMessage("reasoning", "Reasoning", "", ReasoningBrush);
        }
        _reasoningBuffer.Append(text);
        ScheduleStreamingFlush();
    }

    private void FinishStreamingSegment()
    {
        FlushStreamingText();
        _streamingMessage = null;
        _reasoningMessage = null;
    }

    private void StreamFlushTimer_Tick(object? sender, EventArgs e)
    {
        _streamFlushTimer.Stop();
        FlushStreamingText();
    }

    private void FlushStreamingText()
    {
        var changed = false;
        if (_streamingMessage is not null && _streamingBuffer.Length > 0)
        {
            _streamingMessage.Text += _streamingBuffer.ToString();
            _streamingBuffer.Clear();
            EnsureMessageVisible(_streamingMessage);
            changed = true;
        }
        if (_reasoningMessage is not null && _reasoningBuffer.Length > 0)
        {
            _reasoningMessage.Text += _reasoningBuffer.ToString();
            _reasoningBuffer.Clear();
            EnsureMessageVisible(_reasoningMessage);
            changed = true;
        }
        if (changed) ScrollDown();
    }

    private void ScrollDown()
    {
        _followTranscriptLayout = true;
        QueueTranscriptScroll();
        _scrollSettleTimer.Stop();
        _scrollSettleTimer.Start();
    }

    private void QueueTranscriptScroll()
    {
        if (_scrollPostQueued) return;
        _scrollPostQueued = true;
        Dispatcher.UIThread.Post(() =>
        {
            _scrollPostQueued = false;
            if (_followTranscriptLayout) FollowTranscriptBottom();
        }, DispatcherPriority.Background);
    }

    private void ScrollSettleTimer_Tick(object? sender, EventArgs e)
    {
        _scrollSettleTimer.Stop();
        if (_followTranscriptLayout) FollowTranscriptBottom();
        if (!_busy) _followTranscriptLayout = false;
    }

    private void TranscriptContent_SizeChanged(object? sender, SizeChangedEventArgs e)
    {
        if (_followTranscriptLayout) QueueTranscriptScroll();
    }

    private void FollowTranscriptBottom()
    {
        SetTranscriptOffset(double.PositiveInfinity);
    }

    private void TranscriptScroll_PointerWheelChanged(object? sender, PointerWheelEventArgs e)
    {
        e.Handled = true;
        _followTranscriptLayout = false;
        _scrollSettleTimer.Stop();

        var delta = e.Delta.Y;
        if (!double.IsFinite(delta) || Math.Abs(delta) < double.Epsilon) return;
        try
        {
            SetTranscriptOffset(TranscriptScroll.Offset.Y - delta * 48);
        }
        catch (Exception error)
        {
            Debug.WriteLine($"Ignored transcript wheel error: {error}");
        }
    }

    private void SetTranscriptOffset(double requestedY)
    {
        var extent = TranscriptScroll.Extent.Height;
        var viewport = TranscriptScroll.Viewport.Height;
        if (!double.IsFinite(extent) || !double.IsFinite(viewport)) return;

        var maximum = Math.Max(0, extent - viewport);
        var y = double.IsPositiveInfinity(requestedY)
            ? maximum
            : Math.Clamp(requestedY, 0, maximum);
        if (!double.IsFinite(y)) return;
        TranscriptScroll.Offset = new Vector(0, y);
    }

    private void RefreshHeader()
    {
        var current = _sessionRows.FirstOrDefault(session => session.IsCurrent);
        ConversationTitle.Text = current?.Title ?? (string.IsNullOrEmpty(_currentSessionId) ? "New conversation" : "Conversation");
        WorkspaceText.Text = string.IsNullOrEmpty(_workspace)
            ? "Choose a project folder to begin"
            : _workspace + (string.IsNullOrWhiteSpace(_gitBranch) ? "" : $"  ·  {_gitBranch}");
        ConnectionText.Text = $"{_providerName} · {_model}";
        ConnectionText.Foreground = SuccessBrush;
        if (!string.IsNullOrEmpty(_workspace))
            WorkspaceLabel.Text = Path.GetFileName(_workspace.TrimEnd(Path.DirectorySeparatorChar));
        ModelLabel.Text = string.IsNullOrWhiteSpace(_model) ? "Model" : _model;
        SearchButton.Content = _webSearchEnabled ? "Web search ✓" : "Web search";
        ProviderButton.Content = string.IsNullOrEmpty(_providerName) ? "Provider" : _providerName;
    }

    private void RefreshConnectionState(string text, IBrush brush)
    {
        ConnectionText.Text = text;
        ConnectionText.Foreground = brush;
    }

    private void RefreshComposer()
    {
        var queue = CurrentQueue;
        SendButton.Content = _busy ? "Stop" : "Send";
        QueueText.Text = queue.Count == 0
            ? "Enter sends · Shift+Enter inserts a new line"
            : $"{queue.Count} message{(queue.Count == 1 ? "" : "s")} queued";
    }

    private Task SendAsync(Dictionary<string, object?> operation) => _bridge.SendAsync(operation);

    private Task DecideAsync(string callId, string decision) => SendAsync(new()
    {
        ["op"] = "approve",
        ["call_id"] = callId,
        ["decision"] = decision,
    });

    private async Task SendCredentialAsync(string requestId, MessageItem card, bool submit)
    {
        await SendAsync(new()
        {
            ["op"] = "provide_privilege_credential",
            ["request_id"] = requestId,
            ["credential"] = submit && card.InputValue.Length > 0 ? card.InputValue : null,
            ["remember"] = submit && card.InputRemember,
        });
        card.InputValue = "";
        card.HasInput = false;
        card.Status = submit ? "submitted" : "cancelled";
    }

    private async Task DispatchComposerAsync()
    {
        var text = Composer.Text?.Trim() ?? "";
        var attachment = _attachments.FirstOrDefault();
        if (text.Length == 0 && attachment is null) return;
        Composer.Clear();
        _attachments.Clear();
        RefreshAttachmentBar();
        if (text.Length > 0)
        {
            _commandHistory.Add(text);
            _historyPosition = null;
        }
        // Navigation slash commands (especially /new and /resume) must remain
        // available while this conversation is working, otherwise a second
        // concurrent chat could only be opened with the sidebar buttons.
        if (attachment is null && text.StartsWith('/') && await HandleSlashCommandAsync(text)) return;
        if (_busy)
        {
            CurrentQueue.Enqueue(new QueuedSubmission(text, attachment));
            RefreshComposer();
            return;
        }
        await SubmitMessageAsync(text, attachment);
    }

    private async Task SubmitMessageAsync(string text, AttachedFile? attachment)
    {
        if (attachment is null && await HandleSlashCommandAsync(text)) return;
        if (attachment is null && WorkspacePathFromMessage(text) is { Length: > 0 } workspacePath)
        {
            await SendAsync(new() { ["op"] = "set_workspace", ["path"] = workspacePath });
            return;
        }
        var display = text;
        if (attachment is not null)
            display += (display.Length == 0 ? "" : "\n") + $"📎 {attachment.Name}";
        AppendMessage("user", "You", display, UserBrush, HorizontalAlignment.Right);
        ScrollDown();
        if (attachment is null)
            await SendAsync(new() { ["op"] = "submit", ["text"] = text });
        else
            await SendAsync(new() { ["op"] = "submit_attachment", ["text"] = text, ["path"] = attachment.Path });
    }

    private async Task SendNextQueuedAsync()
    {
        var queue = CurrentQueue;
        if (_busy || queue.Count == 0) return;
        var next = queue.Dequeue();
        RefreshComposer();
        await SubmitMessageAsync(next.Text, next.Attachment);
    }

    private async void Send_Click(object? sender, RoutedEventArgs e)
    {
        if (_busy)
        {
            await SendAsync(new() { ["op"] = "interrupt" });
            return;
        }
        await DispatchComposerAsync();
    }

    private async void Composer_KeyDown(object? sender, KeyEventArgs e)
    {
        if (e.Key == Key.Tab && SlashSuggestions.Count > 0)
        {
            e.Handled = true;
            Composer.Text = SlashSuggestions[0].Command;
            Composer.CaretIndex = Composer.Text.Length;
            return;
        }
        if (e.Key == Key.Up && string.IsNullOrEmpty(Composer.Text) && _commandHistory.Count > 0)
        {
            e.Handled = true;
            _historyPosition = _commandHistory.Count - 1;
            Composer.Text = _commandHistory[_historyPosition.Value];
            Composer.CaretIndex = Composer.Text.Length;
            return;
        }
        if (e.Key == Key.Down && _historyPosition is not null)
        {
            e.Handled = true;
            var next = _historyPosition.Value + 1;
            if (next >= _commandHistory.Count)
            {
                _historyPosition = null;
                Composer.Clear();
            }
            else
            {
                _historyPosition = next;
                Composer.Text = _commandHistory[next];
                Composer.CaretIndex = Composer.Text.Length;
            }
            return;
        }
        if (e.Key is not (Key.Enter or Key.Return) || e.KeyModifiers.HasFlag(KeyModifiers.Shift)) return;
        e.Handled = true;
        await DispatchComposerAsync();
    }

    private void Composer_TextChanged(object? sender, TextChangedEventArgs e)
    {
        SlashSuggestions.Clear();
        var prefix = Composer.Text?.TrimStart() ?? "";
        if (prefix.StartsWith('/') && !prefix.Any(char.IsWhiteSpace))
            foreach (var command in Commands.Where(command => command.Command.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)).Take(7))
                SlashSuggestions.Add(command);
        SlashPopup.IsVisible = SlashSuggestions.Count > 0;
    }

    private void SlashSuggestion_Click(object? sender, RoutedEventArgs e)
    {
        if ((sender as Button)?.Tag?.ToString() is not { Length: > 0 } command) return;
        Composer.Text = command;
        Composer.CaretIndex = command.Length;
        Composer.Focus();
    }

    private async Task<bool> HandleSlashCommandAsync(string text)
    {
        if (!text.StartsWith('/')) return false;
        switch (text)
        {
            case "/help" or "/?" or "/commands": await ShowHelpAsync(); break;
            case "/new": await SendAsync(new() { ["op"] = "new_session" }); break;
            case "/sessions": await ShowSessionsAsync(); break;
            case "/resume": ShowNotice("Use /resume ID or choose a conversation from All conversations."); break;
            case "/fork": await SendAsync(new() { ["op"] = "fork_session" }); break;
            case "/compact": await SendAsync(new() { ["op"] = "compact" }); break;
            case "/rollback": await SendAsync(new() { ["op"] = "rollback" }); break;
            case "/workspace" or "/cd": await ChooseWorkspaceAsync(); break;
            case "/provider": await ShowProviderAsync(); break;
            case "/model": await ShowModelAsync(); break;
            case "/websearch": await SendAsync(new() { ["op"] = "set_web_search", ["enabled"] = !_webSearchEnabled }); break;
            case "/whatsapp": await ShowWhatsAppConversationsAsync(); break;
            case "/nodes": await ShowNodesAsync(); break;
            case "/sandbox": ShowNotice("Use /sandbox read-only|normal|full-access or select a mode in Settings."); break;
            case "/skills": await SendAsync(new() { ["op"] = "skills_list" }); break;
            case "/skill": ShowError("Usage: /skill use|inspect|install|update|verify|remove ARG"); break;
            case "/memory": await SendAsync(new() { ["op"] = "memory_show" }); break;
            case "/copy": await CopyLastAssistantReplyAsync(); break;
            case "/theme": ToggleTheme(); break;
            case "/contrast": ToggleContrast(); break;
            case "/notify": _notifications = !_notifications; ShowNotice($"Desktop notifications are {(_notifications ? "enabled" : "disabled")}. "); break;
            case "/mouse": ShowNotice("Mouse input, selection, scrolling, context menus, and drag-and-drop are always enabled."); break;
            case "/tokens": ShowTokenUsage(); break;
            case "/doctor": await SendAsync(new() { ["op"] = "doctor" }); break;
            case "/diff": ActivityPane.IsVisible = true; await SendAsync(new() { ["op"] = "show_diff" }); break;
            case "/export": await ExportConversationAsync(); break;
            case "/clear": ResetVisibleTranscript(); break;
            case "/quit": await SendAsync(new() { ["op"] = "shutdown" }); Close(); break;
            default:
                if (text.StartsWith("/workspace ", StringComparison.Ordinal) || text.StartsWith("/cd ", StringComparison.Ordinal))
                {
                    var path = text[(text.IndexOf(' ') + 1)..].Trim().Trim('"');
                    if (path.Length == 0) ShowError("Usage: /workspace PATH");
                    else await SendAsync(new() { ["op"] = "set_workspace", ["path"] = path });
                }
                else if (text.StartsWith("/resume ", StringComparison.Ordinal))
                    await SendAsync(new() { ["op"] = "resume_session", ["id"] = text[8..].Trim() });
                else if (text.StartsWith("/model ", StringComparison.Ordinal))
                    await SendAsync(new() { ["op"] = "set_model", ["model"] = text[7..].Trim() });
                else if (text.StartsWith("/sandbox ", StringComparison.Ordinal))
                    await SendAsync(new() { ["op"] = "set_sandbox", ["mode"] = text[9..].Trim() });
                else if (text.StartsWith("/websearch ", StringComparison.Ordinal))
                    await HandleToggleCommandAsync("websearch", text[11..].Trim(), enabled => SendAsync(new() { ["op"] = "set_web_search", ["enabled"] = enabled }));
                else if (text.StartsWith("/notify ", StringComparison.Ordinal))
                    await HandleToggleCommandAsync("notify", text[8..].Trim(), enabled => { _notifications = enabled; return Task.CompletedTask; });
                else if (text.StartsWith("/theme ", StringComparison.Ordinal))
                    SetThemeFromCommand(text[7..].Trim());
                else if (text.StartsWith("/memory ", StringComparison.Ordinal))
                    await HandleMemoryCommandAsync(text[8..].Trim());
                else if (text.StartsWith("/skill ", StringComparison.Ordinal))
                    await HandleSkillCommandAsync(text[7..].Trim());
                else return false;
                break;
        }
        return true;
    }

    private async Task HandleToggleCommandAsync(string command, string value, Func<bool, Task> action)
    {
        if (value is "on" or "true" or "1") await action(true);
        else if (value is "off" or "false" or "0") await action(false);
        else ShowError($"Usage: /{command} on|off");
    }

    private async Task HandleMemoryCommandAsync(string value)
    {
        Dictionary<string, object?>? operation = value switch
        {
            "show" or "list" => new() { ["op"] = "memory_show" },
            "status" => new() { ["op"] = "memory_status" },
            "dream" => new() { ["op"] = "memory_dream", ["dry_run"] = false },
            "dream --dry-run" or "dream dry-run" => new() { ["op"] = "memory_dream", ["dry_run"] = true },
            "reindex" => new() { ["op"] = "memory_reindex" },
            "clear" or "wipe" => new() { ["op"] = "memory_clear" },
            "on" => new() { ["op"] = "memory_set", ["enabled"] = true },
            "off" => new() { ["op"] = "memory_set", ["enabled"] = false },
            _ when value.StartsWith("forget ", StringComparison.Ordinal) => new() { ["op"] = "memory_forget", ["id"] = value[7..].Trim() },
            _ => null,
        };
        if (operation is null) ShowError("Usage: /memory status|show|dream [--dry-run]|reindex|forget ID|clear|on|off");
        else await SendAsync(operation);
    }

    private async Task HandleSkillCommandAsync(string value)
    {
        var parts = value.Split(' ', 2, StringSplitOptions.RemoveEmptyEntries);
        if (parts.Length != 2)
        {
            ShowError("Usage: /skill use|inspect|install|update|verify|remove ARG");
            return;
        }
        var op = parts[0] switch
        {
            "use" or "activate" => "skill_activate",
            "show" or "inspect" => "skill_inspect",
            "install" => "skill_install",
            "update" => "skill_update",
            "verify" => "skill_verify",
            "remove" or "uninstall" => "skill_remove",
            _ => "",
        };
        if (op.Length == 0)
        {
            ShowError("Usage: /skill use|inspect|install|update|verify|remove ARG");
            return;
        }
        var key = op == "skill_install" ? "source" : "name";
        await SendAsync(new() { ["op"] = op, [key] = parts[1].Trim() });
    }

    private void ResetVisibleTranscript()
    {
        _allMessages.Clear();
        Messages.Clear();
        WelcomePanel.IsVisible = true;
        ShowNotice("The transcript was cleared; session history is preserved.");
    }

    private async Task CopyLastAssistantReplyAsync()
    {
        var text = _allMessages.LastOrDefault(message => message.Kind == "assistant" && message.Text.Length > 0)?.Text;
        if (text is null) ShowNotice("There is no assistant reply to copy yet.");
        else
        {
            await CopyTextAsync(text);
            ShowNotice("The last assistant reply was copied.");
        }
    }

    private void ShowTokenUsage()
    {
        if (_tokenHistory.Count == 0)
        {
            ShowNotice("No completed turns yet.");
            return;
        }
        var table = new StringBuilder("| Turn | Input | Output | Total | Duration |\n|---:|---:|---:|---:|---:|\n");
        for (var index = 0; index < _tokenHistory.Count; index++)
        {
            var row = _tokenHistory[index];
            table.AppendLine($"| {index + 1} | {row.Input} | {row.Output} | {row.Input + row.Output} | {row.Duration / 1000d:F1}s |");
        }
        table.Append($"\nTotal: {_totalInputTokens} input / {_totalOutputTokens} output · model {_model}");
        AppendMessage("notice", "Token usage", table.ToString(), NoticeBrush);
        ScrollDown();
    }

    private void ToggleTheme()
    {
        ApplyTheme(UseDarkPalette ? "light" : "dark");
        ShowNotice($"The {(_themeMode == "dark" ? "dark" : "light")} theme is active.");
    }

    private void ToggleContrast()
    {
        ApplyTheme(UseDarkPalette ? "light" : "dark");
        ShowNotice($"Theme switched to {_themeMode}; /contrast is kept as an alias for /theme.");
    }

    private void SetThemeFromCommand(string value)
    {
        var normalized = value.Trim().ToLowerInvariant();
        if (normalized is not ("light" or "dark" or "system"))
        {
            ShowError("Usage: /theme light|dark|system");
            return;
        }
        ApplyTheme(normalized);
        ShowNotice($"Theme set to {normalized}.");
    }

    private void ApplyTheme(string mode, bool persist = true)
    {
        _themeMode = UiPreferences.NormalizeThemeMode(mode);
        var variant = UiPreferences.ToThemeVariant(_themeMode);
        if (Application.Current is not null) Application.Current.RequestedThemeVariant = variant;
        RequestedThemeVariant = variant;
        if (persist) UiPreferences.SaveThemeMode(_themeMode);
        RefreshThemePalette();
    }

    private void RefreshThemePalette()
    {
        ThemeButton.Content = UseDarkPalette ? "☀" : "☾";
        ToolTip.SetTip(ThemeButton, UseDarkPalette ? "Switch to light theme" : "Switch to dark theme");
        foreach (var message in _allMessages) message.CardBrush = MessageBrush(message.Kind);
        if (!string.IsNullOrWhiteSpace(_providerName)) RefreshHeader();
    }

    private IBrush MessageBrush(string kind) => kind switch
    {
        "user" => UserBrush,
        "assistant" => AssistantBrush,
        "tool" => ToolBrush,
        "error" => ErrorBrush,
        "reasoning" => ReasoningBrush,
        _ => NoticeBrush,
    };

    private async Task ExportConversationAsync()
    {
        var file = await StorageProvider.SaveFilePickerAsync(new FilePickerSaveOptions
        {
            Title = "Export conversation",
            SuggestedFileName = $"gnomeai_export_{DateTime.Now:yyyyMMdd_HHmmss}.md",
            FileTypeChoices = [new FilePickerFileType("Markdown") { Patterns = ["*.md"] }],
        });
        if (file is null) return;
        await using var stream = await file.OpenWriteAsync();
        stream.SetLength(0);
        await using var writer = new StreamWriter(stream, Encoding.UTF8);
        await writer.WriteAsync(BuildExportMarkdown());
        ShowNotice($"Conversation exported to {file.Name}.");
    }

    private string BuildExportMarkdown()
    {
        var output = new StringBuilder($"# GnomeAI-RS conversation\n\n**Provider:** {_providerName}  \n**Model:** {_model}  \n**Workspace:** {_workspace}\n\n---\n\n");
        foreach (var message in _allMessages)
        {
            var heading = message.Kind switch { "user" => "User", "assistant" => "Assistant", _ => message.Title };
            output.AppendLine($"## {heading}\n\n{message.Text}\n");
        }
        return output.ToString();
    }

    private static List<string> ExtractDiffFiles(string diff)
    {
        var files = new HashSet<string>(StringComparer.Ordinal);
        foreach (var line in diff.Split('\n'))
        {
            if (!line.StartsWith("+++ ", StringComparison.Ordinal) && !line.StartsWith("--- ", StringComparison.Ordinal)) continue;
            var path = line[4..].Trim();
            if (path == "/dev/null") continue;
            if (path.StartsWith("a/", StringComparison.Ordinal) || path.StartsWith("b/", StringComparison.Ordinal)) path = path[2..];
            var tab = path.IndexOf('\t');
            if (tab >= 0) path = path[..tab];
            if (path.Length > 0) files.Add(path);
        }
        return files.OrderBy(path => path, StringComparer.Ordinal).ToList();
    }

    private static string? WorkspacePathFromMessage(string text)
    {
        var lower = text.ToLowerInvariant();
        var namesWorkspace = new[] { "workspace", "folder", "director", "proiect", "directory", "project" }
            .Any(lower.Contains);
        if (!namesWorkspace) return null;
        var requestsChange = new[] { "schimb", "mută", "muta", "seteaz", "change", "switch", "move", "set the" }
            .Any(lower.Contains);
        var statesLocation = new[]
        {
            "proiectul meu este", "proiectul meu e", "folderul meu este", "folderul meu e",
            "directorul meu este", "directorul meu e", "my project is", "my folder is",
            "my directory is", "workspace is", "workspace-ul este", "workspace-ul e",
        }.Any(lower.Contains);
        var trimmed = lower.TrimStart();
        var concise = new[] { "workspace:", "folder:", "director:", "project:" }.Any(trimmed.StartsWith);
        return requestsChange || statesLocation || concise ? ExplicitPath(text) : null;
    }

    private static string? ExplicitPath(string text)
    {
        foreach (var quote in new[] { '"', '\'' })
        {
            var start = text.IndexOf(quote);
            if (start < 0) continue;
            var end = text.IndexOf(quote, start + 1);
            if (end <= start) continue;
            var candidate = text[(start + 1)..end].Trim();
            if (LooksLikePath(candidate)) return candidate;
        }
        foreach (var part in text.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries))
        {
            var candidate = part.Trim('"', '\'', '(', ')', '[', ']', '{', '}', ',', ';', ':', '!', '?').TrimEnd('.');
            if (LooksLikePath(candidate)) return candidate;
        }
        return null;
    }

    private static bool LooksLikePath(string value) => value.StartsWith('/') || value is "~" or "." or ".."
        || value.StartsWith("~/", StringComparison.Ordinal) || value.StartsWith("./", StringComparison.Ordinal)
        || value.StartsWith("../", StringComparison.Ordinal);

    private async void Attach_Click(object? sender, RoutedEventArgs e)
    {
        var files = await StorageProvider.OpenFilePickerAsync(new FilePickerOpenOptions
        {
            Title = "Attach a file",
            AllowMultiple = false,
            FileTypeFilter =
            [
                new FilePickerFileType("Supported files")
                {
                    Patterns = ["*.png", "*.jpg", "*.jpeg", "*.webp", "*.gif", "*.pdf", "*.docx", "*.xlsx", "*.pptx", "*.txt", "*.md", "*.csv", "*.json", "*.rs", "*.cs", "*.py", "*.js", "*.ts", "*.html", "*.css"]
                },
                FilePickerFileTypes.All,
            ],
        });
        if (files.Count == 0) return;
        SetAttachment(files[0]);
    }

    private void Files_Drop(object? sender, DragEventArgs e)
    {
        var file = e.DataTransfer.TryGetFiles()?.FirstOrDefault();
        if (file is null) return;
        SetAttachment(file);
        e.Handled = true;
    }

    private void SetAttachment(IStorageItem item)
    {
        var path = item.TryGetLocalPath();
        if (string.IsNullOrWhiteSpace(path))
        {
            ShowError("The selected file does not provide a local path.");
            return;
        }
        _attachments.Clear();
        _attachments.Add(new AttachedFile(path, item.Name));
        RefreshAttachmentBar();
        Composer.Focus();
    }

    private void RefreshAttachmentBar()
    {
        AttachmentBar.IsVisible = _attachments.Count > 0;
        AttachmentText.Text = _attachments.Count == 0 ? "" : $"File · {_attachments[0].Name}";
    }

    private void RemoveAttachment_Click(object? sender, RoutedEventArgs e)
    {
        _attachments.Clear();
        RefreshAttachmentBar();
    }

    private async void NewSession_Click(object? sender, RoutedEventArgs e) => await SendAsync(new() { ["op"] = "new_session" });

    private async void ResumeSession_Click(object? sender, RoutedEventArgs e)
    {
        if ((sender as Button)?.Tag?.ToString() is { Length: > 0 } id)
            await SendAsync(new() { ["op"] = "resume_session", ["id"] = id });
    }

    private async void DeleteSession_Click(object? sender, RoutedEventArgs e)
    {
        if ((sender as Button)?.Tag?.ToString() is not { Length: > 0 } id) return;
        if (await ConfirmAsync("Delete conversation", "Delete this saved conversation?"))
            await SendAsync(new() { ["op"] = "delete_session", ["id"] = id });
    }

    private async void AllSessions_Click(object? sender, RoutedEventArgs e) => await ShowSessionsAsync();

    private async void Workspace_Click(object? sender, RoutedEventArgs e) => await ChooseWorkspaceAsync();

    private async Task ChooseWorkspaceAsync()
    {
        var folders = await StorageProvider.OpenFolderPickerAsync(new FolderPickerOpenOptions
        {
            Title = "Choose the GnomeAI project folder",
            AllowMultiple = false,
        });
        if (folders.Count == 0) return;
        var path = folders[0].TryGetLocalPath() ?? folders[0].Path.AbsolutePath;
        await SendAsync(new() { ["op"] = "set_workspace", ["path"] = path });
    }

    private async void Model_Click(object? sender, RoutedEventArgs e) => await ShowModelAsync();
    private async void Provider_Click(object? sender, RoutedEventArgs e) => await ShowProviderAsync();
    private async void WhatsApp_Click(object? sender, RoutedEventArgs e) => await ShowWhatsAppConversationsAsync();
    private async void Nodes_Click(object? sender, RoutedEventArgs e) => await ShowNodesAsync();
    private async void Settings_Click(object? sender, RoutedEventArgs e) => await ShowSettingsAsync();
    private void Theme_Click(object? sender, RoutedEventArgs e) => ToggleTheme();
    private async void Help_Click(object? sender, RoutedEventArgs e) => await ShowHelpAsync();
    private async void Search_Click(object? sender, RoutedEventArgs e) => await SendAsync(new() { ["op"] = "set_web_search", ["enabled"] = !_webSearchEnabled });

    private void Activity_Click(object? sender, RoutedEventArgs e) => ActivityPane.IsVisible = !ActivityPane.IsVisible;
    private void CloseActivity_Click(object? sender, RoutedEventArgs e) => ActivityPane.IsVisible = false;
    private async void RefreshDiff_Click(object? sender, RoutedEventArgs e) => await SendAsync(new() { ["op"] = "show_diff" });
    private async void Skills_Click(object? sender, RoutedEventArgs e) => await SendAsync(new() { ["op"] = "skills_list" });
    private async void Memory_Click(object? sender, RoutedEventArgs e) => await SendAsync(new() { ["op"] = "memory_show" });
    private async void Doctor_Click(object? sender, RoutedEventArgs e) => await SendAsync(new() { ["op"] = "doctor" });

    private void Suggestion_Click(object? sender, RoutedEventArgs e)
    {
        Composer.Text = (sender as Button)?.Tag?.ToString() ?? "";
        Composer.CaretIndex = Composer.Text.Length;
        Composer.Focus();
    }

    private async void ShowChangesSuggestion_Click(object? sender, RoutedEventArgs e)
    {
        ActivityPane.IsVisible = true;
        await SendAsync(new() { ["op"] = "show_diff" });
    }

    private async void ConfigureWhatsAppSuggestion_Click(object? sender, RoutedEventArgs e) => await ShowWhatsAppSettingsAsync();

    private void SessionSearch_TextChanged(object? sender, TextChangedEventArgs e) => RefreshSessionFilter();

    private void RefreshSessionFilter()
    {
        var query = SessionSearch.Text?.Trim() ?? "";
        Sessions.Clear();
        foreach (var session in _sessionRows
                     .Where(session => query.Length == 0
                         || session.Title.Contains(query, StringComparison.OrdinalIgnoreCase)
                         || session.Project.Contains(query, StringComparison.OrdinalIgnoreCase))
                     .OrderByDescending(session => session.UpdatedAt)
                     .Take(12))
            Sessions.Add(session);
    }

    private void TitleBar_PointerPressed(object? sender, PointerPressedEventArgs e)
    {
        if (e.GetCurrentPoint(this).Properties.IsLeftButtonPressed) BeginMoveDrag(e);
    }

    private async void Window_KeyDown(object? sender, KeyEventArgs e)
    {
        if (e.Key == Key.F1)
        {
            e.Handled = true;
            await ShowHelpAsync();
        }
        else if (e.Key == Key.L && e.KeyModifiers.HasFlag(KeyModifiers.Control))
        {
            e.Handled = true;
            Composer.Focus();
        }
        else if (e.Key == Key.F && e.KeyModifiers.HasFlag(KeyModifiers.Control))
        {
            e.Handled = true;
            await ShowSettingsAsync(focusSearch: true);
        }
        else if (e.Key == Key.OemPeriod && e.KeyModifiers.HasFlag(KeyModifiers.Control) && _busy)
        {
            e.Handled = true;
            await SendAsync(new() { ["op"] = "interrupt" });
        }
        else if (e.Key == Key.Escape && ActivityPane.IsVisible)
        {
            ActivityPane.IsVisible = false;
            e.Handled = true;
        }
    }

    private async Task ShowProviderAsync()
    {
        if (_providers.Count == 0)
        {
            ShowNotice("The provider catalog is not available yet.");
            return;
        }

        var dialog = CreateDialog("Provider", 600, 470);
        var providerBox = new ComboBox { ItemsSource = _providers, HorizontalAlignment = HorizontalAlignment.Stretch };
        providerBox.SelectedItem = _providers.FirstOrDefault(provider => provider.Name == _providerName) ?? _providers[0];
        var description = MutedText("");
        var baseLabel = new TextBlock { Text = "OpenAI-compatible base URL", FontWeight = FontWeight.Medium };
        var baseUrl = new TextBox { Watermark = "http://127.0.0.1:11434/v1" };
        var keyLabel = new TextBlock { Text = "API key", FontWeight = FontWeight.Medium };
        var key = new TextBox { PasswordChar = '•', Watermark = "Leave blank to reuse the saved key" };
        var accountNote = MutedText("The official application keeps the account session and reuses it while it remains valid.");
        var apply = AccentButton("Use provider");
        var cancel = new Button { Content = "Cancel" };

        void RefreshProviderForm()
        {
            var selected = providerBox.SelectedItem as ProviderInfo ?? _providers[0];
            description.Text = selected.Description;
            baseLabel.IsVisible = baseUrl.IsVisible = selected.Id == "custom";
            baseUrl.Text = selected.BaseUrl;
            var account = selected.Auth == "account";
            keyLabel.IsVisible = key.IsVisible = !account;
            accountNote.IsVisible = account;
            keyLabel.Text = selected.Auth == "optional_api_key" ? "API key (optional)" : "API key";
            apply.Content = account ? "Sign in and use provider" : "Use provider";
        }

        providerBox.SelectionChanged += (_, _) => RefreshProviderForm();
        cancel.Click += (_, _) => dialog.Close();
        apply.Click += async (_, _) =>
        {
            var selected = providerBox.SelectedItem as ProviderInfo ?? _providers[0];
            dialog.Close();
            if (selected.Auth == "account")
                await SendAsync(new() { ["op"] = "login_provider", ["provider_id"] = selected.Id });
            else
            {
                var operation = new Dictionary<string, object?>
                {
                    ["op"] = "set_provider",
                    ["provider_id"] = selected.Id,
                    ["api_key"] = string.IsNullOrWhiteSpace(key.Text) ? null : key.Text.Trim(),
                    ["base_url"] = selected.Id == "custom" ? baseUrl.Text?.Trim() : null,
                };
                await SendAsync(operation);
            }
        };
        RefreshProviderForm();

        dialog.Content = DialogLayout("Provider", "Choose an API provider or an official account-backed connection.",
        [
            Labeled("Provider", providerBox),
            description,
            baseLabel,
            baseUrl,
            keyLabel,
            key,
            accountNote,
            MutedText("New credentials are stored privately. A blank key reuses the existing saved credential."),
        ], apply, cancel);
        await dialog.ShowDialog(this);
    }

    private async Task ShowModelAsync()
    {
        var dialog = CreateDialog("Model", 570, 500);
        var filter = new TextBox { Watermark = "Filter models…" };
        var list = new ListBox { MinHeight = 300 };
        var use = AccentButton("Use model");
        var cancel = new Button { Content = "Cancel" };

        void Refresh()
        {
            var query = filter.Text?.Trim() ?? "";
            var rows = _models.Where(model => query.Length == 0 || model.Contains(query, StringComparison.OrdinalIgnoreCase)).ToList();
            if (!string.IsNullOrWhiteSpace(_model) && !rows.Contains(_model)) rows.Insert(0, _model);
            list.ItemsSource = rows;
            list.SelectedItem = rows.Contains(_model) ? _model : rows.FirstOrDefault();
        }

        filter.TextChanged += (_, _) => Refresh();
        async Task UseSelectedAsync()
        {
            if (list.SelectedItem is not string model || model.Length == 0) return;
            dialog.Close();
            await SendAsync(new() { ["op"] = "set_model", ["model"] = model });
        }
        cancel.Click += (_, _) => dialog.Close();
        use.Click += async (_, _) => await UseSelectedAsync();
        list.DoubleTapped += async (_, _) => await UseSelectedAsync();
        Refresh();
        dialog.Content = DialogLayout("Model", _models.Count == 0
                ? "No provider model list is available. Enter /model MODEL in the composer."
                : "Filter and select a model exposed by the active provider.",
            [filter, list], use, cancel);
        await dialog.ShowDialog(this);
    }

    private async Task ShowSessionsAsync()
    {
        await SendAsync(new() { ["op"] = "list_sessions" });
        var dialog = CreateDialog("All conversations", 760, 560);
        var list = new ListBox { ItemsSource = _sessionRows.OrderByDescending(session => session.UpdatedAt).ToList(), MinHeight = 360 };
        list.ItemTemplate = new FuncDataTemplate<SessionItem>((session, _) =>
        {
            var title = new TextBlock { Text = session.Caption, FontWeight = FontWeight.SemiBold, TextTrimming = TextTrimming.CharacterEllipsis };
            var details = MutedText(session.Details);
            details.TextTrimming = TextTrimming.CharacterEllipsis;
            return new StackPanel { Children = { title, details }, Margin = new Thickness(5) };
        });
        list.SelectedItem = _sessionRows.FirstOrDefault(session => session.IsCurrent) ?? _sessionRows.FirstOrDefault();

        var resume = AccentButton("Resume");
        var rename = new Button { Content = "Rename" };
        var delete = new Button { Content = "Delete" };
        var fork = new Button { Content = "Fork current" };
        var fresh = new Button { Content = "New conversation" };
        var close = new Button { Content = "Close" };

        resume.Click += async (_, _) =>
        {
            if (list.SelectedItem is not SessionItem session) return;
            dialog.Close();
            await SendAsync(new() { ["op"] = "resume_session", ["id"] = session.Id });
        };
        rename.Click += async (_, _) =>
        {
            if (list.SelectedItem is not SessionItem session) return;
            var title = await PromptAsync("Rename conversation", "Conversation title", session.Title);
            if (title is null) return;
            await SendAsync(new() { ["op"] = "rename_session", ["id"] = session.Id, ["title"] = title.Trim() });
            dialog.Close();
        };
        delete.Click += async (_, _) =>
        {
            if (list.SelectedItem is not SessionItem session || !await ConfirmAsync("Delete conversation", $"Delete “{session.Title}”?")) return;
            await SendAsync(new() { ["op"] = "delete_session", ["id"] = session.Id });
            dialog.Close();
        };
        fork.Click += async (_, _) => { dialog.Close(); await SendAsync(new() { ["op"] = "fork_session" }); };
        fresh.Click += async (_, _) => { dialog.Close(); await SendAsync(new() { ["op"] = "new_session" }); };
        close.Click += (_, _) => dialog.Close();

        var actions = new WrapPanel { Orientation = Orientation.Horizontal, ItemWidth = double.NaN };
        foreach (var button in new[] { fresh, fork, delete, rename, resume, close })
        {
            button.Margin = new Thickness(3);
            actions.Children.Add(button);
        }
        dialog.Content = DialogLayout("All conversations", "Resume, rename, fork, or delete persisted agent sessions.", [list, actions]);
        await dialog.ShowDialog(this);
    }

    private async Task ShowSettingsAsync(bool focusSearch = false)
    {
        var dialog = CreateDialog("Settings", 760, 720);
        var content = new StackPanel { Spacing = 9, Margin = new Thickness(22) };
        content.Children.Add(DialogHeading("Settings", "Providers, MCP servers, execution, interface, memory, and tools."));
        content.Children.Add(Section("MODEL AND CONNECTIONS"));
        var connectionActions = new WrapPanel();
        foreach (var button in new[]
        {
            ActionButton($"Provider · {_providerName}", async () => await ShowProviderAsync()),
            ActionButton($"Model · {_model}", async () => await ShowModelAsync()),
            ActionButton("WhatsApp", async () => await ShowWhatsAppSettingsAsync()),
            ActionButton("Devices", async () => await ShowNodesAsync()),
        })
        {
            button.Margin = new Thickness(3);
            connectionActions.Children.Add(button);
        }
        content.Children.Add(Surface(connectionActions));

        content.Children.Add(Section("MCP SERVERS"));
        content.Children.Add(MutedText("Generic Streamable HTTP and stdio servers. MCP calls remain approval-gated, including for delegated providers."));
        var workingServers = _mcpServers.Select(server => server.Clone()).ToList();
        var mcpHost = new StackPanel { Spacing = 7 };
        void RebuildMcp()
        {
            mcpHost.Children.Clear();
            foreach (var server in workingServers.ToList())
                mcpHost.Children.Add(CreateMcpCard(server, () => { workingServers.Remove(server); RebuildMcp(); }));
        }
        RebuildMcp();
        content.Children.Add(mcpHost);
        var mcpActions = new WrapPanel();
        var browserOs = new Button { Content = "+ BrowserOS" };
        var addMcp = new Button { Content = "+ MCP server" };
        browserOs.Click += (_, _) =>
        {
            workingServers.Add(new McpServerEntry { Name = "browseros", Url = "http://127.0.0.1:9239/mcp" });
            RebuildMcp();
        };
        addMcp.Click += (_, _) =>
        {
            workingServers.Add(new McpServerEntry { Name = $"mcp-server-{workingServers.Count + 1}" });
            RebuildMcp();
        };
        mcpActions.Children.Add(browserOs);
        mcpActions.Children.Add(addMcp);
        content.Children.Add(mcpActions);

        content.Children.Add(Section("EXECUTION"));
        var webSearch = new CheckBox { Content = "Web search", IsChecked = _webSearchEnabled };
        var sandbox = new ComboBox { ItemsSource = new[] { "read-only", "normal", "full-access" }, SelectedItem = _sandbox };
        content.Children.Add(webSearch);
        content.Children.Add(Labeled("Sandbox", sandbox));

        var hubEnabled = new CheckBox { Content = "Hub for lightweight devices", IsChecked = _whatsapp?.NodeEnabled ?? false };
        var hubBind = new TextBox { Text = _whatsapp?.NodeBind ?? "0.0.0.0", Watermark = "0.0.0.0" };
        var hubPort = new TextBox { Text = (_whatsapp?.NodePort ?? 9277).ToString(), Watermark = "9277" };
        var hubGrid = new Grid { ColumnDefinitions = new ColumnDefinitions("*,*"), ColumnSpacing = 8 };
        hubGrid.Children.Add(Labeled("Address", hubBind));
        var portField = Labeled("Port", hubPort);
        Grid.SetColumn(portField, 1);
        hubGrid.Children.Add(portField);
        content.Children.Add(Surface(new StackPanel { Spacing = 7, Children = { hubEnabled, hubGrid, MutedText("Listener changes take effect after restarting the application.") } }));

        content.Children.Add(Section("INTERFACE"));
        var notifications = new CheckBox { Content = "Desktop notifications", IsChecked = _notifications };
        var theme = new ComboBox
        {
            ItemsSource = new[] { "System", "Light", "Dark" },
            SelectedItem = char.ToUpperInvariant(_themeMode[0]) + _themeMode[1..],
        };
        var search = new TextBox { Text = _transcriptSearch, Watermark = "Search the current conversation…" };
        content.Children.Add(notifications);
        content.Children.Add(Labeled("Color theme", theme));
        content.Children.Add(Labeled("Search the current conversation", search));

        content.Children.Add(Section("TOOLS"));
        var tools = new WrapPanel();
        foreach (var button in new[]
        {
            ActionButton("Skills", () => SendAsync(new() { ["op"] = "skills_list" })),
            ActionButton("Install SKILL.md", InstallSkillAsync),
            ActionButton("Memory", () => SendAsync(new() { ["op"] = "memory_show" })),
            ActionButton("Compact context", () => SendAsync(new() { ["op"] = "compact" })),
            ActionButton("Rollback patches", () => SendAsync(new() { ["op"] = "rollback" })),
            ActionButton("Diagnostics", () => SendAsync(new() { ["op"] = "doctor" })),
            ActionButton("Tokens", () => { ShowTokenUsage(); return Task.CompletedTask; }),
            ActionButton("Export Markdown", ExportConversationAsync),
        })
        {
            button.Margin = new Thickness(3);
            tools.Children.Add(button);
        }
        content.Children.Add(tools);
        content.Children.Add(MutedText($"{_totalInputTokens} input tokens · {_totalOutputTokens} output tokens"));

        var save = AccentButton("Save and reconnect");
        var close = new Button { Content = "Cancel" };
        var footer = ButtonRow(save, close);
        content.Children.Add(footer);
        close.Click += (_, _) => dialog.Close();
        save.Click += async (_, _) =>
        {
            dialog.Close();
            _notifications = notifications.IsChecked == true;
            ApplyTheme(theme.SelectedItem?.ToString() ?? "System");
            _transcriptSearch = search.Text?.Trim() ?? "";
            RefreshMessageFilter();
            var requestedWeb = webSearch.IsChecked == true;
            if (requestedWeb != _webSearchEnabled)
                await SendAsync(new() { ["op"] = "set_web_search", ["enabled"] = requestedWeb });
            var requestedSandbox = sandbox.SelectedItem?.ToString() ?? "normal";
            if (requestedSandbox != _sandbox)
                await SendAsync(new() { ["op"] = "set_sandbox", ["mode"] = requestedSandbox });
            if (_whatsapp is not null && int.TryParse(hubPort.Text, out var port) && port is > 0 and <= 65535)
                await SendAsync(new()
                {
                    ["op"] = "set_node_hub",
                    ["enabled"] = hubEnabled.IsChecked == true,
                    ["bind"] = hubBind.Text?.Trim() ?? "0.0.0.0",
                    ["port"] = port,
                });
            await SendAsync(new() { ["op"] = "set_mcp_servers", ["servers"] = workingServers });
        };

        dialog.Content = new ScrollViewer { Content = content, VerticalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Auto };
        dialog.Opened += (_, _) => { if (focusSearch) search.Focus(); };
        await dialog.ShowDialog(this);
    }

    private Control CreateMcpCard(McpServerEntry server, Action remove)
    {
        var enabled = new CheckBox { Content = "Enabled", IsChecked = server.Enabled };
        var name = new TextBox { Text = server.Name, Watermark = "server-name" };
        var transport = new ComboBox { ItemsSource = new[] { "streamable-http", "stdio" }, SelectedItem = server.Transport };
        var url = new TextBox { Text = server.Url, Watermark = "http://127.0.0.1:9239/mcp" };
        var command = new TextBox { Text = server.Command, Watermark = "Executable, for example npx" };
        var args = new TextBox { Text = string.Join('\n', server.Args), AcceptsReturn = true, MinHeight = 58, Watermark = "One argument per line" };
        var headers = new TextBox { Text = FormatKeyValues(server.Headers), AcceptsReturn = true, MinHeight = 58, Watermark = "Header=Value" };
        var environment = new TextBox { Text = FormatKeyValues(server.Env), AcceptsReturn = true, MinHeight = 58, Watermark = "ENV_VAR=value" };
        var allowWhatsApp = new CheckBox { Content = "Allow this server in WhatsApp (off by default)", IsChecked = server.AllowWhatsApp };
        var removeButton = new Button { Content = "Remove" };
        var protocolFields = new StackPanel { Spacing = 6 };

        void SaveFields()
        {
            server.Enabled = enabled.IsChecked == true;
            server.Name = name.Text?.Trim() ?? "mcp-server";
            server.Transport = transport.SelectedItem?.ToString() ?? "streamable-http";
            server.Url = url.Text?.Trim() ?? "";
            server.Command = command.Text?.Trim() ?? "";
            server.Args = Lines(args.Text);
            server.Headers = ParseKeyValues(headers.Text);
            server.Env = ParseKeyValues(environment.Text);
            server.AllowWhatsApp = allowWhatsApp.IsChecked == true;
        }

        void RefreshProtocolFields()
        {
            protocolFields.Children.Clear();
            if ((transport.SelectedItem?.ToString() ?? "streamable-http") == "stdio")
            {
                protocolFields.Children.Add(Labeled("Command", command));
                protocolFields.Children.Add(Labeled("Arguments", args));
                protocolFields.Children.Add(Labeled("Environment", environment));
            }
            else
            {
                protocolFields.Children.Add(Labeled("URL", url));
                protocolFields.Children.Add(Labeled("Headers", headers));
            }
        }

        foreach (var textBox in new[] { name, url, command, args, headers, environment })
            textBox.TextChanged += (_, _) => SaveFields();
        enabled.IsCheckedChanged += (_, _) => SaveFields();
        allowWhatsApp.IsCheckedChanged += (_, _) => SaveFields();
        transport.SelectionChanged += (_, _) => { SaveFields(); RefreshProtocolFields(); };
        removeButton.Click += (_, _) => remove();
        RefreshProtocolFields();

        var header = new Grid { ColumnDefinitions = new ColumnDefinitions("Auto,*,Auto,Auto"), ColumnSpacing = 7 };
        header.Children.Add(enabled);
        Grid.SetColumn(name, 1); header.Children.Add(name);
        Grid.SetColumn(transport, 2); header.Children.Add(transport);
        Grid.SetColumn(removeButton, 3); header.Children.Add(removeButton);
        return Surface(new StackPanel { Spacing = 7, Children = { header, protocolFields, allowWhatsApp } });
    }

    private async Task InstallSkillAsync()
    {
        var files = await StorageProvider.OpenFilePickerAsync(new FilePickerOpenOptions
        {
            Title = "Install SKILL.md",
            AllowMultiple = false,
            FileTypeFilter = [new FilePickerFileType("Agent Skill") { Patterns = ["*.md"] }],
        });
        if (files.Count == 0) return;
        if (!files[0].Name.Equals("SKILL.md", StringComparison.Ordinal))
        {
            ShowError("Select a file named exactly SKILL.md.");
            return;
        }
        var path = files[0].TryGetLocalPath();
        var directory = path is null ? null : Path.GetDirectoryName(path);
        if (directory is null) ShowError("SKILL.md does not have a valid local parent directory.");
        else await SendAsync(new() { ["op"] = "skill_install", ["source"] = directory });
    }

    private async Task ShowHelpAsync()
    {
        var dialog = CreateDialog("Commands", 650, 650);
        var rows = new StackPanel { Spacing = 4, Margin = new Thickness(22) };
        rows.Children.Add(DialogHeading("Commands", "Slash commands are completed with Tab and can also be selected in the composer."));
        foreach (var command in Commands)
        {
            var row = new Grid { ColumnDefinitions = new ColumnDefinitions("135,*"), ColumnSpacing = 12 };
            row.Children.Add(new TextBlock { Text = command.Command, FontFamily = new FontFamily("monospace"), FontWeight = FontWeight.SemiBold });
            var description = new TextBlock { Text = command.Description, TextWrapping = TextWrapping.Wrap };
            Grid.SetColumn(description, 1); row.Children.Add(description);
            rows.Children.Add(row);
        }
        rows.Children.Add(new Border
        {
            Height = 1,
            Background = ThemeBrush("#D8DDE5", "#3C3C3C"),
            Margin = new Thickness(0, 8),
        });
        rows.Children.Add(MutedText("Enter sends · Shift+Enter inserts a newline · Ctrl+. stops · Ctrl+L focuses the composer · Ctrl+F searches · F1 opens this window"));
        var close = AccentButton("Close");
        close.Click += (_, _) => dialog.Close();
        rows.Children.Add(ButtonRow(close));
        dialog.Content = new ScrollViewer { Content = rows, VerticalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Auto };
        await dialog.ShowDialog(this);
    }

    private async Task ShowWhatsAppConversationsAsync()
    {
        if (_whatsapp is null)
        {
            ShowNotice("WhatsApp configuration is not available.");
            return;
        }
        var dialog = CreateDialog("WhatsApp conversations", 980, 680);
        var status = MutedText("Loading WhatsApp status…");
        var chats = new ObservableCollection<WhatsAppConversation>();
        var list = new ListBox { ItemsSource = chats, MinWidth = 235 };
        var transcript = new StackPanel { Spacing = 8 };
        var chatTitle = new TextBlock { Text = "WhatsApp conversation", FontSize = 19, FontWeight = FontWeight.SemiBold };
        var refresh = new Button { Content = "Refresh" };
        var settings = new Button { Content = "Connection settings" };
        var close = new Button { Content = "Close" };

        async Task LoadSelectedAsync()
        {
            if (list.SelectedItem is not WhatsAppConversation selected) return;
            transcript.Children.Clear();
            chatTitle.Text = selected.ToString();
            try
            {
                var chat = await GetWhatsAppJsonAsync($"/api/chats/{Uri.EscapeDataString(selected.Id)}");
                if (chat is null || !chat.Value.TryGetProperty("messages", out var messages) || messages.ValueKind != JsonValueKind.Array)
                {
                    transcript.Children.Add(MutedText("This WhatsApp conversation is empty."));
                    return;
                }
                foreach (var message in messages.EnumerateArray())
                {
                    var extracted = ExtractWhatsAppMessage(message);
                    if (extracted is null) continue;
                    var title = extracted.Role == "user" ? "WhatsApp user" : _whatsapp.AssistantName;
                    transcript.Children.Add(new Border
                    {
                        Classes = { "card" },
                        Background = extracted.Role == "user" ? UserBrush : AssistantBrush,
                        Child = new StackPanel
                        {
                            Spacing = 5,
                            Children =
                            {
                                new TextBlock
                                {
                                    Text = title,
                                    FontSize = 11,
                                    FontWeight = FontWeight.SemiBold,
                                    Foreground = ThemeBrush("#5D6671", "#A8A8A8"),
                                },
                                new MarkdownView { Markdown = extracted.Text },
                            },
                        },
                    });
                }
                if (transcript.Children.Count == 0) transcript.Children.Add(MutedText("This WhatsApp conversation is empty."));
            }
            catch (Exception error) { transcript.Children.Add(MutedText($"Cannot load messages: {error.Message}")); }
        }

        async Task LoadChatsAsync()
        {
            status.Text = "Updating…";
            try
            {
                var waStatus = await GetWhatsAppJsonAsync("/api/whatsapp/status");
                _whatsappStatus = waStatus;
                status.Text = waStatus is not null && Bool(waStatus.Value, "connected") ? "Connected" : "Not connected";
                var payload = await GetWhatsAppJsonAsync("/api/chats");
                chats.Clear();
                if (payload is not null && payload.Value.ValueKind == JsonValueKind.Array)
                    foreach (var chat in payload.Value.EnumerateArray()
                                 .Where(chat => String(chat, "id").StartsWith("wa_", StringComparison.Ordinal))
                                 .OrderBy(chat => String(chat, "title"), StringComparer.OrdinalIgnoreCase))
                        chats.Add(new WhatsAppConversation { Id = String(chat, "id"), Title = String(chat, "title", String(chat, "id")) });
                list.SelectedItem ??= chats.FirstOrDefault();
                if (list.SelectedItem is not null) await LoadSelectedAsync();
                else transcript.Children.Add(MutedText("No WhatsApp conversations yet. Incoming messages will appear here."));
            }
            catch (Exception error) { status.Text = $"WhatsApp is unavailable: {error.Message}"; }
        }

        list.SelectionChanged += async (_, _) => await LoadSelectedAsync();
        refresh.Click += async (_, _) => await LoadChatsAsync();
        settings.Click += async (_, _) => await ShowWhatsAppSettingsAsync();
        close.Click += (_, _) => dialog.Close();

        var header = new Grid { ColumnDefinitions = new ColumnDefinitions("*,Auto,Auto,Auto"), ColumnSpacing = 7 };
        header.Children.Add(status);
        Grid.SetColumn(refresh, 1); header.Children.Add(refresh);
        Grid.SetColumn(settings, 2); header.Children.Add(settings);
        Grid.SetColumn(close, 3); header.Children.Add(close);
        var chatPanel = new Grid { ColumnDefinitions = new ColumnDefinitions("240,*"), ColumnSpacing = 14 };
        chatPanel.Children.Add(new Border { Classes = { "surface" }, Padding = new Thickness(8), Child = list });
        var transcriptScroll = new ScrollViewer
        {
            Content = transcript,
            VerticalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Auto,
        };
        var right = new Grid { RowDefinitions = new RowDefinitions("Auto,*"), RowSpacing = 9 };
        right.Children.Add(chatTitle);
        Grid.SetRow(transcriptScroll, 1); right.Children.Add(transcriptScroll);
        Grid.SetColumn(right, 1); chatPanel.Children.Add(right);
        dialog.Content = new Grid
        {
            RowDefinitions = new RowDefinitions("Auto,*"),
            Margin = new Thickness(18),
            Children = { header, chatPanel },
        };
        Grid.SetRow(chatPanel, 1);
        dialog.Opened += async (_, _) => await LoadChatsAsync();
        await dialog.ShowDialog(this);
    }

    private static WhatsAppMessage? ExtractWhatsAppMessage(JsonElement message)
    {
        var role = String(message, "role").Trim().ToLowerInvariant();
        if (role is not ("user" or "assistant" or "gnome")) return null;
        if (!message.TryGetProperty("content", out var content)) return null;
        string text;
        if (content.ValueKind == JsonValueKind.String)
        {
            text = content.GetString()?.Trim() ?? "";
            if (text.StartsWith("[Extracted content from uploaded file:", StringComparison.Ordinal)) return null;
        }
        else if (content.ValueKind == JsonValueKind.Object)
            text = $"[{String(content, "type", "file")}] {String(content, "filename", "attachment")}";
        else text = content.ToString();
        return text.Length == 0 ? null : new WhatsAppMessage { Role = role == "user" ? "user" : "assistant", Text = text };
    }

    private async Task ShowWhatsAppSettingsAsync()
    {
        if (_whatsapp is null)
        {
            ShowNotice("WhatsApp configuration is not available.");
            return;
        }
        var dialog = CreateDialog("WhatsApp", 700, 720);
        var body = new StackPanel { Spacing = 8, Margin = new Thickness(22) };
        body.Children.Add(DialogHeading("WhatsApp", "Pair, restrict, inspect, and test the native WhatsApp bridge."));
        var status = MutedText("Loading status…");
        var enabled = new CheckBox { Content = "Enable WhatsApp integration", IsChecked = _whatsapp.Enabled };
        var assistantName = new TextBox { Text = _whatsapp.AssistantName };
        var ownNumber = new CheckBox { Content = "The assistant uses a dedicated WhatsApp number", IsChecked = _whatsapp.HasOwnNumber };
        var allowed = new TextBox
        {
            Text = string.Join('\n', _whatsapp.AllowedJids),
            AcceptsReturn = true,
            MinHeight = 82,
            Watermark = "40700000000@s.whatsapp.net — one JID per line",
        };
        var qrImage = new Image { Width = 280, Height = 280, Stretch = Stretch.Uniform, IsVisible = false, HorizontalAlignment = HorizontalAlignment.Left };
        var qrNote = MutedText("");
        var testJid = new TextBox { Watermark = "40700000000@s.whatsapp.net" };
        var testMessage = new TextBox { AcceptsReturn = true, MinHeight = 70, Watermark = "Test message" };
        var log = new TextBox { IsReadOnly = true, AcceptsReturn = true, TextWrapping = TextWrapping.NoWrap, MinHeight = 120, FontFamily = new FontFamily("monospace") };

        async Task RefreshStatusAsync()
        {
            try
            {
                var payload = await GetWhatsAppJsonAsync("/api/whatsapp/status");
                _whatsappStatus = payload;
                if (payload is null) return;
                var running = Bool(payload.Value, "bridge_running");
                var authenticated = Bool(payload.Value, "authenticated");
                var connected = Bool(payload.Value, "connected");
                var phone = String(payload.Value, "own_phone");
                status.Text = $"Service: {(running ? "running" : "stopped")} · Authenticated: {(authenticated ? "yes" : "no")} · Connected: {(connected ? "yes" : "no")}" +
                              (phone.Length == 0 ? "" : $" · Number: +{phone}");
                var ownJid = String(payload.Value, "own_jid");
                if (string.IsNullOrWhiteSpace(testJid.Text) && ownJid.Length > 0) testJid.Text = ownJid;
                var qr = String(payload.Value, "qr");
                qrImage.IsVisible = qr.Length > 0 && !connected;
                qrNote.Text = qrImage.IsVisible ? "Scan this code in WhatsApp → Linked devices." : connected ? "WhatsApp is connected." : "No pairing QR is available yet.";
                if (qrImage.IsVisible) qrImage.Source = CreateQrBitmap(qr);
                log.Text = ReadLogTail(_whatsapp.LogFile, 12 * 1024);
            }
            catch (Exception error) { status.Text = $"WhatsApp is unavailable: {error.Message}"; }
        }

        var save = AccentButton("Save and apply");
        var stop = new Button { Content = "Stop" };
        var restart = new Button { Content = "Restart bridge" };
        var newQr = new Button { Content = "New QR code" };
        var refresh = new Button { Content = "Refresh" };
        var conversations = new Button { Content = "View conversations" };
        var sendTest = new Button { Content = "Send test message" };
        var close = new Button { Content = "Close" };

        save.Click += async (_, _) =>
        {
            var jids = Lines(allowed.Text).Distinct(StringComparer.Ordinal).OrderBy(value => value, StringComparer.Ordinal).ToList();
            await SendAsync(new()
            {
                ["op"] = "set_whatsapp",
                ["enabled"] = enabled.IsChecked == true,
                ["assistant_name"] = assistantName.Text?.Trim() ?? "GnomeAI",
                ["has_own_number"] = ownNumber.IsChecked == true,
                ["allowed_jids"] = jids,
            });
            status.Text = "Saving settings…";
        };
        stop.Click += async (_, _) => await SendAsync(new()
        {
            ["op"] = "set_whatsapp", ["enabled"] = false,
            ["assistant_name"] = assistantName.Text?.Trim() ?? "GnomeAI",
            ["has_own_number"] = ownNumber.IsChecked == true,
            ["allowed_jids"] = Lines(allowed.Text),
        });
        restart.Click += async (_, _) => { await PostWhatsAppAsync("/api/whatsapp/reload"); await RefreshStatusAsync(); };
        newQr.Click += async (_, _) =>
        {
            status.Text = "Generating a new QR code…";
            await PostWhatsAppAsync("/api/whatsapp/qr/refresh");
            await Task.Delay(900);
            await RefreshStatusAsync();
        };
        refresh.Click += async (_, _) => await RefreshStatusAsync();
        conversations.Click += async (_, _) => await ShowWhatsAppConversationsAsync();
        sendTest.Click += async (_, _) =>
        {
            if (string.IsNullOrWhiteSpace(testJid.Text) || string.IsNullOrWhiteSpace(testMessage.Text))
            {
                status.Text = "Enter the recipient JID and test message.";
                return;
            }
            await SendWhatsAppTestAsync(testJid.Text.Trim(), testMessage.Text.Trim());
            status.Text = "The test message was queued.";
            testMessage.Clear();
        };
        close.Click += (_, _) => dialog.Close();

        body.Children.Add(status);
        body.Children.Add(ButtonRow(refresh, conversations));
        body.Children.Add(Section("CONNECTION"));
        body.Children.Add(enabled);
        body.Children.Add(Labeled("Assistant display name", assistantName));
        body.Children.Add(ownNumber);
        body.Children.Add(Labeled("Allowed conversations", allowed));
        body.Children.Add(MutedText("When the list is empty, only messages to the account itself are accepted."));
        body.Children.Add(ButtonRow(save, stop, restart, newQr));
        body.Children.Add(qrNote);
        body.Children.Add(qrImage);
        body.Children.Add(Section("SEND A TEST MESSAGE"));
        body.Children.Add(Labeled("Recipient JID", testJid));
        body.Children.Add(Labeled("Message", testMessage));
        body.Children.Add(sendTest);
        body.Children.Add(Section("RECENT LOG"));
        body.Children.Add(MutedText(_whatsapp.LogFile));
        body.Children.Add(log);
        body.Children.Add(ButtonRow(close));
        dialog.Content = new ScrollViewer { Content = body, VerticalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Auto };
        dialog.Opened += async (_, _) => await RefreshStatusAsync();
        await dialog.ShowDialog(this);
    }

    private async Task ShowNodesAsync()
    {
        if (_whatsapp is null)
        {
            ShowNotice("Node Hub configuration is not available.");
            return;
        }
        var dialog = CreateDialog("Devices", 760, 680);
        var body = new StackPanel { Spacing = 9, Margin = new Thickness(22) };
        body.Children.Add(DialogHeading("Devices", "Pair lightweight Linux nodes and control local root permission per device."));
        var status = MutedText(_whatsapp.NodeEnabled
            ? $"Hub · {_whatsapp.NodeBind}:{_whatsapp.NodePort}"
            : "The Hub is disabled. Enable it in Settings and restart the application.");
        var normalCommand = $"gnomeai-node enroll --server http://PC-IP:{_whatsapp.NodePort} --token {_whatsapp.NodeEnrollmentToken} --name NAME";
        var rootCommand = normalCommand + " --allow-root";
        var refresh = new Button { Content = "Refresh" };
        var copyNormal = new Button { Content = "Copy normal command" };
        var copyRoot = new Button { Content = "Copy with local root" };
        var close = new Button { Content = "Close" };
        var nodesHost = new StackPanel { Spacing = 8 };

        async Task RefreshNodesAsync()
        {
            nodesHost.Children.Clear();
            if (!_whatsapp.NodeEnabled)
            {
                nodesHost.Children.Add(MutedText("No listener is active."));
                return;
            }
            try
            {
                var payload = await NodeRequestAsync(HttpMethod.Get, "/v1/nodes");
                var nodes = payload is not null && payload.Value.TryGetProperty("nodes", out var list) && list.ValueKind == JsonValueKind.Array
                    ? list.EnumerateArray().Select(NodeInfo.FromJson).ToList()
                    : new List<NodeInfo>();
                if (nodes.Count == 0) nodesHost.Children.Add(MutedText("No devices are paired yet."));
                foreach (var node in nodes) nodesHost.Children.Add(CreateNodeCard(node));
            }
            catch (Exception error) { nodesHost.Children.Add(MutedText($"Cannot load devices: {error.Message}")); }
        }

        refresh.Click += async (_, _) => await RefreshNodesAsync();
        copyNormal.Click += async (_, _) => await CopyTextAsync(normalCommand);
        copyRoot.Click += async (_, _) => await CopyTextAsync(rootCommand);
        close.Click += (_, _) => dialog.Close();
        body.Children.Add(status);
        body.Children.Add(ButtonRow(refresh, close));
        body.Children.Add(Section("CONNECT A CLIENT"));
        body.Children.Add(MutedText("Install the minimal package on the device, replace PC-IP and NAME, then run one of these commands."));
        body.Children.Add(ButtonRow(copyNormal, copyRoot));
        body.Children.Add(new TextBox
        {
            Text = "#!/bin/sh\nexec /usr/bin/gnomeai-node run",
            IsReadOnly = true,
            AcceptsReturn = true,
            FontFamily = new FontFamily("monospace"),
        });
        body.Children.Add(MutedText("Use a trusted network or VPN such as Tailscale. Do not expose the Hub HTTP port directly to the internet."));
        body.Children.Add(Section("PAIRED DEVICES"));
        body.Children.Add(nodesHost);
        dialog.Content = new ScrollViewer { Content = body, VerticalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Auto };
        dialog.Opened += async (_, _) => await RefreshNodesAsync();
        await dialog.ShowDialog(this);
    }

    private Control CreateNodeCard(NodeInfo node)
    {
        var policy = new ComboBox
        {
            ItemsSource = new[] { "disabled", "ask", "session", "always" },
            SelectedItem = node.RootPolicy,
            MinWidth = 160,
        };
        var apply = new Button { Content = "Apply policy" };
        var feedback = MutedText(node.RootAvailable ? "Local root is available" : "Local root is unavailable");
        apply.Click += async (_, _) =>
        {
            await NodeRequestAsync(HttpMethod.Post, $"/v1/nodes/{Uri.EscapeDataString(node.Id)}/policy",
                new { policy = policy.SelectedItem?.ToString() ?? "ask" });
            feedback.Text = "Root policy was updated.";
        };
        var title = new Grid { ColumnDefinitions = new ColumnDefinitions("*,Auto"), ColumnSpacing = 8 };
        title.Children.Add(new TextBlock { Text = $"{(node.Online ? "●" : "○")} {node.Name}", FontWeight = FontWeight.SemiBold });
        var platform = MutedText(node.Platform); Grid.SetColumn(platform, 1); title.Children.Add(platform);
        var policyRow = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 7, Children = { policy, apply } };
        return Surface(new StackPanel
        {
            Spacing = 6,
            Children =
            {
                title,
                new TextBlock { Text = $"ID: {node.Id}", FontFamily = new FontFamily("monospace"), FontSize = 11 },
                feedback,
                policyRow,
            },
        });
    }

    private async Task<JsonElement?> GetWhatsAppJsonAsync(string path)
    {
        if (_whatsapp is null || string.IsNullOrWhiteSpace(_whatsapp.ApiBase)) return null;
        using var request = new HttpRequestMessage(HttpMethod.Get, _whatsapp.ApiBase.TrimEnd('/') + path);
        request.Headers.Add("X-Gnomef-Token", _whatsapp.Token);
        using var response = await _http.SendAsync(request);
        var text = await response.Content.ReadAsStringAsync();
        if (!response.IsSuccessStatusCode) throw new InvalidOperationException(ExtractHttpError(text, response.ReasonPhrase));
        using var document = JsonDocument.Parse(text);
        return document.RootElement.Clone();
    }

    private async Task<JsonElement?> PostWhatsAppAsync(string path, object? payload = null)
    {
        if (_whatsapp is null || string.IsNullOrWhiteSpace(_whatsapp.ApiBase)) return null;
        using var request = new HttpRequestMessage(HttpMethod.Post, _whatsapp.ApiBase.TrimEnd('/') + path);
        request.Headers.Add("X-Gnomef-Token", _whatsapp.Token);
        if (payload is not null) request.Content = JsonContent.Create(payload);
        using var response = await _http.SendAsync(request);
        var text = await response.Content.ReadAsStringAsync();
        if (!response.IsSuccessStatusCode) throw new InvalidOperationException(ExtractHttpError(text, response.ReasonPhrase));
        if (string.IsNullOrWhiteSpace(text)) return null;
        using var document = JsonDocument.Parse(text);
        return document.RootElement.Clone();
    }

    private async Task SendWhatsAppTestAsync(string jid, string message)
    {
        if (_whatsapp is null || string.IsNullOrWhiteSpace(_whatsapp.BridgeBase)) return;
        using var request = new HttpRequestMessage(HttpMethod.Post, _whatsapp.BridgeBase.TrimEnd('/') + "/send");
        request.Headers.Add("X-Gnomef-Token", _whatsapp.Token);
        request.Content = JsonContent.Create(new { jid, text = message });
        using var response = await _http.SendAsync(request);
        var text = await response.Content.ReadAsStringAsync();
        if (!response.IsSuccessStatusCode) throw new InvalidOperationException(ExtractHttpError(text, response.ReasonPhrase));
    }

    private async Task<JsonElement?> NodeRequestAsync(HttpMethod method, string path, object? payload = null)
    {
        if (_whatsapp is null) return null;
        using var request = new HttpRequestMessage(method, _whatsapp.NodeApiBase.TrimEnd('/') + path);
        request.Headers.Add("X-GnomeAI-Admin-Token", _whatsapp.NodeAdminToken);
        if (payload is not null) request.Content = JsonContent.Create(payload);
        using var response = await _http.SendAsync(request);
        var text = await response.Content.ReadAsStringAsync();
        if (!response.IsSuccessStatusCode) throw new InvalidOperationException(ExtractHttpError(text, response.ReasonPhrase));
        if (string.IsNullOrWhiteSpace(text)) return null;
        using var document = JsonDocument.Parse(text);
        return document.RootElement.Clone();
    }

    private static string ExtractHttpError(string body, string? fallback)
    {
        try
        {
            using var document = JsonDocument.Parse(body);
            return String(document.RootElement, "error", String(document.RootElement, "message", fallback ?? "The operation failed."));
        }
        catch (JsonException) { return string.IsNullOrWhiteSpace(body) ? fallback ?? "The operation failed." : body; }
    }

    private static Bitmap CreateQrBitmap(string payload)
    {
        using var generator = new QRCodeGenerator();
        using var data = generator.CreateQrCode(payload, QRCodeGenerator.ECCLevel.Q);
        var qr = new PngByteQRCode(data);
        var bytes = qr.GetGraphic(8);
        return new Bitmap(new MemoryStream(bytes));
    }

    private static string ReadLogTail(string path, int limit)
    {
        try
        {
            if (!File.Exists(path)) return "The log is empty.";
            var bytes = File.ReadAllBytes(path);
            var start = Math.Max(0, bytes.Length - limit);
            return Encoding.UTF8.GetString(bytes, start, bytes.Length - start);
        }
        catch (Exception error) { return $"Cannot read the log: {error.Message}"; }
    }

    private static List<string> Lines(string? text) => (text ?? "")
        .Split(['\r', '\n', ','], StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries)
        .Where(value => value.Length > 0)
        .ToList();

    private static string FormatKeyValues(IReadOnlyDictionary<string, string> values) =>
        string.Join('\n', values.Select(pair => $"{pair.Key}={pair.Value}"));

    private static Dictionary<string, string> ParseKeyValues(string? text)
    {
        var values = new Dictionary<string, string>(StringComparer.Ordinal);
        foreach (var line in (text ?? "").Split(['\r', '\n'], StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
        {
            var separator = line.IndexOf('=');
            if (separator <= 0) separator = line.IndexOf(':');
            if (separator <= 0) continue;
            values[line[..separator].Trim()] = line[(separator + 1)..].Trim();
        }
        return values;
    }

    private async Task CopyTextAsync(string text)
    {
        var clipboard = TopLevel.GetTopLevel(this)?.Clipboard;
        if (clipboard is not null) await clipboard.SetTextAsync(text);
    }

    private static Task OpenUrlAsync(string url)
    {
        Process.Start(new ProcessStartInfo { FileName = url, UseShellExecute = true });
        return Task.CompletedTask;
    }

    private static void SendDesktopNotification(string title, string body)
    {
        try
        {
            if (RuntimeInformation.IsOSPlatform(OSPlatform.Linux))
            {
                var info = new ProcessStartInfo("notify-send") { UseShellExecute = false, CreateNoWindow = true };
                info.ArgumentList.Add("--app-name=GnomeAI-RS");
                info.ArgumentList.Add("--expire-time=5000");
                info.ArgumentList.Add(title);
                info.ArgumentList.Add(body);
                Process.Start(info);
            }
            else if (RuntimeInformation.IsOSPlatform(OSPlatform.OSX))
            {
                var escapedTitle = title.Replace("\"", "\\\"", StringComparison.Ordinal);
                var escapedBody = body.Replace("\"", "\\\"", StringComparison.Ordinal);
                var info = new ProcessStartInfo("osascript") { UseShellExecute = false, CreateNoWindow = true };
                info.ArgumentList.Add("-e");
                info.ArgumentList.Add($"display notification \"{escapedBody}\" with title \"{escapedTitle}\"");
                Process.Start(info);
            }
        }
        catch { /* Notifications are optional and must never break the turn. */ }
    }

    private async Task<string?> PromptAsync(string title, string label, string initial = "", bool masked = false)
    {
        var dialog = CreateDialog(title, 500, 240);
        var input = new TextBox { Text = initial, Watermark = label };
        if (masked) input.PasswordChar = '•';
        var result = new TaskCompletionSource<string?>();
        var ok = AccentButton("Save");
        var cancel = new Button { Content = "Cancel" };
        ok.Click += (_, _) => { result.TrySetResult(input.Text); dialog.Close(); };
        cancel.Click += (_, _) => { result.TrySetResult(null); dialog.Close(); };
        dialog.Closed += (_, _) => result.TrySetResult(null);
        dialog.Content = DialogLayout(title, label, [input], ok, cancel);
        await dialog.ShowDialog(this);
        return await result.Task;
    }

    private async Task<bool> ConfirmAsync(string title, string message)
    {
        var dialog = CreateDialog(title, 440, 220);
        dialog.MinWidth = dialog.MaxWidth = 440;
        dialog.MinHeight = dialog.MaxHeight = 220;
        dialog.CanResize = false;
        var result = new TaskCompletionSource<bool>();
        var yes = new Button
        {
            Content = "Delete",
            MinWidth = 88,
            HorizontalContentAlignment = HorizontalAlignment.Center,
            VerticalContentAlignment = VerticalAlignment.Center,
        };
        yes.Classes.Add("danger");
        var no = new Button
        {
            Content = "Cancel",
            MinWidth = 88,
            HorizontalContentAlignment = HorizontalAlignment.Center,
            VerticalContentAlignment = VerticalAlignment.Center,
        };
        yes.Click += (_, _) => { result.TrySetResult(true); dialog.Close(); };
        no.Click += (_, _) => { result.TrySetResult(false); dialog.Close(); };
        dialog.Closed += (_, _) => result.TrySetResult(false);

        var layout = new Grid
        {
            RowDefinitions = new RowDefinitions("Auto,*,Auto,Auto"),
            Margin = new Thickness(24),
        };
        layout.Children.Add(DialogHeading(title, message));
        var separator = new Border
        {
            Height = 1,
            Background = ThemeBrush("#D7DEE7", "#3C3C3C"),
            Margin = new Thickness(0, 0, 0, 14),
        };
        Grid.SetRow(separator, 2);
        layout.Children.Add(separator);
        var actions = new StackPanel
        {
            Orientation = Orientation.Horizontal,
            Spacing = 8,
            HorizontalAlignment = HorizontalAlignment.Right,
            Children = { no, yes },
        };
        Grid.SetRow(actions, 3);
        layout.Children.Add(actions);
        dialog.Content = layout;
        await dialog.ShowDialog(this);
        return await result.Task;
    }

    private static Window CreateDialog(string title, double width, double height) => new()
    {
        Title = title,
        Width = width,
        Height = height,
        MinWidth = Math.Min(420, width),
        MinHeight = Math.Min(240, height),
        WindowStartupLocation = WindowStartupLocation.CenterOwner,
        ShowInTaskbar = false,
        CanResize = true,
        RequestedThemeVariant = Application.Current?.RequestedThemeVariant ?? ThemeVariant.Default,
    };

    private static Control DialogLayout(string title, string subtitle, IEnumerable<Control> controls, params Button[] buttons)
    {
        var body = new StackPanel { Spacing = 9, Margin = new Thickness(22) };
        body.Children.Add(DialogHeading(title, subtitle));
        foreach (var control in controls) body.Children.Add(control);
        if (buttons.Length > 0) body.Children.Add(ButtonRow(buttons));
        return new ScrollViewer { Content = body, VerticalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Auto };
    }

    private static Control DialogHeading(string title, string subtitle) => new StackPanel
    {
        Spacing = 4,
        Margin = new Thickness(0, 0, 0, 8),
        Children =
        {
            new TextBlock { Text = title, FontSize = 23, FontWeight = FontWeight.SemiBold },
            MutedText(subtitle),
        },
    };

    private static TextBlock Section(string text)
    {
        var block = new TextBlock { Text = text, Margin = new Thickness(0, 9, 0, 1) };
        block.Classes.Add("section");
        return block;
    }

    private static TextBlock MutedText(string text)
    {
        var block = new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap, FontSize = 12 };
        block.Classes.Add("muted");
        return block;
    }

    private static Control Labeled(string label, Control control) => new StackPanel
    {
        Spacing = 4,
        Children = { new TextBlock { Text = label, FontWeight = FontWeight.Medium }, control },
    };

    private static Border Surface(Control child)
    {
        var border = new Border { Padding = new Thickness(11), Child = child };
        border.Classes.Add("surface");
        return border;
    }

    private static Button AccentButton(string text)
    {
        var button = new Button { Content = text };
        button.Classes.Add("accent");
        return button;
    }

    private static Button ActionButton(string text, Func<Task> action)
    {
        var button = new Button { Content = text };
        button.Click += async (_, _) => await action();
        return button;
    }

    private static StackPanel ButtonRow(params Button[] buttons)
    {
        var row = new StackPanel
        {
            Orientation = Orientation.Horizontal,
            Spacing = 7,
            HorizontalAlignment = HorizontalAlignment.Right,
            Margin = new Thickness(0, 9, 0, 0),
        };
        foreach (var button in buttons) row.Children.Add(button);
        return row;
    }

    private sealed class SessionRuntime
    {
        public bool Busy { get; set; }
        public bool NeedsAttention { get; set; }
        public Queue<QueuedSubmission> Queue { get; } = new();
        public List<BufferedSessionEvent> LiveEvents { get; } = [];
    }

    private sealed class BufferedSessionEvent(string kind, JsonElement node, string text)
    {
        public string Kind { get; } = kind;
        public JsonElement Node { get; } = node;
        public StringBuilder Text { get; } = new(text);
    }

    private sealed record QueuedSubmission(string Text, AttachedFile? Attachment);
}
