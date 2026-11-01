using System.Text.Json;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Platform.Storage;
using GnomeAI.Client;
using CheckBox = Avalonia.Controls.CheckBox;
using Orientation = Avalonia.Layout.Orientation;

namespace GnomeAI.Android.UI;

/// <summary>
/// The messenger on a phone: the Android counterpart of the desktop Chat tab.
/// </summary>
/// <remarks>
/// A separate screen rather than a shared view, because the mobile shell has its
/// own navigation, insets and density. What *is* shared is the core: the same
/// `messenger_*` operations, the same accounts, the same onions and the same
/// on-disk history, so a message sent here arrives on the PC and the reverse.
///
/// There is no server and no relay. An undeliverable message stays Pending in
/// the core's outbox and is flushed when the peer's onion answers again, so this
/// view only ever renders the core's truth.
/// </remarks>
public sealed partial class MainView
{
    private readonly List<MessengerAccountInfo> _accounts = [];
    private readonly List<MessengerConversationInfo> _conversations = [];
    private readonly Dictionary<string, (string Kind, string Target, string Title)> _conversationIndex = [];
    private readonly List<(string UserId, string Name)> _contacts = [];
    /// <summary>Scanned invites that still need this device's answer.</summary>
    private readonly List<MessengerContactRequest> _contactRequests = [];
    private string _account = "";
    private string _conversation = "";
    private string _conversationKind = "direct";
    private string _conversationTarget = "";
    private string _replyTo = "";
    private string _editing = "";
    /// <summary>Files staged for the open chat; any number, any type.</summary>
    private readonly List<string> _chatAttachments = [];
    private string _invite = "";
    private bool _chatsScreen;
    /// <summary>
    /// True while the Messages *home* screen is the visible surface. Only then
    /// do messenger events re-render it; sub-screens (add contact, groups…) and
    /// other tabs are never disturbed.
    /// </summary>
    private bool _chatsHome;
    /// <summary>A home request round is in flight; repeats coalesce into one.</summary>
    private bool _chatsRendering, _chatsRenderAgain;
    /// <summary>
    /// Raw JSON of the last rendered accounts/conversations payloads. Presence
    /// chatter re-emits identical lists; re-rendering those would needlessly
    /// rebuild the panel under the user's finger.
    /// </summary>
    private string _lastAccountsJson = "", _lastConversationsJson = "";
    /// <summary>
    /// The rawest record seen for each message id, so a status event can
    /// rebuild one bubble without refetching the whole history.
    /// </summary>
    private readonly Dictionary<string, JsonElement> _lastRecords = new();

    // -----------------------------------------------------------------------
    // The conversation list
    // -----------------------------------------------------------------------

    /// <summary>
    /// Enter the Messages home screen: fire the request round once, then let
    /// the core's events draw the result. Events never call back in here.
    /// </summary>
    private async Task ShowChatsHomeAsync()
    {
        ShowPanel();
        _chatsScreen = true;
        _chatsHome = true;
        // Fresh-entry markers: the first accounts/conversations events must
        // render even if the payloads match the previous visit, because the
        // panel was just cleared.
        _lastAccountsJson = "";
        _lastConversationsJson = "";
        SelectTab("Messages");
        if (_disposed) return;
        // Draw immediately from whatever local state is already known, so the
        // screen never sits empty while the round trip runs (and still shows
        // the create-account card on a first run even if the core is slow).
        RenderMessengerHome();
        if (_chatsRendering) { _chatsRenderAgain = true; return; }
        _chatsRendering = true;
        try
        {
            if (_account.Length == 0)
            {
                await SendMessengerAsync(new() { ["op"] = "messenger_list_accounts" });
                return;
            }
            // switch_account re-emits accounts, conversations and groups in one
            // round trip, and list_conversations re-answers conversations, so
            // the render below draws fresh state without any event calling
            // back into this method (the old design looped forever).
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_switch_account",
                ["account_id"] = _account,
            });
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_list_conversations",
                ["account_id"] = _account,
            });
            // Requests arrive over the onion while this tab is elsewhere; ask for
            // the current list on every entry so nothing waits unseen.
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_list_contact_requests",
                ["account_id"] = _account,
            });
        }
        finally
        {
            _chatsRendering = false;
            // Re-run only when the Messages screen is still the visible one:
            // the user may have left while this round was in flight, and
            // re-firing here would pull them back to it.
            if (_chatsRenderAgain && _chatsHome && !_disposed)
            {
                _chatsRenderAgain = false;
                _ = ShowChatsHomeAsync();
            }
        }
    }

    /// <summary>
    /// Draw the Messages home screen from local state only. Called by the
    /// core's messenger events; issues no requests of its own.
    /// </summary>
    private void RenderMessengerHome()
    {
        if (!_chatsHome || _disposed) return;
        ShowPanel();
        _chatsScreen = true;
        _chatsHome = true;
        SelectTab("Messages");
        _panel.Children.Add(Heading("Messages",
            "End-to-end encrypted over Tor. No server, no relay."));
        if (_accounts.Count == 0)
        {
            _panel.Children.Add(Card(new StackPanel
            {
                Spacing = 10,
                Children =
                {
                    new TextBlock
                    {
                        Text = "Create a messenger account to get an encrypted identity and its own onion address.",
                        TextWrapping = TextWrapping.Wrap,
                    },
                    Button("Create account", CreateAccountAsync),
                },
            }));
            _status.Text = "No messenger account yet";
            return;
        }
        _panel.Children.Add(AccountPicker());
        // A scanned QR is a question for this device, so it is answered here,
        // before any conversation is opened.
        foreach (var request in _contactRequests.ToArray())
        {
            var asking = new StackPanel { Spacing = 8 };
            asking.Children.Add(new TextBlock
            {
                Text = request.DisplayName,
                FontWeight = FontWeight.SemiBold,
                TextWrapping = TextWrapping.Wrap,
            });
            asking.Children.Add(new TextBlock
            {
                Text = request.Badge,
                FontSize = 12,
                Foreground = Ink("#9FB4A9"),
            });
            if (request.Incoming)
            {
                asking.Children.Add(Button("Accept", () => RespondRequestAsync(request, true)));
                asking.Children.Add(Button("Decline", () => RespondRequestAsync(request, false)));
            }
            _panel.Children.Add(Card(asking));
        }
        _panel.Children.Add(Card(new StackPanel
        {
            Spacing = 10,
            Children =
            {
                Heading("Conversations", _conversations.Count == 0
                    ? "Add a contact with their QR or invite to start a chat."
                    : $"{_conversations.Count} chat(s)"),
                Button("Add contact", AddContactAsync),
                Button("Groups", ShowGroupsAsync),
                Button("My profile and QR", ShowProfileAsync),
            },
        }));
        foreach (var conversation in _conversations)
            _panel.Children.Add(ConversationCard(conversation));
        _status.Text = _accounts.FirstOrDefault(a => a.AccountId == _account)?.ToString() ?? "Messenger";
    }

    /// <summary>
    /// The account picker. Switching is local and instant: every account keeps
    /// its own identity, onion, contacts and history, and the core already holds
    /// them, so no logout or Tor restart happens here.
    /// </summary>
    private Control AccountPicker()
    {
        var picker = new ComboBox
        {
            ItemsSource = _accounts,
            SelectedItem = _accounts.FirstOrDefault(a => a.AccountId == _account) ?? _accounts.FirstOrDefault(),
            HorizontalAlignment = HorizontalAlignment.Stretch,
            MinHeight = 44,
        };
        picker.SelectionChanged += async (_, _) =>
        {
            if (picker.SelectedItem is not MessengerAccountInfo chosen || chosen.AccountId == _account) return;
            _account = chosen.AccountId;
            _conversation = "";
            _conversations.Clear();
            _conversationIndex.Clear();
            await Guard(ShowChatsHomeAsync);
        };
        return picker;
    }

    private Control ConversationCard(MessengerConversationInfo conversation)
    {
        var row = new Grid { ColumnDefinitions = new ColumnDefinitions("Auto,*,Auto"), ColumnSpacing = 10 };
        row.Children.Add(new TextBlock
        {
            Text = conversation.Badge,
            FontSize = 15,
            VerticalAlignment = VerticalAlignment.Center,
        });
        var labels = new StackPanel { Spacing = 2 };
        labels.Children.Add(new TextBlock
        {
            Text = conversation.Title,
            FontSize = 16,
            FontWeight = FontWeight.SemiBold,
            TextTrimming = TextTrimming.CharacterEllipsis,
        });
        labels.Children.Add(new TextBlock
        {
            Text = conversation.Preview.Length == 0 ? "No messages yet" : conversation.Preview,
            FontSize = 12,
            Foreground = Ink("#9FB4A9"),
            TextTrimming = TextTrimming.CharacterEllipsis,
            MaxLines = 1,
        });
        Grid.SetColumn(labels, 1);
        row.Children.Add(labels);
        if (conversation.HasUnread)
        {
            var badge = new Border
            {
                Background = Ink("#A5E3C5"),
                CornerRadius = new CornerRadius(11),
                Padding = new Thickness(8, 2),
                VerticalAlignment = VerticalAlignment.Center,
                Child = new TextBlock
                {
                    Text = conversation.UnreadLabel,
                    FontSize = 11,
                    FontWeight = FontWeight.SemiBold,
                    Foreground = Ink("#123226"),
                },
            };
            Grid.SetColumn(badge, 2);
            row.Children.Add(badge);
        }
        var button = Button(conversation.Title, () => OpenConversationAsync(conversation));
        button.Content = row;
        button.HorizontalContentAlignment = HorizontalAlignment.Stretch;
        return Card(button);
    }

    // -----------------------------------------------------------------------
    // One conversation
    // -----------------------------------------------------------------------

    private async Task OpenConversationAsync(MessengerConversationInfo conversation)
    {
        ShowChatTranscript();
        SelectTab("Messages");
        _conversation = conversation.Id;
        _conversationKind = conversation.Kind;
        _conversationTarget = conversation.Target;
        _replyTo = "";
        _editing = "";
        _chatAttachments.Clear();
        _messages.Children.Clear();
        _lastRecords.Clear();
        _chatsScreen = false;
        UpdateChatComposer();
        if (_conversationKind == "group")
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_get_group_messages",
                ["account_id"] = _account,
                ["group_id"] = _conversationTarget,
            });
        else
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_get_messages",
                ["account_id"] = _account,
                ["conversation_id"] = _conversation,
            });
        await SendMessengerAsync(new()
        {
            ["op"] = "messenger_mark_read",
            ["account_id"] = _account,
            ["conversation_id"] = _conversation,
        });
        _status.Text = conversation.Kind == "group"
            ? conversation.Title + " · group"
            : conversation.Title + " · encrypted";
    }

    /// <summary>
    /// Show the chat transcript in the same surface the agent conversation uses,
    /// so the composer, insets and scroll behaviour stay familiar.
    /// </summary>
    private void ShowChatTranscript()
    {
        _settingsGeneration++;
        if (_composeSurface is not null) _composeSurface.IsVisible = true;
        _devicesVisible = false;
        _chatsScreen = false;
        _chatsHome = false;
        _panelScroll.IsVisible = false;
        _scroll.IsVisible = true;
        if (_copyBar is not null) _copyBar.IsVisible = false;
        UpdateChatComposer();
    }

    /// <summary>
    /// The answer buttons for a request that belongs to the conversation on
    /// screen.
    /// </summary>
    /// <remarks>
    /// The core now writes "X wants to chat" into the transcript, so the phone
    /// shows the invitation wherever the user happens to be. Showing the text
    /// without a way to answer it would be worse than not showing it at all:
    /// this is the accept/decline pair for exactly that conversation.
    /// </remarks>
    private Control? PendingRequestActions()
    {
        var request = _contactRequests.FirstOrDefault(row =>
            row.Incoming && row.UserId == _conversationTarget);
        if (request is null) return null;
        var actions = new StackPanel { Spacing = 8 };
        actions.Children.Add(new TextBlock
        {
            Text = request.DisplayName + " " + request.Badge,
            FontSize = 12,
            Foreground = Ink("#9FB4A9"),
            TextWrapping = TextWrapping.Wrap,
        });
        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        row.Children.Add(Button("Accept", () => RespondRequestAsync(request, true)));
        row.Children.Add(Button("Decline", () => RespondRequestAsync(request, false)));
        actions.Children.Add(row);
        return Card(actions);
    }

    private void RenderChatHistory(JsonElement messages)
    {
        _messages.Children.Clear();
        _lastRecords.Clear();
        // Drawn before the bubbles so the invitation sits at the top of the
        // transcript, where the note that announced it also lands.
        if (PendingRequestActions() is { } pending) _messages.Children.Add(pending);
        var count = 0;
        foreach (var message in messages.EnumerateArray())
        {
            _messages.Children.Add(ChatBubble(message));
            count++;
        }
        if (count == 0)
            _messages.Children.Add(new TextBlock
            {
                Text = "No messages yet. Type below.",
                Foreground = Ink("#9FB4A9"),
                HorizontalAlignment = HorizontalAlignment.Center,
                Margin = new Thickness(0, 24, 0, 0),
            });
        QueueTranscriptScroll(true);
    }

    private Control ChatBubble(JsonElement message)
    {
        var outgoing = Text(message, "direction") == "outgoing";
        var kind = Text(message, "kind", "text");
        var id = Text(message, "id");
        var body = Text(message, "text");
        var edited = Flag(message, "edited");
        var status = Text(message, "status");
        var delivered = (int)Number(message, "delivered_to");
        var total = (int)Number(message, "total_members");
        var ts = Number(message, "ts");

        var stack = new StackPanel { Spacing = 5 };
        stack.Children.Add(new TextBlock
        {
            Text = (kind == "system" ? "system" : outgoing ? "You" : Short(Text(message, "from_user_id")))
                   + " · " + ChatStatus(ts, edited, status, delivered, total),
            FontSize = 10,
            Foreground = Ink("#9FB4A9"),
        });
        if (kind is "text" or "edit" or "system")
        {
            // Markdown source is stored; the mobile view renders it with the same
            // rules the agent transcript uses.
            stack.Children.Add(ReplyView(body));
        }
        else
        {
            stack.Children.Add(ChatMedia(kind, message));
            if (body.Length > 0) stack.Children.Add(ReplyView(body));
        }
        if (outgoing && kind is "text" or "edit")
        {
            var actions = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
            actions.Children.Add(Button("Edit", () => { StartChatEdit(id, body); return Task.CompletedTask; }));
            actions.Children.Add(Button("Reply", () => { _replyTo = id; UpdateChatComposer(); return Task.CompletedTask; }));
            stack.Children.Add(actions);
        }
        var bubble = new Border
        {
            Child = stack,
            Padding = new Thickness(12, 9),
            CornerRadius = new CornerRadius(16),
            MaxWidth = 520,
            HorizontalAlignment = outgoing ? HorizontalAlignment.Right : HorizontalAlignment.Left,
            Background = outgoing ? Ink("#254037") : Ink("#1B282E"),
            BorderBrush = Ink("#31433B"),
            BorderThickness = new Thickness(1),
        };
        // The message id rides on the bubble so a status event can find and
        // replace exactly this view.
        if (id.Length > 0)
        {
            bubble.Tag = id;
            _lastRecords[id] = message.Clone();
        }
        return bubble;
    }

    private Control ChatMedia(string kind, JsonElement message)
    {
        var media = message.TryGetProperty("media", out var node) && node.ValueKind == JsonValueKind.Object
            ? node
            : default;
        // A message may carry several files: one row each, in pick order.
        if (media.ValueKind == JsonValueKind.Object
            && media.TryGetProperty("parts", out var parts)
            && parts.ValueKind == JsonValueKind.Array)
        {
            var list = new StackPanel { Spacing = 6 };
            foreach (var part in parts.EnumerateArray())
                list.Children.Add(ChatMediaRow(kind, part));
            return list;
        }
        return ChatMediaRow(kind, media);
    }

    private Control ChatMediaRow(string kind, JsonElement media)
    {
        var name = media.ValueKind == JsonValueKind.Object ? Text(media, "file_name", "attachment") : "attachment";
        var size = media.ValueKind == JsonValueKind.Object ? Number(media, "size_bytes") : 0;
        var local = media.ValueKind == JsonValueKind.Object ? Text(media, "local_path") : "";
        var icon = kind switch { "image" => "🖼", "video" => "🎥", "audio" => "🎤", _ => "📎" };
        var label = kind switch { "image" => "Photo", "video" => "Video", "audio" => "Audio", _ => "File" };
        var row = new Grid { ColumnDefinitions = new ColumnDefinitions("Auto,*"), ColumnSpacing = 10 };
        row.Children.Add(new TextBlock { Text = icon, FontSize = 24, VerticalAlignment = VerticalAlignment.Center });
        var labels = new StackPanel { Spacing = 2 };
        labels.Children.Add(new TextBlock
        {
            Text = name,
            FontWeight = FontWeight.SemiBold,
            TextTrimming = TextTrimming.CharacterEllipsis,
        });
        labels.Children.Add(new TextBlock
        {
            Text = $"{label} · {HumanSize(size)} · {(File.Exists(local) ? "saved" : "not downloaded")}",
            FontSize = 11,
            Foreground = Ink("#9FB4A9"),
        });
        Grid.SetColumn(labels, 1);
        row.Children.Add(labels);
        return row;
    }

    private static string ChatStatus(long ts, bool edited, string status, int delivered, int total)
    {
        var parts = new List<string>();
        if (ts > 0)
            parts.Add(DateTimeOffset.FromUnixTimeMilliseconds(ts).ToLocalTime().ToString("HH:mm"));
        if (edited) parts.Add("edited");
        parts.Add(status switch
        {
            "pending" => "pending",
            "failed" => "not delivered",
            "sent" => total > 0 ? $"sent {delivered}/{total}" : "sent",
            "delivered" => total > 0 ? $"delivered {delivered}/{total}" : "delivered",
            "read" => "read",
            _ => status,
        });
        return string.Join(" · ", parts);
    }

    private static string HumanSize(long bytes) => bytes switch
    {
        <= 0 => "",
        < 1024 => bytes + " B",
        < 1024 * 1024 => (bytes / 1024.0).ToString("0.0") + " KiB",
        < 1024L * 1024 * 1024 => (bytes / (1024.0 * 1024)).ToString("0.0") + " MiB",
        _ => (bytes / (1024.0 * 1024 * 1024)).ToString("0.00") + " GiB",
    };

    private void StartChatEdit(string id, string body)
    {
        _editing = id;
        _composer.Text = body;
        UpdateChatComposer();
        _composer.Focus();
    }

    private void UpdateChatComposer()
    {
        _composer.Watermark = _editing.Length > 0
            ? "Editing a sent message"
            : _replyTo.Length > 0
                ? "Replying · " + Short(_replyTo)
                : "Message… (Markdown works)";
    }

    /// <summary>
    /// Send whatever the composer holds: an edit, an attachment, or text.
    /// Returns false when there is nothing to send, so the shared Send button can
    /// fall through to the agent path instead of becoming a dead control.
    /// </summary>
    private async Task<bool> SendChatAsync()
    {
        if (_account.Length == 0 || _conversation.Length == 0) return false;
        var text = _composer.Text ?? "";
        if (_editing.Length > 0)
        {
            var update = text.Trim();
            if (update.Length == 0) return false;
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_edit_message",
                ["account_id"] = _account,
                ["id"] = _editing,
                ["new_text"] = update,
            });
            _editing = "";
            _composer.Text = "";
            UpdateChatComposer();
            return true;
        }
        if (_chatAttachments.Count > 0)
        {
            // One message carries every staged file, in the order picked.
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_send_media",
                ["account_id"] = _account,
                ["to"] = _conversationTarget,
                ["paths"] = _chatAttachments.ToArray(),
                ["caption"] = text.Trim().Length > 0 ? text.Trim() : null,
                ["reply_to"] = _replyTo.Length > 0 ? _replyTo : null,
                ["group"] = _conversationKind == "group",
            });
            _chatAttachments.Clear();
            _composer.Text = "";
            _replyTo = "";
            UpdateChatComposer();
            return true;
        }
        var trimmed = text.Trim();
        if (trimmed.Length == 0) return false;
        await SendMessengerAsync(new()
        {
            ["op"] = "messenger_send_text",
            ["account_id"] = _account,
            ["to"] = _conversationTarget,
            ["text"] = trimmed,
            ["reply_to"] = _replyTo.Length > 0 ? _replyTo : null,
            ["group"] = _conversationKind == "group",
        });
        _composer.Text = "";
        _replyTo = "";
        UpdateChatComposer();
        return true;
    }

    private Task SendMessengerAsync(Dictionary<string, object?> operation) =>
        _hub.Bridge.SendAsync(operation);

    // -----------------------------------------------------------------------
    // Accounts, contacts, profile, groups
    // -----------------------------------------------------------------------

    private async Task CreateAccountAsync()
    {
        var name = await PromptTextAsync("New messenger account", "Display name", "");
        if (name is null || name.Trim().Length == 0) return;
        await SendMessengerAsync(new()
        {
            ["op"] = "messenger_create_account",
            ["display_name"] = name.Trim(),
        });
        // The prompt sub-screen cleared the home marker; re-enter so the
        // picker shows at once and the core's `messenger_accounts_changed`
        // (a real payload change) can re-render with the new account.
        await ShowChatsHomeAsync();
    }

    /// <summary>A small modal text prompt, used for account and group names.</summary>
    private async Task<string?> PromptTextAsync(string title, string hint, string initial)
    {
        ShowPanel();
        _chatsScreen = true;
        SelectTab("Messages");
        _panel.Children.Add(Heading(title, hint));
        var input = new TextBox { Text = initial, MinHeight = 44 };
        _panel.Children.Add(Card(input));
        var result = new TaskCompletionSource<string?>();
        _panel.Children.Add(Button("Save", () =>
        {
            result.TrySetResult(input.Text ?? "");
            return ShowChatsHomeAsync();
        }));
        _panel.Children.Add(Button("Cancel", () =>
        {
            result.TrySetResult(null);
            return ShowChatsHomeAsync();
        }));
        input.Focus();
        return await result.Task;
    }

    private Task AddContactAsync()
    {
        ShowPanel();
        _chatsScreen = true;
        SelectTab("Messages");
        _panel.Children.Add(Heading("Add contact",
            "Paste the invite they sent you. It is signed by their identity, so a name cannot be forged."));
        var invite = new TextBox
        {
            AcceptsReturn = true,
            Watermark = "Paste invite JSON",
            TextWrapping = TextWrapping.Wrap,
            MinHeight = 110,
        };
        _panel.Children.Add(Card(invite));
        // Scanning reuses the existing QR pipeline from device pairing.
        if (_scanQr is not null)
            _panel.Children.Add(Button("Scan QR", async () =>
            {
                var scanned = await _scanQr();
                if (scanned is not null) invite.Text = scanned;
            }));
        _panel.Children.Add(Button("Add", async () =>
        {
            var payload = (invite.Text ?? "").Trim();
            if (payload.Length == 0)
            {
                _status.Text = "Paste an invite first.";
                return;
            }
            try
            {
                using var _ = JsonDocument.Parse(payload);
            }
            catch (JsonException)
            {
                _status.Text = "That is not a messenger invite.";
                return;
            }
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_add_contact",
                ["account_id"] = _account,
                ["invite"] = payload,
            });
            _status.Text = "Contact added and verified";
            await ShowChatsHomeAsync();
        }));
        _panel.Children.Add(Button("Show my QR", ShowProfileAsync));
        _panel.Children.Add(Button("Back", ShowChatsHomeAsync));
        return Task.CompletedTask;
    }

    private Task ShowProfileAsync()
    {
        var account = _accounts.FirstOrDefault(a => a.AccountId == _account);
        ShowPanel();
        _chatsScreen = true;
        SelectTab("Messages");
        _panel.Children.Add(Heading(account?.DisplayName ?? "Profile",
            "This onion belongs to this account alone — not to the device pairing."));
        _invite = account?.Invite() ?? "";
        if (_invite.Length == 0)
        {
            _panel.Children.Add(Card(new TextBlock
            {
                Text = "Tor is still publishing this account's onion. Try again once the account shows online.",
                TextWrapping = TextWrapping.Wrap,
            }));
        }
        else
        {
            var image = new Image
            {
                // Phones read on-screen codes best when the quiet zone is gone
                // and the module size stays readable; 520sp fills most of a
                // 6-inch panel without scrolling.
                MaxWidth = 520,
                MaxHeight = 520,
                Stretch = Stretch.Uniform,
                HorizontalAlignment = HorizontalAlignment.Center,
            };
            try
            {
                image.Source = QrBitmap(_invite);
            }
            catch
            {
                // A QR is a convenience; the copy button below is the fallback.
            }
            _panel.Children.Add(Card(image));
            _panel.Children.Add(Button("Enlarge QR", () =>
            {
                ShowEnlargedQr(_invite, "Scan this chat invite");
                return Task.CompletedTask;
            }));
            _panel.Children.Add(Button("Copy invite", async () =>
            {
                if (TopLevel.GetTopLevel(this)?.Clipboard is { } clipboard)
                    await clipboard.SetTextAsync(_invite);
                _status.Text = "Invite copied";
            }));
        }
        _panel.Children.Add(Card(ReadOnlyBlock("User id", account?.UserId ?? "")));
        _panel.Children.Add(Card(ReadOnlyBlock("Onion", account?.Onion ?? "")));
        var name = new TextBox { Text = account?.DisplayName ?? "", MinHeight = 44 };
        _panel.Children.Add(SettingField("Display name", name));
        var pro = new CheckBox
        {
            Content = "Local Pro tier (up to 2 GiB per file)",
            IsChecked = account?.Pro == true,
            MinHeight = 44,
        };
        _panel.Children.Add(pro);
        _panel.Children.Add(Button("Save", async () =>
        {
            var trimmed = (name.Text ?? "").Trim();
            if (trimmed.Length > 0)
                await SendMessengerAsync(new()
                {
                    ["op"] = "messenger_set_profile",
                    ["account_id"] = _account,
                    ["display_name"] = trimmed,
                });
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_set_tier",
                ["account_id"] = _account,
                ["pro"] = pro.IsChecked == true,
                ["chunk_size"] = 262144,
            });
            _status.Text = "Profile saved";
            await ShowChatsHomeAsync();
        }));
        _panel.Children.Add(Button("Back", ShowChatsHomeAsync));
        return Task.CompletedTask;
    }

    private async Task ShowGroupsAsync()
    {
        ShowPanel();
        _chatsScreen = true;
        SelectTab("Messages");
        _panel.Children.Add(Heading("Groups",
            "Up to 300 members. Each message is sealed with the sender's key and delivered to every member over their own onion."));
        await SendMessengerAsync(new()
        {
            ["op"] = "messenger_list_groups",
            ["account_id"] = _account,
        });
        foreach (var conversation in _conversations.Where(c => c.Kind == "group"))
            _panel.Children.Add(ConversationCard(conversation));
        _panel.Children.Add(Button("Create group", async () =>
        {
            var name = await PromptTextAsync("New group", "Group name", "");
            if (name is null || name.Trim().Length == 0) return;
            var members = await PickMembersAsync(new HashSet<string>());
            if (members is null) return;
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_create_group",
                ["account_id"] = _account,
                ["name"] = name.Trim(),
                ["member_ids"] = members.ToArray(),
            });
            await Task.Delay(200);
            await ShowChatsHomeAsync();
        }));
        _panel.Children.Add(Button("Manage members", ManageMembersAsync));
        _panel.Children.Add(Button("Back", ShowChatsHomeAsync));
    }

    /// <summary>
    /// Pick group members from known contacts. Returns null on cancel and an
    /// empty list when the user picked nobody: those are different answers.
    /// </summary>
    private async Task<List<string>?> PickMembersAsync(HashSet<string> already)
    {
        ShowPanel();
        _chatsScreen = true;
        SelectTab("Messages");
        _panel.Children.Add(Heading("Choose members",
            "A group can only include people you already verified."));
        var result = new TaskCompletionSource<List<string>?>();
        var boxes = new List<(string Id, CheckBox Box)>();
        foreach (var (id, name) in _contacts)
        {
            if (already.Contains(id)) continue;
            var box = new CheckBox { Content = name, MinHeight = 44 };
            boxes.Add((id, box));
            _panel.Children.Add(box);
        }
        if (boxes.Count == 0)
            _panel.Children.Add(Card(new TextBlock
            {
                Text = "No contacts yet. Add someone with their invite first.",
                TextWrapping = TextWrapping.Wrap,
            }));
        _panel.Children.Add(Button("Done", () =>
        {
            result.TrySetResult(boxes.Where(pair => pair.Box.IsChecked == true)
                .Select(pair => pair.Id).ToList());
            return ShowChatsHomeAsync();
        }));
        _panel.Children.Add(Button("Cancel", () =>
        {
            result.TrySetResult(null);
            return ShowChatsHomeAsync();
        }));
        return await result.Task;
    }

    /// <summary>
    /// Add or remove members of an existing group. Removing a member rotates
    /// every sender chain for that group in the core, so the removed person
    /// cannot read anything after the removal.
    /// </summary>
    private async Task ManageMembersAsync()
    {
        var group = _conversations.FirstOrDefault(c => c.Kind == "group");
        if (group is null)
        {
            _status.Text = "Create a group first.";
            return;
        }
        ShowPanel();
        _chatsScreen = true;
        SelectTab("Messages");
        _panel.Children.Add(Heading(group.Title, "Members of this group."));
        var members = await FetchMembersAsync(group.Target);
        if (members.Count == 0)
            _panel.Children.Add(Card(new TextBlock
            {
                Text = "The member list arrives with the next group update.",
                TextWrapping = TextWrapping.Wrap,
            }));
        foreach (var member in members)
        {
            var row = new Grid { ColumnDefinitions = new ColumnDefinitions("*,Auto"), ColumnSpacing = 8 };
            row.Children.Add(new TextBlock
            {
                Text = member.Name.Length > 0 ? member.Name : Short(member.UserId),
                VerticalAlignment = VerticalAlignment.Center,
            });
            var remove = Button("Remove", async () =>
            {
                await SendMessengerAsync(new()
                {
                    ["op"] = "messenger_remove_group_member",
                    ["account_id"] = _account,
                    ["group_id"] = group.Target,
                    ["user_id"] = member.UserId,
                });
                _status.Text = "Member removed · sender keys rotated";
                await ManageMembersAsync();
            });
            Grid.SetColumn(remove, 1);
            row.Children.Add(remove);
            _panel.Children.Add(Card(row));
        }
        _panel.Children.Add(Button("Add members", async () =>
        {
            var chosen = await PickMembersAsync(members.Select(m => m.UserId).ToHashSet());
            if (chosen is null || chosen.Count == 0) return;
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_add_group_members",
                ["account_id"] = _account,
                ["group_id"] = group.Target,
                ["member_ids"] = chosen.ToArray(),
            });
            _status.Text = "Members added · sender keys redistributed";
            await ShowChatsHomeAsync();
        }));
        _panel.Children.Add(Button("Back", ShowChatsHomeAsync));
    }

    /// <summary>
    /// Ask the core for the group roster. The reply arrives as a
    /// `messenger_group_member_update` event, so a brief wait is the honest way
    /// to read it on a phone without a second round trip.
    /// </summary>
    private async Task<List<MessengerMemberInfo>> FetchMembersAsync(string groupId)
    {
        var members = new List<MessengerMemberInfo>();
        await SendMessengerAsync(new()
        {
            ["op"] = "messenger_get_group_messages",
            ["account_id"] = _account,
            ["group_id"] = groupId,
        });
        foreach (var member in _lastGroupMembers) members.Add(member);
        return members;
    }

    private List<MessengerMemberInfo> _lastGroupMembers = [];

    // -----------------------------------------------------------------------
    // Events from the core
    // -----------------------------------------------------------------------

    /// <summary>
    /// Handle one `messenger_*` event. Returns true when it was consumed, so the
    /// shared core handler never mistakes a chat event for a session event.
    /// </summary>
    private bool HandleMessengerEvent(JsonElement node, string kind)
    {
        switch (kind)
        {
            case "messenger_accounts_changed":
                if (!node.TryGetProperty("accounts", out var accounts)) return true;
                // Presence chatter re-emits the same list; only real changes
                // (and the first event after entering the screen) re-render.
                if (accounts.GetRawText() == _lastAccountsJson) return true;
                _lastAccountsJson = accounts.GetRawText();
                _accounts.Clear();
                foreach (var item in accounts.EnumerateArray())
                    _accounts.Add(MessengerAccountInfo.FromJson(item));
                if (_account.Length == 0)
                    _account = (_accounts.FirstOrDefault(a => a.Active) ?? _accounts.FirstOrDefault())?.AccountId ?? "";
                if (_chatsScreen) RenderMessengerHome();
                return true;

            case "messenger_conversations":
                if (Text(node, "account_id") != _account) return true;
                // Identical payload = presence echo; skip the redraw.
                if (node.TryGetProperty("conversations", out var incoming)
                    && incoming.GetRawText() == _lastConversationsJson) return true;
                _conversations.Clear();
                _conversationIndex.Clear();
                if (node.TryGetProperty("conversations", out var conversations))
                {
                    _lastConversationsJson = conversations.GetRawText();
                    foreach (var item in conversations.EnumerateArray())
                    {
                        var conversation = MessengerConversationInfo.FromJson(item);
                        _conversations.Add(conversation);
                        _conversationIndex[conversation.Id] =
                            (conversation.Kind, conversation.Target, conversation.Title);
                    }
                }
                if (_chatsScreen) RenderMessengerHome();
                return true;

            case "messenger_contacts":
                if (Text(node, "account_id") != _account) return true;
                _contacts.Clear();
                if (node.TryGetProperty("contacts", out var contacts))
                    foreach (var item in contacts.EnumerateArray())
                    {
                        var id = Text(item, "user_id");
                        if (id.Length == 0) continue;
                        _contacts.Add((id, Text(item, "display_name", Short(id))));
                    }
                return true;

            case "messenger_contact_requests":
                // A scanned QR must be answered here; until then nothing about
                // the other device becomes a contact.
                _contactRequests.Clear();
                if (Text(node, "account_id") == _account
                    && node.TryGetProperty("requests", out var requests))
                    foreach (var item in requests.EnumerateArray())
                    {
                        var request = MessengerContactRequest.FromJson(item);
                        if (request.UserId.Length > 0) _contactRequests.Add(request);
                    }
                if (_chatsScreen) RenderMessengerHome();
                // The accept buttons live inside the open conversation too, so a
                // request that arrives — or is answered elsewhere — has to update
                // that surface, not only the list.
                if (!_chatsScreen && _conversation.Length > 0 && _conversationKind == "direct")
                    _ = SendMessengerAsync(new()
                    {
                        ["op"] = "messenger_get_messages",
                        ["account_id"] = _account,
                        ["conversation_id"] = _conversation,
                    });
                return true;

            case "messenger_groups":
                if (node.TryGetProperty("groups", out var groupList))
                    foreach (var entry in groupList.EnumerateArray())
                    {
                        var id = Text(entry, "group_id");
                        var name = Text(entry, "name", "Group");
                        _conversationIndex[$"grp:{_account}:{id}"] = ("group", id, name);
                    }
                return true;

            case "messenger_history":
                if (Text(node, "account_id") != _account) return true;
                if (Text(node, "conversation_id") != _conversation) return true;
                if (node.TryGetProperty("messages", out var messages)) RenderChatHistory(messages);
                return true;

            case "messenger_message_received":
            case "messenger_group_message_received":
                if (node.TryGetProperty("message", out var received)
                    // Only append into the conversation that is actually open.
                    && Text(received, "conversation_id") == _conversation)
                {
                    _messages.Children.Add(ChatBubble(received));
                    QueueTranscriptScroll();
                }
                return true;

            case "messenger_message_status":
                // Replace the open conversation's copy of the message with the
                // newer record, so "pending" flips to "sent"/"delivered" live.
                if (Text(node, "message_id") is { Length: > 0 } updatedId)
                    for (var index = 0; index < _messages.Children.Count; index++)
                    {
                        if (_messages.Children[index] is not Border bubble
                            || bubble.Tag is not string bubbleId
                            || bubbleId != updatedId) continue;
                        if (_lastRecords.TryGetValue(updatedId, out var updated))
                        {
                            var refreshed = ChatBubble(updated);
                            refreshed.Tag = bubbleId;
                            _messages.Children[index] = refreshed;
                        }
                        break;
                    }
                if (Number(node, "total_members") > 0)
                    _status.Text = $"Delivered to {Number(node, "delivered_to")}/{Number(node, "total_members")}";
                return true;

            case "messenger_message_edited":
                _status.Text = "Message edited";
                return true;

            case "messenger_peer_online":
                _status.Text = Short(Text(node, "user_id")) + " is online";
                return true;

            case "messenger_peer_offline":
                _status.Text = Short(Text(node, "user_id")) + " is offline · messages stay queued";
                return true;

            case "messenger_media_progress":
                ChatProgress(node);
                return true;

            case "messenger_group_member_update":
                if (node.TryGetProperty("group", out var groupNode))
                {
                    _lastGroupMembers = groupNode.TryGetProperty("members", out var members)
                        ? members.EnumerateArray().Select(member => new MessengerMemberInfo
                        {
                            UserId = Text(member, "user_id"),
                            Name = Text(member, "display_name"),
                            Onion = Text(member, "onion"),
                        }).Where(member => member.UserId.Length > 0).ToList()
                        : [];
                    // A group in the list may have been renamed or gained members.
                    RenderMessengerHome();
                }
                return true;

            case "messenger_invite":
                if (node.TryGetProperty("payload", out var payload))
                    _invite = payload.GetRawText();
                return true;

            default:
                return false;
        }
    }

    private async Task RespondRequestAsync(MessengerContactRequest request, bool accept)
    {
        await SendMessengerAsync(new()
        {
            ["op"] = "messenger_respond_contact_request",
            ["account_id"] = _account,
            ["user_id"] = request.UserId,
            ["accept"] = accept,
        });
        _contactRequests.RemoveAll(row => row.UserId == request.UserId);
        // The user may be answering from inside the conversation, where
        // ShowChatsHomeAsync would yank them back to the list. Re-render
        // whichever surface is actually on screen, so the answer buttons
        // disappear where they were pressed.
        if (_chatsHome) { await ShowChatsHomeAsync(); return; }
        if (_conversation.Length > 0)
            await SendMessengerAsync(new()
            {
                ["op"] = "messenger_get_messages",
                ["account_id"] = _account,
                ["conversation_id"] = _conversation,
            });
    }

    private void ChatProgress(JsonElement node)
    {
        var received = Number(node, "received");
        var total = Math.Max(1, Number(node, "total"));
        var name = Text(node, "file_name", "attachment");
        var percent = (int)(received * 100 / total);
        _status.Text = Text(node, "status", "active") switch
        {
            "complete" => "Sent " + name,
            "failed" => "Could not send " + name,
            "interrupted" => $"Paused at {percent}% · {name}",
            _ => $"Transferring {name} · {percent}%",
        };
    }

    private static string Short(string value) =>
        value.Length > 10 ? value[..10] + "…" : value;

    private static string Text(JsonElement node, string name, string fallback = "") =>
        MessengerAccountInfo.Text(node, name, fallback);

    private static bool Flag(JsonElement node, string name) =>
        MessengerAccountInfo.Flag(node, name);

    private static long Number(JsonElement node, string name) =>
        MessengerAccountInfo.Number(node, name);

    // -----------------------------------------------------------------------
    // Small building blocks
    // -----------------------------------------------------------------------

    private static Control Heading(string title, string subtitle)
    {
        var panel = new StackPanel { Spacing = 4 };
        panel.Children.Add(new TextBlock
        {
            Text = title,
            FontSize = 24,
            FontWeight = FontWeight.SemiBold,
            TextWrapping = TextWrapping.Wrap,
        });
        panel.Children.Add(new TextBlock
        {
            Text = subtitle,
            FontSize = 12,
            Foreground = Ink("#9FB4A9"),
            TextWrapping = TextWrapping.Wrap,
        });
        return panel;
    }

    private static Control Card(Control child) => new Border
    {
        Child = child,
        Background = Ink("#1B282E"),
        CornerRadius = new CornerRadius(16),
        Padding = new Thickness(14),
        BorderThickness = new Thickness(1),
        BorderBrush = Ink("#31433B"),
    };

    private static Control ReadOnlyBlock(string label, string value)
    {
        var panel = new StackPanel { Spacing = 3 };
        panel.Children.Add(new TextBlock { Text = label, FontSize = 11, Foreground = Ink("#9FB4A9") });
        panel.Children.Add(new TextBlock
        {
            Text = value,
            FontSize = 11,
            FontFamily = FontFamily.Parse("monospace"),
            TextWrapping = TextWrapping.Wrap,
        });
        return panel;
    }

    /// <summary>
    /// A QR code for an invite. Rendered locally, so scanning never needs a
    /// third party and the invite never leaves the device unencrypted.
    /// </summary>
    private static Avalonia.Media.Imaging.Bitmap QrBitmap(string payload)
    {
        using var generator = new QRCoder.QRCodeGenerator();
        using var data = generator.CreateQrCode(payload, QRCoder.QRCodeGenerator.ECCLevel.M);
        using var png = new QRCoder.PngByteQRCode(data);
        return new Avalonia.Media.Imaging.Bitmap(new MemoryStream(png.GetGraphic(6)));
    }

    /// <summary>
    /// Pick a file for the open conversation and hand its path to the core, which
    /// hashes it, seals it chunk by chunk and reports progress per chunk. The
    /// phone never has to hold the whole file in memory.
    /// </summary>
    private async Task AttachToChatAsync()
    {
        var storage = TopLevel.GetTopLevel(this)?.StorageProvider;
        if (storage is null)
        {
            _status.Text = "File picker is unavailable.";
            return;
        }
        var files = await storage.OpenFilePickerAsync(new FilePickerOpenOptions
        {
            Title = "Attach a photo, video, audio file or document",
            // Any type, any number: one message may carry every file picked.
            AllowMultiple = true,
        });
        if (files.Count == 0) return;
        var account = _accounts.FirstOrDefault(a => a.AccountId == _account);
        var limit = account?.Pro == true ? 2L * 1024 * 1024 * 1024 : 100L * 1024 * 1024;
        var rejected = new List<string>();
        foreach (var file in files)
        {
            var path = file.TryGetLocalPath();
            if (string.IsNullOrEmpty(path))
            {
                // A content:// URI has no filesystem path; copy it into app storage
                // so the core can read it after the picker's grant expires.
                var root = Path.Combine(_home, "store", "messenger-drafts");
                Directory.CreateDirectory(root);
                var copy = Path.Combine(root, Guid.NewGuid().ToString("N") + Path.GetExtension(file.Name));
                try
                {
                    await using var input = await file.OpenReadAsync();
                    await using var output = File.Create(copy);
                    await input.CopyToAsync(output);
                }
                catch (Exception error)
                {
                    rejected.Add(file.Name + ": " + error.Message);
                    continue;
                }
                path = copy;
            }
            var size = new FileInfo(path).Length;
            if (size > limit)
            {
                rejected.Add($"{file.Name} ({HumanSize(size)})");
                continue;
            }
            if (_chatAttachments.Any(staged => string.Equals(staged, path, StringComparison.Ordinal)))
                continue;
            _chatAttachments.Add(path);
        }
        if (rejected.Count > 0)
            _status.Text = $"Not attached: {string.Join(", ", rejected)}. Limit per file: {HumanSize(limit)}.";
        else
            _status.Text = _chatAttachments.Count == 1
                ? "Attached " + Path.GetFileName(_chatAttachments[0])
                : $"{_chatAttachments.Count} attachments staged";
        _composer.Watermark = "Add a caption, then Send";
        _composer.Focus();
    }
}
