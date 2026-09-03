using System.Security.Cryptography;
using System.Text.Json;
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
    CheckNativeCoreLifecycle();
    CheckNativeCorePendingCap();
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

/// <summary>Pending 命令上限的來源級契約(免 native DLL):上限落在低十位數、閒置時配額全滿。</summary>
static void CheckNativeCorePendingCap()
{
    Assert(NativeCore.MaxPendingCommands >= 8 && NativeCore.MaxPendingCommands <= 64,
        $"Pending 上限必須是低十位數(合法 UI 併發為個位數)：{NativeCore.MaxPendingCommands}");
    Assert(NativeCore.PendingSlotsAvailable == NativeCore.MaxPendingCommands,
        "閒置時剩餘配額必須等於上限");
}
/// <summary>NativeCore 生命週期契約(免 native DLL):單一實例、dispose 後 fail-closed、命令/啟動不假成功。</summary>
static void CheckNativeCoreLifecycle()
{
    var first = new NativeCore();
    ExpectThrows<InvalidOperationException>(() => new NativeCore(), "同程序第二個實例必須拒絕");
    first.Dispose();

    // dispose 後新建一律失敗:不擋的話,殘留 _initialized=1 會讓新實例 BootAsync 假成功。
    ExpectThrows<ObjectDisposedException>(() => new NativeCore(), "dispose 後新建必須 fail-closed");

    // dispose 後命令以明確失敗回覆收尾(不懸住 caller、不碰 native)。
    var reply = first.SendAsync("StopAllMonitoring").GetAwaiter().GetResult();
    Assert(reply.TryGetProperty("ok", out var ok) && ok.ValueKind == JsonValueKind.False,
        "dispose 後 SendAsync 必須回失敗回覆");

    // dispose 後啟動不得假成功:尚未 init 的 handle 必須丟 ObjectDisposedException(在碰 native 之前)。
    var bootDir = Path.Combine(Path.GetTempPath(), $"autotronclass-nc-boot-{Guid.NewGuid():N}");
    try
    {
        ExpectThrows<ObjectDisposedException>(() => first.BootAsync(bootDir).GetAwaiter().GetResult(),
            "dispose 後 BootAsync 必須失敗,不得假成功");
    }
    finally
    {
        // disposed 立即 fault 時 bootDir 從未被建立(DeviceKeyProvider 沒被呼叫),只在存在時刪。
        if (Directory.Exists(bootDir)) Directory.Delete(bootDir, recursive: true);
    }
}

static void ExpectThrows<T>(Action action, string message) where T : Exception
{
    try
    {
        action();
    }
    catch (T)
    {
        return;
    }
    catch (Exception other)
    {
        throw new InvalidOperationException($"檢查失敗：{message}（實際例外 {other.GetType().Name}）");
    }
    throw new InvalidOperationException($"檢查失敗：{message}（未拋例外）");
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
