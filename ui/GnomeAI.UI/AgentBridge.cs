using System.Collections.Concurrent;
using System.Text.Json;
using Avalonia.Threading;

namespace GnomeAI.UI;

public sealed class AgentBridge : IAsyncDisposable
{
    private readonly StreamReader _reader = new(Console.OpenStandardInput());
    private readonly StreamWriter _writer = new(Console.OpenStandardOutput()) { AutoFlush = true };
    private readonly SemaphoreSlim _writeLock = new(1, 1);
    private readonly CancellationTokenSource _stop = new();
    private readonly ConcurrentQueue<JsonElement> _pendingEvents = new();
    private int _eventDrainScheduled;
    private int _started;
    private int _disposed;
    private int _disconnectNotified;
    private string? _disconnectReason;
    private Task? _readerTask;

    public event Func<JsonElement, Task>? EventReceived;
    public event Action<string>? Disconnected;

    public void Start()
    {
        if (Interlocked.CompareExchange(ref _started, 1, 0) == 0)
            _readerTask = Task.Run(ReadLoopAsync);
    }

    public async Task SendAsync(IReadOnlyDictionary<string, object?> operation)
    {
        if (Volatile.Read(ref _disposed) != 0 || Volatile.Read(ref _disconnectReason) is not null)
            throw new IOException("The connection to the Rust core is closed.");
        var json = JsonSerializer.Serialize(operation);
        await _writeLock.WaitAsync(_stop.Token);
        try { await _writer.WriteLineAsync(json.AsMemory(), _stop.Token); }
        finally { _writeLock.Release(); }
    }

    private async Task ReadLoopAsync()
    {
        try
        {
            while (!_stop.IsCancellationRequested)
            {
                var line = await _reader.ReadLineAsync(_stop.Token);
                if (line is null) break;
                if (string.IsNullOrWhiteSpace(line)) continue;
                try
                {
                    using var document = JsonDocument.Parse(line);
                    var payload = document.RootElement;
                    if (payload.ValueKind != JsonValueKind.Object
                        || !payload.TryGetProperty("event", out var kind)
                        || kind.ValueKind != JsonValueKind.String)
                        throw new JsonException("Expected an event object.");
                    _pendingEvents.Enqueue(payload.Clone());
                }
                catch (JsonException)
                {
                    // One malformed line must not discard the remaining stream.
                    // Never echo the line: it may contain credentials or content.
                    _pendingEvents.Enqueue(JsonSerializer.SerializeToElement(new
                    {
                        @event = "ui_bridge_warning",
                        message = "An invalid event from the Rust core was skipped.",
                    }));
                }
                ScheduleEventDrain();
            }
            Volatile.Write(ref _disconnectReason, "The connection to the Rust core was closed.");
        }
        catch (OperationCanceledException) { }
        catch (Exception error)
        {
            Volatile.Write(ref _disconnectReason, error.Message);
        }
        finally { ScheduleEventDrain(); }
    }

    private void ScheduleEventDrain()
    {
        if (Interlocked.CompareExchange(ref _eventDrainScheduled, 1, 0) == 0)
            Dispatcher.UIThread.Post(DrainEvents, DispatcherPriority.Background);
    }

    private async void DrainEvents()
    {
        if (Volatile.Read(ref _disposed) != 0) return;
        const int maxEventsPerPass = 128;
        var processed = 0;
        while (processed < maxEventsPerPass && _pendingEvents.TryDequeue(out var payload))
        {
            try
            {
                if (EventReceived is { } handlers)
                    foreach (Func<JsonElement, Task> handler in handlers.GetInvocationList())
                        await handler(payload);
            }
            catch (Exception error)
            {
                // Guard the async-void dispatcher boundary even when a future
                // subscriber does not have its own UI error boundary.
                System.Diagnostics.Debug.WriteLine($"Cannot handle a core event: {error.GetType().Name}");
            }
            if (Volatile.Read(ref _disposed) != 0) return;
            processed++;
        }

        if (!_pendingEvents.IsEmpty)
        {
            Dispatcher.UIThread.Post(DrainEvents, DispatcherPriority.Background);
            return;
        }

        Interlocked.Exchange(ref _eventDrainScheduled, 0);
        if (!_pendingEvents.IsEmpty) ScheduleEventDrain();
        else if (Volatile.Read(ref _disconnectReason) is { } reason
                 && Interlocked.Exchange(ref _disconnectNotified, 1) == 0)
        {
            // Deliver the last buffered tokens before announcing EOF.
            Disconnected?.Invoke(reason);
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        _stop.Cancel();
        if (_readerTask is not null)
        {
            try { await _readerTask; } catch (OperationCanceledException) { }
        }
        // Let a cancelled writer release its lock before disposing shared I/O.
        await _writeLock.WaitAsync();
        try
        {
            _reader.Dispose();
            await _writer.DisposeAsync();
        }
        finally { _writeLock.Release(); }
        _stop.Dispose();
    }
}
