using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Threading;
using System.Collections.ObjectModel;
using System.Text.Json;
using Avalonia.Platform.Storage;
using GnomeAI.Client;

namespace GnomeAI.UI;

/// <summary>
/// The Chat tab: an end-to-end encrypted messenger that talks only over Tor.
/// </summary>
/// <remarks>
/// Everything here is deliberately separate from the coding agent. A messenger
/// account has its own identity, its own onion and its own history; switching
/// accounts never touches the device pairing, and sending a message never
/// starts or interrupts an agent turn.
///
/// There is no server, no relay and no store-and-forward. A message that cannot
/// be delivered is written Pending by the Rust core and flushed when the peer's
/// onion answers again, so this file only ever renders the core's truth.
/// </remarks>
public sealed partial class MainWindow
{
    private readonly ObservableCollection<MessengerAccountItem> _messengerAccounts = [];
    private readonly ObservableCollection<MessengerConversationItem> _messengerConversations = [];
    private readonly Dictionary<string, MessengerAccountItem> _messengerAccountById = [];
    private readonly Dictionary<string, string> _messengerContactNames = [];
    private readonly List<string> _messengerContactIds = [];
    private readonly Dictionary<string, (string Kind, string Target, string Title)> _messengerOpen = [];
    private readonly Dictionary<string, string> _messengerTitles = [];
    private string _messengerAccount = "";
    private string _messengerConversation = "";
    private string _messengerConversationKind = "direct";
    private string _messengerConversationTarget = "";
    private string _messengerReplyTo = "";
    /// <summary>Files staged for the open conversation; any number, any type.</summary>
    private readonly List<AttachedFile> _messengerAttachments = [];
    private string _messengerEditing = "";
    private bool _messengerTabActive;
    private bool _messengerSwitchingAccount;
    private bool _messengerRefreshingAccounts;
    /// Set while a transfer's progress bar is showing, so "Cancel" knows what
    /// it is dismissing rather than guessing from the last event.
    private string _messengerActiveTransfer = "";

    private bool MessengerReady => _messengerAccount.Length > 0;

    private void InitializeChat()
    {
        MessengerConversations.ItemsSource = _messengerConversations;
        RefreshChatTabs();
    }

    private void RefreshChatTabs()
    {
        // The active tab is only a visual emphasis; both surfaces stay mounted so
        // switching is instantaneous and no scroll position or draft is lost.
        AgentTabButton?.Classes.Set("accent", !_messengerTabActive);
        ChatTabButton?.Classes.Set("accent", _messengerTabActive);
        // Keep the pending-request count visible whichever tab is open.
        if (ChatTabButton is not null)
        {
            var incoming = _messengerRequests.Count(request => request.Incoming);
            ChatTabButton.Content = incoming > 0 ? $"Chat ({incoming})" : "Chat";
        }
        if (ChatPage is not null) ChatPage.IsVisible = _messengerTabActive;
        if (DesktopWorkspace is not null)
        {
            foreach (var child in DesktopWorkspace.Children.OfType<Control>())
            {
                var isChat = ReferenceEquals(child, ChatPage);
                if (!isChat) child.IsVisible = !_messengerTabActive;
            }
        }
        DevicesPage.IsVisible = false;
    }

    private async void AgentTab_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        _messengerTabActive = false;
        RefreshChatTabs();
        Composer.Focus();
        await Task.CompletedTask;
    });

    private async void ChatTab_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        ShowChatTab();
        _messengerTabActive = true;
        RefreshChatTabs();
        MessengerComposer.Focus();
        await RefreshMessengerAccountsAsync();
        // Requests arrive over the onion while this tab is elsewhere; ask for
        // the current list on every entry so nothing waits unseen.
        if (MessengerReady)
            await SendAsync(new()
            {
                ["op"] = "messenger_list_contact_requests",
                ["account_id"] = _messengerAccount,
            });
    });

    private void ShowChatTab()
    {
        _messengerTabActive = true;
        RefreshChatTabs();
    }

    private async Task RefreshMessengerAccountsAsync()
    {
        if (_messengerRefreshingAccounts) return;
        _messengerRefreshingAccounts = true;
        try { await SendAsync(new() { ["op"] = "messenger_list_accounts" }); }
        finally { _messengerRefreshingAccounts = false; }
    }

    /// <summary>
    /// Draw the "somebody scanned your QR" cards above the conversation list.
    /// </summary>
    /// <remarks>
    /// Accept and Decline are the only two ways a request leaves this panel, and
    /// both are answered by the core over Tor, so the other phone learns the
    /// decision instead of guessing from silence.
    /// </remarks>
    private void RenderMessengerRequests()
    {
        MessengerRequests.Children.Clear();
        MessengerRequests.IsVisible = _messengerRequests.Count > 0;
        // The panel lives inside the Chat tab, so a request that arrives while
        // the Agent tab is open would otherwise be invisible. The tab label
        // carries the count the way the phone's badge does.
        var incoming = _messengerRequests.Count(request => request.Incoming);
        if (ChatTabButton is not null)
            ChatTabButton.Content = incoming > 0 ? $"Chat ({incoming})" : "Chat";
        foreach (var request in _messengerRequests.ToArray())
        {
            var card = new Border
            {
                Padding = new Thickness(10),
                CornerRadius = new CornerRadius(7),
                Background = ThemeBrush("#F2F7FF", "#26313D"),
                BorderBrush = ThemeBrush("#D7E3F5", "#3A4757"),
                BorderThickness = new Thickness(1),
                Margin = new Thickness(0, 0, 0, 6),
            };
            var body = new StackPanel { Spacing = 5 };
            body.Children.Add(new TextBlock
            {
                Text = request.DisplayName,
                FontWeight = FontWeight.SemiBold,
                TextTrimming = TextTrimming.CharacterEllipsis,
            });
            body.Children.Add(new TextBlock
            {
                Text = request.Badge,
                FontSize = 10,
                Foreground = ThemeBrush("#5A6B7D", "#9FB4A9"),
            });
            var actions = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
            if (request.Incoming)
            {
                actions.Children.Add(SmallChatAction("Accept", () => RespondMessengerRequestAsync(request, true)));
                actions.Children.Add(SmallChatAction("Decline", () => RespondMessengerRequestAsync(request, false)));
            }
            else
            {
                actions.Children.Add(SmallChatAction("Cancel request", () => RespondMessengerRequestAsync(request, false)));
            }
            body.Children.Add(actions);
            card.Child = body;
            MessengerRequests.Children.Add(card);
        }
    }

    private async Task RespondMessengerRequestAsync(MessengerContactRequest request, bool accept)
    {
        await SendAsync(new()
        {
            ["op"] = "messenger_respond_contact_request",
            ["account_id"] = _messengerAccount,
            ["user_id"] = request.UserId,
            ["accept"] = accept,
        });
        _messengerRequests.RemoveAll(row => row.UserId == request.UserId);
        RenderMessengerRequests();
        // The answer buttons are also in the open transcript, so it has to be
        // rebuilt: otherwise the card stays on screen claiming a request that
        // has just been answered. Refetch rather than patching one child, because
        // the acceptance also adds the peer's queued messages.
        if (_messengerConversation.Length > 0 && _messengerConversationKind == "direct")
            await SendAsync(new()
            {
                ["op"] = "messenger_get_messages",
                ["account_id"] = _messengerAccount,
                ["conversation_id"] = _messengerConversation,
            });
    }

    private void RenderMessengerAccounts(JsonElement accounts)
    {
        var previous = _messengerAccount;
        _messengerAccounts.Clear();
        _messengerAccountById.Clear();
        foreach (var node in accounts.EnumerateArray())
        {
            var account = MessengerAccountItem.FromJson(node);
            _messengerAccounts.Add(account);
            _messengerAccountById[account.AccountId] = account;
        }
        _messengerSwitchingAccount = true;
        try
        {
            MessengerAccountPicker.ItemsSource = _messengerAccounts;
            var active = _messengerAccounts.FirstOrDefault(a => a.AccountId == _messengerAccount)
                         ?? _messengerAccounts.FirstOrDefault(a => a.Active)
                         ?? _messengerAccounts.FirstOrDefault();
            MessengerAccountPicker.SelectedItem = active;
            _messengerAccount = active?.AccountId ?? "";
        }
        finally { _messengerSwitchingAccount = false; }

        var current = _messengerAccountById.GetValueOrDefault(_messengerAccount);
        MessengerEmptyState.IsVisible = _messengerAccounts.Count == 0;
        MessengerProfileLabel.Text = current is null ? "My profile" : "My profile · " + current.DisplayName;
        RefreshMessengerComposer();
        if (previous != _messengerAccount)
        {
            MessengerTranscript.Children.Clear();
            _messengerConversation = "";
            _messengerConversations.Clear();
            // Requests belong to the account that received them.
            _messengerRequests.Clear();
            RenderMessengerRequests();
            if (MessengerReady)
                _ = RunUiAsync(async () =>
                {
                    await SendAsync(new() { ["op"] = "messenger_list_conversations", ["account_id"] = _messengerAccount });
                    await SendAsync(new() { ["op"] = "messenger_list_groups", ["account_id"] = _messengerAccount });
                    await SendAsync(new() { ["op"] = "messenger_list_contact_requests", ["account_id"] = _messengerAccount });
                });
        }
    }

    private async void MessengerAccount_SelectionChanged(object? sender, SelectionChangedEventArgs e) => await RunUiAsync(async () =>
    {
        if (_messengerSwitchingAccount) return;
        if (MessengerAccountPicker.SelectedItem is not MessengerAccountItem account) return;
        if (account.AccountId == _messengerAccount) return;
        _messengerAccount = account.AccountId;
        // Switching is local and instant: no logout, no Tor restart from the UI.
        await SendAsync(new() { ["op"] = "messenger_switch_account", ["account_id"] = account.AccountId });
        MessengerTranscript.Children.Clear();
        _messengerConversation = "";
        _messengerConversations.Clear();
        _messengerRequests.Clear();
        RenderMessengerRequests();
        await SendAsync(new() { ["op"] = "messenger_list_conversations", ["account_id"] = account.AccountId });
        await SendAsync(new() { ["op"] = "messenger_list_groups", ["account_id"] = account.AccountId });
        await SendAsync(new() { ["op"] = "messenger_list_contact_requests", ["account_id"] = account.AccountId });
    });

    private async void MessengerNewAccount_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        var name = await PromptAsync("New messenger account", "Display name for this identity", "");
        if (name is null) return;
        var trimmed = name.Trim();
        if (trimmed.Length == 0) return;
        await SendAsync(new() { ["op"] = "messenger_create_account", ["display_name"] = trimmed });
    });

    private async Task<string?> PromptAsync(string title, string hint, string initial)
    {
        var dialog = CreateDialog(title, 460, 250);
        var result = new TaskCompletionSource<string?>();
        var input = new TextBox { Text = initial, Watermark = hint };
        var body = new StackPanel { Spacing = 10, Margin = new Thickness(22) };
        body.Children.Add(DialogHeading(title, hint));
        body.Children.Add(input);
        body.Children.Add(ButtonRow(
            ActionButton("Cancel", () => { result.TrySetResult(null); dialog.Close(); return Task.CompletedTask; }),
            ActionButton("Save", () => { result.TrySetResult(input.Text ?? ""); dialog.Close(); return Task.CompletedTask; })));
        dialog.Content = body;
        dialog.Closed += (_, _) => result.TrySetResult(null);
        await dialog.ShowDialog(this);
        return await result.Task;
    }

    private async Task<bool> MessengerRequireAccountAsync()
    {
        if (MessengerReady) return true;
        ShowNotice("Create a messenger account first.");
        return false;
    }

    private void RefreshMessengerComposer()
    {
        var canSend = MessengerReady && _messengerConversation.Length > 0;
        MessengerComposer.IsEnabled = MessengerReady;
        MessengerAttachButton.IsEnabled = canSend;
        MessengerSendButton.IsEnabled = canSend;
        MessengerEditButton.IsVisible = _messengerEditing.Length > 0;
        MessengerHint.Text = _messengerEditing.Length > 0
            ? "Editing a sent message · Enter saves, Esc cancels"
            : "Enter sends · Shift+Enter inserts a new line";
        MessengerAttachmentBar.IsVisible = _messengerAttachments.Count > 0;
        if (_messengerAttachments.Count > 0)
            MessengerAttachmentText.Text = _messengerAttachments.Count == 1
                ? "📎 " + _messengerAttachments[0].Name
                : $"📎 {_messengerAttachments.Count} files · "
                  + string.Join(", ", _messengerAttachments.Take(3).Select(file => file.Name))
                  + (_messengerAttachments.Count > 3 ? "…" : "");
        ToolTip.SetTip(MessengerAttachmentText,
            string.Join('\n', _messengerAttachments.Select(file => file.Name)));
        MessengerReplyBar.IsVisible = _messengerReplyTo.Length > 0;
    }

    // -----------------------------------------------------------------------
    // Conversation list
    // -----------------------------------------------------------------------

    private void RenderMessengerConversations(string accountId, JsonElement conversations)
    {
        if (accountId != _messengerAccount) return;
        _messengerConversations.Clear();
        foreach (var node in conversations.EnumerateArray())
        {
            var item = MessengerConversationItem.FromJson(node);
            _messengerTitles[item.Id] = item.Title;
            _messengerOpen[item.Id] = (item.Kind, item.Target, item.Title);
            var filter = (MessengerSearch.Text ?? "").Trim();
            if (filter.Length > 0
                && !item.Title.Contains(filter, StringComparison.OrdinalIgnoreCase)
                && !item.Preview.Contains(filter, StringComparison.OrdinalIgnoreCase)) continue;
            _messengerConversations.Add(item);
        }
    }

    private void MessengerSearch_TextChanged(object? sender, TextChangedEventArgs e)
    {
        if (!MessengerReady) return;
        _ = RunUiAsync(async () =>
            await SendAsync(new() { ["op"] = "messenger_list_conversations", ["account_id"] = _messengerAccount }));
    }

    private async void MessengerConversation_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        if ((sender as Button)?.Tag?.ToString() is not { Length: > 0 } id) return;
        await OpenMessengerConversationAsync(id);
    });

    private async Task OpenMessengerConversationAsync(string conversationId)
    {
        if (!await MessengerRequireAccountAsync()) return;
        var known = _messengerOpen.GetValueOrDefault(conversationId);
        _messengerConversation = conversationId;
        // `dm:<account>:<peer>` and `grp:<account>:<group>` are the core's ids;
        // the UI only needs the kind and the target inside them.
        var parts = conversationId.Split(':');
        _messengerConversationKind = known.Kind ?? (parts.Length > 0 && parts[0] == "grp" ? "group" : "direct");
        _messengerConversationTarget = known.Target
            ?? (parts.Length > 2 ? string.Join(':', parts[2..]) : "");
        _messengerReplyTo = "";
        _messengerEditing = "";
        _messengerAttachments.Clear();
        _messengerTitles.TryGetValue(conversationId, out var title);
        MessengerTitle.Text = title ?? (known.Title ?? "Conversation");
        MessengerSubtitle.Text = _messengerConversationKind == "group"
            ? "Group · sender-key encrypted · delivered per member over Tor"
            : "Direct · X3DH + Double Ratchet over Tor";
        MessengerEmptyState.IsVisible = false;
        MessengerTranscript.Children.Clear();
        RefreshMessengerComposer();
        if (_messengerConversationKind == "group")
            await SendAsync(new() { ["op"] = "messenger_get_group_messages", ["account_id"] = _messengerAccount, ["group_id"] = _messengerConversationTarget });
        else
            await SendAsync(new() { ["op"] = "messenger_get_messages", ["account_id"] = _messengerAccount, ["conversation_id"] = conversationId });
        await SendAsync(new() { ["op"] = "messenger_mark_read", ["account_id"] = _messengerAccount, ["conversation_id"] = conversationId });
        MessengerComposer.Focus();
    }

    // -----------------------------------------------------------------------
    // Transcript
    // -----------------------------------------------------------------------

    private void RenderMessengerHistory(string accountId, string conversationId, JsonElement messages)
    {
        if (accountId != _messengerAccount || conversationId != _messengerConversation) return;
        MessengerTranscript.Children.Clear();
        // Above the bubbles, where the note that announced the request also is.
        if (PendingMessengerRequest() is { } pending) MessengerTranscript.Children.Add(pending);
        var count = 0;
        foreach (var message in messages.EnumerateArray())
        {
            MessengerTranscript.Children.Add(BuildMessengerBubble(message));
            count++;
        }
        if (count == 0)
            MessengerTranscript.Children.Add(MutedBubble(
                "No messages yet. Type below — Markdown is rendered as you would expect."));
        ScrollMessengerToEnd();
    }

    /// <summary>
    /// Rewrite one bubble's status line in place.
    /// </summary>
    /// <remarks>
    /// A message queued for an offline peer is stored `pending`, and its bubble
    /// says so. When the peer finally answers, the core emits
    /// `messenger_message_status` — and this is what turns that bubble into
    /// `sent` or `delivered`. Patching the one TextBlock keeps the rest of the
    /// transcript intact, so a text selection or scroll position survives.
    /// </remarks>
    private void UpdateMessengerBubbleStatus(JsonElement update, string messageId)
    {
        foreach (var bubble in MessengerTranscript.Children.OfType<Border>())
        {
            if (bubble.Tag as string != messageId) continue;
            if (bubble.Child is not StackPanel stack) continue;
            var meta = stack.Children.OfType<Grid>()
                .SelectMany(row => row.Children.OfType<TextBlock>())
                .FirstOrDefault(block => block.Name == "MessengerBubbleStatus");
            if (meta is null) continue;
            // The event carries status, not time, so the timestamp rides on the
            // block itself instead of being parsed back out of rendered text.
            var ts = meta.Tag as long? ?? 0;
            meta.Text = Json.String(update, "status") is { Length: > 0 } status
                ? StatusLine(
                    ts,
                    edited: false,
                    status,
                    (int)Json.Long(update, "delivered_to"),
                    (int)Json.Long(update, "total_members"),
                    _messengerConversationKind)
                : meta.Text;
            return;
        }
    }

    private void AppendMessengerBubble(JsonElement message)
    {
        // A message for a conversation that is not the one on screen must not be
        // appended to it: the transcript would show somebody else's message.
        // Its conversation list row still updates, which is what the user needs
        // to notice and open it.
        var conversation = Json.String(message, "conversation_id");
        if (conversation.Length > 0 && conversation != _messengerConversation) return;
        // The empty-state hint is never a message; drop it before the first real
        // bubble and after a conversation switch.
        if (MessengerTranscript.Children.Count == 1
            && MessengerTranscript.Children[0] is TextBlock hint
            && hint.Classes.Contains("muted"))
            MessengerTranscript.Children.Clear();
        MessengerTranscript.Children.Add(BuildMessengerBubble(message));
        ScrollMessengerToEnd();
    }

    private Control BuildMessengerBubble(JsonElement message)
    {
        var outgoing = Json.String(message, "direction") == "outgoing";
        var kind = Json.String(message, "kind", "text");
        var id = Json.String(message, "id");
        var text = Json.String(message, "text");
        var edited = Json.Bool(message, "edited");
        var status = Json.String(message, "status");
        var deliveredTo = (int)Json.Long(message, "delivered_to");
        var total = (int)Json.Long(message, "total_members");
        var ts = Json.Long(message, "ts");

        var stack = new StackPanel { Spacing = 5 };
        var header = new Grid { ColumnDefinitions = new ColumnDefinitions("*,Auto"), ColumnSpacing = 8 };
        var author = new TextBlock
        {
            Text = kind == "system"
                ? "system"
                : (outgoing ? "You" : (Json.String(message, "from_user_id") is { Length: > 10 } from ? from[..10] + "…" : "Peer")),
            FontSize = 10,
            FontWeight = FontWeight.SemiBold,
        };
        author.Classes.Add("muted");
        header.Children.Add(author);
        var meta = new TextBlock
        {
            Text = StatusLine(ts, edited, status, deliveredTo, total, _messengerConversationKind),
            FontSize = 9,
            // Read back by `UpdateMessengerBubbleStatus` to rewrite just the
            // status, without rebuilding the bubble and losing selection.
            Name = "MessengerBubbleStatus",
            Tag = ts,
        };
        meta.Classes.Add("muted");
        Grid.SetColumn(meta, 1);
        header.Children.Add(meta);
        stack.Children.Add(header);

        if (kind == "text" || kind == "edit" || kind == "system")
        {
            // Markdown source is stored; the view renders bold, italic, code,
            // lists, links and fenced blocks exactly like the agent transcript.
            stack.Children.Add(new MarkdownView { Markdown = text, MaxWidth = 760 });
        }
        else
        {
            stack.Children.Add(BuildMessengerMedia(kind, message));
            if (text.Length > 0)
                stack.Children.Add(new MarkdownView { Markdown = text, MaxWidth = 760 });
        }

        if (Json.String(message, "reply_to") is { Length: > 0 } reply)
        {
            var quoted = new Border
            {
                Padding = new Thickness(7, 4),
                CornerRadius = new CornerRadius(4),
                BorderThickness = new Thickness(2, 0, 0, 0),
                BorderBrush = ThemeBrush("#4C8DCE", "#60CDFF"),
                Child = new TextBlock
                {
                    Text = "replying to " + (reply.Length > 10 ? reply[..10] + "…" : reply),
                    FontSize = 10,
                },
            };
            stack.Children.Add(quoted);
        }

        if (outgoing && kind is "text" or "edit")
        {
            var actions = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
            actions.Children.Add(SmallChatAction("Edit", () => { StartMessengerEdit(id, text); return Task.CompletedTask; }));
            actions.Children.Add(SmallChatAction("Reply", () => { _messengerReplyTo = id; MessengerReplyText.Text = "Replying to " + Short(id); RefreshMessengerComposer(); MessengerComposer.Focus(); return Task.CompletedTask; }));
            stack.Children.Add(actions);
        }
        else if (kind is "text" or "edit")
        {
            stack.Children.Add(SmallChatAction("Reply", () => { _messengerReplyTo = id; MessengerReplyText.Text = "Replying to " + Short(id); RefreshMessengerComposer(); MessengerComposer.Focus(); return Task.CompletedTask; }));
        }

        var bubble = new Border
        {
            Child = stack,
            // The status event names a message id, and this is how the bubble
            // finds itself again when that message is finally delivered.
            Tag = id.Length > 0 ? id : null,
            Padding = new Thickness(11, 8),
            CornerRadius = new CornerRadius(9),
            MaxWidth = 800,
            HorizontalAlignment = outgoing ? HorizontalAlignment.Right : HorizontalAlignment.Left,
            Background = outgoing
                ? ThemeBrush("#E7F1FB", "#243447")
                : ThemeBrush("#FFFFFF", "#2B2B2B"),
            BorderBrush = ThemeBrush("#DCE3EC", "#3C3C3C"),
            BorderThickness = new Thickness(1),
        };
        return bubble;
    }

    private static string Short(string value) => value.Length > 10 ? value[..10] + "…" : value;

    private static string StatusLine(long ts, bool edited, string status, int deliveredTo, int total, string kind)
    {
        var when = ts > 0
            ? DateTimeOffset.FromUnixTimeMilliseconds(ts).ToLocalTime().ToString("HH:mm")
            : "";
        var parts = new List<string>();
        if (when.Length > 0) parts.Add(when);
        if (edited) parts.Add("edited");
        if (status == "pending") parts.Add("pending");
        else if (status == "failed") parts.Add("not delivered");
        else if (status == "sent") parts.Add(kind == "group" && total > 0 ? $"sent {deliveredTo}/{total}" : "sent");
        else if (status == "delivered") parts.Add(kind == "group" && total > 0 ? $"delivered {deliveredTo}/{total}" : "delivered");
        else if (status == "read") parts.Add("read");
        return string.Join(" · ", parts);
    }

    private Button SmallChatAction(string label, Func<Task> action)
    {
        var button = new Button { Content = label, Padding = new Thickness(7, 1), MinHeight = 22, FontSize = 10 };
        button.Click += async (_, _) => await RunUiAsync(action);
        return button;
    }

    private Control MutedBubble(string text)
    {
        var block = MutedText(text);
        block.HorizontalAlignment = HorizontalAlignment.Center;
        block.Margin = new Thickness(0, 28, 0, 0);
        return block;
    }

    /// <summary>
    /// Answer buttons for the request that belongs to the conversation on
    /// screen, drawn above its bubbles.
    /// </summary>
    /// <remarks>
    /// The sidebar panel is the primary surface, but it sits in the
    /// conversation list. Somebody who opened a chat is looking at the
    /// transcript, not the list, so the invitation has to be answerable right
    /// there — the same pair of buttons the phone shows. The core writes a
    /// "wants to chat" system note into this conversation, and a note the reader
    /// cannot act on would be a dead end.
    /// </remarks>
    private Control? PendingMessengerRequest()
    {
        if (_messengerConversationKind != "direct") return null;
        var request = _messengerRequests.FirstOrDefault(row =>
            row.Incoming && row.UserId == _messengerConversationTarget);
        if (request is null) return null;
        var body = new StackPanel { Spacing = 6 };
        body.Children.Add(new TextBlock
        {
            Text = $"{request.DisplayName} {request.Badge}",
            FontSize = 11,
            Foreground = ThemeBrush("#5A6B7D", "#9FB4A9"),
        });
        var actions = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
        actions.Children.Add(SmallChatAction("Accept", () => RespondMessengerRequestAsync(request, true)));
        actions.Children.Add(SmallChatAction("Decline", () => RespondMessengerRequestAsync(request, false)));
        body.Children.Add(actions);
        return new Border
        {
            Padding = new Thickness(10),
            CornerRadius = new CornerRadius(7),
            Background = ThemeBrush("#F2F7FF", "#26313D"),
            BorderBrush = ThemeBrush("#D7E3F5", "#3A4757"),
            BorderThickness = new Thickness(1),
            HorizontalAlignment = HorizontalAlignment.Left,
            Child = body,
        };
    }

    /// <summary>
    /// Media, as an attachment card. A photo that has fully arrived is shown;
    /// anything else is a labelled row with size, so a 1 GiB file never tries to
    /// render itself.
    /// </summary>
    private Control BuildMessengerMedia(string kind, JsonElement message)
    {
        var media = message.TryGetProperty("media", out var node) && node.ValueKind == JsonValueKind.Object
            ? node
            : default;
        // One message may carry several files; they share a bubble but keep one
        // card each, in the order they were picked.
        if (media.ValueKind == JsonValueKind.Object
            && media.TryGetProperty("parts", out var parts)
            && parts.ValueKind == JsonValueKind.Array)
        {
            var list = new StackPanel { Spacing = 6 };
            foreach (var part in parts.EnumerateArray())
                list.Children.Add(BuildMessengerMediaCard(kind, part));
            return list;
        }
        return BuildMessengerMediaCard(kind, media);
    }

    private Control BuildMessengerMediaCard(string kind, JsonElement media)
    {
        var name = media.ValueKind == JsonValueKind.Object ? Json.String(media, "file_name", "attachment") : "attachment";
        var size = media.ValueKind == JsonValueKind.Object ? Json.Long(media, "size_bytes") : 0;
        var localPath = media.ValueKind == JsonValueKind.Object ? Json.String(media, "local_path") : "";
        var duration = media.ValueKind == JsonValueKind.Object ? Json.Long(media, "duration_ms") : 0;
        var icon = kind switch { "image" => "🖼", "video" => "🎥", "audio" => "🎤", _ => "📎" };
        var label = kind switch
        {
            "image" => "Photo",
            "video" => "Video",
            "audio" => "Audio",
            _ => "File",
        };
        var details = string.Join(" · ", new[]
        {
            label,
            HumanSize(size),
            duration > 0 ? TimeSpan.FromMilliseconds(duration).ToString(@"m\:ss") : "",
            File.Exists(localPath) ? "saved" : "not downloaded",
        }.Where(part => part.Length > 0));

        var row = new Grid
        {
            ColumnDefinitions = new ColumnDefinitions("Auto,*,Auto"),
            ColumnSpacing = 9,
        };
        row.Children.Add(new TextBlock { Text = icon, FontSize = 22, VerticalAlignment = VerticalAlignment.Center });
        var labels = new StackPanel { Spacing = 1 };
        labels.Children.Add(new TextBlock { Text = name, FontWeight = FontWeight.SemiBold, TextTrimming = TextTrimming.CharacterEllipsis, MaxWidth = 460 });
        labels.Children.Add(MutedText(details));
        Grid.SetColumn(labels, 1);
        row.Children.Add(labels);
        if (File.Exists(localPath))
        {
            var open = new Button { Content = "Open", Padding = new Thickness(8, 2), MinHeight = 24 };
            open.Click += (_, _) => OpenLocalFile(localPath);
            Grid.SetColumn(open, 2);
            row.Children.Add(open);
        }
        else
        {
            var progress = new TextBlock
            {
                Text = "…",
                FontSize = 12,
                VerticalAlignment = VerticalAlignment.Center,
            };
            progress.Classes.Add("muted");
            Grid.SetColumn(progress, 2);
            row.Children.Add(progress);
        }
        return row;
    }

    private static string HumanSize(long bytes) => bytes switch
    {
        <= 0 => "",
        < 1024 => bytes + " B",
        < 1024 * 1024 => (bytes / 1024.0).ToString("0.0") + " KiB",
        < 1024L * 1024 * 1024 => (bytes / (1024.0 * 1024)).ToString("0.0") + " MiB",
        _ => (bytes / (1024.0 * 1024 * 1024)).ToString("0.00") + " GiB",
    };

    private void OpenLocalFile(string path)
    {
        try
        {
            var info = new System.Diagnostics.ProcessStartInfo(path) { UseShellExecute = true };
            System.Diagnostics.Process.Start(info)?.Dispose();
        }
        catch (Exception error)
        {
            ShowError("Could not open the file: " + error.Message);
        }
    }

    private void ScrollMessengerToEnd() =>
        Dispatcher.UIThread.Post(() => MessengerScroll.ScrollToEnd(), DispatcherPriority.Background);

    // -----------------------------------------------------------------------
    // Composer and actions
    // -----------------------------------------------------------------------

    private async void MessengerSend_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        await SendMessengerComposerAsync();
    });

    private async void MessengerComposer_KeyDown(object? sender, KeyEventArgs e) => await RunUiAsync(async () =>
    {
        if (e.Key == Key.Escape)
        {
            CancelMessengerEdit();
            e.Handled = true;
            return;
        }
        if (e.Key != Key.Enter) return;
        if (e.KeyModifiers.HasFlag(KeyModifiers.Shift)) return;
        e.Handled = true;
        await SendMessengerComposerAsync();
    });

    private async Task<bool> SendMessengerComposerAsync()
    {
        if (!await MessengerRequireAccountAsync()) return false;
        if (_messengerConversation.Length == 0)
        {
            ShowNotice("Open a conversation first.");
            return false;
        }
        var text = MessengerComposer.Text ?? "";
        if (_messengerEditing.Length > 0)
        {
            var newText = text.Trim();
            if (newText.Length == 0) { ShowNotice("An edit cannot be empty."); return false; }
            await SendAsync(new()
            {
                ["op"] = "messenger_edit_message",
                ["account_id"] = _messengerAccount,
                ["id"] = _messengerEditing,
                ["new_text"] = newText,
            });
            CancelMessengerEdit();
            return true;
        }
        if (_messengerAttachments.Count > 0)
        {
            // One message carries every staged file; the core streams them one
            // after another in this order and the peer renders one bubble.
            await SendAsync(new()
            {
                ["op"] = "messenger_send_media",
                ["account_id"] = _messengerAccount,
                ["to"] = _messengerConversationTarget,
                ["paths"] = _messengerAttachments.Select(file => file.Path).ToArray(),
                ["caption"] = text.Trim().Length > 0 ? text.Trim() : null,
                ["reply_to"] = _messengerReplyTo.Length > 0 ? _messengerReplyTo : null,
                ["group"] = _messengerConversationKind == "group",
            });
            _messengerAttachments.Clear();
            MessengerComposer.Text = "";
            ClearMessengerReply();
            RefreshMessengerComposer();
            return true;
        }
        var trimmed = text.Trim();
        if (trimmed.Length == 0) return false;
        await SendAsync(new()
        {
            ["op"] = "messenger_send_text",
            ["account_id"] = _messengerAccount,
            ["to"] = _messengerConversationTarget,
            ["text"] = trimmed,
            ["reply_to"] = _messengerReplyTo.Length > 0 ? _messengerReplyTo : null,
            ["group"] = _messengerConversationKind == "group",
        });
        MessengerComposer.Text = "";
        ClearMessengerReply();
        RefreshMessengerComposer();
        return true;
    }

    private void StartMessengerEdit(string id, string text)
    {
        _messengerEditing = id;
        MessengerComposer.Text = text;
        RefreshMessengerComposer();
        MessengerComposer.Focus();
        MessengerComposer.CaretIndex = MessengerComposer.Text?.Length ?? 0;
    }

    private void CancelMessengerEdit()
    {
        if (_messengerEditing.Length == 0) return;
        _messengerEditing = "";
        if (_messengerAttachments.Count == 0) MessengerComposer.Text = "";
        RefreshMessengerComposer();
    }

    private void ClearMessengerReply()
    {
        _messengerReplyTo = "";
        RefreshMessengerComposer();
    }

    private async void MessengerCancelReply_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        ClearMessengerReply();
        await Task.CompletedTask;
    });

    private async void MessengerRemoveAttachment_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        _messengerAttachments.Clear();
        RefreshMessengerComposer();
        await Task.CompletedTask;
    });

    private async void MessengerEdit_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        if (_messengerEditing.Length == 0) return;
        await SendMessengerComposerAsync();
    });

    private async void MessengerAttach_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        if (!await MessengerRequireAccountAsync()) return;
        var files = await StorageProvider.OpenFilePickerAsync(new FilePickerOpenOptions
        {
            Title = "Attach a photo, video, audio file or document",
            // Any type, any number, in one message.
            AllowMultiple = true,
        });
        if (files.Count == 0) return;
        var account = _messengerAccountById.GetValueOrDefault(_messengerAccount);
        var limit = account?.Pro == true ? 2L * 1024 * 1024 * 1024 : 100L * 1024 * 1024;
        var rejected = new List<string>();
        foreach (var file in files)
        {
            var path = file.TryGetLocalPath();
            if (string.IsNullOrEmpty(path)) { rejected.Add(file.Name + " (not a local file)"); continue; }
            var info = new FileInfo(path);
            if (!info.Exists) { rejected.Add(file.Name + " (missing)"); continue; }
            if (info.Length > limit)
            {
                rejected.Add($"{file.Name} ({HumanSize(info.Length)})");
                continue;
            }
            if (_messengerAttachments.Any(staged => string.Equals(staged.Path, path, StringComparison.Ordinal)))
                continue;
            _messengerAttachments.Add(new AttachedFile(path, file.Name));
        }
        if (rejected.Count > 0)
            ShowError($"Not attached: {string.Join(", ", rejected)}. This account allows "
                      + $"{HumanSize(limit)} per file; raise it in My profile.");
        RefreshMessengerComposer();
        MessengerComposer.Focus();
    });

    private async void MessengerCancelTransfer_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        // The core owns delivery, so cancelling here means "stop showing this and
        // stop resuming it": the chunk in flight still completes, which is why
        // the notice says the file stays resumable instead of claiming an abort.
        if (_messengerActiveTransfer.Length > 0)
            _messengerCancelledTransfers.Add(_messengerActiveTransfer);
        _messengerActiveTransfer = "";
        MessengerTransferBar.IsVisible = false;
        ShowNotice("The transfer stops after the chunk in flight. The file stays resumable.");
        await Task.CompletedTask;
    });

    // -----------------------------------------------------------------------
    // Contacts, profile, groups
    // -----------------------------------------------------------------------

    private string _messengerInviteJson = "";
    private int _messengerGroupCount;
    private const int MessengerGroupLimit = 300;

    private async void MessengerAddContact_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        if (!await MessengerRequireAccountAsync()) return;
        var dialog = CreateDialog("Add contact", 620, 560);
        var body = new StackPanel { Spacing = 11, Margin = new Thickness(22) };
        body.Children.Add(DialogHeading("Add contact",
            "Scan the other person's QR code, or paste the invite they sent you. The invite is signed by their identity."));
        var invite = new TextBox
        {
            Watermark = "Paste the invite JSON",
            AcceptsReturn = true,
            TextWrapping = TextWrapping.Wrap,
            MaxHeight = 150,
        };
        var feedback = MutedText("Nothing is sent anywhere until you press Add.");
        // Big from the start: this is the code another phone has to read.
        var qr = new Image
        {
            MaxWidth = 520,
            MaxHeight = 520,
            Stretch = Stretch.Uniform,
            HorizontalAlignment = HorizontalAlignment.Center,
        };
        body.Children.Add(invite);
        body.Children.Add(ButtonRow(
            ActionButton("Add", async () =>
            {
                var text = (invite.Text ?? "").Trim();
                if (text.Length == 0) { feedback.Text = "Paste an invite first."; return; }
                try
                {
                    using var parsed = JsonDocument.Parse(text);
                    var name = Json.String(parsed.RootElement, "name", "contact");
                    await SendAsync(new() { ["op"] = "messenger_add_contact", ["account_id"] = _messengerAccount, ["invite"] = text });
                    // Scanning asks; it does not add. The request appears in the
                    // panel above the conversation list until the other device
                    // accepts, and the notice from the core says which happened.
                    feedback.Text = "Request sent to " + name
                        + ". Their invite signature checked out; you can write now —"
                        + " messages wait in the queue until they accept.";
                }
                catch (JsonException) { feedback.Text = "That is not a messenger invite."; }
            }),
            ActionButton("Show my QR", async () =>
            {
                // Build the invite from the account we already have instead of
                // waiting for an event round trip: the fields are the same ones
                // the core would put in it.
                var payload = BuildInvite(_messengerAccountById.GetValueOrDefault(_messengerAccount));
                if (payload.Length == 0)
                {
                    feedback.Text = "Tor is still publishing this account's onion. Try again once it shows online.";
                    return;
                }
                _messengerInviteJson = payload;
                qr.Source = CreateQrBitmap(payload);
                feedback.Text = "Scan this with GnomeAI on the other device.";
            }),
            ActionButton("Enlarge QR", async () =>
                await ShowEnlargedQrAsync(
                    BuildInvite(_messengerAccountById.GetValueOrDefault(_messengerAccount)),
                    "Scan this chat invite")),
            ActionButton("Copy invite", () => CopyTextAsync(CurrentInvite())),
            ActionButton("Close", () => { dialog.Close(); return Task.CompletedTask; })));
        body.Children.Add(feedback);
        body.Children.Add(qr);
        dialog.Content = new ScrollViewer { Content = body };
        await dialog.ShowDialog(this);
    });

    private async void MessengerProfile_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        if (!await MessengerRequireAccountAsync()) return;
        var account = _messengerAccountById.GetValueOrDefault(_messengerAccount);
        var dialog = CreateDialog("My profile", 660, 700);
        var body = new StackPanel { Spacing = 11, Margin = new Thickness(22) };
        body.Children.Add(DialogHeading(account?.DisplayName ?? "Profile",
            "Identity, onion and local limits for this account. Switching accounts never touches the device pairing."));
        var name = new TextBox { Text = account?.DisplayName ?? "", Watermark = "Display name" };
        body.Children.Add(Labeled("Display name", name));
        body.Children.Add(Labeled("User id (identity key)", ReadOnlyValue(account?.UserId ?? "")));
        body.Children.Add(Labeled("Identity prekey", ReadOnlyValue(account?.IdentityPk ?? "")));
        body.Children.Add(Labeled("Signed prekey", ReadOnlyValue(account?.SignedPrekey ?? "")));
        body.Children.Add(Labeled("Onion (this account only)", ReadOnlyValue(
            (account?.Onion ?? "") is { Length: > 0 } onion ? onion : "Tor is still publishing it…")));
        body.Children.Add(MutedText(
            "This onion belongs to this account alone. It is not the device-mesh onion, and no messenger key is derived from a device key."));

        // Inside the profile the code is already fully readable; the shrink only
        // applies when the dialog has to fit a small window.
        var qr = new Image
        {
            MaxWidth = 520,
            MaxHeight = 520,
            Stretch = Stretch.Uniform,
            HorizontalAlignment = HorizontalAlignment.Center,
        };
        body.Children.Add(qr);
        body.Children.Add(ButtonRow(
            ActionButton("Show QR", async () =>
            {
                var payload = BuildInvite(account);
                if (payload.Length == 0) return;
                _messengerInviteJson = payload;
                qr.Source = CreateQrBitmap(payload);
                await Task.CompletedTask;
            }),
            ActionButton("Enlarge QR", async () =>
                await ShowEnlargedQrAsync(BuildInvite(account), "Scan this chat invite")),
            ActionButton("Copy invite", () => CopyTextAsync(CurrentInvite()))));

        var pro = new CheckBox
        {
            Content = "Local Pro tier (up to 2 GiB per file, more accounts)",
            IsChecked = account?.Pro == true,
        };
        body.Children.Add(pro);
        body.Children.Add(MutedText(
            "A local policy, not a subscription: no server is contacted and no account is validated."));

        var chunk = new ComboBox
        {
            ItemsSource = new[] { "64 KiB", "256 KiB", "1 MiB" },
            SelectedIndex = (account?.ChunkSize ?? 262144) switch
            {
                <= 65536 => 0,
                >= 1048576 => 2,
                _ => 1,
            },
            HorizontalAlignment = HorizontalAlignment.Left,
            MinWidth = 160,
        };
        body.Children.Add(Labeled("Chunk size for large transfers", chunk));

        body.Children.Add(ButtonRow(
            ActionButton("Save", async () =>
            {
                var trimmed = (name.Text ?? "").Trim();
                if (trimmed.Length > 0)
                    await SendAsync(new() { ["op"] = "messenger_set_profile", ["account_id"] = _messengerAccount, ["display_name"] = trimmed });
                var bytes = chunk.SelectedIndex switch { 0 => 65536, 2 => 1048576, _ => 262144 };
                await SendAsync(new()
                {
                    ["op"] = "messenger_set_tier",
                    ["account_id"] = _messengerAccount,
                    ["pro"] = pro.IsChecked == true,
                    ["chunk_size"] = bytes,
                });
                await RefreshMessengerAccountsAsync();
                dialog.Close();
            }),
            ActionButton("Delete this account", async () =>
            {
                if (account is null) return;
                if (!await ConfirmAsync("Delete account",
                    "Deleting this messenger account removes its identity, its onion and every local message. Peers can never reach it again. This cannot be undone."))
                    return;
                await SendAsync(new() { ["op"] = "messenger_delete_account", ["account_id"] = account.AccountId });
                dialog.Close();
            }),
            ActionButton("Close", () => { dialog.Close(); return Task.CompletedTask; })));
        dialog.Content = new ScrollViewer { Content = body };
        await dialog.ShowDialog(this);
    });

    private static TextBox ReadOnlyValue(string value) => new()
    {
        Text = value,
        IsReadOnly = true,
        TextWrapping = TextWrapping.Wrap,
        FontFamily = FontFamily.Parse("monospace"),
        FontSize = 11,
    };

    /// <summary>
    /// The invite the QR encodes, assembled from the account view. An onion is
    /// required: an invite without one cannot be reached, and the core would
    /// reject it as malformed anyway.
    /// </summary>
    private static string BuildInvite(MessengerAccountItem? account)
    {
        if (account is null || account.Onion.Length == 0) return "";
        return JsonSerializer.Serialize(new Dictionary<string, object?>
        {
            ["v"] = 1,
            ["user_id"] = account.UserId,
            ["identity_pk"] = account.IdentityPk,
            ["identity_public"] = account.IdentityPublic,
            ["signed_prekey"] = account.SignedPrekey,
            ["spk_sig"] = account.PrekeySignature,
            ["onion"] = account.Onion,
            ["name"] = account.DisplayName,
        });
    }

    /// <summary>
    /// The invite to copy: rebuilt from the live account so it can never be a
    /// stale payload from another account or an earlier session.
    /// </summary>
    private string CurrentInvite() =>
        BuildInvite(_messengerAccountById.GetValueOrDefault(_messengerAccount)) is { Length: > 0 } invite
            ? invite
            : _messengerInviteJson;

    private async void MessengerGroups_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        if (!await MessengerRequireAccountAsync()) return;
        var dialog = CreateDialog("Groups", 760, 620);
        var body = new StackPanel { Spacing = 10, Margin = new Thickness(22) };
        body.Children.Add(DialogHeading("Groups",
            $"Up to {MessengerGroupLimit} members. Each message is sealed with the sender's key and delivered to every member over that member's own onion."));
        var list = new StackPanel { Spacing = 8 };
        async Task Refresh()
        {
            list.Children.Clear();
            await SendAsync(new() { ["op"] = "messenger_list_groups", ["account_id"] = _messengerAccount });
            foreach (var pair in _messengerOpen.Where(entry => entry.Value.Kind == "group").ToArray())
            {
                var id = pair.Key;
                var title = pair.Value.Title;
                list.Children.Add(Surface(new StackPanel
                {
                    Spacing = 5,
                    Children =
                    {
                        new TextBlock { Text = title, FontSize = 16 },
                        MutedText("Sender-key group · opened from the chat list"),
                        ActionButton("Open", async () => { dialog.Close(); await OpenMessengerConversationAsync(id); }),
                        ActionButton("Members", async () => { dialog.Close(); await ShowGroupMembersAsync(id); }),
                    },
                }));
            }
            if (_messengerGroupCount == 0)
                list.Children.Add(MutedText("No groups yet. Create one and add contacts to it."));
        }
        body.Children.Add(ActionButton("Create group", async () =>
        {
            var name = await PromptAsync("New group", "Group name", "");
            if (name is null || name.Trim().Length == 0) return;
            var members = await PickGroupMembersAsync(new HashSet<string>());
            if (members is null) return;
            await SendAsync(new()
            {
                ["op"] = "messenger_create_group",
                ["account_id"] = _messengerAccount,
                ["name"] = name.Trim(),
                ["member_ids"] = members.ToArray(),
            });
            await Refresh();
        }));
        body.Children.Add(list);
        dialog.Content = new ScrollViewer { Content = body };
        await dialog.ShowDialog(this);
        await Refresh();
    });

    /// <summary>
    /// Choose group members from the contacts this account already knows.
    /// Returns null when the user cancelled, and an empty list when they chose
    /// nobody — those are different answers and the caller must not merge them.
    /// </summary>
    private async Task<List<string>?> PickGroupMembersAsync(HashSet<string> already)
    {
        var dialog = CreateDialog("Group members", 560, 560);
        var body = new StackPanel { Spacing = 8, Margin = new Thickness(22) };
        body.Children.Add(DialogHeading("Choose members",
            $"Up to {MessengerGroupLimit - 1} contacts beside you. You are always a member."));
        var result = new TaskCompletionSource<List<string>?>();
        var boxes = new List<(string Id, CheckBox Box)>();
        foreach (var id in _messengerContactIds)
        {
            if (already.Contains(id)) continue;
            var box = new CheckBox { Content = _messengerContactNames.GetValueOrDefault(id, Short(id)) };
            boxes.Add((id, box));
            body.Children.Add(box);
        }
        if (boxes.Count == 0)
            body.Children.Add(MutedText("Add a contact first: a group can only include people you already verified."));
        body.Children.Add(ButtonRow(
            ActionButton("Cancel", () => { result.TrySetResult(null); dialog.Close(); return Task.CompletedTask; }),
            ActionButton("Done", () =>
            {
                result.TrySetResult(boxes.Where(pair => pair.Box.IsChecked == true)
                    .Select(pair => pair.Id).ToList());
                dialog.Close();
                return Task.CompletedTask;
            })));
        dialog.Content = new ScrollViewer { Content = body };
        dialog.Closed += (_, _) => result.TrySetResult(null);
        await dialog.ShowDialog(this);
        return await result.Task;
    }

    /// <summary>
    /// Add or remove members of an existing group. Only an admin may do either,
    /// and removal rotates every sender chain for that group.
    /// </summary>
    private async Task ShowGroupMembersAsync(string groupId)
    {
        var dialog = CreateDialog("Group members", 620, 620);
        var body = new StackPanel { Spacing = 9, Margin = new Thickness(22) };
        var name = _messengerGroupNames.GetValueOrDefault(groupId, "Group");
        body.Children.Add(DialogHeading(name, "Members and admins. Removing a member rotates every sender key."));
        var members = _messengerGroupMembers.GetValueOrDefault(groupId) ?? [];
        var admins = _messengerGroupAdmins.GetValueOrDefault(groupId) ?? [];
        var me = _messengerAccountById.GetValueOrDefault(_messengerAccount)?.UserId ?? "";
        body.Children.Add(MutedText($"{members.Count} of {MessengerGroupLimit} members"));
        foreach (var (userId, memberName) in members)
        {
            var row = new Grid { ColumnDefinitions = new ColumnDefinitions("*,Auto,Auto"), ColumnSpacing = 8 };
            var label = new TextBlock
            {
                Text = memberName + (userId == me ? " (you)" : "") + (admins.Contains(userId) ? " · admin" : ""),
                VerticalAlignment = VerticalAlignment.Center,
            };
            row.Children.Add(label);
            if (userId != me && admins.Contains(me))
            {
                var remove = new Button { Content = "Remove", Padding = new Thickness(8, 2), MinHeight = 24 };
                remove.Click += async (_, _) => await RunUiAsync(async () =>
                {
                    await SendAsync(new()
                    {
                        ["op"] = "messenger_remove_group_member",
                        ["account_id"] = _messengerAccount,
                        ["group_id"] = groupId,
                        ["user_id"] = userId,
                    });
                    dialog.Close();
                });
                Grid.SetColumn(remove, 2);
                row.Children.Add(remove);
            }
            body.Children.Add(Surface(row));
        }
        if (admins.Contains(me))
        {
            body.Children.Add(ActionButton("Add members", async () =>
            {
                var chosen = await PickGroupMembersAsync(members.Select(member => member.UserId).ToHashSet());
                if (chosen is null || chosen.Count == 0) return;
                await SendAsync(new()
                {
                    ["op"] = "messenger_add_group_members",
                    ["account_id"] = _messengerAccount,
                    ["group_id"] = groupId,
                    ["member_ids"] = chosen.ToArray(),
                });
                dialog.Close();
            }));
        }
        body.Children.Add(ButtonRow(ActionButton("Close", () => { dialog.Close(); return Task.CompletedTask; })));
        dialog.Content = new ScrollViewer { Content = body };
        await dialog.ShowDialog(this);
    }

    private async void MessengerHelp_Click(object? sender, RoutedEventArgs e) => await RunUiAsync(async () =>
    {
        var dialog = CreateDialog("Messenger", 700, 620);
        var body = new StackPanel { Spacing = 10, Margin = new Thickness(22) };
        body.Children.Add(DialogHeading("How Chat works",
            "End-to-end encrypted, peer-to-peer, and nothing in the middle."));
        foreach (var line in new[]
        {
            "There is no server, no relay and no store-and-forward anywhere on the network.",
            "Direct messages start with X3DH and continue with a Double Ratchet, so every message gets its own key.",
            "Groups use sender keys: a member's message is sealed with that member's key, and the key itself travels only inside a 1:1 envelope.",
            $"A group holds up to {MessengerGroupLimit} members. The sender delivers to each one and the transcript shows \"delivered X/Y\".",
            "Offline peers are normal: a message stays Pending locally and is flushed when their onion answers again.",
            "Files travel on the same Tor connection, sealed chunk by chunk with their own key. Standard allows 100 MiB per file; the local Pro tier raises it to 2 GiB. Both are app policy, never a server limit.",
        })
        {
            body.Children.Add(MutedText("• " + line));
        }
        body.Children.Add(ButtonRow(ActionButton("Close", () => { dialog.Close(); return Task.CompletedTask; })));
        dialog.Content = new ScrollViewer { Content = body };
        await dialog.ShowDialog(this);
    });

    // -----------------------------------------------------------------------
    // Events from the core
    // -----------------------------------------------------------------------

    private bool HandleMessengerEvent(JsonElement node, string kind)
    {
        switch (kind)
        {
            case "messenger_accounts_changed":
                if (node.TryGetProperty("accounts", out var accounts)) RenderMessengerAccounts(accounts);
                return true;
            case "messenger_conversations":
                if (node.TryGetProperty("conversations", out var conversations))
                    RenderMessengerConversations(Json.String(node, "account_id"), conversations);
                return true;
            case "messenger_history":
                if (node.TryGetProperty("messages", out var messages))
                    RenderMessengerHistory(Json.String(node, "account_id"), Json.String(node, "conversation_id"), messages);
                return true;
            case "messenger_groups":
                if (node.TryGetProperty("groups", out var groups)) RenderMessengerGroups(groups);
                return true;
            case "messenger_contacts":
                if (node.TryGetProperty("contacts", out var contacts))
                    RenderMessengerContacts(Json.String(node, "account_id"), contacts);
                return true;
            case "messenger_contact_requests":
                // A scanned QR is a question, not consent: the other device shows
                // this and only writes the contact when someone answers.
                // Requests are per account, so one account's list must never be
                // rendered while another account is selected.
                if (Json.String(node, "account_id") is { Length: > 0 } requestAccount
                    && requestAccount != _messengerAccount) return true;
                _messengerRequests.Clear();
                if (node.TryGetProperty("requests", out var requests))
                    foreach (var request in requests.EnumerateArray())
                    {
                        var parsed = MessengerContactRequest.FromJson(request);
                        if (parsed.UserId.Length > 0) _messengerRequests.Add(parsed);
                    }
                RenderMessengerRequests();
                // The answer card also lives in the open transcript. A request
                // that arrives while a chat is on screen has to show up there,
                // not only in the sidebar list behind it.
                if (_messengerConversation.Length > 0 && _messengerConversationKind == "direct")
                    _ = RunUiAsync(async () => await SendAsync(new()
                    {
                        ["op"] = "messenger_get_messages",
                        ["account_id"] = _messengerAccount,
                        ["conversation_id"] = _messengerConversation,
                    }));
                return true;
            case "messenger_invite":
                if (node.TryGetProperty("payload", out var payload))
                {
                    _messengerInviteJson = payload.GetRawText();
                    MessengerStatus.Text = "Invite ready · scan or copy it";
                }
                return true;
            case "messenger_message_received":
            case "messenger_group_message_received":
                if (node.TryGetProperty("message", out var received)) AppendMessengerBubble(received);
                return true;
            case "messenger_message_status":
                // The row itself has to change, not only the status bar. A
                // message that was queued shows "pending" in its bubble, and
                // without this the bubble kept claiming it forever even after
                // the peer had it — the one place the user actually looks.
                if (Json.String(node, "message_id") is { Length: > 0 } updatedId)
                    UpdateMessengerBubbleStatus(node, updatedId);
                if (Json.String(node, "status") is "delivered" or "sent")
                {
                    var total = Json.Long(node, "total_members");
                    var delivered = Json.Long(node, "delivered_to");
                    if (total > 0) MessengerStatus.Text = $"Delivered to {delivered}/{total}";
                }
                return true;
            case "messenger_message_edited":
                MessengerStatus.Text = "Message edited";
                return true;
            case "messenger_peer_online":
                SetMessengerBanner($"{Short(Json.String(node, "user_id"))} is online.");
                return true;
            case "messenger_peer_offline":
                SetMessengerBanner($"{Short(Json.String(node, "user_id"))} is offline. Messages stay queued and go out when they return.");
                return true;
            case "messenger_media_progress":
                RenderMessengerProgress(node);
                return true;
            case "messenger_group_member_update":
                SetMessengerBanner("Group membership changed; sender keys were rotated.");
                return true;
            default:
                return false;
        }
    }

    private void RenderMessengerGroups(JsonElement groups)
    {
        _messengerGroupCount = 0;
        foreach (var group in groups.EnumerateArray())
        {
            _messengerGroupCount++;
            var id = Json.String(group, "group_id");
            var name = Json.String(group, "name", "Group");
            _messengerTitles[$"grp:{_messengerAccount}:{id}"] = name;
            _messengerOpen[$"grp:{_messengerAccount}:{id}"] = ("group", id, name);
            _messengerGroupMembers[id] = group.TryGetProperty("members", out var members)
                ? members.EnumerateArray()
                    .Select(member => (Json.String(member, "user_id"), Json.String(member, "display_name", "member")))
                    .Where(pair => pair.Item1.Length > 0)
                    .ToList()
                : [];
            _messengerGroupNames[id] = name;
            _messengerGroupAdmins[id] = group.TryGetProperty("admins", out var admins)
                ? admins.EnumerateArray().Select(Json.Element).Where(value => value.Length > 0).ToList()
                : [];
        }
    }

    private readonly Dictionary<string, List<(string UserId, string Name)>> _messengerGroupMembers = [];
    private readonly Dictionary<string, string> _messengerGroupNames = [];
    private readonly Dictionary<string, List<string>> _messengerGroupAdmins = [];
    /// Invites that arrived from another device and still need an answer.
    private readonly List<MessengerContactRequest> _messengerRequests = [];

    private void RenderMessengerContacts(string accountId, JsonElement contacts)
    {
        if (accountId != _messengerAccount) return;
        _messengerContactNames.Clear();
        _messengerContactIds.Clear();
        foreach (var contact in contacts.EnumerateArray())
        {
            var id = Json.String(contact, "user_id");
            if (id.Length == 0) continue;
            _messengerContactIds.Add(id);
            _messengerContactNames[id] = Json.String(contact, "display_name",
                id.Length > 10 ? id[..10] + "…" : id);
        }
    }

    private void RenderMessengerProgress(JsonElement node)
    {
        var status = Json.String(node, "status");
        var received = Json.Long(node, "received");
        var total = Math.Max(1, Json.Long(node, "total"));
        var name = Json.String(node, "file_name", "attachment");
        var direction = Json.String(node, "direction", "outgoing");
        var transferId = Json.String(node, "transfer_id");
        // A cancelled transfer stays cancelled: later progress for the same id
        // must not reopen the bar the user just dismissed.
        if (transferId.Length > 0 && _messengerCancelledTransfers.Contains(transferId)) return;
        if (status is "active" && transferId.Length > 0) _messengerActiveTransfer = transferId;
        MessengerTransferBar.IsVisible = status is "active" or "interrupted";
        MessengerTransferProgress.Maximum = total;
        MessengerTransferProgress.Value = Math.Min(received, total);
        var percent = (int)(received * 100 / total);
        MessengerTransferLabel.Text = status switch
        {
            "complete" => $"{(direction == "incoming" ? "Received" : "Sent")} {name}",
            "failed" => $"Could not transfer {name}",
            "interrupted" => $"Interrupted at {percent}% · {name}",
            _ => $"{(direction == "incoming" ? "Receiving" : "Sending")} {name} · {percent}%",
        };
        MessengerStatus.Text = MessengerTransferLabel.Text;
    }

    private readonly HashSet<string> _messengerCancelledTransfers = [];

    private void SetMessengerBanner(string text)
    {
        MessengerBanner.IsVisible = true;
        MessengerBannerText.Text = text;
    }
}
