using Android.App;
using Android.Content;
using Android.Content.PM;
using Android.OS;

namespace GnomeAI.Android;

// Started by a visible activity. No boot receiver or automatic restart after
// force-stop: Android and the user retain control over this foreground service.
// specialUse is declared in AndroidManifest.xml; API 34 is guarded below.
[Service(Name="io.github.adrgu372.gnomeai.DeviceService",Exported=false)]
public sealed class DeviceService : Service
{
    private const string Channel="gnomeai_devices";
    public override IBinder? OnBind(Intent? intent)=>null;
    public override StartCommandResult OnStartCommand(Intent? intent,StartCommandFlags flags,int startId)
    {
        var manager=(NotificationManager)GetSystemService(NotificationService)!;
        manager.CreateNotificationChannel(new NotificationChannel(Channel,"Agent and paired devices",NotificationImportance.Low));
        var open=new Intent(this,typeof(MainActivity));open.AddFlags(ActivityFlags.SingleTop|ActivityFlags.ClearTop);
        var pending=PendingIntent.GetActivity(this,0,open,PendingIntentFlags.UpdateCurrent|PendingIntentFlags.Immutable);
        var notification=new Notification.Builder(this,Channel)
            .SetContentTitle("GnomeAI is available")
            .SetContentText("Agent sessions and paired devices remain connected.")
            .SetSmallIcon(global::Android.Resource.Drawable.IcDialogInfo)
            .SetContentIntent(pending).SetOngoing(true).SetOnlyAlertOnce(true).Build();
        if(OperatingSystem.IsAndroidVersionAtLeast(34))
            StartForeground(104,notification,ForegroundService.TypeSpecialUse);
        else StartForeground(104,notification);
        return StartCommandResult.NotSticky;
    }
}
