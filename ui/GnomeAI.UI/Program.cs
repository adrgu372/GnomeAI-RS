using Avalonia;

namespace GnomeAI.UI;

internal static class Program
{
    [STAThread]
    public static void Main(string[] args) => BuildAvaloniaApp().StartWithClassicDesktopLifetime(args);

    public static AppBuilder BuildAvaloniaApp() =>
        AppBuilder.Configure<App>()
            .UsePlatformDetect()
            .WithInterFont()
            .With(new Avalonia.Media.FontManagerOptions {
                FontFallbacks = new[] {
                    new Avalonia.Media.FontFallback {
                        FontFamily = new Avalonia.Media.FontFamily("avares://GnomeAI.UI/Assets/Fonts#Noto Color Emoji")
                    }
                }
            })
            .LogToTrace();
}
