using System.Text.Json;

namespace GnomeAI.Client;

public interface IAgentBridge : IAsyncDisposable
{
    event Func<JsonElement, Task>? EventReceived;
    event Action<string>? Disconnected;
    void Start();
    Task SendAsync(IReadOnlyDictionary<string, object?> operation);
}
