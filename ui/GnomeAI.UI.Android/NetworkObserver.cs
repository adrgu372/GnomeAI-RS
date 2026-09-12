using Android.Content;
using Android.Net;
using GnomeAI.Client;

namespace GnomeAI.Android;

internal sealed class NetworkObserver : ConnectivityManager.NetworkCallback, IDisposable
{
    private readonly ConnectivityManager _manager;
    private readonly DeviceHub _hub;
    private readonly Timer _debounce;
    private bool _disposed;
    public NetworkObserver(DeviceHub hub) {
        _hub=hub;
        _manager=(ConnectivityManager)global::Android.App.Application.Context.GetSystemService(Context.ConnectivityService)!;
        _debounce=new Timer(_=>{if(!_disposed)_hub.ReconnectAll();},null,Timeout.Infinite,Timeout.Infinite);
        _manager.RegisterDefaultNetworkCallback(this);
    }
    public override void OnAvailable(Network network){if(!_disposed)_debounce.Change(1000,Timeout.Infinite);}
    public override void OnLost(Network network){if(!_disposed)_debounce.Change(1000,Timeout.Infinite);}
    public new void Dispose(){_disposed=true;_manager.UnregisterNetworkCallback(this);_debounce.Dispose();base.Dispose();}
}
