using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace GnomeAI.Client;

internal sealed record WorkspaceEntry(string Hash,long Size);
internal sealed record WorkspaceManifest(string WorkspaceId,Dictionary<string,WorkspaceEntry> Files);

/// Explicit workspace scope, SHA-256 chunks, compare-before-replace and conflict
/// copies. Arbitrary absolute paths and links never cross the peer API.
internal sealed class WorkspaceSync
{
    private const long MaxFile=64L*1024*1024;
    private const int Chunk=256*1024;
    private readonly string _directory;
    private readonly Func<string,object,Task<JsonElement>> _local;
    private readonly SemaphoreSlim _gate=new(1,1);
    public WorkspaceSync(string directory,Func<string,object,Task<JsonElement>> local) {
        _directory=Path.Combine(directory,"workspace-sync");Directory.CreateDirectory(_directory);_local=local;
    }
    private static readonly HashSet<string> Excluded=new(StringComparer.OrdinalIgnoreCase) {".git",".env",".ssh",".gnupg",".gnomeai-conflicts","node_modules","target","bin","obj","worker_outputs","workers.db",".DS_Store"};
    private static bool Allowed(string relative)=>!Path.IsPathRooted(relative) && relative.Length is >0 and <2048 &&
        !relative.Contains('\\') && !relative.Contains(':') && relative.Split('/').All(p=>p.Length>0 && !p.EndsWith(".",StringComparison.Ordinal) && !p.EndsWith(" ",StringComparison.Ordinal) && p is not "." and not ".." && !Excluded.Contains(p) && !p.StartsWith(".env.",StringComparison.OrdinalIgnoreCase)) &&
        !relative.EndsWith(".keystore",StringComparison.OrdinalIgnoreCase) && !relative.EndsWith(".pem",StringComparison.OrdinalIgnoreCase) && !relative.EndsWith(".key",StringComparison.OrdinalIgnoreCase);
    private static string Resolve(string root,string relative) {
        if(!Allowed(relative))throw new IOException("This file is outside the sync policy.");
        var path=Path.GetFullPath(root);
        if((File.GetAttributes(path)&FileAttributes.ReparsePoint)!=0)throw new IOException("Linked workspaces cannot be synced.");
        foreach(var part in relative.Split('/')) {
            path=Path.Combine(path,part);
            if(Path.Exists(path) && (File.GetAttributes(path)&FileAttributes.ReparsePoint)!=0)throw new IOException("Symbolic links are not synchronized.");
        }
        return path;
    }
    private static string? Hash(string path) {
        if(!File.Exists(path))return null;
        if(new FileInfo(path).Length>MaxFile)throw new IOException("A file exceeds the 64 MiB sync limit.");
        using var input=File.OpenRead(path);return Convert.ToHexString(SHA256.HashData(input));
    }
    private static void ValidHash(string hash) {
        if(hash.Length!=64 || !hash.All(Uri.IsHexDigit))throw new IOException("Invalid content hash.");
    }
    private string Blob(string hash) {ValidHash(hash);return Path.Combine(_directory,hash+".blob");}
    private static FileStream PrivateFile(string path,FileMode mode) {
        var options=new FileStreamOptions {Mode=mode,Access=FileAccess.Write,Share=FileShare.None};
        if(!OperatingSystem.IsWindows())options.UnixCreateMode=UnixFileMode.UserRead|UnixFileMode.UserWrite;
        return new FileStream(path,options);
    }
    public async Task<JsonElement> HandleAsync(string action,JsonElement payload) {
        await _gate.WaitAsync().ConfigureAwait(false);
        string? lockedSession=null;var lockToken=Guid.NewGuid().ToString();
        try {
            var session=payload.GetProperty("session_id").GetString()!;
            var location=await _local("workspace_location",new {session_id=session}).ConfigureAwait(false);
            if(location.GetProperty("busy").GetBoolean())throw new IOException("Workspace sync waits until the agent's current turn finishes.");
            var root=location.GetProperty("root").GetString()!;
            if(action=="workspace_apply") {await _local("workspace_lock",new {session_id=session,token=lockToken}).ConfigureAwait(false);lockedSession=session;}
            if(action=="workspace_manifest") {
                var files=new Dictionary<string,WorkspaceEntry>(StringComparer.Ordinal);
                var directories=new Stack<string>();directories.Push("");long total=0;
                while(directories.TryPop(out var relative)) {
                    var directory=relative.Length==0?root:Resolve(root,relative);
                    foreach(var path in Directory.EnumerateFileSystemEntries(directory)) {
                        var name=Path.GetRelativePath(root,path).Replace(Path.DirectorySeparatorChar,'/');
                        if(!Allowed(name) || (File.GetAttributes(path)&FileAttributes.ReparsePoint)!=0)continue;
                        if(Directory.Exists(path)){directories.Push(name);continue;}
                        if(files.Count>=10000)throw new IOException("Workspace exceeds 10,000 files.");
                        var length=new FileInfo(path).Length;total+=length;
                        if(total>512L*1024*1024)throw new IOException("Workspace exceeds 512 MiB. Exclude generated files first.");
                        files[name]=new(Hash(Resolve(root,name))!,length);
                    }
                }
                return JsonSerializer.SerializeToElement(new WorkspaceManifest(location.GetProperty("workspace_id").GetString()!,files));
            }
            if(action=="workspace_read") {
                var path=Resolve(root,payload.GetProperty("path").GetString()!);
                var expected=payload.GetProperty("hash").GetString()!;
                if(Hash(path)!=expected)throw new IOException("Workspace changed; retry the sync.");
                using var file=File.OpenRead(path);var offset=payload.GetProperty("offset").GetInt64();
                if(offset<0 || offset>file.Length)throw new IOException("Invalid chunk offset.");
                file.Position=offset;var bytes=new byte[Math.Min(Chunk,(int)(file.Length-offset))];file.ReadExactly(bytes);
                return JsonSerializer.SerializeToElement(new {data=Convert.ToBase64String(bytes)});
            }
            if(action=="workspace_chunk") {
                var hash=payload.GetProperty("hash").GetString()!;var blob=Blob(hash);
                var bytes=Convert.FromBase64String(payload.GetProperty("data").GetString()!);
                var offset=payload.GetProperty("offset").GetInt64();var total=payload.GetProperty("total").GetInt64();
                if(bytes.Length>Chunk || offset<0 || total<0 || total>MaxFile || offset+bytes.Length>total)throw new IOException("Invalid file chunk.");
                if(File.Exists(blob))return JsonSerializer.SerializeToElement(new {received=total});
                var partial=blob+".part";
                var cached=Directory.EnumerateFiles(_directory).Where(p=>p.EndsWith(".blob",StringComparison.Ordinal)||p.EndsWith(".part",StringComparison.Ordinal)).Sum(p=>new FileInfo(p).Length);
                if(cached+bytes.Length>1024L*1024*1024)throw new IOException("Workspace transfer cache reached 1 GiB. Clear the workspace-sync blob cache while sync is stopped.");
                if(offset==0) {using var start=PrivateFile(partial,FileMode.Create);}
                using(var output=PrivateFile(partial,FileMode.Open)) {
                    if(output.Length!=offset)throw new IOException("Chunk order changed; restart this file.");
                    output.Position=offset;output.Write(bytes);output.Flush(true);
                }
                if(offset+bytes.Length==total) {
                    if(Hash(partial)!=hash){File.Delete(partial);throw new IOException("File hash mismatch.");}
                    File.Move(partial,blob,true);
                }
                return JsonSerializer.SerializeToElement(new {received=offset+bytes.Length});
            }
            if(action=="workspace_apply") {
                if(location.GetProperty("status").GetString()=="transferring")throw new IOException("The source workspace is frozen for handoff.");
                var relative=payload.GetProperty("path").GetString()!;var path=Resolve(root,relative);
                var desired=payload.GetProperty("hash").GetString();var expected=payload.GetProperty("expected").GetString();
                var current=Hash(path);
                if(current==desired)return JsonSerializer.SerializeToElement(new {conflict=false});
                var conflict=current!=expected;
                if(conflict) {
                    var folder=Path.Combine(root,".gnomeai-conflicts");
                    if(Path.Exists(folder) && (File.GetAttributes(folder)&FileAttributes.ReparsePoint)!=0)throw new IOException("Conflict directory cannot be a symbolic link.");
                    Directory.CreateDirectory(folder);
                    var key=Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(relative)));
                    var alternative=Path.Combine(folder,key+"-"+(desired??"deleted"));
                    if(desired is not null)File.Copy(Blob(desired),alternative,true);
                    File.WriteAllText(alternative+".json",JsonSerializer.Serialize(new {path=relative,current,incoming=desired}));
                } else if(desired is null)File.Delete(path);
                else {
                    Directory.CreateDirectory(Path.GetDirectoryName(path)!);
                    Resolve(root,relative); // Check parents again after creating directories.
                    var temp=path+"."+Guid.NewGuid()+".sync";
                    using(var input=File.OpenRead(Blob(desired)))using(var output=PrivateFile(temp,FileMode.CreateNew)){input.CopyTo(output);output.Flush(true);}
                    File.Move(temp,path,true);
                }
                return JsonSerializer.SerializeToElement(new {conflict});
            }
            throw new IOException("Unknown workspace operation.");
        } finally {try {if(lockedSession is not null)await _local("workspace_unlock",new {session_id=lockedSession,token=lockToken}).ConfigureAwait(false);}finally{_gate.Release();}}
    }
    public async Task<int> SynchronizeAsync(PeerLink link,string session,bool move,bool pull=false) {
        async Task<JsonElement> At(bool remote,string action,object payload) {
            if(!remote)return await HandleAsync(action,JsonSerializer.SerializeToElement(payload));
            var result=await link.RequestAsync(action,payload);
            if(!result.GetProperty("ok").GetBoolean())throw new IOException(result.GetProperty("error").GetString());
            return result.GetProperty("data").Clone();
        }
        var local=(await At(false,"workspace_manifest",new {session_id=session})).Deserialize<WorkspaceManifest>()!;
        var remote=(await At(true,"workspace_manifest",new {session_id=session})).Deserialize<WorkspaceManifest>()!;
        if(local.WorkspaceId!=remote.WorkspaceId)throw new IOException("Workspace identities differ.");
        var baselinePath=Path.Combine(_directory,$"base-{link.Peer.Id}-{local.WorkspaceId}.json");
        var baseline=File.Exists(baselinePath)?JsonSerializer.Deserialize<Dictionary<string,string?>>(File.ReadAllText(baselinePath))!:new(StringComparer.Ordinal);
        var next=new Dictionary<string,string?>(baseline,StringComparer.Ordinal);var conflicts=0;
        async Task<bool> Copy(bool fromRemote,string name,WorkspaceEntry? entry,string? expected) {
            if(entry is not null) {
                for(long offset=0;offset<Math.Max(1,entry.Size);offset+=Chunk) {
                    var chunk=await At(fromRemote,"workspace_read",new {session_id=session,path=name,hash=entry.Hash,offset});
                    await At(!fromRemote,"workspace_chunk",new {session_id=session,hash=entry.Hash,total=entry.Size,offset,data=chunk.GetProperty("data").GetString()});
                }
            }
            var applied=await At(!fromRemote,"workspace_apply",new {session_id=session,path=name,hash=entry?.Hash,expected});
            return !applied.GetProperty("conflict").GetBoolean();
        }
        foreach(var name in local.Files.Keys.Union(remote.Files.Keys).Union(baseline.Keys).Order(StringComparer.Ordinal)) {
            local.Files.TryGetValue(name,out var a);remote.Files.TryGetValue(name,out var b);baseline.TryGetValue(name,out var before);
            if(a?.Hash==b?.Hash){next[name]=a?.Hash;continue;}
            if(move) {
                var source=pull?b:a;var target=pull?a:b;
                // No baseline + different destination content is a conflict.
                var expected=target?.Hash==before?target?.Hash:null;
                if(await Copy(pull,name,source,expected))next[name]=source?.Hash;else conflicts++;
            } else if(a?.Hash==before) {
                if(await Copy(true,name,b,a?.Hash))next[name]=b?.Hash;else conflicts++;
            } else if(b?.Hash==before) {
                if(await Copy(false,name,a,b?.Hash))next[name]=a?.Hash;else conflicts++;
            } else {
                await Copy(false,name,a,before);await Copy(true,name,b,before);conflicts++;
            }
        }
        using(var output=PrivateFile(baselinePath+".tmp",FileMode.Create)){JsonSerializer.Serialize(output,next);output.Flush(true);}
        File.Move(baselinePath+".tmp",baselinePath,true);
        if(move && conflicts==0) {
            var a=(await At(false,"workspace_manifest",new {session_id=session})).Deserialize<WorkspaceManifest>()!;
            var b=(await At(true,"workspace_manifest",new {session_id=session})).Deserialize<WorkspaceManifest>()!;
            if(a.Files.Count!=b.Files.Count || a.Files.Any(item=>!b.Files.TryGetValue(item.Key,out var value)||value!=item.Value))throw new IOException("Workspace changed during transfer. Resume after file edits stop.");
        }
        return conflicts;
    }
}
