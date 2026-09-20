using Android.Content;
using Android.Net;
using GnomeAI.Client;

namespace GnomeAI.Android;

internal sealed class NetworkObserver : ConnectivityManager.NetworkCallback, IDisposable
{
    private readonly ConnectivityManager _manager;
    private readonly DeviceHub _hub;
    private readonly Timer _debounce;
    private int _available;
    private bool _disposed;
    public NetworkObserver(DeviceHub hub) {
        _hub=hub;
        _manager=(ConnectivityManager)global::Android.App.Application.Context.GetSystemService(Context.ConnectivityService)!;
        // USB plugging, Wi-Fi/cell handovers and captive-portal checks all
        // fire callback storms. Only a real loss of the last network (or its
        // return) is worth tearing down live peer sockets; everything else
        // keeps its connections and its battery.
        _debounce=new Timer(_=>{
            if(_disposed)return;
            bool connected=Volatile.Read(ref _available)>0;
            if(connected || _hadNetwork) _hub.ReconnectAll();
            _hadNetwork=connected;
        },null,Timeout.Infinite,Timeout.Infinite);
        _manager.RegisterDefaultNetworkCallback(this);
    }
    public override void OnAvailable(Network network){
        if(_disposed)return;
        Interlocked.Increment(ref _available);
        _debounce.Change(5000,Timeout.Infinite);
    }
    public override void OnLost(Network network){
        if(_disposed)return;
        Interlocked.Decrement(ref _available);
        _debounce.Change(5000,Timeout.Infinite);
    }
    private bool _hadNetwork;
    public new void Dispose(){_disposed=true;_manager.UnregisterNetworkCallback(this);_debounce.Dispose();base.Dispose();}
}
