using System.Net;
using System.Net.Sockets;
using System.Net.WebSockets;
using System.Security.Cryptography;
using System.Text.Json;
using GnomeAI.Client;

// Real PeerLink encryption and DeviceHub persistence over a local in-memory relay.
// No Tor, native core, external account, or third-party test package is required.
static class Program
{
    static async Task Until(Func<bool> test)
    {
        using var limit=new CancellationTokenSource(TimeSpan.FromSeconds(12));
        while(!test())await Task.Delay(20,limit.Token);
    }
    static void Check(bool test,string message){if(!test)throw new Exception(message);}
    static async Task Pump(WebSocket from,WebSocket to,CancellationToken stop)
    {
        var bytes=new byte[65536];
        try{while(!stop.IsCancellationRequested){var r=await from.ReceiveAsync(bytes.AsMemory(),stop);if(r.MessageType==WebSocketMessageType.Close)return;await to.SendAsync(bytes.AsMemory(0,r.Count),r.MessageType,r.EndOfMessage,stop);}}
        catch(Exception) when(stop.IsCancellationRequested || from.State!=WebSocketState.Open || to.State!=WebSocketState.Open){}
    }
    static async Task Main()
    {
        var directory=Path.Combine(Path.GetTempPath(),"gnomeai-device-test-"+Guid.NewGuid());Directory.CreateDirectory(directory);
        try
        {
            using var stop=new CancellationTokenSource();
            var reserve=new TcpListener(IPAddress.Loopback,0);reserve.Start();var port=((IPEndPoint)reserve.LocalEndpoint).Port;reserve.Stop();
            using var relay=new HttpListener();relay.Prefixes.Add($"http://127.0.0.1:{port}/");relay.Start();
            var relayTask=Task.Run(async()=>{
                var first=await relay.GetContextAsync();using var a=(await first.AcceptWebSocketAsync(null)).WebSocket;
                var second=await relay.GetContextAsync();using var b=(await second.AcceptWebSocketAsync(null)).WebSocket;
                await Task.WhenAll(Pump(a,b,stop.Token),Pump(b,a,stop.Token));
            });
            var aDir=Path.Combine(directory,"a");var bDir=Path.Combine(directory,"b");
            var aIdentity=DeviceIdentity.Open(aDir,"A");var bIdentity=DeviceIdentity.Open(bDir,"B");
            var key=Convert.ToBase64String(RandomNumberGenerator.GetBytes(32));var channel=DeviceIdentity.Random();
            PeerRecord Record(DeviceIdentity other)=>new(){Id=other.Id,Name=other.Name,PublicKey=other.PublicKey,Key=key,Channel=channel,Relay=$"ws://127.0.0.1:{port}",Version=2,Trusted=true,LocalConfirmed=true,RemoteConfirmed=true};
            aIdentity.Peers.Add(Record(bIdentity));aIdentity.Save();bIdentity.Peers.Add(Record(aIdentity));bIdentity.Save();
            await using var aHub=new DeviceHub(new UnusedBridge(),aDir,"A");
            await using var bHub=new DeviceHub(new UnusedBridge(),bDir,"B");
            var aLink=aHub.Links.Single();var bLink=bHub.Links.Single();
            await Until(()=>aLink.Online && bLink.Online);
            var stamp=File.GetLastWriteTimeUtc(Path.Combine(bDir,"devices.json"));
            await Task.Delay(6500);
            Check(File.GetLastWriteTimeUtc(Path.Combine(bDir,"devices.json"))==stamp,"Idle pairing rewrote devices.json");
            Console.WriteLine("PASS: authenticated connection idle does not rewrite identity at the old 5-second interval.");
            var sendConfirm=typeof(PeerLink).GetMethod("SendConfirmationAsync",System.Reflection.BindingFlags.NonPublic|System.Reflection.BindingFlags.Instance)!;
            await (Task)sendConfirm.Invoke(aLink,null)!;await Task.Delay(250);
            Check(File.GetLastWriteTimeUtc(Path.Combine(bDir,"devices.json"))==stamp,"Duplicate confirmation rewrote identity");
            Console.WriteLine("PASS: repeated authenticated confirmation is idempotent.");
            var sendPlain=typeof(PeerLink).GetMethod("SendPlainAsync",System.Reflection.BindingFlags.NonPublic|System.Reflection.BindingFlags.Instance)!;
            await (Task)sendPlain.Invoke(aLink,new object[]{new {type="revoke_pairing",id=Guid.NewGuid().ToString()}})!;await Task.Delay(250);
            Check(aHub.Links.Count==1 && bHub.Links.Count==1,"Unauthenticated revocation was accepted");
            Console.WriteLine("PASS: a plaintext revocation cannot remove pairing.");
            var cache=Path.Combine(aDir,"workspace-sync",$"base-{bIdentity.Id}-test.json");File.WriteAllText(cache,"{}");
            await bHub.ForgetAsync(bLink);await Until(()=>aHub.Links.Count==0 && bHub.Links.Count==0);
            await Until(()=>!File.Exists(cache));
            Check(DeviceIdentity.Open(aDir,"A").Peers.Count==0 && DeviceIdentity.Open(bDir,"B").Peers.Count==0,"Revocation not persisted on both devices");
            Check(bHub.SyncStatus=="Pairing revoked on both devices","Revocation was not acknowledged");
            Console.WriteLine("PASS: authenticated revoke/ack removes both peer stores and associated sync baseline.");
            stop.Cancel();relay.Stop();try{await relayTask;}catch(OperationCanceledException){}
        }
        finally{Directory.Delete(directory,true);}
    }
    sealed class UnusedBridge : IAgentBridge
    {
        public event Func<JsonElement,Task>? EventReceived {add{} remove{}}
        public event Action<string>? Disconnected {add{} remove{}}
        public void Start(){}
        public Task SendAsync(IReadOnlyDictionary<string,object?> operation)=>throw new InvalidOperationException("Native core should not be used in relay lifecycle tests.");
        public ValueTask DisposeAsync()=>ValueTask.CompletedTask;
    }
}
