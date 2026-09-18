using System.Text.Json;
using Android.App;
using Android.Content;
using Android.Content.PM;
using Android.OS;
using GnomeAI.Client;

namespace GnomeAI.Android;

// Owned by the foreground service, not the activity: background replies still notify.
internal sealed class ReplyNotifications : IDisposable
{
    private const string Channel = "gnomeai_replies";
    private readonly Context _context;
    private readonly DeviceHub _hub;
    private readonly Handler _main = new(Looper.MainLooper!);
    private readonly object _gate = new();
    private readonly HashSet<string> _seen = new(StringComparer.Ordinal);
    private readonly Queue<string> _order = new();
    private bool _disposed;

    public ReplyNotifications(Context context, DeviceHub hub)
    {
        _context = context;
        _hub = hub;
        var manager = (NotificationManager?)context.GetSystemService(Context.NotificationService);
        manager?.CreateNotificationChannel(new NotificationChannel(Channel, "Completed responses", NotificationImportance.Default) {
            Description = "Notifications when a local or paired-device response finishes."
        });
        _hub.SessionEvent += OnSessionEvent;
    }

    private void OnSessionEvent(PeerLink? peer, JsonElement frame)
    {
        // Only actual completion events, never history replay, tokens, errors or interruption.
        if (!frame.TryGetProperty("payload", out var envelope) ||
            !envelope.TryGetProperty("event", out var kind) || kind.GetString() != "session_event" ||
            !envelope.TryGetProperty("session_id", out var session) ||
            !envelope.TryGetProperty("payload", out var payload) ||
            !payload.TryGetProperty("event", out var detail) || detail.GetString() != "turn_completed" ||
            !frame.TryGetProperty("epoch", out var epoch) ||
            !frame.TryGetProperty("revision", out var revision) || !revision.TryGetInt64(out var number)) return;
        var id = session.GetString();
        if (string.IsNullOrEmpty(id)) return;
        var source = peer?.Peer.Id ?? "local";
        var key = source + ":" + epoch.GetString() + ":" + number;
        var tag = "reply:" + source + ":" + id;
        lock (_gate) {
            if (_disposed || !_seen.Add(key)) return;
            _order.Enqueue(key);
            while (_order.Count > 512) _seen.Remove(_order.Dequeue());
            _main.Post(() => Show(tag));
        }
    }

    private void Show(string tag)
    {
        lock (_gate) { if (_disposed) return; }
        try {
            if (OperatingSystem.IsAndroidVersionAtLeast(33) &&
                _context.CheckSelfPermission(global::Android.Manifest.Permission.PostNotifications) != Permission.Granted) return;
            var manager = (NotificationManager?)_context.GetSystemService(Context.NotificationService);
            if (manager is null || !manager.AreNotificationsEnabled()) return;
            var open = new Intent(_context, typeof(MainActivity));
            open.AddFlags(ActivityFlags.SingleTop | ActivityFlags.ClearTop);
            var pending = PendingIntent.GetActivity(_context, 105, open, PendingIntentFlags.UpdateCurrent | PendingIntentFlags.Immutable);
            using var builder = new Notification.Builder(_context, Channel);
            using var notification = builder
                .SetContentTitle("GnomeAI · Response ready")
                .SetContentText("Your response is complete. Tap to open GnomeAI.")
                .SetSmallIcon(global::Android.Resource.Drawable.IcDialogInfo)
                .SetCategory(Notification.CategoryMessage)
                .SetVisibility(NotificationVisibility.Private)
                .SetContentIntent(pending)
                .SetAutoCancel(true)
                .Build();
            // Independent from the ongoing service notification (104).
            // New replies in one session replace its previous unread notification.
            manager.Notify(tag, 105, notification);
        } catch (Exception error) {
            global::Android.Util.Log.Warn("GnomeAI", "Could not display reply notification: " + error.Message);
        }
    }

    public void Dispose()
    {
        lock (_gate) { _disposed = true; _seen.Clear(); _order.Clear(); }
        _hub.SessionEvent -= OnSessionEvent;
        _main.RemoveCallbacksAndMessages(null);
        _main.Dispose();
    }
}
