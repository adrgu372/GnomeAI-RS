using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Runtime.CompilerServices;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Windows.Input;
using Avalonia.Layout;
using Avalonia.Media;

namespace GnomeAI.UI;

public abstract class ObservableObject : INotifyPropertyChanged
{
    public event PropertyChangedEventHandler? PropertyChanged;

    protected void OnPropertyChanged([CallerMemberName] string? name = null) =>
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));

    protected bool Set<T>(ref T field, T value, [CallerMemberName] string? name = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value)) return false;
        field = value;
        OnPropertyChanged(name);
        return true;
    }
}

public sealed class MessageItem : ObservableObject
{
    private string _title = "";
    private string _text = "";
    private string _status = "";
    private string _inputValue = "";
    private bool _inputRemember;
    private bool _hasInput;
    private bool _isExpanded;
    private IBrush _cardBrush = Brush.Parse("#FFFFFF");

    public required string Kind { get; init; }
    public string Title { get => _title; set => Set(ref _title, value); }
    public string Text { get => _text; set => Set(ref _text, value); }
    public string Status { get => _status; set => Set(ref _status, value); }
    public string InputValue { get => _inputValue; set => Set(ref _inputValue, value); }
    public bool InputRemember { get => _inputRemember; set => Set(ref _inputRemember, value); }
    public string InputHint { get; init; } = "";
    public bool HasInput { get => _hasInput; set => Set(ref _hasInput, value); }
    public bool InputRememberVisible { get; init; }
    public bool IsCollapsible { get; init; }
    public bool IsNotCollapsible => !IsCollapsible;
    public bool IsExpanded
    {
        get => _isExpanded;
        set
        {
            if (!Set(ref _isExpanded, value)) return;
            OnPropertyChanged(nameof(DetailsLabel));
            OnPropertyChanged(nameof(DetailsChevron));
        }
    }
    public string DetailsLabel => IsExpanded ? "Hide details" : "Show details";
    public string DetailsChevron => IsExpanded ? "⌃" : "⌄";
    public ObservableCollection<MessageAction> Actions { get; } = [];
    public IBrush CardBrush { get => _cardBrush; set => Set(ref _cardBrush, value); }
    public IBrush TextBrush { get; init; } = Brush.Parse("#202124");
    public HorizontalAlignment Alignment { get; init; } = HorizontalAlignment.Stretch;
}

public sealed class MessageAction : ICommand
{
    public required string Label { get; init; }
    public required Func<Task> Handler { get; init; }
    public ICommand Run => this;
    public event EventHandler? CanExecuteChanged
    {
        add { }
        remove { }
    }
    public bool CanExecute(object? parameter) => true;
    public void Execute(object? parameter) => _ = Handler();
}

public sealed class SessionItem : ObservableObject
{
    private bool _isBusy;
    private bool _needsAttention;

    public required string Id { get; init; }
    public required string Title { get; init; }
    public required string Project { get; init; }
    public string Model { get; init; } = "";
    public long Turns { get; init; }
    public long UpdatedAt { get; init; }
    public bool IsCurrent { get; init; }
    public bool IsBusy
    {
        get => _isBusy;
        set
        {
            if (!Set(ref _isBusy, value)) return;
            OnPropertyChanged(nameof(Caption));
            OnPropertyChanged(nameof(StatusLine));
        }
    }
    public bool NeedsAttention
    {
        get => _needsAttention;
        set
        {
            if (!Set(ref _needsAttention, value)) return;
            OnPropertyChanged(nameof(Caption));
            OnPropertyChanged(nameof(StatusLine));
        }
    }
    public string Caption => IsCurrent ? $"● {Title}" : IsBusy ? $"◌ {Title}" : NeedsAttention ? $"! {Title}" : Title;
    public string StatusLine => IsBusy ? $"Working… · {Project}" : NeedsAttention ? $"Needs attention · {Project}" : Project;
    public string Details => $"{Project} · {Model} · {Turns} turns";
}

public sealed class SlashCommandEntry
{
    public required string Command { get; init; }
    public required string Description { get; init; }
}

public sealed class ActivityItem
{
    public required string Title { get; init; }
    public string Details { get; init; } = "";
    public IBrush StatusBrush { get; init; } = Brush.Parse("#0067C0");
}

public sealed record AttachedFile(string Path, string Name);

public sealed class SessionSummaryEntry
{
    [JsonPropertyName("id")] public string Id { get; set; } = "";
    [JsonPropertyName("title")] public string? Title { get; set; }
    [JsonPropertyName("workspace")] public string Workspace { get; set; } = "";
    [JsonPropertyName("model")] public string Model { get; set; } = "";
    [JsonPropertyName("updated_at")] public long UpdatedAt { get; set; }
    [JsonPropertyName("turns")] public long Turns { get; set; }
    [JsonPropertyName("is_current")] public bool IsCurrent { get; set; }
}

public sealed class McpServerEntry
{
    [JsonPropertyName("name")] public string Name { get; set; } = "mcp-server";
    [JsonPropertyName("enabled")] public bool Enabled { get; set; } = true;
    [JsonPropertyName("transport")] public string Transport { get; set; } = "streamable-http";
    [JsonPropertyName("url")] public string Url { get; set; } = "";
    [JsonPropertyName("command")] public string Command { get; set; } = "";
    [JsonPropertyName("args")] public List<string> Args { get; set; } = [];
    [JsonPropertyName("env")] public Dictionary<string, string> Env { get; set; } = [];
    [JsonPropertyName("headers")] public Dictionary<string, string> Headers { get; set; } = [];
    [JsonPropertyName("allow_whatsapp")] public bool AllowWhatsApp { get; set; }

    public McpServerEntry Clone() => new()
    {
        Name = Name,
        Enabled = Enabled,
        Transport = Transport,
        Url = Url,
        Command = Command,
        Args = [.. Args],
        Env = new Dictionary<string, string>(Env),
        Headers = new Dictionary<string, string>(Headers),
        AllowWhatsApp = AllowWhatsApp,
    };
}

public sealed class ProviderInfo
{
    public string Id { get; set; } = "";
    public string Name { get; set; } = "";
    public string Auth { get; set; } = "";
    public string BaseUrl { get; set; } = "";
    public string DefaultModel { get; set; } = "";
    public string Description { get; set; } = "";
    public override string ToString() => Name;
}

public sealed class WhatsAppConfig
{
    [JsonPropertyName("api_base")] public string ApiBase { get; set; } = "";
    [JsonPropertyName("bridge_base")] public string BridgeBase { get; set; } = "";
    [JsonPropertyName("token")] public string Token { get; set; } = "";
    [JsonPropertyName("enabled")] public bool Enabled { get; set; }
    [JsonPropertyName("assistant_name")] public string AssistantName { get; set; } = "GnomeAI";
    [JsonPropertyName("has_own_number")] public bool HasOwnNumber { get; set; }
    [JsonPropertyName("allowed_jids")] public List<string> AllowedJids { get; set; } = [];
    [JsonPropertyName("log_file")] public string LogFile { get; set; } = "";
    [JsonPropertyName("node_api_base")] public string NodeApiBase { get; set; } = "";
    [JsonPropertyName("node_admin_token")] public string NodeAdminToken { get; set; } = "";
    [JsonPropertyName("node_enrollment_token")] public string NodeEnrollmentToken { get; set; } = "";
    [JsonPropertyName("node_enabled")] public bool NodeEnabled { get; set; }
    [JsonPropertyName("node_bind")] public string NodeBind { get; set; } = "0.0.0.0";
    [JsonPropertyName("node_port")] public int NodePort { get; set; }
    [JsonPropertyName("launch_error")] public string? LaunchError { get; set; }
}

public sealed class WhatsAppConversation
{
    public string Id { get; init; } = "";
    public string Title { get; init; } = "";
    public override string ToString() => Title.StartsWith("WhatsApp - ", StringComparison.Ordinal)
        ? Title[11..].Trim()
        : Title;
}

public sealed class WhatsAppMessage
{
    public string Role { get; init; } = "assistant";
    public string Text { get; init; } = "";
    public string Author { get; init; } = "";
    public bool IsUser => Role.Equals("user", StringComparison.OrdinalIgnoreCase);
}

public sealed class NodeInfo
{
    public string Id { get; init; } = "";
    public string Name { get; init; } = "";
    public bool Online { get; init; }
    public string Os { get; init; } = "?";
    public string Architecture { get; init; } = "?";
    public string InitSystem { get; init; } = "manual";
    public bool RootAvailable { get; init; }
    public string RootPolicy { get; init; } = "ask";
    public string Status => Online ? "Online" : "Offline";
    public string Platform => $"{Os} · {Architecture} · {InitSystem}";

    public static NodeInfo FromJson(JsonElement node) => new()
    {
        Id = GetString(node, "node_id", "?"),
        Name = GetString(node, "name", GetString(node, "node_id", "?")),
        Online = GetBool(node, "online"),
        Os = GetString(node, "os", "?"),
        Architecture = GetString(node, "arch", "?"),
        InitSystem = GetString(node, "init_system", "manual"),
        RootAvailable = GetBool(node, "root_available"),
        RootPolicy = GetString(node, "root_policy", "ask"),
    };

    private static string GetString(JsonElement node, string name, string fallback) =>
        node.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String
            ? value.GetString() ?? fallback
            : fallback;

    private static bool GetBool(JsonElement node, string name) =>
        node.TryGetProperty(name, out var value) && value.ValueKind is JsonValueKind.True;
}
