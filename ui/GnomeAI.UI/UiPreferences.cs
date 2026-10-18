using System.Text.Json;
using Avalonia.Styling;

namespace GnomeAI.UI;

internal static class UiPreferences
{
    private sealed class StoredPreferences
    {
        public string Theme { get; set; } = "system";
        public Dictionary<string, string> Flags { get; set; } = new(StringComparer.Ordinal);
    }

    public static string Get(string key)
    {
        try
        {
            if (!File.Exists(FilePath)) return "";
            var preferences = JsonSerializer.Deserialize<StoredPreferences>(File.ReadAllText(FilePath));
            return preferences?.Flags.TryGetValue(key, out var value) == true ? value : "";
        }
        catch
        {
            return "";
        }
    }

    public static void Set(string key, string value)
    {
        try
        {
            StoredPreferences? preferences = null;
            if (File.Exists(FilePath))
                preferences = JsonSerializer.Deserialize<StoredPreferences>(File.ReadAllText(FilePath));
            preferences ??= new StoredPreferences();
            preferences.Flags[key] = value;
            Directory.CreateDirectory(DirectoryPath);
            var json = JsonSerializer.Serialize(preferences,
                new JsonSerializerOptions { WriteIndented = true });
            File.WriteAllText(FilePath, json);
        }
        catch
        {
            // Preference persistence must never prevent the desktop UI from opening.
        }
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
