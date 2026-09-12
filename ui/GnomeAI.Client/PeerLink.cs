using System.Collections.Concurrent;
using System.Net.WebSockets;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Threading.Channels;

namespace GnomeAI.Client;

public enum PeerConnectionState { Connecting, Reconnecting, Connected, Offline }

/// ECDH + QR-authenticated pairing; AES-GCM data is bound to both fresh
/// connection challenges and an increasing counter, rejecting recorded traffic.
public sealed class PeerLink : IAsyncDisposable
{
    private readonly DeviceIdentity identity;
    private readonly PeerRecord peer;
    private readonly PeerTransport transport;
    internal PeerLink(DeviceIdentity identity,PeerRecord peer,PeerTransport transport) {this.identity=identity;this.peer=peer;this.transport=transport;transport.Register(peer,Reconnect);}
    public PeerRecord Peer => peer;
    public bool Online { get; private set; }
    public PeerConnectionState ConnectionState {get;private set;}=PeerConnectionState.Connecting;
    public string ConnectionLabel=>Expired?"Invitation expired":AwaitingConfirmation?
        (peer.LocalConfirmed?"Awaiting peer confirmation":"Confirm matching digits"):
        ConnectionState switch {
            PeerConnectionState.Connected=>"Connected",
            PeerConnectionState.Connecting=>"Connecting…",
            PeerConnectionState.Reconnecting=>"Reconnecting…",
            _=>"Offline · automatic retry"
        };
    private void SetConnectionState(PeerConnectionState state) {
        if(ConnectionState==state)return;
        ConnectionState=state;Changed?.Invoke();
    }
    private bool Expired=>!peer.Trusted && peer.Expires<DateTimeOffset.UtcNow.ToUnixTimeSeconds();
    public bool AwaitingConfirmation=>!Expired && peer.Version==2 && !peer.Trusted && peer.Key.Length>0 && _candidateSeen;
    private bool _candidateSeen;
    public string SecurityCode {get {if(!AwaitingConfirmation)return "";var bytes=Convert.FromBase64String(Proof("sas",peer.Channel));return (System.Buffers.Binary.BinaryPrimitives.ReadUInt32BigEndian(bytes)%1000000).ToString("D6");}}
    public async Task ConfirmAsync() {if(!AwaitingConfirmation)throw new IOException("Wait for the pairing request.");identity.Update(()=>peer.LocalConfirmed=true);await SendConfirmationAsync();FinishPairing();if(peer.Trusted)await SendHelloAsync("");}
    private Task SendConfirmationAsync()=>SendPlainAsync(new {type="confirm",id=identity.Id,proof=Proof("confirm",identity.Id,peer.Id,peer.Channel)});
    private void FinishPairing() {if(!peer.Trusted && peer.LocalConfirmed && peer.RemoteConfirmed) {identity.Update(()=>{peer.Trusted=true;peer.PairSecret="";peer.EphemeralPrivate="";});_lastHello=Environment.TickCount64;lock(_connectionGate)_attempt?.CancelAfter(Timeout.InfiniteTimeSpan);Changed?.Invoke();}}
    public event Action? Changed;
    public event Action? PairingRevoked;
    private int _forgetting, _disposed;
    public void StopReconnect()=>Interlocked.Exchange(ref _forgetting,1);
    public event Action<JsonElement>? Event;
    public Func<string, string, JsonElement, Task<JsonElement>>? RequestReceived { get; set; }
    private readonly CancellationTokenSource _stop = new();
    private readonly SemaphoreSlim _send = new(1,1);
    private readonly ConcurrentDictionary<string, TaskCompletionSource<JsonElement>> _pending = new();
    private IPeerConnection? _socket;
    private readonly object _connectionGate=new();
    private CancellationTokenSource? _attempt;
    private readonly SemaphoreSlim _retrySignal=new(0,1);
    public void Reconnect() {
        if(Volatile.Read(ref _forgetting)!=0 || Volatile.Read(ref _disposed)!=0)return;
        lock(_connectionGate){_attempt?.Cancel();_socket?.Abort();}
        try{_retrySignal.Release();}catch(SemaphoreFullException){}
    }
    private Task? _runner;
    private Task? _eventSender;
    private readonly Channel<JsonElement> _events=Channel.CreateBounded<JsonElement>(new BoundedChannelOptions(128)
        { SingleReader=true,FullMode=BoundedChannelFullMode.DropOldest });
    private string _challenge = "", _remoteChallenge = "";
    private long _sent, _received, _lastHello;
    private const int MaxFrame = 24 * 1024 * 1024;
    public void Start() { _eventSender ??= Task.Run(SendEventsAsync);_runner ??= Task.Run(RunAsync); }
    private void SetOnline(bool online)
    {
        var changed=Online!=online;
        Online=online;
        var next=online?PeerConnectionState.Connected:PeerConnectionState.Offline;
        if(ConnectionState!=next)SetConnectionState(next);
        else if(changed)Changed?.Invoke();
    }
    private string Proof(params string[] parts) => Convert.ToBase64String(HMACSHA256.HashData(
        Convert.FromBase64String(peer.Key), Encoding.UTF8.GetBytes(string.Join('|',parts))));
    private static bool Equal(string a, string b)
    {
        try { return CryptographicOperations.FixedTimeEquals(Convert.FromBase64String(a),Convert.FromBase64String(b)); }
        catch (FormatException) { return false; }
    }
    private async Task RunAsync()
    {
        var backoff = 1;var attempts=0;
        while (!_stop.IsCancellationRequested && Volatile.Read(ref _forgetting)==0)
        {
            if (Expired) {try{await transport.RevokeAsync(peer);}catch{} Changed?.Invoke(); return; }
            SetConnectionState(attempts++==0?PeerConnectionState.Connecting:PeerConnectionState.Reconnecting);
            IPeerConnection? socket=null;
            using var attempt = CancellationTokenSource.CreateLinkedTokenSource(_stop.Token);
            lock(_connectionGate)_attempt=attempt;
            try
            {
                if(!peer.Trusted)attempt.CancelAfter(TimeSpan.FromSeconds(Math.Max(1,peer.Expires-DateTimeOffset.UtcNow.ToUnixTimeSeconds())));
                socket=await transport.ConnectAsync(peer,attempt.Token);
                lock(_connectionGate){attempt.Token.ThrowIfCancellationRequested();_socket = socket;}
                _challenge = DeviceIdentity.Random(); _remoteChallenge = "";
                _sent = _received = 0; _lastHello = Environment.TickCount64;
                var heartbeat = HeartbeatAsync(attempt);
                try
                {
                    while (!attempt.IsCancellationRequested)
                    {
                        var bytes = await socket.ReceiveAsync(attempt.Token);
                        using var document = JsonDocument.Parse(bytes);
                        await ProcessAsync(document.RootElement,attempt.Token);
                        if(Online)backoff=1;
                    }
                }
                finally { attempt.Cancel(); try { await heartbeat; } catch (OperationCanceledException) { } }
            }
            catch (OperationCanceledException) when (_stop.IsCancellationRequested) { break; }
            catch (Exception) when (!_stop.IsCancellationRequested)
            { /* Reconnect without logging content, pairing secrets or keys. */ }
            finally
            {
                lock(_connectionGate){_attempt=null;_socket = null;} socket?.Dispose(); SetOnline(false);
                foreach (var item in _pending) item.Value.TrySetException(new IOException("Peer disconnected. Check the session before retrying a message."));
                _pending.Clear();
            }
            try { await _retrySignal.WaitAsync(TimeSpan.FromMilliseconds(backoff*1000+RandomNumberGenerator.GetInt32(500)),_stop.Token); } catch (OperationCanceledException) { break; }
            backoff = Math.Min(120,backoff*2);
        }
    }
    private async Task HeartbeatAsync(CancellationTokenSource attempt)
    {
        var cancel=attempt.Token;
        while (!cancel.IsCancellationRequested)
        {
            try
            {
                if(Expired){_socket?.Abort();Changed?.Invoke();return;}
                if (!peer.Trusted && !peer.Host)
                    await SendPlainAsync(new { type="pair", id=identity.Id, name=identity.Name, key=identity.PublicKey, ephemeral=peer.EphemeralPublic,
                        proof=Proof("pair",peer.Channel,identity.Id,identity.PublicKey,peer.Id) });
                else if (peer.Trusted)
                    await SendHelloAsync("");
                if(!peer.Trusted && peer.LocalConfirmed && peer.Key.Length>0) await SendConfirmationAsync();
                if (Environment.TickCount64-_lastHello > 95000) {
                    // Pending invitations wait for human confirmation, but their
                    // authenticated hello transport still has a finite deadline.
                    SetOnline(false);attempt.Cancel();_socket?.Abort();return;
                }
            }
            catch (Exception) { attempt.Cancel();_socket?.Abort();return; }
            await Task.Delay(peer.Trusted && Online?30000:5000,cancel);
        }
    }
    private Task SendHelloAsync(string echo) => SendPlainAsync(new { type="hello", id=identity.Id,
        challenge=_challenge, echo, proof=Proof("hello",identity.Id,peer.Id,_challenge,echo) });
    private async Task ProcessAsync(JsonElement frame,CancellationToken cancel)
    {
        if(Expired)throw new IOException("Pairing invitation expired.");
        var type = frame.GetProperty("type").GetString();
        if (type == "pair" && peer.Host)
        {
            var id=frame.GetProperty("id").GetString()!;
            var publicKey=frame.GetProperty("key").GetString()!;
            if (!Guid.TryParse(id,out _) || id==identity.Id) return;
            if (peer.Id.Length>0 && (id != peer.Id || publicKey != peer.PublicKey)) return;
            if (peer.Key.Length==0)
            {
                if (peer.Expires < DateTimeOffset.UtcNow.ToUnixTimeSeconds()) return;
                var candidate = new PeerRecord { Id=id, PublicKey=publicKey, Channel=peer.Channel,Version=peer.Version,EphemeralPrivate=peer.EphemeralPrivate };
                var key = identity.Derive(candidate,peer.PairSecret,frame.GetProperty("ephemeral").GetString()!);
                var proof = Convert.ToBase64String(HMACSHA256.HashData(key,Encoding.UTF8.GetBytes($"pair|{peer.Channel}|{id}|{publicKey}|{identity.Id}")));
                if (!Equal(proof,frame.GetProperty("proof").GetString()!)) { CryptographicOperations.ZeroMemory(key); return; }
                identity.Update(()=> {
                    peer.Id=id; peer.PublicKey=publicKey; peer.Key=Convert.ToBase64String(key);
                    peer.Name=frame.GetProperty("name").GetString() ?? "Paired device";
                    peer.PairSecret=""; // Consume the invitation at the first authenticated candidate.
                    _candidateSeen=true;
                });
                CryptographicOperations.ZeroMemory(key);
                Changed?.Invoke();
            }
            if (!Equal(Proof("pair",peer.Channel,id,publicKey,identity.Id),frame.GetProperty("proof").GetString()!)) return;
            _candidateSeen=true;if(!peer.Trusted)_lastHello=Environment.TickCount64;
            await SendPlainAsync(new { type="paired", id=identity.Id, proof=Proof("paired",peer.Channel,identity.Id,peer.Id) });
            if(peer.LocalConfirmed)await SendConfirmationAsync();
            if(peer.Trusted)await SendHelloAsync("");
            Changed?.Invoke();
        }
        else if (type == "paired" && !peer.Host)
        {
            if (frame.GetProperty("id").GetString()!=peer.Id || !Equal(Proof("paired",peer.Channel,peer.Id,identity.Id),frame.GetProperty("proof").GetString()!)) return;
            _candidateSeen=true;if(!peer.Trusted)_lastHello=Environment.TickCount64;Changed?.Invoke();
            if(peer.LocalConfirmed)await SendConfirmationAsync();
            if(peer.Trusted)await SendHelloAsync("");
        }
        else if(type=="confirm" && peer.Key.Length>0) {
            if(frame.GetProperty("id").GetString()!=peer.Id || !Equal(Proof("confirm",peer.Id,identity.Id,peer.Channel),frame.GetProperty("proof").GetString()!))return;
            if(!peer.Trusted)_lastHello=Environment.TickCount64;
            if(peer.Trusted && peer.RemoteConfirmed)return;
            var firstConfirm=!peer.RemoteConfirmed;
            _candidateSeen=true;identity.Update(()=>peer.RemoteConfirmed=true);FinishPairing();
            if(peer.Trusted) {if(firstConfirm)await SendConfirmationAsync();await SendHelloAsync("");}
            Changed?.Invoke();
        }
        else if (type == "hello" && peer.Trusted)
        {
            var id=frame.GetProperty("id").GetString()!;
            var challenge=frame.GetProperty("challenge").GetString()!;
            var echo=frame.GetProperty("echo").GetString()!;
            if (id!=peer.Id || DeviceIdentity.Decode(challenge).Length!=32 || !Equal(Proof("hello",id,identity.Id,challenge,echo),frame.GetProperty("proof").GetString()!)) return;
            if (_remoteChallenge != challenge) { _remoteChallenge=challenge; _received=0; SetOnline(false); }
            if (echo==_challenge) {_lastHello=Environment.TickCount64;SetOnline(true);}
            if (echo.Length==0) await SendHelloAsync(challenge);
        }
        else if (type == "data" && peer.Trusted && _remoteChallenge.Length != 0)
        {
            var epoch=frame.GetProperty("epoch").GetString()!;
            var target=frame.GetProperty("target").GetString()!;
            var sequence=frame.GetProperty("seq").GetInt64();
            if (epoch!=_remoteChallenge || target!=_challenge || sequence<=_received) return;
            var ciphertext=Convert.FromBase64String(frame.GetProperty("body").GetString()!);
            var clear=new byte[ciphertext.Length];
            using (var aes=new AesGcm(Convert.FromBase64String(peer.Key),16))
                aes.Decrypt(Convert.FromBase64String(frame.GetProperty("nonce").GetString()!),ciphertext,
                    Convert.FromBase64String(frame.GetProperty("tag").GetString()!),clear,
                    Encoding.UTF8.GetBytes($"1|{peer.Id}|{identity.Id}|{epoch}|{target}|{sequence}"));
            _received=sequence;
            _lastHello=Environment.TickCount64;
            SetOnline(true);
            using var message=JsonDocument.Parse(clear);
            var node=message.RootElement;
            var kind=node.GetProperty("kind").GetString();
            if(Volatile.Read(ref _forgetting)!=0 && kind is not ("reply" or "revoke_pairing"))return;
            if (kind=="reply")
            {
                if (_pending.TryRemove(node.GetProperty("id").GetString()!,out var pending)) pending.TrySetResult(node.GetProperty("result").Clone());
            }
            else if(kind=="revoke_pairing") {
                await SendDataAsync(new {kind="reply",id=node.GetProperty("id").GetString(),result=new {ok=true}});
                StopReconnect();PairingRevoked?.Invoke();
            }
            else if (kind=="event") Event?.Invoke(node.GetProperty("payload").Clone());
            else if (kind=="request" && RequestReceived is { } handler)
            {
                var id=node.GetProperty("id").GetString()!;
                JsonElement result;
                try { result=await handler(id,node.GetProperty("action").GetString()!,node.GetProperty("payload").Clone()).WaitAsync(cancel); }
                catch (OperationCanceledException) when(cancel.IsCancellationRequested){throw;}
                catch (Exception error) { result=JsonSerializer.SerializeToElement(new { ok=false,error=error.Message }); }
                await SendDataAsync(new { kind="reply",id,result });
            }
        }
    }
    private async Task SendPlainAsync(object message)
    {
        var bytes=JsonSerializer.SerializeToUtf8Bytes(message);
        await _send.WaitAsync(_stop.Token);
        try { await SendBytesAsync(bytes); } finally { _send.Release(); }
    }
    private async Task SendBytesAsync(byte[] bytes)
    {
        if (bytes.Length>MaxFrame) throw new IOException("Device message exceeds 24 MiB.");
        var socket=_socket ?? throw new IOException("Relay is disconnected.");
        using var timeout=CancellationTokenSource.CreateLinkedTokenSource(_stop.Token);
        timeout.CancelAfter(TimeSpan.FromSeconds(20));
        try { await socket.SendAsync(bytes,timeout.Token); }
        catch { socket.Abort();throw; }
    }
    private async Task SendDataAsync(object message)
    {
        await _send.WaitAsync(_stop.Token);
        try
        {
            if (!Online) throw new IOException("The paired device is offline.");
            var clear=JsonSerializer.SerializeToUtf8Bytes(message);
            var ciphertext=new byte[clear.Length]; var nonce=RandomNumberGenerator.GetBytes(12); var tag=new byte[16];
            var sequence=++_sent;
            using (var aes=new AesGcm(Convert.FromBase64String(peer.Key),16))
                aes.Encrypt(nonce,clear,ciphertext,tag,Encoding.UTF8.GetBytes($"1|{identity.Id}|{peer.Id}|{_challenge}|{_remoteChallenge}|{sequence}"));
            await SendBytesAsync(JsonSerializer.SerializeToUtf8Bytes(new { type="data",epoch=_challenge,target=_remoteChallenge,seq=sequence,
                nonce=Convert.ToBase64String(nonce),tag=Convert.ToBase64String(tag),body=Convert.ToBase64String(ciphertext) }));
        }
        finally { _send.Release(); }
    }
    // Keep the newest event when a slow connection fills the queue. Its revision
    // exposes the gap and the receiving view requests a consistent snapshot.
    public Task NotifyAsync(JsonElement payload)
    {
        if(Online) _events.Writer.TryWrite(payload.Clone());
        return Task.CompletedTask;
    }
    private async Task SendEventsAsync()
    {
        try
        {
            await foreach(var payload in _events.Reader.ReadAllAsync(_stop.Token))
            {
                try { await SendDataAsync(new {kind="event",payload}); }
                catch(Exception) when(!_stop.IsCancellationRequested) { /* Reconnection triggers a snapshot. */ }
            }
        }
        catch(OperationCanceledException) when(_stop.IsCancellationRequested) { }
    }
    public async Task<JsonElement> RequestAsync(string action, object payload, string? requestId=null)
    {
        var id=requestId ?? Guid.NewGuid().ToString();
        var pending=new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        if (!_pending.TryAdd(id,pending)) throw new InvalidOperationException("This request is already in flight.");
        try
        {
            await SendDataAsync(new { kind="request",id,action,payload });
            return await pending.Task.WaitAsync(TimeSpan.FromSeconds(120),_stop.Token);
        }
        finally { _pending.TryRemove(id,out _); }
    }
    public async Task RevokePairingAsync()
    {
        var id=Guid.NewGuid().ToString();
        var pending=new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        _pending[id]=pending;
        try {
            await SendDataAsync(new {kind="revoke_pairing",id}).WaitAsync(TimeSpan.FromSeconds(5));
            await pending.Task.WaitAsync(TimeSpan.FromSeconds(5),_stop.Token);
        } finally {_pending.TryRemove(id,out _);}
    }
    public async ValueTask DisposeAsync()
    {
        if(Interlocked.Exchange(ref _disposed,1)!=0)return;
        StopReconnect();_stop.Cancel();
        lock(_connectionGate){_attempt?.Cancel();_socket?.Abort();}
        _events.Writer.TryComplete();
        if (_runner is not null) { try { await _runner; } catch (OperationCanceledException) { } }
        if (_eventSender is not null) await _eventSender;
        _stop.Dispose(); _send.Dispose();
    }
}
