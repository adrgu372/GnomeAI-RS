using System.Text.Json;

namespace GnomeAI.Client;

/// A device's conversations, snapshots and revision-ordered live events.
/// This class contains no UI; desktop and mobile choose their own presentation.
public sealed class DeviceSessionClient : IAsyncDisposable
{
    private readonly DeviceHub _hub;
    public PeerLink Peer {get;}
    public string SessionId {get;private set;}="";
    public event Action<JsonElement>? SessionsReceived;
    public event Action<string,JsonElement>? SnapshotReceived;
    public event Action<JsonElement>? SessionEventReceived;
    public event Action<string>? Error;
    private readonly CancellationTokenSource _stop=new();
    private readonly SemaphoreSlim _refresh=new(1,1);
    private readonly object _gate=new();
    private readonly List<JsonElement> _buffer=[];
    private string _epoch="";
    private long _revision;
    private int _generation;
    private int _needList=1,_needSnapshot=1;
    private bool _snapshotting;
    private bool _wasOnline;
    private Task? _loop;
    private readonly SemaphoreSlim _wake=new(1,1);
    private void Wake(){try{_wake.Release();}catch(SemaphoreFullException){}}
    private void NeedList(){Interlocked.Exchange(ref _needList,1);Wake();}
    private void NeedSnapshot(){Interlocked.Exchange(ref _needSnapshot,1);Wake();}
    public DeviceSessionClient(DeviceHub hub,PeerLink peer) {
        _hub=hub;Peer=peer;hub.SessionEvent+=OnEvent;peer.Changed+=OnConnectionChanged;
        _wasOnline=peer.Online;
    }
    public void Start()=>_loop??=Task.Run(RefreshLoopAsync);
    private void OnConnectionChanged() {
        var online=Peer.Online;
        if(online && !_wasOnline){NeedList();NeedSnapshot();}
        _wasOnline=online;
    }
    public async Task RefreshListAsync() {
        var result=await _hub.RequestAsync(Peer,"list",new {}).WaitAsync(_stop.Token);
        if(!_stop.IsCancellationRequested)SessionsReceived?.Invoke(result);
    }
    public async Task SelectSessionAsync(string id) {
        lock(_gate){SessionId=id;_generation++;_buffer.Clear();}
        if(id.Length>0)await RefreshSnapshotAsync();
    }
    public async Task<string> NewAsync() {
        var result=await _hub.RequestAsync(Peer,"new",new {}).WaitAsync(_stop.Token);
        var id=result.GetProperty("session_id").GetString()!;
        await SelectSessionAsync(id);await RefreshListAsync();return id;
    }
    public Task<JsonElement> RequestAsync(string action,object payload)=>_hub.RequestAsync(Peer,action,payload).WaitAsync(_stop.Token);
    public async Task RefreshSnapshotAsync() {
        await _refresh.WaitAsync(_stop.Token);
        try {
            string id;int generation;
            lock(_gate){id=SessionId;generation=_generation;if(id.Length==0)return;_snapshotting=true;}
            var snapshot=await _hub.RequestAsync(Peer,"snapshot",new {session_id=id}).WaitAsync(_stop.Token);
            lock(_gate) {
                if(generation!=_generation)return;
                _epoch=snapshot.GetProperty("epoch").GetString()!;_revision=snapshot.GetProperty("revision").GetInt64();
                SnapshotReceived?.Invoke(id,snapshot);
                var buffered=_buffer.ToArray();_buffer.Clear();_snapshotting=false;
                // Publish buffered callbacks before admitting newer live frames.
                // Subscribers enqueue UI work; no asynchronous work runs here.
                foreach(var frame in buffered)OnEvent(Peer,frame);
            }
        } finally {lock(_gate)_snapshotting=false;_refresh.Release();}
    }
    private void OnEvent(PeerLink? source,JsonElement frame) {
        if(source!=Peer || _stop.IsCancellationRequested)return;
        JsonElement payload;bool gap;
        lock(_gate) {
            if(_snapshotting){if(_buffer.Count<4096)_buffer.Add(frame.Clone());else NeedSnapshot();return;}
            var epoch=frame.GetProperty("epoch").GetString()!;var revision=frame.GetProperty("revision").GetInt64();
            if(epoch==_epoch && revision<=_revision)return;
            gap=epoch!=_epoch || (_revision>0 && revision!=_revision+1);
            _epoch=epoch;_revision=revision;payload=frame.GetProperty("payload").Clone();
        if(gap)NeedSnapshot();
        var kind=payload.GetProperty("event").GetString();
        if(kind=="session_list"){NeedList();return;}
        if(kind=="session_event") {
            SessionEventReceived?.Invoke(payload);
            var detail=payload.GetProperty("payload").GetProperty("event").GetString();
            if(payload.GetProperty("session_id").GetString()==SessionId && detail is "turn_completed" or "interrupted" or "error")NeedSnapshot();
        }
    }
    }
    private async Task RefreshLoopAsync() {
        try {while(!_stop.IsCancellationRequested) {
            await _wake.WaitAsync(_stop.Token);
            try {
                if(Interlocked.Exchange(ref _needList,0)!=0)await RefreshListAsync();
                if(Interlocked.Exchange(ref _needSnapshot,0)!=0 && SessionId.Length>0)await RefreshSnapshotAsync();
            }catch(OperationCanceledException) when(_stop.IsCancellationRequested){break;}
            catch(Exception error){Error?.Invoke(error.Message);await Task.Delay(5000,_stop.Token);NeedList();NeedSnapshot();}
        }}catch(OperationCanceledException) when(_stop.IsCancellationRequested){}
    }
    public async ValueTask DisposeAsync() {
        _stop.Cancel();_hub.SessionEvent-=OnEvent;Peer.Changed-=OnConnectionChanged;if(_loop is not null)await _loop;
        // An explicit navigation refresh may still be unwinding its cancellation.
    }
}
