using Android.App;
using Android.Content.PM;
using Android.Content;
using Android.OS;
using Avalonia;
using Avalonia.Android;
using Avalonia.Controls;
using Avalonia.Controls.ApplicationLifetimes;
using Avalonia.Themes.Fluent;
using GnomeAI.Client;
using GnomeAI.Android.UI;

namespace GnomeAI.Android;

[Activity(Label="GnomeAI",Theme="@style/GnomeAITheme",MainLauncher=true,Exported=true,
    ConfigurationChanges=ConfigChanges.Orientation|ConfigChanges.ScreenSize|ConfigChanges.UiMode|ConfigChanges.KeyboardHidden,
    WindowSoftInputMode=global::Android.Views.SoftInput.AdjustResize)]
[IntentFilter(new[] { Intent.ActionView }, Categories=new[] { Intent.CategoryDefault,Intent.CategoryBrowsable }, DataScheme="gnomeai",DataHost="pair")]
public class MainActivity : AvaloniaMainActivity<MobileApp>
{
    private WindowInsetsObserver? _insets;
    protected override void OnCreate(Bundle? savedInstanceState)
    {
        CameraCapture.Attach(this);PairingScanner.Attach(this);
        MobileHost.Ensure(FilesDir!.AbsolutePath);
        base.OnCreate(savedInstanceState);
        _insets=new WindowInsetsObserver(this);
        StartForegroundService(new Intent(this,typeof(DeviceService)));
        if(OperatingSystem.IsAndroidVersionAtLeast(33) && CheckSelfPermission(global::Android.Manifest.Permission.PostNotifications)!=Permission.Granted)
            RequestPermissions(new[]{global::Android.Manifest.Permission.PostNotifications},104);
        if(savedInstanceState is null) HandlePairing(Intent);
    }
    protected override void OnActivityResult(int requestCode,Result resultCode,Intent? data)
    {
        if(!PairingScanner.OnResult(requestCode,resultCode,data) && !CameraCapture.OnResult(this,requestCode,resultCode))base.OnActivityResult(requestCode,resultCode,data);
    }
    public override void OnRequestPermissionsResult(int requestCode,string[] permissions,Permission[] grantResults) {base.OnRequestPermissionsResult(requestCode,permissions,grantResults);CameraPermission.OnResult(requestCode,grantResults);}
    protected override void OnNewIntent(Intent? intent) { base.OnNewIntent(intent);HandlePairing(intent); }
    private void HandlePairing(Intent? intent) {
        if(intent?.DataString is not { } code || !code.StartsWith("gnomeai://pair/",StringComparison.Ordinal)) return;
        try { MobileHost.Hub!.Pair(code); }
        catch(Exception error) { new AlertDialog.Builder(this).SetTitle("Pairing").SetMessage(error.Message).SetPositiveButton("OK",(_,_)=>{}).Show(); }
    }
    protected override AppBuilder CustomizeAppBuilder(AppBuilder builder) => base.CustomizeAppBuilder(builder).WithInterFont();
    protected override void OnDestroy()
    {
        _insets?.Dispose();
        CameraCapture.Detach(this);PairingScanner.Detach(this);if(IsFinishing)CameraPermission.Cancel();
        // The foreground service owns background availability, not the screen.
        base.OnDestroy();
    }
}
public sealed class MobileApp : Avalonia.Application
{
    public override void Initialize() {
        RequestedThemeVariant=Avalonia.Styling.ThemeVariant.Dark;
        Styles.Add(new FluentTheme());
        Styles.Add(new Avalonia.Markup.Xaml.Styling.StyleInclude(new Uri("avares://GnomeAI.UI.Android/")) {Source=new Uri("avares://GnomeAI.UI.Android/MobileTheme.axaml")});
    }
    public override void OnFrameworkInitializationCompleted()
    {
        if(ApplicationLifetime is ISingleViewApplicationLifetime mobile)
        {
            mobile.MainView=new MainView(MobileHost.Hub!,MobileHost.Home,CameraCapture.TakePhotoAsync,PairingScanner.ScanAsync);
            MobileHost.Bridge!.Start();
        }
        base.OnFrameworkInitializationCompleted();
    }
}
internal static class MobileHost
{
    public static event Action<Thickness,bool>? InsetsChanged;
    public static Thickness Insets {get;private set;}
    public static bool KeyboardVisible {get;private set;}
    public static void SetInsets(Thickness padding,bool keyboard) {Insets=padding;KeyboardVisible=keyboard;InsetsChanged?.Invoke(padding,keyboard);}
    private static NetworkObserver? _network;
    public static string Home { get; private set; }="";
    public static NativeAgentBridge? Bridge { get; private set; }
    public static DeviceHub? Hub { get; private set; }
    public static void Ensure(string home)
    {
        if(Bridge is not null) return;
        Home=home;
        Bridge=new NativeAgentBridge(home);
        Hub=new DeviceHub(Bridge,Path.Combine(home,"store","devices"),global::Android.OS.Build.Model??"Android",new AndroidIdentityProtector());
        _network=new NetworkObserver(Hub);
    }
    public static async Task ShutdownAsync()
    {
        _network?.Dispose();_network=null;
        if(Hub is {} hub) await hub.DisposeAsync();
        if(Bridge is {} bridge) await bridge.DisposeAsync();
        Hub=null;Bridge=null;
    }
}
