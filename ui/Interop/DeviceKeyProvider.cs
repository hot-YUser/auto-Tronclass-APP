using System.Security.Cryptography;

namespace TronClass.Interop;

/// <summary>
/// Owns the raw 32-byte vault key at the managed/platform boundary. Only an OS-protected envelope is
/// persisted. A legacy plaintext <c>device.key</c> is imported exactly once and removed only after
/// the protected envelope is durable and was verified to contain the same key.
/// </summary>
internal static class DeviceKeyProvider
{
    private const string ProtectedFileName = "device.key.os";
    private const string LegacyFileName = "device.key";
    private const int KeyLength = 32;

    public static byte[] LoadOrCreate(string dataDir)
    {
        Directory.CreateDirectory(dataDir);
        var protectedPath = Path.Combine(dataDir, ProtectedFileName);
        var legacyPath = Path.Combine(dataDir, LegacyFileName);

        if (File.Exists(protectedPath))
        {
            var key = UnprotectAndValidate(protectedPath);
            try
            {
                RemoveMatchingLegacyKey(legacyPath, key);
                return key;
            }
            catch
            {
                CryptographicOperations.ZeroMemory(key);
                throw;
            }
        }

        byte[]? candidate = null;
        byte[]? envelope = null;
        try
        {
            candidate = File.Exists(legacyPath)
                ? ReadExactKey(legacyPath, "舊版裝置金鑰")
                : RandomNumberGenerator.GetBytes(KeyLength);
            envelope = DeviceKeyProtector.Protect(candidate);

            if (TryCreateAtomically(protectedPath, envelope))
            {
                var verified = UnprotectAndValidate(protectedPath);
                try
                {
                    if (!CryptographicOperations.FixedTimeEquals(candidate, verified))
                        throw new InvalidDataException(
                            "OS 保護裝置金鑰寫入後驗證不一致；已保留舊版明文 key，不會啟動 vault。");
                }
                finally
                {
                    CryptographicOperations.ZeroMemory(verified);
                }
                RemoveMatchingLegacyKey(legacyPath, candidate);
                var result = candidate;
                candidate = null;
                return result;
            }

            // Another initializer won the create-new race. Its complete envelope is authoritative;
            // a legacy migration may only proceed when both envelopes resolve to the same key.
            var winner = UnprotectAndValidate(protectedPath);
            if (File.Exists(legacyPath)
                && !CryptographicOperations.FixedTimeEquals(candidate, winner))
            {
                CryptographicOperations.ZeroMemory(winner);
                throw new InvalidDataException(
                    "OS 保護金鑰與既有 device.key 不一致；已保留兩者，拒絕猜測 vault 應使用哪一把金鑰。");
            }
            RemoveMatchingLegacyKey(legacyPath, winner);
            return winner;
        }
        finally
        {
            if (candidate is not null) CryptographicOperations.ZeroMemory(candidate);
            if (envelope is not null) CryptographicOperations.ZeroMemory(envelope);
        }
    }

    private static byte[] UnprotectAndValidate(string path)
    {
        byte[]? envelope = null;
        try
        {
            envelope = File.ReadAllBytes(path);
            var key = DeviceKeyProtector.Unprotect(envelope);
            if (key.Length == KeyLength) return key;
            CryptographicOperations.ZeroMemory(key);
            throw new InvalidDataException(
                $"OS 保護裝置金鑰損毀：預期 {KeyLength} bytes，實際不是合法長度。");
        }
        catch (Exception error) when (error is not InvalidDataException)
        {
            throw new InvalidDataException(
                $"無法解開 OS 保護裝置金鑰；原檔保留於 {path}。", error);
        }
        finally
        {
            if (envelope is not null) CryptographicOperations.ZeroMemory(envelope);
        }
    }

    private static byte[] ReadExactKey(string path, string label)
    {
        var key = File.ReadAllBytes(path);
        if (key.Length == KeyLength) return key;
        CryptographicOperations.ZeroMemory(key);
        throw new InvalidDataException(
            $"{label}損毀：預期 {KeyLength} bytes；已保留原檔 {path}，不會生成新 key 覆蓋。");
    }

    private static void RemoveMatchingLegacyKey(string legacyPath, ReadOnlySpan<byte> protectedKey)
    {
        if (!File.Exists(legacyPath)) return;
        var legacy = ReadExactKey(legacyPath, "舊版裝置金鑰");
        try
        {
            if (!CryptographicOperations.FixedTimeEquals(legacy, protectedKey))
                throw new InvalidDataException(
                    "OS 保護金鑰與既有 device.key 不一致；已保留明文原檔，拒絕自動刪除。");
        }
        finally
        {
            CryptographicOperations.ZeroMemory(legacy);
        }

        File.Delete(legacyPath);
        if (File.Exists(legacyPath))
            throw new IOException($"OS 保護金鑰已保存，但無法移除舊版明文金鑰：{legacyPath}");
    }

    private static bool TryCreateAtomically(string destination, ReadOnlySpan<byte> bytes)
    {
        var temp = $"{destination}.tmp.{Guid.NewGuid():N}";
        try
        {
            using (var stream = new FileStream(
                temp, FileMode.CreateNew, FileAccess.Write, FileShare.None, 4096,
                FileOptions.WriteThrough))
            {
                stream.Write(bytes);
                stream.Flush(flushToDisk: true);
            }
            return DeviceKeyProtector.CommitNew(temp, destination);
        }
        finally
        {
            if (File.Exists(temp)) File.Delete(temp);
        }
    }
}
