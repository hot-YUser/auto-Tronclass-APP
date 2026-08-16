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
public sealed class NativeCore : ICore, IDisposable
{
    private static NativeCore? _self; // the instance the native callback routes events to
    private static unsafe void* _handle;
    private static long _nextId;
    private static readonly object BootGate = new();
    private static Task? _bootTask;
    private static int _initialized;  // 1 = Init 已成功;同一 handle 不再重送 Init
    private static int _disposed;     // 1 = 已釋放:不再接受 core_send/事件
    private static readonly object SendGate = new(); // 序列化 core_send;與 core_free 互斥
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
        // 已釋放的 static 生命週期不得復活:不擋的話,殘留 _initialized=1 會讓新實例 BootAsync
        // 直接回已完成 task 而假成功。檢查放在 CompareExchange 之後,與 Dispose 併行也 fail-closed;
        // 失敗時把自己從 _self 還原,不留下無效的靜態路由目標。
        if (Volatile.Read(ref _disposed) == 1)
        {
            Interlocked.CompareExchange(ref _self, null, this);
            throw new ObjectDisposedException(nameof(NativeCore));
        }
    }

    /// <summary>
    /// Thread-safe, exactly-once teardown. Windows 關窗時由 App 呼叫(WINDOWS-only);Android 的
    /// FGS 可能保住 process,core 必須存活,所以 Android 一律不 core_free。Dispose 後:
    /// 新 send 直接失敗、pending 命令以失敗回覆收尾、核心事件不再投遞;core_free 只在沒有
    /// 任何進行中的 core_send 時執行(SendGate 互斥),且絕不在 native callback 內發生。
    /// </summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return; // 恰一次
        // 完成所有 pending:等待中的 SendAsync 立刻以失敗收尾,不會懸住 caller。
        foreach (var (id, tcs) in Pending)
            if (Pending.TryRemove(id, out var t))
                t.TrySetResult(DisposedReply(id));
        lock (SendGate)
        {
            unsafe
            {
#if WINDOWS
                var handle = _handle;
                _handle = null;
                if (handle != null) NativeMethods.core_free(handle);
#else
                _handle = null; // Android:不 free(見 class doc);設 null 使任何殘留 send 失敗而非誤用
#endif
            }
        }
        _self = null; // 之後 native 不再路由事件給任何實例
    }

    /// <summary>
    /// Start the core and load state from <paramref name="dataDir"/> — exactly once per handle.
    /// Single-flight:並發呼叫共享同一個 task;失敗後下次呼叫自動重試。已成功 Init 過的 handle
    /// 直接回已完成 task,絕不重送 Init(重送會把執行中的 monitor 等狀態整個重設)。
    /// </summary>
    public Task BootAsync(string dataDir)
    {
        // disposed 檢查優先:已釋放的實例(即使成功 boot 過、_initialized=1)不得假成功回已完成 task——
        // 同物件路徑也要 fail-closed,以 faulted task 讓 caller 明確看到 ObjectDisposedException。
        if (Volatile.Read(ref _disposed) == 1)
            return Task.FromException(new ObjectDisposedException(nameof(NativeCore)));
        if (Volatile.Read(ref _initialized) == 1) return Task.CompletedTask;
        lock (BootGate)
        {
            if (Volatile.Read(ref _disposed) == 1)
                return Task.FromException(new ObjectDisposedException(nameof(NativeCore)));
            if (Volatile.Read(ref _initialized) == 1) return Task.CompletedTask;
            return _bootTask ??= BootCoreAsync(dataDir);
        }
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
            Volatile.Write(ref _initialized, 1);
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
        if (Volatile.Read(ref _disposed) == 1) throw new ObjectDisposedException(nameof(NativeCore));
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
        if (Volatile.Read(ref _disposed) == 1) return DisposedReply(id);
        if (!HasNativeHandle()) return CoreUnavailableReply(id, cmd);
        var tcs = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        Pending[id] = tcs;

        try
        {
            Send(JsonWire.SerializeCommand(id, cmd, fields));
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

    private static JsonElement TimeoutReply(ulong id, string cmd) =>
        FailedReply(id, $"命令逾時：核心未在 {CommandTimeout.TotalSeconds:0} 秒內回覆（{cmd}）");

    private static JsonElement CoreUnavailableReply(ulong id, string cmd) =>
        FailedReply(id, $"核心尚未完成啟動，無法執行 {cmd}。");

    private static JsonElement DisposedReply(ulong id) =>
        FailedReply(id, "核心已釋放，無法執行命令。");

    /// <summary>本地合成的失敗 Reply(核心沒回時才用),欄位形狀與核心的 Reply 信封一致。</summary>
    private static JsonElement FailedReply(ulong id, string error) =>
        JsonWire.Object(("id", id), ("event", "Reply"), ("ok", false), ("error", error));

    private static unsafe bool HasNativeHandle() => _handle != null;

    private unsafe void Send(string json)
    {
        // SendGate 序列化所有 core_send,並與 Dispose 的 core_free 互斥:
        // free 一定等進行中的 send 結束才執行,不會有用到已釋放 handle 的 send。
        // (Rust 會在 core_send 內同步回呼 OnEvent,但回呼只碰 managed 狀態、不碰 SendGate,故無死鎖。)
        lock (SendGate)
        {
            if (Volatile.Read(ref _disposed) == 1) throw new ObjectDisposedException(nameof(NativeCore));
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
    }

    [UnmanagedCallersOnly(CallConvs = new[] { typeof(CallConvCdecl) })]
    private static unsafe void OnEvent(byte* ptr, nuint len)
    {
        // No managed exception may cross an UnmanagedCallersOnly ABI boundary. The Rust core's FFI
        // entry points catch_unwind (release panic=unwind), but the managed side must still isolate
        // its own failures here — a throw across the ABI would be UB, not a recoverable UI error.
        try
        {
            ProcessEvent(new ReadOnlySpan<byte>(ptr, checked((int)len)));
        }
        catch (Exception error)
        {
            ReportIsolatedFault("原生回呼", error);
        }
    }

    // ── 隔離故障的可觀測性 ──────────────────────────────────────────────
    // 這兩個 catch(ABI 邊界與 subscriber 迴圈)必須吞掉例外,但**不能無聲**:
    // Debug.WriteLine 帶 [Conditional("DEBUG")],Release 會被整個編掉 —— 而 Release 正是
    // NativeAOT / full-trim 迴歸最先咬人的地方。真實案例:事件處理一拋例外,UI 就永遠停在
    // 「啟動中」,logcat 乾淨、沒有崩潰、什麼線索都沒有。故改成走與真實事件同一條延後佇列,
    // 讓它出現在 App 自己的日誌與 Toast。
    private static int _isolatedFaults;
    private const int MaxIsolatedFaultReports = 5;

    /// <summary>
    /// 把被隔離的例外轉成可見的 Error 事件。**本身必須 no-throw**(邊界之上再拋就是 UB),
    /// 且**不得**在此直接呼叫 managed 訂閱者(Rust 可能還握著 state mutex)——只入佇列。
    /// 只回報前 N 次:系統性失敗不該把日誌洗爆,也避免「回報本身又失敗」的無限迴圈。
    /// </summary>
    private static void ReportIsolatedFault(string origin, Exception error)
    {
        try
        {
            var seen = Interlocked.Increment(ref _isolatedFaults);
            if (seen > MaxIsolatedFaultReports) return;
            var tail = seen == MaxIsolatedFaultReports ? "；後續同類錯誤不再回報" : "";
            EventQueue.Enqueue(JsonWire.Object(
                ("id", null),
                ("event", "Error"),
                ("severity", "error"),
                ("message", $"FFI 縫（{origin}）已隔離例外 #{seen}：" +
                            $"{error.GetType().Name}：{error.Message}{tail}")));
            ScheduleEventDrain();
        }
        catch { /* the ABI boundary itself must remain no-throw */ }
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
                        // 訂閱者(AppState)在處理事件時拋例外 → 該事件的狀態更新就沒發生。
                        // 這比回呼本身失敗更常見,也更難察覺,同樣不能無聲。回報自己也會經過
                        // 這個迴圈,靠 MaxIsolatedFaultReports 上限收斂,不會無限自我餵食。
                        ReportIsolatedFault("事件訂閱者", error);
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
