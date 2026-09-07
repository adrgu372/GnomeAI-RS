using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace GnomeAI.UI;

internal sealed record ComposerDraft(string Text, AttachedFile? Attachment, int CaretIndex)
{
    public static ComposerDraft Empty { get; } = new("", null, 0);
}

internal sealed record QueuedSubmission(string Text, AttachedFile? Attachment);
internal sealed record SavedComposer(ComposerDraft Draft, List<QueuedSubmission> Queue);

/// <summary>Private, atomic snapshots of work that has not been sent yet.</summary>
internal sealed class ComposerDraftStore
{
    private readonly string _directory = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "gnomeai-rs", "drafts");

    private string PathFor(string sessionId) => Path.Combine(_directory,
        Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(sessionId))) + ".json");

    public SavedComposer Load(string sessionId)
    {
        var path = PathFor(sessionId);
        if (!File.Exists(path)) return new(ComposerDraft.Empty, []);
        var saved = JsonSerializer.Deserialize<SavedComposer>(File.ReadAllText(path));
        if (saved?.Draft?.Text is null || saved.Queue is null
            || saved.Queue.Any(item => item is null || item.Text is null))
            throw new InvalidDataException("The saved draft has an invalid format.");
        return saved;
    }

    public void Save(string sessionId, SavedComposer saved)
    {
        var path = PathFor(sessionId);
        if (saved.Draft.Text.Length == 0 && saved.Draft.Attachment is null && saved.Queue.Count == 0)
        {
            File.Delete(path);
            return;
        }

        Directory.CreateDirectory(_directory);
        if (!OperatingSystem.IsWindows())
            File.SetUnixFileMode(_directory, UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute);

        // A unique sibling avoids partial JSON and collisions between windows.
        var temporary = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
        try
        {
            var options = new FileStreamOptions { Mode = FileMode.CreateNew, Access = FileAccess.Write };
            if (!OperatingSystem.IsWindows())
                options.UnixCreateMode = UnixFileMode.UserRead | UnixFileMode.UserWrite;
            using (var stream = new FileStream(temporary, options))
            {
                JsonSerializer.Serialize(stream, saved);
                stream.Flush(flushToDisk: true);
            }
            File.Move(temporary, path, overwrite: true);
        }
        finally
        {
            if (File.Exists(temporary)) File.Delete(temporary);
        }
    }
}
