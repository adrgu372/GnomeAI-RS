using Avalonia.Media;
using Avalonia.Styling;

namespace GnomeAI.Android;

internal static class MobileAppearance
{
    private static WeakReference<MainActivity>? _activity;
    private static global::Android.Content.ISharedPreferences Preferences=>global::Android.App.Application.Context.GetSharedPreferences("gnomeai-appearance",global::Android.Content.FileCreationMode.Private)!;
    public static string Preference=>Preferences.GetString("theme","system")??"system";
    public static ThemeVariant LoadTheme()=>Preference switch{"dark"=>ThemeVariant.Dark,"light"=>ThemeVariant.Light,_=>ThemeVariant.Default};
    public static void Attach(MainActivity activity)=>_activity=new(activity);
    public static void Detach(MainActivity activity){if(_activity?.TryGetTarget(out var current)==true && current==activity)_activity=null;}
    public static void SetTheme(string preference)
    {
        using var editor=Preferences.Edit();editor!.PutString("theme",preference);editor.Apply();
        if(Avalonia.Application.Current is {} app)app.RequestedThemeVariant=LoadTheme();
        Refresh();
    }
    public static void Refresh()
    {
        var dark=Avalonia.Application.Current?.ActualThemeVariant==ThemeVariant.Dark;
        MobilePalette.Apply(dark);
        if(_activity?.TryGetTarget(out var activity)!=true || activity.Window is not {} window)return;
        activity.RunOnUiThread(()=>{
            var color=global::Android.Graphics.Color.ParseColor(dark?"#10191E":"#F5F8F6");
            window.DecorView.SetBackgroundColor(color);
            window.SetBackgroundDrawable(new global::Android.Graphics.Drawables.ColorDrawable(color));
#pragma warning disable CS0618
            if(!OperatingSystem.IsAndroidVersionAtLeast(35)){window.SetStatusBarColor(color);window.SetNavigationBarColor(color);}
#pragma warning restore CS0618
            var controller=global::AndroidX.Core.View.WindowCompat.GetInsetsController(window,window.DecorView);
            controller.AppearanceLightStatusBars=!dark;controller.AppearanceLightNavigationBars=!dark;
        });
    }
}

// Shared mutable brushes update already-created cards/icons without rebuilding conversations.
internal static class MobilePalette
{
    private static readonly Dictionary<string,string> Light=new() {
        ["#10191E"]="#F5F8F6",["#1B282E"]="#FFFFFF",["#31433B"]="#CCDAD2",
        ["#94ABA0"]="#536B5E",["#9FB4A9"]="#536B5E",["#AEC5BA"]="#385D49",
        ["#254037"]="#DCEEE3",["#263C34"]="#E4F0E8"
    };
    private static readonly Dictionary<string,SolidColorBrush> Brushes=new();
    private static bool _dark;
    public static IBrush Ink(string hex)
    {
        if(!Brushes.TryGetValue(hex,out var brush))Brushes[hex]=brush=new SolidColorBrush(Color.Parse(_dark?hex:Light.GetValueOrDefault(hex,hex)));
        return brush;
    }
    public static void Apply(bool dark)
    {
        _dark=dark;
        foreach(var pair in Brushes)pair.Value.Color=Color.Parse(dark?pair.Key:Light.GetValueOrDefault(pair.Key,pair.Key));
    }
}
