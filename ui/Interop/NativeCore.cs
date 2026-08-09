using System.Collections.Concurrent;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
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
    private static readonly object BootGate = new();
    private static Task? _bootTask;
    private static readonly ConcurrentDictionary<ulong, TaskCompletionSource<JsonElement>> Pending = new();
    private static readonly ConcurrentQueue<JsonElement> EventQueue = new();
    private static int _eventDrainScheduled;

    public event Action<JsonElement>? EventReceived;

    public JsonElement? LastCaps { get; private set; }
    public JsonElement? LastProviders { get; private set; }
    public JsonElement? LastAccounts { get; private set; }
    public JsonElement? LastVaultState { get; private set; }
    public JsonElement? LastNextClass { get; private set; }

    public NativeCore()
    {
        var existing = Interlocked.CompareExchange(ref _self, this, null);
        if (existing is not null && !ReferenceEquals(existing, this))
            throw new InvalidOperationException("NativeCore 每個程序只能建立一個實例。");
    }

    /// <summary>Start the core and load state from <paramref name="dataDir"/> — exactly once.</summary>
    public Task BootAsync(string dataDir)
    {
        lock (BootGate) return _bootTask ??= BootCoreAsync(dataDir);
    }

    private async Task BootCoreAsync(string dataDir)
    {
        byte[]? deviceKey = null;
        try
        {
            // Android Keystore may involve secure hardware and must not block the UI thread.
            deviceKey = await Task.Run(() => DeviceKeyProvider.LoadOrCreate(dataDir));
            Start();
            var reply = await SendAsync(
                "Init",
                ("data_dir", dataDir),
                ("device_key_b64", Convert.ToBase64String(deviceKey)));
            if (reply.TryGetProperty("ok", out var ok) && ok.ValueKind == JsonValueKind.False)
            {
                var error = reply.TryGetProperty("error", out var value) ? value.GetString() : null;
                throw new InvalidOperationException(error ?? "核心初始化失敗。");
            }
        }
        catch
        {
            // A transient keystore/core error may be retried; callers racing the first boot still
            // observe this same Task rather than returning early from a half-initialized process.
            lock (BootGate) _bootTask = null;
            throw;
        }
        finally
        {
            if (deviceKey is not null) CryptographicOperations.ZeroMemory(deviceKey);
        }
    }

    private unsafe void Start()
    {
        if (_handle != null) return;
        _handle = NativeMethods.core_init(&OnEvent);
    }

    // A generous safety net. Every command the core RECEIVES now replies — even unknown/malformed ones
    // (see engine::send's id recovery) — so this only ever trips if the native side is truly lost (a
    // crash or a dropped event). It is longer than the core's 180s captcha-login window, so a slow
    // interactive login never trips it; without it, a lost reply would leak the pending entry forever.
    private static readonly TimeSpan CommandTimeout = TimeSpan.FromSeconds(300);

    public async Task<JsonElement> SendAsync(string cmd, params (string Key, object? Value)[] fields)
    {
        var id = (ulong)Interlocked.Increment(ref _nextId);
        if (!HasNativeHandle()) return CoreUnavailableReply(id, cmd);
        var tcs = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        Pending[id] = tcs;

        var dict = new Dictionary<string, object?> { ["id"] = id, ["cmd"] = cmd };
        foreach (var (k, v) in fields) dict[k] = v;
        try
        {
            Send(JsonSerializer.Serialize(dict));
        }
        catch
        {
            Pending.TryRemove(id, out _);
            throw;
        }

        using var cts = new CancellationTokenSource(CommandTimeout);
        using var reg = cts.Token.Register(() =>
        {
            if (Pending.TryRemove(id, out var t))
                t.TrySetResult(TimeoutReply(id, cmd));
        });
        return await tcs.Task;
    }

    private static JsonElement TimeoutReply(ulong id, string cmd) => JsonSerializer.SerializeToElement(new
    {
        id,
        @event = "Reply",
        ok = false,
        error = $"命令逾時：核心未在 {CommandTimeout.TotalSeconds:0} 秒內回覆（{cmd}）",
    });

    private static JsonElement CoreUnavailableReply(ulong id, string cmd) => JsonSerializer.SerializeToElement(new
    {
        id,
        @event = "Reply",
        ok = false,
        error = $"核心尚未完成啟動，無法執行 {cmd}。",
    });

    private static unsafe bool HasNativeHandle() => _handle != null;

    private unsafe void Send(string json)
    {
        var bytes = Encoding.UTF8.GetBytes(json);
        try
        {
            fixed (byte* p = bytes)
            {
                NativeMethods.core_send(_handle, p, (nuint)bytes.Length);
            }
        }
        finally
        {
            CryptographicOperations.ZeroMemory(bytes);
        }
    }

    [UnmanagedCallersOnly(CallConvs = new[] { typeof(CallConvCdecl) })]
    private static unsafe void OnEvent(byte* ptr, nuint len)
    {
        // No managed exception may cross an UnmanagedCallersOnly ABI boundary. With the Rust core's
        // panic=abort profile that would be a process-level failure, not a recoverable UI error.
        try
        {
            ProcessEvent(new ReadOnlySpan<byte>(ptr, checked((int)len)));
        }
        catch (Exception error)
        {
            try { System.Diagnostics.Debug.WriteLine($"Native callback 已隔離 {error.GetType().Name}"); }
            catch { /* the ABI boundary itself must remain no-throw */ }
        }
    }

    private static void ProcessEvent(ReadOnlySpan<byte> bytes)
    {
        var json = Encoding.UTF8.GetString(bytes);
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
                case "NextClass": self.LastNextClass = clone; break;
            }
        }
        // Never invoke managed subscribers while Rust may still hold its state mutex. Queue all
        // unsolicited events onto one FIFO drain: preserves event order and makes callback reentry safe.
        EventQueue.Enqueue(clone);
        ScheduleEventDrain();
    }

    private static void ScheduleEventDrain()
    {
        if (Interlocked.CompareExchange(ref _eventDrainScheduled, 1, 0) != 0) return;
        ThreadPool.QueueUserWorkItem(static _ => DrainEvents());
    }

    private static void DrainEvents()
    {
        try
        {
            while (EventQueue.TryDequeue(out var coreEvent))
            {
                var subscribers = _self?.EventReceived;
                if (subscribers is null) continue;
                foreach (Action<JsonElement> subscriber in subscribers.GetInvocationList())
                {
                    try { subscriber(coreEvent); }
                    catch (Exception error)
                    {
                        try { System.Diagnostics.Debug.WriteLine($"Core event subscriber 已隔離 {error.GetType().Name}"); }
                        catch { }
                    }
                }
            }
        }
        finally
        {
            Volatile.Write(ref _eventDrainScheduled, 0);
            if (!EventQueue.IsEmpty) ScheduleEventDrain();
        }
    }
}
