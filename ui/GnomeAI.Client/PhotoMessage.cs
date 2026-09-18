using System.Text.Json;
namespace GnomeAI.Client;

public static class PhotoMessage
{
    // Leaves room for JSON, encryption and envelope overhead in a 24 MiB frame.
    public const int MaxBytes = 8 * 1024 * 1024;
    public static async Task<string> FromFileAsync(string path, string text)
    {
        var mime = Path.GetExtension(path).ToLowerInvariant() switch {
            ".jpg" or ".jpeg" => "image/jpeg", ".png" => "image/png",
            ".webp" => "image/webp", ".gif" => "image/gif",
            _ => throw new IOException("Remote attachments support JPEG, PNG, WebP and GIF photos.")
        };
        await using var input = File.OpenRead(path);
        if (input.Length == 0 || input.Length > MaxBytes)
            throw new IOException("Photo must contain between 1 byte and 8 MiB.");
        using var output = new MemoryStream();
        var buffer = new byte[65536];
        int count;
        while ((count = await input.ReadAsync(buffer)) > 0) {
            if (output.Length + count > MaxBytes) throw new IOException("Photo exceeds 8 MiB.");
            output.Write(buffer, 0, count);
        }
        if (text.Length > 65536) throw new IOException("Photo caption exceeds 65536 characters.");
        return JsonSerializer.Serialize(new object[] {
            new { type = "text", text = string.IsNullOrWhiteSpace(text) ? "Describe this photo." : text },
            new { type = "image_url", image_url = new { url = "data:" + mime + ";base64," + Convert.ToBase64String(output.ToArray()) } }
        });
    }
}
