using System.Text.Json;
using Avalonia.Styling;

namespace GnomeAI.UI;

internal static class UiPreferences
{
    private sealed class StoredPreferences
    {
        public string Theme { get; set; } = "system";
    }

    private static readonly string DirectoryPath = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "gnomeai-rs");
    private static readonly string FilePath = Path.Combine(DirectoryPath, "ui.json");

    public static string LoadThemeMode()
    {
        try
        {
            if (!File.Exists(FilePath)) return "system";
            var preferences = JsonSerializer.Deserialize<StoredPreferences>(File.ReadAllText(FilePath));
            return NormalizeThemeMode(preferences?.Theme);
        }
        catch
        {
            return "system";
        }
    }

    public static void SaveThemeMode(string mode)
    {
        try
        {
            Directory.CreateDirectory(DirectoryPath);
            var json = JsonSerializer.Serialize(new StoredPreferences { Theme = NormalizeThemeMode(mode) },
                new JsonSerializerOptions { WriteIndented = true });
            File.WriteAllText(FilePath, json);
        }
        catch
        {
            // Theme persistence must never prevent the desktop UI from opening.
        }
    }

    public static string NormalizeThemeMode(string? mode) => mode?.Trim().ToLowerInvariant() switch
    {
        "light" => "light",
        "dark" => "dark",
        _ => "system",
    };

    public static ThemeVariant ToThemeVariant(string mode) => NormalizeThemeMode(mode) switch
    {
        "light" => ThemeVariant.Light,
        "dark" => ThemeVariant.Dark,
        _ => ThemeVariant.Default,
    };
}
