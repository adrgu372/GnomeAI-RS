using Android.App;
using Android.OS;
using Android.Views;
using AndroidX.Core.View;
using Avalonia;

namespace GnomeAI.Android;

/// Measure the actual native content rectangle, including AdjustResize. Only
/// the remaining overlap is inset, so a resized window is never shrunk twice.
internal sealed class WindowInsetsObserver : Java.Lang.Object, ViewTreeObserver.IOnGlobalLayoutListener, IDisposable
{
    private readonly Activity _activity;
    private readonly View _decor, _content;
    private Thickness _last;
    private bool _lastIme,_initialized;
    public WindowInsetsObserver(Activity activity) {
        _activity=activity;_decor=activity.Window!.DecorView;
        _content=activity.FindViewById(global::Android.Resource.Id.Content)??_decor;
        _decor.ViewTreeObserver!.AddOnGlobalLayoutListener(this);
        _decor.Post(OnGlobalLayout);
    }
    public void OnGlobalLayout() {
        if(_content.Height==0)return;
        var insets=ViewCompat.GetRootWindowInsets(_decor);
        var ime=insets?.IsVisible(WindowInsetsCompat.Type.Ime())==true;
        var pos=new int[2];_content.GetLocationOnScreen(pos);
        using var visible=new global::Android.Graphics.Rect();_decor.GetWindowVisibleDisplayFrame(visible);
        double left=0,top=0,right=0,bottom=Math.Max(0,pos[1]+_content.Height-visible.Bottom);
        if(OperatingSystem.IsAndroidVersionAtLeast(30) && insets is not null) {
            using var bounds=_activity.WindowManager!.CurrentWindowMetrics.Bounds;
            var bars=insets.GetInsets(WindowInsetsCompat.Type.SystemBars()|WindowInsetsCompat.Type.DisplayCutout());
            var keyboard=ime?insets.GetInsets(WindowInsetsCompat.Type.Ime()).Bottom:0;
            left=Math.Max(0,bounds.Left+bars.Left-pos[0]);
            top=Math.Max(0,bounds.Top+bars.Top-pos[1]);
            right=Math.Max(0,pos[0]+_content.Width-(bounds.Right-bars.Right));
            bottom=Math.Max(0,pos[1]+_content.Height-(bounds.Bottom-Math.Max(keyboard,bars.Bottom)));
        }
        var density=_activity.Resources!.DisplayMetrics!.Density;
        var padding=new Thickness(left/density,top/density,right/density,bottom/density);
        if(_initialized && padding==_last && ime==_lastIme)return;
        _initialized=true;
        _last=padding;_lastIme=ime;MobileHost.SetInsets(padding,ime);
    }
    public new void Dispose() {
        if(_decor.ViewTreeObserver?.IsAlive==true)_decor.ViewTreeObserver.RemoveOnGlobalLayoutListener(this);
        base.Dispose();
    }
}
