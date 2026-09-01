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
    private Task? _readerTask;

    public event Func<JsonElement, Task>? EventReceived;
    public event Action<string>? Disconnected;

    public void Start() => _readerTask = Task.Run(ReadLoopAsync);

    public async Task SendAsync(IReadOnlyDictionary<string, object?> operation)
    {
        var json = JsonSerializer.Serialize(operation);
        await _writeLock.WaitAsync(_stop.Token);
        try { await _writer.WriteLineAsync(json); }
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
                using var document = JsonDocument.Parse(line);
                var payload = document.RootElement.Clone();
                _pendingEvents.Enqueue(payload);
                ScheduleEventDrain();
            }
            Dispatcher.UIThread.Post(() => Disconnected?.Invoke("The connection to the Rust core was closed."));
        }
        catch (OperationCanceledException) { }
        catch (Exception error)
        {
            Dispatcher.UIThread.Post(() => Disconnected?.Invoke(error.Message));
        }
    }

    private void ScheduleEventDrain()
    {
        if (Interlocked.CompareExchange(ref _eventDrainScheduled, 1, 0) == 0)
            Dispatcher.UIThread.Post(DrainEvents, DispatcherPriority.Background);
    }

    private async void DrainEvents()
    {
        const int maxEventsPerPass = 128;
        var processed = 0;
        while (processed < maxEventsPerPass && _pendingEvents.TryDequeue(out var payload))
        {
            if (EventReceived is { } handler) await handler(payload);
            processed++;
        }

        if (!_pendingEvents.IsEmpty)
        {
            Dispatcher.UIThread.Post(DrainEvents, DispatcherPriority.Background);
            return;
        }

        Interlocked.Exchange(ref _eventDrainScheduled, 0);
        if (!_pendingEvents.IsEmpty) ScheduleEventDrain();
    }

    public async ValueTask DisposeAsync()
    {
        _stop.Cancel();
        if (_readerTask is not null)
        {
            try { await _readerTask; } catch (OperationCanceledException) { }
        }
        _writeLock.Dispose();
        _stop.Dispose();
    }
}
