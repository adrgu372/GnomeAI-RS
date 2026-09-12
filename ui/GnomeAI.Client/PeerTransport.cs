using System.Buffers.Binary;
using System.Collections.Concurrent;
using System.Net;
using System.Net.Sockets;
using System.Net.WebSockets;
using System.Text;
using System.Security.Cryptography;
using System.Text.Json;
using System.Threading.Channels;

namespace GnomeAI.Client;

internal interface IPeerConnection : IDisposable
{
    Task SendAsync(byte[] bytes,CancellationToken cancel);
    Task<byte[]> ReceiveAsync(CancellationToken cancel);
    void Abort();
}
internal sealed class FramedConnection(TcpClient socket) : IPeerConnection
{
    private readonly NetworkStream _stream=socket.GetStream();
    public async Task SendAsync(byte[] bytes,CancellationToken cancel) {
        if(bytes.Length>24*1024*1024) throw new IOException("Peer frame too large.");
        var header=new byte[4];BinaryPrimitives.WriteInt32BigEndian(header,bytes.Length);
        await _stream.WriteAsync(header,cancel);await _stream.WriteAsync(bytes,cancel);
    }
    public Task<byte[]> ReceiveAsync(CancellationToken cancel)=>ReceiveLimitedAsync(24*1024*1024,cancel);
    public async Task<byte[]> ReceiveLimitedAsync(int limit,CancellationToken cancel) {
        var header=new byte[4];await _stream.ReadExactlyAsync(header,cancel);
        var size=BinaryPrimitives.ReadInt32BigEndian(header);
        if(size<1 || size>limit) throw new IOException("Invalid peer frame length.");
        var bytes=new byte[size];await _stream.ReadExactlyAsync(bytes,cancel);return bytes;
    }
    public void Abort()=>socket.Dispose();
    public void Dispose()=>socket.Dispose();
}
internal sealed class RelayConnection(ClientWebSocket socket) : IPeerConnection
{
    public async Task SendAsync(byte[] bytes,CancellationToken cancel)=>await socket.SendAsync(bytes.AsMemory(),WebSocketMessageType.Text,true,cancel);
    public async Task<byte[]> ReceiveAsync(CancellationToken cancel) {
        using var output=new MemoryStream();var buffer=new byte[16384];
        while(true) {
            var result=await socket.ReceiveAsync(buffer.AsMemory(),cancel);
            if(result.MessageType!=WebSocketMessageType.Text) throw new IOException("Relay closed.");
            if(output.Length+result.Count>24*1024*1024) throw new IOException("Peer frame too large.");
            output.Write(buffer,0,result.Count);if(result.EndOfMessage)return output.ToArray();
        }
    }
    public void Abort()=>socket.Abort();
    public void Dispose()=>socket.Dispose();
}

/// Arti lives in the shared Rust runtime. Loopback sockets bridge its streams to
/// the managed protocol; onion hostnames are sent to Arti, never system DNS.
internal sealed class PeerTransport : IAsyncDisposable
{
    private readonly Func<string,object,Task<JsonElement>> _core;
    private readonly TcpListener _listener=new(IPAddress.Loopback,0);
    private readonly CancellationTokenSource _stop=new();
    private readonly ConcurrentDictionary<string,Channel<IPeerConnection>> _incoming=new();
    private sealed record Registration(PeerRecord Peer,Action Replace);
    private readonly ConcurrentDictionary<string,Registration> _registrations=new();
    private readonly Task _accept;
    private readonly SemaphoreSlim _routes=new(16,16);
    private int Port=>((IPEndPoint)_listener.LocalEndpoint).Port;
    public string Status {get;private set;}="Tor stopped";
    public PeerTransport(Func<string,object,Task<JsonElement>> core) {
        _core=core;_listener.Start(16);_accept=AcceptAsync();
    }
    public void Register(PeerRecord peer,Action replace) {
        _registrations[peer.Channel]=new(peer,replace);
        if(peer.Host && peer.Relay.StartsWith("tor://",StringComparison.Ordinal))
            _incoming.TryAdd(peer.Channel,Channel.CreateBounded<IPeerConnection>(1));
    }
    private async Task<JsonElement> ReadyAsync(string channel,CancellationToken cancel) {
        await _core("mesh_start",new {channel,port=Port}).WaitAsync(cancel);
        var until=DateTime.UtcNow.AddMinutes(4);
        while(DateTime.UtcNow<until) {
            cancel.ThrowIfCancellationRequested();
            var state=await _core("mesh_status",new {}).WaitAsync(cancel);
            Status=state.GetProperty("status").GetString()??"Tor";
            if(Status.StartsWith("Tor failed:",StringComparison.Ordinal)) throw new IOException(Status);
            if(state.GetProperty("socks_port").GetInt32()>0 && (channel.Length==0 || state.GetProperty("onions").TryGetProperty(channel,out _)))return state;
            await Task.Delay(1000,cancel);
        }
        throw new TimeoutException("Tor is still connecting. Check connectivity and try again.");
    }
    public async Task<string> PublishAsync(string channel,CancellationToken cancel) {
        var state=await ReadyAsync(channel,cancel);
        return "tor://"+state.GetProperty("onions").GetProperty(channel).GetString();
    }
    public async Task<IPeerConnection> ConnectAsync(PeerRecord peer,CancellationToken cancel) {
        using var dial=CancellationTokenSource.CreateLinkedTokenSource(cancel);
        if(!peer.Host || !peer.Relay.StartsWith("tor://",StringComparison.Ordinal))dial.CancelAfter(TimeSpan.FromSeconds(150));
        cancel=dial.Token;
        if(!peer.Relay.StartsWith("tor://",StringComparison.Ordinal)) {
            var socket=new ClientWebSocket();
            try {await socket.ConnectAsync(new Uri($"{peer.Relay}/ws/{peer.Channel}"),cancel);return new RelayConnection(socket);}
            catch {socket.Dispose();throw;}
        }
        var state=await ReadyAsync(peer.Host?peer.Channel:"",cancel);
        if(peer.Host) {
            if(!_incoming.TryGetValue(peer.Channel,out var queue))throw new IOException("Invitation was revoked.");
            return await queue.Reader.ReadAsync(cancel);
        }
        DeviceIdentity.ValidateRelay(peer.Relay);
        var socketClient=new TcpClient();
        try {
            await socketClient.ConnectAsync(IPAddress.Loopback,state.GetProperty("socks_port").GetInt32(),cancel);
            var stream=socketClient.GetStream();await stream.WriteAsync(new byte[]{5,1,0},cancel);
            var response=new byte[2];await stream.ReadExactlyAsync(response,cancel);
            if(!response.SequenceEqual(new byte[]{5,0}))throw new IOException("Tor proxy negotiation failed.");
            var host=Encoding.ASCII.GetBytes(new Uri(peer.Relay).Host);
            var request=new byte[7+host.Length];request[0]=5;request[1]=1;request[3]=3;request[4]=(byte)host.Length;
            host.CopyTo(request,5);request[^1]=80;await stream.WriteAsync(request,cancel);
            var connected=new byte[10];await stream.ReadExactlyAsync(connected,cancel);
            if(connected[0]!=5 || connected[1]!=0)throw new IOException("Onion device is unavailable.");
            var connection=new FramedConnection(socketClient);
            await connection.SendAsync(JsonSerializer.SerializeToUtf8Bytes(new {channel=peer.Channel,transport=3}),cancel);
            using var handshake=CancellationTokenSource.CreateLinkedTokenSource(cancel);
            handshake.CancelAfter(TimeSpan.FromSeconds(20));
            using var welcome=JsonDocument.Parse(await connection.ReceiveLimitedAsync(1024,handshake.Token));
            var nonce=welcome.RootElement.GetProperty("nonce").GetString()!;
            if(nonce.Length>0) {
                if(DeviceIdentity.Decode(nonce).Length!=32 || peer.Key.Length==0)throw new IOException("Invalid transport challenge.");
                await connection.SendAsync(JsonSerializer.SerializeToUtf8Bytes(new {proof=TransportProof(peer,nonce)}),handshake.Token);
            }
            return connection;
        } catch {socketClient.Dispose();throw;}
    }
    private static string TransportProof(PeerRecord peer,string nonce)=>Convert.ToBase64String(HMACSHA256.HashData(
        Convert.FromBase64String(peer.Key),Encoding.UTF8.GetBytes($"GnomeAI transport v3|{peer.Channel}|{nonce}")));
    private async Task AcceptAsync() {
        try {while(!_stop.IsCancellationRequested) {
            var socket=await _listener.AcceptTcpClientAsync(_stop.Token);
            if(!_routes.Wait(0)){socket.Dispose();continue;}
            _=RouteAsync(socket);
        }}catch(Exception) when(_stop.IsCancellationRequested){}
    }
    private async Task RouteAsync(TcpClient socket) {
        FramedConnection? connection=new FramedConnection(socket);
        using var timeout=CancellationTokenSource.CreateLinkedTokenSource(_stop.Token);timeout.CancelAfter(TimeSpan.FromSeconds(20));
        try {
            using var doc=JsonDocument.Parse(await connection.ReceiveLimitedAsync(128,timeout.Token));
            var channel=doc.RootElement.GetProperty("channel").GetString()!;
            if(!_incoming.TryGetValue(channel,out var queue) || !_registrations.TryGetValue(channel,out var registration))return;
            var authenticated=false;
            if(doc.RootElement.TryGetProperty("transport",out var version) && version.GetInt32()==3) {
                var peer=registration.Peer;
                var nonce=peer.Key.Length>0?DeviceIdentity.Random():"";
                await connection.SendAsync(JsonSerializer.SerializeToUtf8Bytes(new {nonce}),timeout.Token);
                if(nonce.Length>0) {
                    using var proof=JsonDocument.Parse(await connection.ReceiveLimitedAsync(1024,timeout.Token));
                    var received=Convert.FromBase64String(proof.RootElement.GetProperty("proof").GetString()!);
                    if(!CryptographicOperations.FixedTimeEquals(received,Convert.FromBase64String(TransportProof(peer,nonce))))return;
                    authenticated=true;
                }
            }
            // Only proof of the pinned pairing key on a fresh challenge may
            // evict an established stream. A channel name or a replay cannot.
            lock(queue) {
                if(authenticated) {
                    registration.Replace(); // Cancel OLD attempt before publishing its successor.
                    while(queue.Reader.TryRead(out var stale))stale.Dispose();
                }
                if(queue.Writer.TryWrite(connection))connection=null;
            }
        }catch(Exception){}finally{connection?.Dispose();_routes.Release();}
    }
    public async Task RevokeAsync(PeerRecord peer) {
        _registrations.TryRemove(peer.Channel,out _);
        if(_incoming.TryRemove(peer.Channel,out var queue)) {queue.Writer.TryComplete();while(queue.Reader.TryRead(out var connection))connection.Dispose();}
        if((peer.Relay=="tor" || peer.Relay.StartsWith("tor://",StringComparison.Ordinal)) && peer.Host)await _core("mesh_revoke",new {channel=peer.Channel});
    }
    public async ValueTask DisposeAsync() {
        _stop.Cancel();_listener.Stop();await _accept;
        foreach(var queue in _incoming.Values) {queue.Writer.TryComplete();while(queue.Reader.TryRead(out var connection))connection.Dispose();}
        _stop.Dispose();
    }
}
