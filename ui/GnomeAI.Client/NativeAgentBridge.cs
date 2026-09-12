using System.Runtime.InteropServices;
using System.Text.Json;

namespace GnomeAI.Client;

public sealed class NativeAgentBridge(string dataDirectory) : IAgentBridge
{
    private ulong _handle;
    private readonly CancellationTokenSource _stop = new();
    private Task? _reader;
    private int _disposed;
    public event Func<JsonElement, Task>? EventReceived;
    public event Action<string>? Disconnected;
    [DllImport("gnomeai_core", CallingConvention = CallingConvention.Cdecl)]
    private static extern ulong gnomeai_create([MarshalAs(UnmanagedType.LPUTF8Str)] string path);
    [DllImport("gnomeai_core", CallingConvention = CallingConvention.Cdecl)]
    private static extern int gnomeai_send_json(ulong handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string json);
    [DllImport("gnomeai_core", CallingConvention = CallingConvention.Cdecl)]
    private static extern IntPtr gnomeai_next_event(ulong handle);
    [DllImport("gnomeai_core", CallingConvention = CallingConvention.Cdecl)]
    private static extern void gnomeai_free_string(IntPtr text);
    [DllImport("gnomeai_core", CallingConvention = CallingConvention.Cdecl)]
    private static extern void gnomeai_destroy(ulong handle);
    [DllImport("gnomeai_core", CallingConvention = CallingConvention.Cdecl)]
    private static extern void gnomeai_interrupt_events(ulong handle);

    public void Start()
    {
        if (_reader is not null) return;
        _handle = gnomeai_create(dataDirectory);
        if (_handle == 0) throw new IOException("Cannot initialize the native Rust core.");
        _reader = Task.Run(async () =>
        {
            try
            {
                while (!_stop.IsCancellationRequested)
                {
                    var pointer = gnomeai_next_event(_handle);
                    if (pointer == IntPtr.Zero) { if(_stop.IsCancellationRequested)return;throw new IOException("Native event stream ended unexpectedly."); }
                    JsonElement element;
                    try
                    {
                        using var document = JsonDocument.Parse(Marshal.PtrToStringUTF8(pointer)!);
                        element = document.RootElement.Clone();
                    }
                    finally { gnomeai_free_string(pointer); }
                    if (_disposed != 0) return;
                    if (EventReceived is { } handlers)
                        foreach (Func<JsonElement, Task> handler in handlers.GetInvocationList())
                            await handler(element);
                }
            }
            catch (Exception error)
            {
                Disconnected?.Invoke(error.Message);
            }
        });
    }
    public async Task SendAsync(IReadOnlyDictionary<string, object?> operation)
    {
        ObjectDisposedException.ThrowIf(_disposed != 0, this);
        var json = JsonSerializer.Serialize(operation);
        for (var attempt = 0; attempt < 100; attempt++)
        {
            var result = gnomeai_send_json(_handle, json);
            if (result == 0) return;
            if (result != -2) throw new IOException("The native core rejected this operation.");
            await Task.Delay(20, _stop.Token);
        }
        throw new IOException("The native core is busy; try again.");
    }
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        _stop.Cancel();
        if(_handle!=0)gnomeai_interrupt_events(_handle);
        if (_reader is not null) await _reader;
        await Task.Run(() => gnomeai_destroy(_handle));
        _stop.Dispose();
    }
}
