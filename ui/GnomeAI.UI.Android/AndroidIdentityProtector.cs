using Android.Security.Keystore;
using Java.Security;
using Javax.Crypto;
using Javax.Crypto.Spec;
using System.Text;
using GnomeAI.Client;

namespace GnomeAI.Android;

/// Wrap identity and pairing keys at rest with a non-exportable Android AES key.
/// Decrypted peer keys still exist in application memory while in use.
internal sealed class AndroidIdentityProtector : IIdentityProtector
{
    private const string Alias="gnomeai.device-identity.v1", Prefix="android-keystore-v1:";
    private static IKey OpenKey()
    {
        using var store=KeyStore.GetInstance("AndroidKeyStore")!;store.Load(null);
        if(!store.ContainsAlias(Alias)) {
            using var generator=KeyGenerator.GetInstance(KeyProperties.KeyAlgorithmAes,"AndroidKeyStore")!;
            using var builder=new KeyGenParameterSpec.Builder(Alias,KeyStorePurpose.Encrypt|KeyStorePurpose.Decrypt);
            using var spec=builder.SetBlockModes(KeyProperties.BlockModeGcm)!
                .SetEncryptionPaddings(KeyProperties.EncryptionPaddingNone)!.SetKeySize(256)!.Build();
            generator.Init(spec);using var generated=generator.GenerateKey();
        }
        return store.GetKey(Alias,null)??throw new IOException("Device identity key is unavailable.");
    }
    public string Protect(string clear)
    {
        using var key=OpenKey();using var cipher=Cipher.GetInstance("AES/GCM/NoPadding")!;
        cipher.Init(CipherMode.EncryptMode,key);
        var encrypted=cipher.DoFinal(Encoding.UTF8.GetBytes(clear))!;
        return Prefix+Convert.ToBase64String(cipher.GetIV()!)+":"+Convert.ToBase64String(encrypted);
    }
    public string Unprotect(string saved)
    {
        if(!saved.StartsWith(Prefix,StringComparison.Ordinal)) {
            if(saved.TrimStart().StartsWith("{",StringComparison.Ordinal))return saved; // Migrate preview2 once.
            throw new IOException("Device identity storage format is invalid.");
        }
        var parts=saved[Prefix.Length..].Split(':');if(parts.Length!=2)throw new IOException("Invalid encrypted device identity.");
        using var key=OpenKey();using var cipher=Cipher.GetInstance("AES/GCM/NoPadding")!;
        using var parameters=new GCMParameterSpec(128,Convert.FromBase64String(parts[0]));
        cipher.Init(CipherMode.DecryptMode,key,parameters);
        return Encoding.UTF8.GetString(cipher.DoFinal(Convert.FromBase64String(parts[1]))!);
    }
}
