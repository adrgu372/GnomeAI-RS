using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace GnomeAI.Android.UI;

// The existing desktop snapshot contains answer text only. Keep reasoning actually
// received by this phone separately, keyed by source/session and exact answer.
internal sealed class MobileThinkingStore(string home)
{
    private readonly string _directory=Path.Combine(home,"store","mobile-thinking");
    private readonly SemaphoreSlim _writes=new(1,1);
    internal static string Digest(string text)=>Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(text)));
    internal static string TurnKey(int index,string text)=>index+":"+Digest(text);
    private string PathFor(string scope)=>Path.Combine(_directory,Digest(scope)+".json");
    public async Task<Dictionary<string,string>> LoadAsync(string scope)
    {
        var path=PathFor(scope);if(!File.Exists(path))return new();
        try{return JsonSerializer.Deserialize<Dictionary<string,string>>(await File.ReadAllTextAsync(path))??new();}
        catch(IOException){return new();}catch(JsonException){return new();}
    }
    public async Task SaveAsync(string scope,Dictionary<string,string> values)
    {
        var snapshot=JsonSerializer.Serialize(values);await _writes.WaitAsync();
        try{Directory.CreateDirectory(_directory);var path=PathFor(scope);var temporary=path+"."+Guid.NewGuid().ToString("N")+".tmp";try{await File.WriteAllTextAsync(temporary,snapshot);File.Move(temporary,path,true);}finally{if(File.Exists(temporary))File.Delete(temporary);}}
        finally{_writes.Release();}
    }
}
