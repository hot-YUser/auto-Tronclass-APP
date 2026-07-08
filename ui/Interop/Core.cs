using System.Collections.Concurrent;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;

namespace TronClass.Interop;

/// <summary>
/// Process-lifetime wrapper over the native core — the entire C# side of the FFI seam. One
/// instance for the whole app; never owned by a Page. Commands correlate by id; unsolicited
/// events are raised on <see cref="EventReceived"/> (and the latest of each cached for dumb views).
/// </summary>
public sealed class Core
{
    public static Core Instance { get; } = new();

    private static unsafe void* _handle;
    private static long _nextId;
    private static int _booted;
    private static readonly ConcurrentDictionary<ulong, TaskCompletionSource<JsonElement>> Pending = new();

    /// <summary>Raised (on a worker thread) for unsolicited events. Subscribers marshal to the UI thread.</summary>
    public event Action<JsonElement>? EventReceived;

    // Latest snapshot of each state event, so a screen can render immediately on appearing.
    public JsonElement? LastProviders { get; private set; }
    public JsonElement? LastAccounts { get; private set; }
    public JsonElement? LastVaultState { get; private set; }

    private Core() { }

    /// <summary>Start the core and load state from <paramref name="dataDir"/> — exactly once.</summary>
    public async Task BootAsync(string dataDir)
    {
        if (Interlocked.Exchange(ref _booted, 1) == 1) return;
        Start();
        await InitAsync(dataDir);
    }

    public unsafe void Start()
    {
        if (_handle != null) return;
        _handle = NativeMethods.core_init(&OnEvent);
    }

    public Task<JsonElement> InitAsync(string dataDir) => SendAsync("Init", ("data_dir", dataDir));
    public Task<JsonElement> CreateVaultAsync(string pw) => SendAsync("CreateVault", ("master_password", pw));
    public Task<JsonElement> UnlockAsync(string pw) => SendAsync("Unlock", ("master_password", pw));
    public Task<JsonElement> AddAccountAsync(string label, string school, string user, string pass) =>
        SendAsync("AddAccount", ("label", label), ("school", school), ("username", user), ("password", pass));
    public Task<JsonElement> SwitchAccountAsync(string accountId) => SendAsync("SwitchAccount", ("account_id", accountId));
    public Task<JsonElement> DeleteAccountAsync(string accountId) => SendAsync("DeleteAccount", ("account_id", accountId));
    public Task<JsonElement> LoginAsync(string accountId) => SendAsync("Login", ("account_id", accountId));

    private Task<JsonElement> SendAsync(string cmd, params (string Key, object? Value)[] fields)
    {
        var id = (ulong)Interlocked.Increment(ref _nextId);
        var tcs = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        Pending[id] = tcs;

        var dict = new Dictionary<string, object?> { ["id"] = id, ["cmd"] = cmd };
        foreach (var (k, v) in fields) dict[k] = v;
        Send(JsonSerializer.Serialize(dict));
        return tcs.Task;
    }

    private unsafe void Send(string json)
    {
        var bytes = Encoding.UTF8.GetBytes(json);
        fixed (byte* p = bytes)
        {
            NativeMethods.core_send(_handle, p, (nuint)bytes.Length);
        }
    }

    [UnmanagedCallersOnly(CallConvs = new[] { typeof(CallConvCdecl) })]
    private static unsafe void OnEvent(byte* ptr, nuint len)
    {
        var json = Encoding.UTF8.GetString(new ReadOnlySpan<byte>(ptr, (int)len));
        using var doc = JsonDocument.Parse(json);
        var root = doc.RootElement;

        // id-tagged events are command replies (Reply / LoginResult) → complete the awaiting Task.
        if (root.GetProperty("id").ValueKind == JsonValueKind.Number)
        {
            if (Pending.TryRemove(root.GetProperty("id").GetUInt64(), out var tcs))
                tcs.SetResult(root.Clone());
            return;
        }

        var clone = root.Clone();
        switch (root.GetProperty("event").GetString())
        {
            case "Providers": Instance.LastProviders = clone; break;
            case "Accounts": Instance.LastAccounts = clone; break;
            case "VaultState": Instance.LastVaultState = clone; break;
        }
        Instance.EventReceived?.Invoke(clone);
    }
}
