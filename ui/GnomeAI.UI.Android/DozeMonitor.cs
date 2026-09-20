using Android.Content;
using Android.OS;
using GnomeAI.Client;

namespace GnomeAI.Android;

/// Android tears sockets down in Doze and the peer links only notice at their
/// next beat, burning battery on guaranteed-failed handshakes. While the
/// system is idle the links are parked; leaving Doze resumes them at once.
internal sealed class DozeMonitor : BroadcastReceiver, IDisposable
{
    private readonly PowerManager _power;
    private readonly DeviceHub _hub;
    private bool _registered;
    private bool _disposed;
    public DozeMonitor(DeviceHub hub)
    {
        _hub=hub;
        _power=(PowerManager)global::Android.App.Application.Context.GetSystemService(Context.PowerService)!;
        var filter=new IntentFilter(PowerManager.ActionDeviceIdleModeChanged);
        global::Android.App.Application.Context.RegisterReceiver(this,filter);
        _registered=true;
    }
    private void Refresh()
    {
        if(_disposed)return;
        foreach(var link in _hub.Links.ToArray()){
            if(_power.IsDeviceIdleMode)link.StopReconnect();
            else link.ResumeReconnect();
        }
        if(!_power.IsDeviceIdleMode)_hub.ReconnectAll();
    }
    public override void OnReceive(Context? context,Intent? intent)
    {
        if(intent?.Action==PowerManager.ActionDeviceIdleModeChanged)Refresh();
    }
    public new void Dispose()
    {
        if(_disposed)return;
        _disposed=true;
        if(_registered)global::Android.App.Application.Context.UnregisterReceiver(this);
        base.Dispose();
    }
}
