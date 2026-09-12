using System.Collections.Concurrent;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace GnomeAI.Client;

/// One service per UI process; desktop and Android both expose the same peer API.
public sealed class DeviceHub : IAsyncDisposable
{
    public DeviceIdentity Identity { get; }
    public IAgentBridge Bridge { get; }
    public JsonElement Config { get; private set; }
    private readonly ConcurrentDictionary<string,PeerLink> _links=new();
    public IReadOnlyList<PeerLink> Links=>_links.Values.ToArray();
    public event Action? Changed;
    public event Action<PeerLink?, JsonElement>? SessionEvent;
    private readonly string _directory;
    private readonly PeerTransport _transport;
    private readonly WorkspaceSync _workspace;
    private readonly SemaphoreSlim _syncLock=new(1,1);
    private readonly CancellationTokenSource _stop=new();
    private readonly Task _autoSync;
    private readonly SemaphoreSlim _syncWake=new(0,1);
    private void WakeSync(){try{_syncWake.Release();}catch(SemaphoreFullException){}}
    private sealed record SyncBinding(string Peer,string Session);
    private readonly List<SyncBinding> _bindings=[];
    public string SyncStatus {get;private set;}="";
    private readonly ConcurrentDictionary<string, TaskCompletionSource<JsonElement>> _requests = new();
    private readonly ConcurrentDictionary<string, JsonElement> _live = new();
    private readonly Dictionary<string,Dictionary<string,JsonElement>> _approvals=[];
    private readonly SemaphoreSlim _transferLock = new(1,1);
    private readonly object _stateGate=new();
    private readonly ConcurrentDictionary<string,string> _snapshotRequests=new();
    private long _revision;
    private readonly string _epoch = Guid.NewGuid().ToString();
    private bool _disposed;
    public DeviceHub(IAgentBridge bridge, string directory, string name,IIdentityProtector? protector=null)
    {
        Bridge=bridge; _directory=directory;
        Identity=DeviceIdentity.Open(directory,name,protector);
        _transport=new PeerTransport(async(action,payload)=>Unwrap(await CoreRawAsync(Identity.Id,action,payload)));
        _workspace=new WorkspaceSync(directory,async(action,payload)=>Unwrap(await CoreRawAsync(Identity.Id,action,payload)));
        var bindings=Path.Combine(directory,"sync-bindings.json");
        if(File.Exists(bindings))_bindings=JsonSerializer.Deserialize<List<SyncBinding>>(File.ReadAllText(bindings))??[];
        Bridge.EventReceived+=OnCoreEvent;
        foreach (var peer in Identity.Peers.Where(p => p.Trusted || p.Expires>DateTimeOffset.UtcNow.ToUnixTimeSeconds()).ToArray()) Attach(peer);
        _autoSync=Task.Run(AutoSyncAsync);
    }
    private PeerLink Attach(PeerRecord peer)
    {
        var link=new PeerLink(Identity,peer,_transport);
        link.RequestReceived=(id,action,payload) => {
            if(action.StartsWith("mesh_",StringComparison.Ordinal) || action=="workspace_location")throw new IOException("Local operation only.");
            return action.StartsWith("workspace_",StringComparison.Ordinal)?WorkspaceReplyAsync(action,payload):CoreRawAsync(peer.Id,action,payload,id);
        };
        link.Changed+=() => Changed?.Invoke();
        link.PairingRevoked+=()=>{_=RemoveRevokedAsync(link);};
        link.Event+=message => SessionEvent?.Invoke(link,message);
        _links[peer.Channel]=link; link.Start(); return link;
    }
    public void ReconnectAll() {if(!_disposed)foreach(var link in Links)link.Reconnect();}
    public async Task<string> CreatePairingAsync(string relay="tor")
    {
        var (peer,code)=Identity.CreateInvitation(relay);
        try {
            if(relay=="tor") {
                var endpoint=await _transport.PublishAsync(peer.Channel,CancellationToken.None);
                Identity.Update(()=>peer.Relay=endpoint);code=Identity.InvitationCode(peer);
            }
            Attach(peer);Changed?.Invoke();return code;
        } catch {Identity.Update(()=>Identity.Peers.Remove(peer));try{await _transport.RevokeAsync(peer);}catch{}throw;}
    }
    public void Pair(string code) { Attach(Identity.AcceptInvitation(code.Trim())); Changed?.Invoke(); }
    public async Task ForgetAsync(PeerLink link)
    {
        if(HasPendingTransfer)throw new IOException("Finish the pending transfer before revoking a device.");
        link.StopReconnect();
        var acknowledged=false;
        if(link.Online && link.Peer.Trusted) {
            try{await link.RevokePairingAsync();acknowledged=true;}catch(Exception){/* Local revocation must still finish. */}
        }
        await RemovePeerAsync(link);
        SyncStatus=acknowledged?"Pairing revoked on both devices":"Device forgotten locally. If the other device was offline, forget this device there too.";
        Changed?.Invoke();
    }
    private async Task RemoveRevokedAsync(PeerLink link)
    {
        try{await RemovePeerAsync(link);SyncStatus="The other device revoked this pairing.";Changed?.Invoke();}
        catch(Exception){SyncStatus="Pairing stopped, but removing its saved state failed. Retry Forget device.";Changed?.Invoke();}
    }
    private async Task RemovePeerAsync(PeerLink link)
    {
        link.StopReconnect();
        if(!_links.TryRemove(link.Peer.Channel,out _))return;
        try {
            Identity.Update(()=>Identity.Peers.Remove(link.Peer));
            lock(_bindings){_bindings.RemoveAll(b=>b.Peer==link.Peer.Id);SaveBindings();}
            if(Guid.TryParse(link.Peer.Id,out _)) {
                var cache=Path.Combine(_directory,"workspace-sync");
                if(Directory.Exists(cache))foreach(var file in Directory.EnumerateFiles(cache,"base-"+link.Peer.Id+"-*.json"))File.Delete(file);
            }
        } finally {
            await link.DisposeAsync();
            try{await _transport.RevokeAsync(link.Peer);}finally{Changed?.Invoke();}
        }
    }
    private Task OnCoreEvent(JsonElement message) { lock(_stateGate) return ProcessCoreEvent(message); }
    private Task ProcessCoreEvent(JsonElement message)
    {
        if (_disposed) return Task.CompletedTask;
        var kind=message.GetProperty("event").GetString();
        if (kind=="ui_config") Config=message.Clone();
        if (kind=="device_response")
        {
            var requestId=message.GetProperty("request_id").GetString()!;
            var result=message.GetProperty("result").Clone();
            if (_snapshotRequests.TryRemove(requestId,out var session) && result.GetProperty("ok").GetBoolean())
            {
                var data=result.GetProperty("data");_live.TryGetValue(session,out var live);
                result=JsonSerializer.SerializeToElement(new { ok=true,data=new {
                    session=data.GetProperty("session"),turns=data.GetProperty("turns"),busy=data.GetProperty("busy"),
                    approvals=_approvals.TryGetValue(session,out var approvals)?approvals.Values.ToArray():Array.Empty<JsonElement>(),
                    live=live.ValueKind==JsonValueKind.Undefined?(object?)null:live,epoch=_epoch,revision=_revision } });
            }
            if (_requests.TryRemove(requestId,out var request)) request.TrySetResult(result);
            return Task.CompletedTask;
        }
        // Never export Ready/ui_config: these contain local service credentials
        // or configured MCP headers. Only transcript events cross this boundary.
        if (kind=="session_event")
        {
            var id=message.GetProperty("session_id").GetString()!;
            var payload=message.GetProperty("payload");
            var eventKind=payload.GetProperty("event").GetString();
            if (eventKind=="privilege_credential_request") return Task.CompletedTask;
            if(eventKind=="approval_request") {
                if(!_approvals.TryGetValue(id,out var pending)) _approvals[id]=pending=[];
                pending[payload.GetProperty("call_id").GetString()!]=payload.Clone();
            }
            if(eventKind=="tool_call_ended" && _approvals.TryGetValue(id,out var waiting))
                waiting.Remove(payload.GetProperty("call_id").GetString()!);
            if (eventKind=="turn_started") _live[id]=JsonSerializer.SerializeToElement(new { text="",reasoning="" });
            if (eventKind is "token" or "reasoning")
            {
                _live.TryGetValue(id,out var previous);
                string Read(string key) => previous.ValueKind==JsonValueKind.Object && previous.TryGetProperty(key,out var value) ? value.GetString() ?? "" : "";
                var text=Read("text"); var reasoning=Read("reasoning");
                if (eventKind=="token") text+=payload.GetProperty("text").GetString(); else reasoning+=payload.GetProperty("text").GetString();
                // Live cache is bounded; durable transcript stays in Rust/SQLite.
                _live[id]=JsonSerializer.SerializeToElement(new { text=text.Length>262144?text[^262144..]:text,
                    reasoning=reasoning.Length>65536?reasoning[^65536..]:reasoning });
            }
            if (eventKind is "turn_completed" or "interrupted" or "error") { _live.TryRemove(id,out _);_approvals.Remove(id); }
        }
        else if (kind is not ("session_list" or "session_reset")) return Task.CompletedTask;
        var routed=JsonSerializer.SerializeToElement(new { epoch=_epoch,revision=Interlocked.Increment(ref _revision),payload=message });
        SessionEvent?.Invoke(null,routed);
        foreach (var link in Links.ToArray().Where(l=>l.Online)) _=NotifySafelyAsync(link,routed);
        return Task.CompletedTask;
    }
    private static async Task NotifySafelyAsync(PeerLink link,JsonElement message)
    {
        try { await link.NotifyAsync(message); } catch (Exception) { /* Revision gaps cause the view to re-snapshot. */ }
    }
    private async Task<JsonElement> CoreRawAsync(string peer,string action,object payload,string? id=null)
    {
        var requestId=id ?? Guid.NewGuid().ToString();
        var completion=new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        if (!_requests.TryAdd(requestId,completion)) throw new IOException("Device request is already pending.");
        try
        {
            if(action=="snapshot") _snapshotRequests[requestId]=JsonSerializer.SerializeToElement(payload).GetProperty("session_id").GetString()!;
            await Bridge.SendAsync(new Dictionary<string,object?> { ["op"]="device_request",["request_id"]=requestId,["peer_id"]=peer,["action"]=action,["payload"]=payload });
            return await completion.Task.WaitAsync(TimeSpan.FromSeconds(40));
        }
        finally { _requests.TryRemove(requestId,out _);_snapshotRequests.TryRemove(requestId,out _); }
    }
    private static JsonElement Unwrap(JsonElement result)
    {
        if (!result.GetProperty("ok").GetBoolean()) throw new IOException(result.GetProperty("error").GetString());
        return result.GetProperty("data").Clone();
    }
    public async Task<JsonElement> RequestAsync(PeerLink? link,string action,object payload)
    {
        if(link is null) return action.StartsWith("workspace_",StringComparison.Ordinal)?await _workspace.HandleAsync(action,JsonSerializer.SerializeToElement(payload)):Unwrap(await CoreRawAsync(Identity.Id,action,payload));
        var cacheable=action is "list" or "snapshot";
        var key=Convert.ToHexString(SHA256.HashData(JsonSerializer.SerializeToUtf8Bytes(payload)));
        var cache=Path.Combine(_directory,$"cache-{link.Peer.Id}-{action}-{key}.json");
        try
        {
            var data=Unwrap(await link.RequestAsync(action,payload));
            if(cacheable)
            {
                var options=new FileStreamOptions {Mode=FileMode.Create,Access=FileAccess.Write};
                if(!OperatingSystem.IsWindows()) options.UnixCreateMode=UnixFileMode.UserRead|UnixFileMode.UserWrite;
                var temporary=cache+"."+Guid.NewGuid()+".tmp";
                using(var file=new FileStream(temporary,options)) JsonSerializer.Serialize(file,data);
                File.Move(temporary,cache,true);
            }
            return data;
        }
        catch(Exception error) when(cacheable && !link.Online && File.Exists(cache) && error is IOException or TimeoutException)
        { return JsonSerializer.Deserialize<JsonElement>(await File.ReadAllTextAsync(cache)); }
    }

    private sealed record Transfer(string Id,string PeerId,string SessionId,bool Pull,bool WorkspaceSynced=false);
    private string TransferFile => Path.Combine(_directory,"pending-transfer.json");
    public bool HasPendingTransfer => File.Exists(TransferFile);
    private static string StepId(string id,string step) => new Guid(SHA256.HashData(Encoding.UTF8.GetBytes(id+"|"+step)).AsSpan(0,16)).ToString();
    public async Task MergeMemoriesAsync(PeerLink link)
    {
        var local=await RequestAsync(null,"memory_export",new {});
        var remote=await RequestAsync(link,"memory_export",new {});
        await RequestAsync(null,"memory_merge",remote);
        await RequestAsync(link,"memory_merge",local);
    }
    public async Task MoveAsync(PeerLink link,string sessionId,bool pull)
    {
        await _transferLock.WaitAsync();
        try
        {
            if (HasPendingTransfer) throw new IOException("Resume the pending session transfer first.");
            var info=await RequestAsync(pull?link:null,"handoff_info",new {session_id=sessionId});
            await RequestAsync(pull?null:link,"can_stage",info);
            var transfer=new Transfer(Guid.NewGuid().ToString(),link.Peer.Id,sessionId,pull);
            var options=new FileStreamOptions { Mode=FileMode.CreateNew,Access=FileAccess.Write };
            if (!OperatingSystem.IsWindows()) options.UnixCreateMode=UnixFileMode.UserRead|UnixFileMode.UserWrite;
            using (var file=new FileStream(TransferFile,options)) { JsonSerializer.Serialize(file,transfer); file.Flush(true); }
            await ContinueMoveAsync(link,transfer);
        }
        finally { _transferLock.Release(); }
    }
    public async Task ResumeMoveAsync()
    {
        await _transferLock.WaitAsync();
        try
        {
            var transfer=JsonSerializer.Deserialize<Transfer>(await File.ReadAllTextAsync(TransferFile)) ?? throw new IOException("Invalid pending transfer.");
            var link=Links.Single(l=>l.Peer.Id==transfer.PeerId);
            await ContinueMoveAsync(link,transfer);
        }
        finally { _transferLock.Release(); }
    }
    private async Task ContinueMoveAsync(PeerLink link,Transfer transfer)
    {
        async Task<JsonElement> At(bool remote,string action,object payload)
            => Unwrap(remote ? await link.RequestAsync(action,payload,StepId(transfer.Id,action))
                : await CoreRawAsync(link.Peer.Id,action,payload,StepId(transfer.Id,action)));
        var payload=new { session_id=transfer.SessionId,transfer_id=transfer.Id };
        var info=await At(transfer.Pull,"handoff_info",payload);
        await At(!transfer.Pull,"can_stage",info);
        var snapshot=await At(transfer.Pull,"prepare_handoff",payload);
        await At(!transfer.Pull,"stage_handoff",new { snapshot });
        if(!transfer.WorkspaceSynced) {
        await _syncLock.WaitAsync();
        try {
            var conflicts=await _workspace.SynchronizeAsync(link,transfer.SessionId,true,transfer.Pull);
            if(conflicts>0)throw new IOException($"{conflicts} workspace conflicts. Resolve the copies in .gnomeai-conflicts, then resume transfer.");
        } finally {_syncLock.Release();}
        transfer=transfer with {WorkspaceSynced=true};
        var checkpoint=TransferFile+".tmp";
        using(var file=new FileStream(checkpoint,FileMode.Create,FileAccess.Write,FileShare.None)){JsonSerializer.Serialize(file,transfer);file.Flush(true);}
        File.Move(checkpoint,TransferFile,true);
        }
        await At(transfer.Pull,"commit_handoff",payload);
        await At(!transfer.Pull,"activate_handoff",payload);
        EnableSync(link,transfer.SessionId);
        File.Delete(TransferFile);
        Changed?.Invoke();
    }
    private async Task<JsonElement> WorkspaceReplyAsync(string action,JsonElement payload) {
        var data=await _workspace.HandleAsync(action,payload);return JsonSerializer.SerializeToElement(new {ok=true,data});
    }
    private void SaveBindings() {
        var path=Path.Combine(_directory,"sync-bindings.json");
        using(var file=new FileStream(path+".tmp",FileMode.Create,FileAccess.Write,FileShare.None)){JsonSerializer.Serialize(file,_bindings);file.Flush(true);}
        File.Move(path+".tmp",path,true);
    }
    private void EnableSync(PeerLink link,string session) {
        if(!_links.ContainsKey(link.Peer.Channel))return;
        lock(_bindings) {if(!_bindings.Contains(new(link.Peer.Id,session)))_bindings.Add(new(link.Peer.Id,session));SaveBindings();}WakeSync();
    }
    public async Task<string> LocalWorkspaceAsync(string session) {var data=Unwrap(await CoreRawAsync(Identity.Id,"workspace_location",new {session_id=session}));return data.GetProperty("root").GetString()!;}
    public async Task SyncWorkspaceAsync(PeerLink link,string session) {
        await _syncLock.WaitAsync();
        try {
            var count=await _workspace.SynchronizeAsync(link,session,false);
            SyncStatus=count==0?"Workspace synchronized":$"{count} conflicts preserved in .gnomeai-conflicts";
            EnableSync(link,session);Changed?.Invoke();
        } finally {_syncLock.Release();}
    }
    public void DisableSync(PeerLink link,string session) {lock(_bindings){_bindings.RemoveAll(b=>b.Peer==link.Peer.Id && b.Session==session);SaveBindings();}Changed?.Invoke();}
    private async Task AutoSyncAsync() {
        try {while(!_stop.IsCancellationRequested) {
            bool enabled;lock(_bindings)enabled=_bindings.Count>0;
            if(!enabled){await _syncWake.WaitAsync(_stop.Token);continue;}
            await _syncWake.WaitAsync(TimeSpan.FromSeconds(60),_stop.Token);
            if(HasPendingTransfer)continue;
            SyncBinding[] bindings;lock(_bindings)bindings=_bindings.ToArray();
            foreach(var binding in bindings) {
                var link=Links.ToArray().FirstOrDefault(l=>l.Peer.Id==binding.Peer && l.Online);
                if(link is null || !await _syncLock.WaitAsync(0,_stop.Token))continue;
                try {
                    var count=await _workspace.SynchronizeAsync(link,binding.Session,false);
                    SyncStatus=count==0?"Workspace synchronized":$"{count} workspace conflicts";
                }catch(Exception error){SyncStatus=error.Message;}finally{_syncLock.Release();}
            }
        }}catch(OperationCanceledException) when(_stop.IsCancellationRequested){}
    }
    public async ValueTask DisposeAsync()
    {
        _disposed=true;_stop.Cancel(); Bridge.EventReceived-=OnCoreEvent;
        foreach (var link in Links.ToArray()) await link.DisposeAsync();
        foreach (var request in _requests.Values) request.TrySetCanceled();
        await _autoSync;await _transport.DisposeAsync();
        _transferLock.Dispose();
    }
}
