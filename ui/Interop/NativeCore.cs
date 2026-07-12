using System.Collections.Concurrent;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;

namespace TronClass.Interop;

/// <summary>
/// The real FFI implementation of <see cref="ICore"/> — the entire C# side of the seam over the
/// native <c>tronclass_core</c> library. There is one native runtime per process, so the unmanaged
/// callback routes to the single live instance (<see cref="_self"/>). Commands correlate by id;
/// unsolicited events (id == null) are raised on <see cref="EventReceived"/>.
/// </summary>
public sealed class NativeCore : ICore
{
    private static NativeCore? _self; // the instance the native callback routes events to
    private static unsafe void* _handle;
    private static long _nextId;
    private static int _booted;
    private static readonly ConcurrentDictionary<ulong, TaskCompletionSource<JsonElement>> Pending = new();

    public event Action<JsonElement>? EventReceived;

    public JsonElement? LastCaps { get; private set; }
    public JsonElement? LastProviders { get; private set; }
    public JsonElement? LastAccounts { get; private set; }
    public JsonElement? LastVaultState { get; private set; }

    public NativeCore() => _self = this;

    /// <summary>Start the core and load state from <paramref name="dataDir"/> — exactly once.</summary>
    public async Task BootAsync(string dataDir)
    {
        if (Interlocked.Exchange(ref _booted, 1) == 1) return;
        Start();
        await SendAsync("Init", ("data_dir", dataDir));
    }

    private unsafe void Start()
    {
        if (_handle != null) return;
        _handle = NativeMethods.core_init(&OnEvent);
    }

    public Task<JsonElement> SendAsync(string cmd, params (string Key, object? Value)[] fields)
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

        // A numeric id is a command reply → complete the awaiting Task. (Events carry id == null.)
        if (root.TryGetProperty("id", out var idEl) && idEl.ValueKind == JsonValueKind.Number)
        {
            if (Pending.TryRemove(idEl.GetUInt64(), out var tcs)) tcs.SetResult(root.Clone());
            return;
        }

        var self = _self;
        if (self is null) return;
        var clone = root.Clone();
        if (root.TryGetProperty("event", out var ev))
        {
            switch (ev.GetString())
            {
                case "Caps": self.LastCaps = clone; break;
                case "Providers": self.LastProviders = clone; break;
                case "Accounts": self.LastAccounts = clone; break;
                case "VaultState": self.LastVaultState = clone; break;
            }
        }
        self.EventReceived?.Invoke(clone);
    }
}
