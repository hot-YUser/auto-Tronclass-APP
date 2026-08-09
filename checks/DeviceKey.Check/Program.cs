using System.Security.Cryptography;
using TronClass.Interop;

var root = Path.Combine(Path.GetTempPath(), $"autotronclass-device-key-check-{Guid.NewGuid():N}");
Directory.CreateDirectory(root);
try
{
    CheckFirstRunAndReload(Path.Combine(root, "first-run"));
    CheckLegacyMigration(Path.Combine(root, "migration"));
    CheckCorruptEnvelopeIsPreserved(Path.Combine(root, "corrupt-envelope"));
    CheckConflictingLegacyKeyIsPreserved(Path.Combine(root, "conflict"));
    CheckCorruptLegacyKeyIsPreserved(Path.Combine(root, "corrupt-legacy"));
    Console.WriteLine("DeviceKey.Check：全部通過");
}
finally
{
    Directory.Delete(root, recursive: true);
}

static void CheckFirstRunAndReload(string directory)
{
    var first = DeviceKeyProvider.LoadOrCreate(directory);
    var second = DeviceKeyProvider.LoadOrCreate(directory);
    try
    {
        Assert(first.Length == 32, "首次 key 長度");
        Assert(CryptographicOperations.FixedTimeEquals(first, second), "重啟取得相同 key");
        Assert(File.Exists(Path.Combine(directory, "device.key.os")), "受保護 envelope 已保存");
        Assert(!File.Exists(Path.Combine(directory, "device.key")), "不得產生明文 key");
    }
    finally
    {
        CryptographicOperations.ZeroMemory(first);
        CryptographicOperations.ZeroMemory(second);
    }
}

static void CheckLegacyMigration(string directory)
{
    Directory.CreateDirectory(directory);
    var expected = Enumerable.Range(0, 32).Select(value => (byte)value).ToArray();
    File.WriteAllBytes(Path.Combine(directory, "device.key"), expected);
    var actual = DeviceKeyProvider.LoadOrCreate(directory);
    try
    {
        Assert(CryptographicOperations.FixedTimeEquals(expected, actual), "遷移不得更換 vault key");
        Assert(File.Exists(Path.Combine(directory, "device.key.os")), "遷移後 envelope 存在");
        Assert(!File.Exists(Path.Combine(directory, "device.key")), "遷移後移除明文 key");
    }
    finally
    {
        CryptographicOperations.ZeroMemory(expected);
        CryptographicOperations.ZeroMemory(actual);
    }
}

static void CheckCorruptEnvelopeIsPreserved(string directory)
{
    Directory.CreateDirectory(directory);
    var path = Path.Combine(directory, "device.key.os");
    var evidence = "not-a-dpapi-envelope"u8.ToArray();
    File.WriteAllBytes(path, evidence);
    ExpectFailure(() => DeviceKeyProvider.LoadOrCreate(directory));
    Assert(File.ReadAllBytes(path).SequenceEqual(evidence), "損毀 envelope 必須原樣保留");
    Assert(!File.Exists(Path.Combine(directory, "device.key")), "失敗時不得生成明文 key");
}

static void CheckConflictingLegacyKeyIsPreserved(string directory)
{
    Directory.CreateDirectory(directory);
    var original = Enumerable.Repeat((byte)0x11, 32).ToArray();
    var conflict = Enumerable.Repeat((byte)0x22, 32).ToArray();
    var legacyPath = Path.Combine(directory, "device.key");
    File.WriteAllBytes(legacyPath, original);
    var loaded = DeviceKeyProvider.LoadOrCreate(directory);
    CryptographicOperations.ZeroMemory(loaded);

    File.WriteAllBytes(legacyPath, conflict);
    ExpectFailure(() => DeviceKeyProvider.LoadOrCreate(directory));
    Assert(File.ReadAllBytes(legacyPath).SequenceEqual(conflict), "不一致明文 key 必須保留");
    CryptographicOperations.ZeroMemory(original);
    CryptographicOperations.ZeroMemory(conflict);
}

static void CheckCorruptLegacyKeyIsPreserved(string directory)
{
    Directory.CreateDirectory(directory);
    var legacyPath = Path.Combine(directory, "device.key");
    var evidence = new byte[] { 1, 2, 3 };
    File.WriteAllBytes(legacyPath, evidence);
    ExpectFailure(() => DeviceKeyProvider.LoadOrCreate(directory));
    Assert(File.ReadAllBytes(legacyPath).SequenceEqual(evidence), "損毀明文 key 必須保留");
    Assert(!File.Exists(Path.Combine(directory, "device.key.os")), "損毀 key 不得建立新 envelope");
}

static void ExpectFailure(Func<byte[]> action)
{
    try
    {
        var unexpected = action();
        CryptographicOperations.ZeroMemory(unexpected);
        throw new InvalidOperationException("預期操作失敗，但實際成功。");
    }
    catch (InvalidDataException)
    {
    }
}

static void Assert(bool condition, string message)
{
    if (!condition) throw new InvalidOperationException($"檢查失敗：{message}");
}
