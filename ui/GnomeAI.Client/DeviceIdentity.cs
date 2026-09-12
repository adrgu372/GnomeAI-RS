using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace GnomeAI.Client;

public sealed class PeerRecord
{
    public string Relay { get; set; } = "";
    public string Channel { get; set; } = "";
    public string Id { get; set; } = "";
    public string Name { get; set; } = "Paired device";
    public string PublicKey { get; set; } = "";
    public string Key { get; set; } = "";
    public string PairSecret { get; set; } = "";
    public long Expires { get; set; }
    public bool Host { get; set; }
    public bool Trusted { get; set; }
    public int Version { get; set; }=1;
    public string EphemeralPrivate {get;set;}="";
    public string EphemeralPublic {get;set;}="";
    public bool LocalConfirmed {get;set;}
    public bool RemoteConfirmed {get;set;}
}
public sealed record PairInvitation(int Version, string Relay, string Channel, string Id,
    string Name, string PublicKey, string Secret, long Expires,string EphemeralPublic="");

public interface IIdentityProtector { string Protect(string clear); string Unprotect(string saved); }

public sealed class DeviceIdentity
{
    public string Id { get; set; } = Guid.NewGuid().ToString();
    public string Name { get; set; } = Environment.MachineName;
    public string PrivateKey { get; set; } = "";
    public List<PeerRecord> Peers { get; set; } = [];
    private string _path = "";
    private IIdentityProtector? _protector;
    private readonly object _gate = new();
    public static DeviceIdentity Open(string directory, string name,IIdentityProtector? protector=null)
    {
        Directory.CreateDirectory(directory);
        var path = Path.Combine(directory, "devices.json");
        DeviceIdentity identity;
        if (File.Exists(path)) identity = JsonSerializer.Deserialize<DeviceIdentity>(protector?.Unprotect(File.ReadAllText(path))??File.ReadAllText(path))
            ?? throw new IOException("Device identity is invalid; restore its backup before pairing again.");
        else
        {
            using var key = ECDiffieHellman.Create(ECCurve.NamedCurves.nistP256);
            identity = new() { Name = name, PrivateKey = Convert.ToBase64String(key.ExportPkcs8PrivateKey()) };
        }
        identity._path = path;identity._protector=protector;
        identity.Save();
        return identity;
    }
    public ECDiffieHellman OpenKey()
    {
        var key = ECDiffieHellman.Create();
        key.ImportPkcs8PrivateKey(Convert.FromBase64String(PrivateKey), out _);
        return key;
    }
    public string PublicKey { get { using var key = OpenKey(); return Convert.ToBase64String(key.ExportSubjectPublicKeyInfo()); } }
    public void Save()
    {
        lock (_gate)
        {
            var options = new FileStreamOptions { Mode = FileMode.Create, Access = FileAccess.Write };
            if (!OperatingSystem.IsWindows()) options.UnixCreateMode = UnixFileMode.UserRead | UnixFileMode.UserWrite;
            using (var file = new FileStream(_path + ".tmp", options))
            {
                var clear=JsonSerializer.Serialize(this);
                var bytes=Encoding.UTF8.GetBytes(_protector?.Protect(clear)??clear);file.Write(bytes);
                file.Flush(true);
            }
            File.Move(_path + ".tmp", _path, true);
        }
    }
    public void Update(Action change) { lock(_gate) { change();Save(); } }
    public static string Encode(byte[] bytes) => Convert.ToBase64String(bytes).TrimEnd('=').Replace('+', '-').Replace('/', '_');
    public static byte[] Decode(string value) => Convert.FromBase64String(value.Replace('-', '+').Replace('_', '/') + new string('=', (4-value.Length%4)%4));
    public static string Random() => Encode(RandomNumberGenerator.GetBytes(32));
    public static void ValidateRelay(string relay)
    {
        if(relay=="tor") return; // Internal draft; never emitted as a pairing endpoint.
        var uri = new Uri(relay);
        if(uri.Scheme=="tor" && uri.Host.Length==62 && uri.Host.EndsWith(".onion",StringComparison.Ordinal) && uri.Host[..56].All(c=>c is >='a' and <='z' or >='2' and <='7') && (uri.AbsolutePath=="/" || uri.AbsolutePath.Length==0) && uri.Port==-1 && uri.UserInfo.Length==0 && uri.Query.Length==0 && uri.Fragment.Length==0) return;
        if (uri.Scheme != "wss" || !string.IsNullOrEmpty(uri.UserInfo) || !string.IsNullOrEmpty(uri.Query) || !string.IsNullOrEmpty(uri.Fragment))
            throw new ArgumentException("Use a Tor onion endpoint or a wss:// relay without credentials or query parameters.");
    }
    public (PeerRecord Peer, string Code) CreateInvitation(string relay)
    {
        ValidateRelay(relay);
        var peer = new PeerRecord { Relay=relay.TrimEnd('/'), Channel=Random(), PairSecret=Random(),
            Host=true, Version=2, Expires=DateTimeOffset.UtcNow.AddMinutes(10).ToUnixTimeSeconds() };
        NewEphemeral(peer);
        Update(()=>Peers.Add(peer));
        return (peer,InvitationCode(peer));
    }
    public string InvitationCode(PeerRecord peer) {
        var invitation=new PairInvitation(peer.Version,peer.Relay,peer.Channel,Id,Name,PublicKey,peer.PairSecret,peer.Expires,peer.EphemeralPublic);
        return "gnomeai://pair/"+Encode(JsonSerializer.SerializeToUtf8Bytes(invitation));
    }
    private static void NewEphemeral(PeerRecord peer) {using var key=ECDiffieHellman.Create(ECCurve.NamedCurves.nistP256);peer.EphemeralPrivate=Convert.ToBase64String(key.ExportPkcs8PrivateKey());peer.EphemeralPublic=Convert.ToBase64String(key.ExportSubjectPublicKeyInfo());}
    public static PairInvitation ReadInvitation(string code)
    {
        const string prefix="gnomeai://pair/";
        if(string.IsNullOrWhiteSpace(code) || !code.StartsWith(prefix,StringComparison.Ordinal) || code.Length>8192)throw new ArgumentException("Invalid GnomeAI pairing code.");
        var invite=JsonSerializer.Deserialize<PairInvitation>(Decode(code[prefix.Length..]))??throw new ArgumentException("Invalid invitation.");
        ValidateRelay(invite.Relay);
        var now=DateTimeOffset.UtcNow.ToUnixTimeSeconds();
        if(invite.Version!=2 || invite.Relay=="tor" || !Guid.TryParse(invite.Id,out _) || invite.Expires<=now || invite.Expires>now+900)
            throw new ArgumentException("This GnomeAI invitation is invalid or expired.");
        if(Decode(invite.Secret).Length!=32 || Decode(invite.Channel).Length!=32 || string.IsNullOrWhiteSpace(invite.Name) || invite.Name.Length>256)
            throw new ArgumentException("Invalid invitation fields.");
        foreach(var encoded in new[]{invite.PublicKey,invite.EphemeralPublic}) {
            using var key=ECDiffieHellman.Create();var bytes=Convert.FromBase64String(encoded);
            key.ImportSubjectPublicKeyInfo(bytes,out var read);
            if(read!=bytes.Length || key.ExportParameters(false).Curve.Oid.Value!=ECCurve.NamedCurves.nistP256.Oid.Value)
                throw new ArgumentException("Invalid invitation public key.");
        }
        return invite;
    }
    public PeerRecord AcceptInvitation(string code)
    {
        var invite=ReadInvitation(code);
        if(invite.Id==Id)throw new ArgumentException("This invitation belongs to this device.");
        if (Peers.Any(p => p.Channel == invite.Channel || p.Id == invite.Id)) throw new ArgumentException("This device is already paired or awaiting pairing.");
        var peer = new PeerRecord { Relay=invite.Relay, Channel=invite.Channel, Id=invite.Id, Name=invite.Name,
            PublicKey=invite.PublicKey, Expires=invite.Expires, Version=invite.Version };
        NewEphemeral(peer);
        peer.Key = Convert.ToBase64String(Derive(peer, invite.Secret,invite.EphemeralPublic));
        Update(()=>Peers.Add(peer));
        return peer;
    }
    public byte[] Derive(PeerRecord peer, string secret,string remoteEphemeral="")
    {
        using var local = ECDiffieHellman.Create(); using var remote = ECDiffieHellman.Create();
        local.ImportPkcs8PrivateKey(Convert.FromBase64String(peer.Version==2?peer.EphemeralPrivate:PrivateKey),out _);
        remote.ImportSubjectPublicKeyInfo(Convert.FromBase64String(peer.Version==2?remoteEphemeral:peer.PublicKey), out _);
        var shared = local.DeriveKeyFromHash(remote.PublicKey, HashAlgorithmName.SHA256);
        try
        {
            var ids = new[] { Id, peer.Id }; Array.Sort(ids,StringComparer.Ordinal);
            return HKDF.DeriveKey(HashAlgorithmName.SHA256, shared, 32, Decode(secret),
                Encoding.UTF8.GetBytes($"GnomeAI devices v{peer.Version}|{peer.Channel}|{ids[0]}|{ids[1]}"));
        }
        finally { CryptographicOperations.ZeroMemory(shared); }
    }
}
