using System.Text.Json;

namespace GnomeAI.Client;

/// <summary>
/// One messenger account, as the Rust core reports it. Shared by the desktop
/// and Android shells so both render the same truth.
/// </summary>
public sealed class MessengerAccountInfo
{
    public string AccountId { get; init; } = "";
    /// <summary>Hex of the Ed25519 identity key: the name peers see.</summary>
    public string UserId { get; init; } = "";
    public string IdentityPk { get; init; } = "";
    public string IdentityPublic { get; init; } = "";
    public string SignedPrekey { get; init; } = "";
    public string PrekeySignature { get; init; } = "";
    public string DisplayName { get; init; } = "";
    /// <summary>This account's own onion, never the device-mesh one.</summary>
    public string Onion { get; init; } = "";
    public bool Active { get; init; }
    public bool Online { get; init; }
    public bool Pro { get; init; }
    public long ChunkSize { get; init; } = 262144;

    public override string ToString() =>
        DisplayName + (Onion.Length == 0 ? " · starting Tor…" : Online ? " · online" : " · offline");

    public static MessengerAccountInfo FromJson(JsonElement node) => new()
    {
        AccountId = Text(node, "account_id"),
        UserId = Text(node, "user_id"),
        IdentityPk = Text(node, "identity_pk"),
        IdentityPublic = Text(node, "identity_public"),
        SignedPrekey = Text(node, "signed_prekey"),
        PrekeySignature = Text(node, "prekey_signature"),
        DisplayName = Text(node, "display_name", "Account"),
        Onion = Text(node, "onion_address"),
        Active = Flag(node, "active"),
        Online = Flag(node, "online"),
        Pro = Flag(node, "pro"),
        ChunkSize = node.TryGetProperty("chunk_size", out var chunk) && chunk.TryGetInt64(out var size)
            ? size
            : 262144,
    };

    /// <summary>
    /// The signed invite a QR encodes. Empty when Tor has not published this
    /// account's onion yet: an unreachable invite is worse than no invite.
    /// </summary>
    public string Invite()
    {
        if (Onion.Length == 0) return "";
        return JsonSerializer.Serialize(new Dictionary<string, object?>
        {
            ["v"] = 1,
            ["user_id"] = UserId,
            ["identity_pk"] = IdentityPk,
            ["identity_public"] = IdentityPublic,
            ["signed_prekey"] = SignedPrekey,
            ["spk_sig"] = PrekeySignature,
            ["onion"] = Onion,
            ["name"] = DisplayName,
        });
    }

    public static string Text(JsonElement node, string name, string fallback = "") =>
        node.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String
            ? value.GetString() ?? fallback
            : fallback;

    public static bool Flag(JsonElement node, string name) =>
        node.TryGetProperty(name, out var value) && value.ValueKind is JsonValueKind.True;

    public static long Number(JsonElement node, string name) =>
        node.TryGetProperty(name, out var value) && value.TryGetInt64(out var number) ? number : 0;
}

/// <summary>One row in the messenger conversation list.</summary>
public sealed class MessengerConversationInfo
{
    public string Id { get; init; } = "";
    /// <summary>`direct` or `group`.</summary>
    public string Kind { get; init; } = "direct";
    /// <summary>Peer user id for a direct chat, group id for a group.</summary>
    public string Target { get; init; } = "";
    public string Title { get; init; } = "";
    public string Preview { get; init; } = "";
    public long Unread { get; init; }
    public bool HasUnread => Unread > 0;
    public string UnreadLabel => Unread > 99 ? "99+" : Unread.ToString();
    public string Badge => Kind == "group" ? "⋯" : "@";

    public static MessengerConversationInfo FromJson(JsonElement node) => new()
    {
        Id = MessengerAccountInfo.Text(node, "conversation_id"),
        Kind = MessengerAccountInfo.Text(node, "kind", "direct"),
        Target = MessengerAccountInfo.Text(node, "target"),
        Title = MessengerAccountInfo.Text(node, "title", "Conversation"),
        Preview = MessengerAccountInfo.Text(node, "preview"),
        Unread = MessengerAccountInfo.Number(node, "unread"),
    };
}

/// <summary>One member of a messenger group.</summary>
public sealed class MessengerMemberInfo
{
    public string UserId { get; init; } = "";
    public string Name { get; init; } = "";
    public string Onion { get; init; } = "";
}

/// <summary>
/// An invite that another device sent and this one has not answered yet.
/// </summary>
/// <remarks>
/// A scanned QR is a proposal, not consent: the receiving device shows this and
/// only writes the contact when the person accepts. `Direction` is `incoming`
/// when they scanned our code and `outgoing` when we scanned theirs and are
/// waiting for the answer.
/// </remarks>
public sealed class MessengerContactRequest
{
    public string UserId { get; init; } = "";
    public string DisplayName { get; init; } = "";
    public string Onion { get; init; } = "";
    public string Direction { get; init; } = "incoming";
    public long CreatedAt { get; init; }

    public bool Incoming => Direction != "outgoing";
    public string Badge => Incoming ? "wants to chat" : "waiting for them";

    public static MessengerContactRequest FromJson(JsonElement node) => new()
    {
        UserId = MessengerAccountInfo.Text(node, "user_id"),
        DisplayName = MessengerAccountInfo.Text(node, "display_name", "Someone"),
        Onion = MessengerAccountInfo.Text(node, "onion"),
        Direction = MessengerAccountInfo.Text(node, "direction", "incoming"),
        CreatedAt = MessengerAccountInfo.Number(node, "created_at"),
    };
}
