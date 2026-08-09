using System.Security.Cryptography;
using Android.Security.Keystore;
using Java.Security;
using Javax.Crypto;
using Javax.Crypto.Spec;

namespace TronClass.Interop;

/// <summary>
/// Wraps the vault key with a non-exportable AES-256 key owned by AndroidKeyStore. The random GCM IV
/// travels with the ciphertext; backup rules exclude the envelope because the keystore key is bound
/// to this app installation.
/// </summary>
internal static class DeviceKeyProtector
{
    private const string Alias = "com.autotronclass.app.vault-wrap.v1";
    private const string StoreName = "AndroidKeyStore";
    private const string Transformation = "AES/GCM/NoPadding";
    private const int TagBits = 128;
    private static ReadOnlySpan<byte> Magic => "ATK1A"u8;

    public static byte[] Protect(ReadOnlySpan<byte> plaintext)
    {
        using var key = GetOrCreateKey();
        using var cipher = Cipher.GetInstance(Transformation)
            ?? throw new CryptographicException("Android 不支援 AES/GCM/NoPadding。");
        cipher.Init(Javax.Crypto.CipherMode.EncryptMode, key);
        var iv = cipher.GetIV() ?? throw new CryptographicException("Android Keystore 未產生 GCM IV。");
        var input = plaintext.ToArray();
        byte[] ciphertext;
        try
        {
            ciphertext = cipher.DoFinal(input)
                ?? throw new CryptographicException("Android Keystore 未產生密文。");
        }
        finally
        {
            CryptographicOperations.ZeroMemory(input);
        }
        if (iv.Length > byte.MaxValue)
            throw new CryptographicException("Android Keystore 產生的 GCM IV 過長。");

        var envelope = new byte[Magic.Length + 1 + iv.Length + ciphertext.Length];
        Magic.CopyTo(envelope);
        envelope[Magic.Length] = (byte)iv.Length;
        iv.CopyTo(envelope, Magic.Length + 1);
        ciphertext.CopyTo(envelope, Magic.Length + 1 + iv.Length);
        CryptographicOperations.ZeroMemory(ciphertext);
        return envelope;
    }

    public static byte[] Unprotect(ReadOnlySpan<byte> envelope)
    {
        if (!envelope.StartsWith(Magic) || envelope.Length <= Magic.Length + 1)
            throw new InvalidDataException("裝置金鑰不是 Android Keystore 格式。");
        var ivLength = envelope[Magic.Length];
        var payloadOffset = Magic.Length + 1 + ivLength;
        if (ivLength < 12 || payloadOffset + TagBits / 8 > envelope.Length)
            throw new InvalidDataException("Android Keystore 裝置金鑰 envelope 損毀。");

        var iv = envelope.Slice(Magic.Length + 1, ivLength).ToArray();
        var ciphertext = envelope[payloadOffset..].ToArray();
        try
        {
            using var key = GetExistingKey();
            using var cipher = Cipher.GetInstance(Transformation)
                ?? throw new CryptographicException("Android 不支援 AES/GCM/NoPadding。");
            using var parameters = new GCMParameterSpec(TagBits, iv);
            cipher.Init(Javax.Crypto.CipherMode.DecryptMode, key, parameters);
            return cipher.DoFinal(ciphertext)
                ?? throw new CryptographicException("Android Keystore 未傳回解密結果。");
        }
        finally
        {
            CryptographicOperations.ZeroMemory(ciphertext);
        }
    }

    /// <summary>Atomically create the envelope, then fsync its parent directory metadata.</summary>
    public static bool CommitNew(string temporaryPath, string destination)
    {
        try
        {
            File.Move(temporaryPath, destination, overwrite: false);
        }
        catch (IOException) when (File.Exists(destination))
        {
            return false;
        }

        var directory = Path.GetDirectoryName(destination)
            ?? throw new InvalidOperationException("裝置金鑰路徑沒有父目錄。");
        var descriptor = Android.Systems.Os.Open(
            directory,
            // O_CLOEXEC only exists from API 27; this project supports API 24 and this short-lived
            // descriptor never crosses a child-process boundary.
            Android.Systems.OsConstants.ORdonly,
            0);
        try { Android.Systems.Os.Fsync(descriptor); }
        finally { Android.Systems.Os.Close(descriptor); }
        return true;
    }

    private static IKey GetOrCreateKey()
    {
        using var store = OpenStore();
        if (store.GetKey(Alias, null) is { } existing) return existing;

        using var generator = KeyGenerator.GetInstance(KeyProperties.KeyAlgorithmAes, StoreName)
            ?? throw new CryptographicException("無法開啟 AndroidKeyStore AES KeyGenerator。");
        using var builder = new KeyGenParameterSpec.Builder(
            Alias, KeyStorePurpose.Encrypt | KeyStorePurpose.Decrypt);
        using var specification = builder
            .SetKeySize(256)
            .SetBlockModes(KeyProperties.BlockModeGcm)
            .SetEncryptionPaddings(KeyProperties.EncryptionPaddingNone)
            .Build();
        generator.Init(specification);
        return generator.GenerateKey()
            ?? throw new CryptographicException("AndroidKeyStore 無法建立 wrapping key。");
    }

    private static IKey GetExistingKey()
    {
        using var store = OpenStore();
        return store.GetKey(Alias, null)
            ?? throw new CryptographicException(
                "AndroidKeyStore wrapping key 不存在；可能是 App 資料被還原到另一台裝置。");
    }

    private static KeyStore OpenStore()
    {
        var store = KeyStore.GetInstance(StoreName)
            ?? throw new CryptographicException("無法開啟 AndroidKeyStore。");
        store.Load(null);
        return store;
    }
}
