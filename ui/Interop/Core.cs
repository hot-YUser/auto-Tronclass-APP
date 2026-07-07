using System.Collections.Concurrent;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;

namespace TronClass.Interop;

/// <summary>
/// Process-lifetime wrapper over the native core. Exactly one instance for the whole app —
/// never owned by a Page or Activity, so backgrounding or view recreation never disturbs the
/// running core. This is the entire C# side of the FFI seam.
/// </summary>
public sealed class Core
{
    public static Core Instance { get; } = new();

    private static unsafe void* _handle;
    private static long _nextId;

    // Correlates a command's reply back to its awaiting caller. RunContinuationsAsynchronously
    // is essential: SetResult runs on a Rust tokio worker thread, and we must not let the
    // caller's await-continuation run inline there and block the worker.
    private static readonly ConcurrentDictionary<ulong, TaskCompletionSource<JsonElement>> Pending = new();

    /// <summary>Unsolicited events (id == null): StateChanged, LogLine, Tick, Error. Raised on a
    /// worker thread — subscribers must marshal to the UI thread before touching UI.</summary>
    public event Action<JsonElement>? EventReceived;

    private Core() { }

    public unsafe void Start()
    {
        if (_handle != null) return;
        _handle = NativeMethods.core_init(&OnEvent);
    }

    public void Shutdown()
    {
        unsafe
        {
            if (_handle == null) return;
            NativeMethods.core_free(_handle);
            _handle = null;
        }
    }

    /// <summary>Perform one login. Completes when the core replies with the correlated result.</summary>
    public Task<JsonElement> LoginAsync(string baseUrl, string username, string password)
    {
        var id = (ulong)Interlocked.Increment(ref _nextId);
        var tcs = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        Pending[id] = tcs;

        var json = JsonSerializer.Serialize(new
        {
            id,
            cmd = "Login",
            base_url = baseUrl,
            username,
            password,
        });
        Send(json);
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
        // Copy synchronously: the buffer is only valid for this call, which is on a Rust
        // worker thread. Return quickly.
        var json = Encoding.UTF8.GetString(new ReadOnlySpan<byte>(ptr, (int)len));
        using var doc = JsonDocument.Parse(json);
        var root = doc.RootElement;

        var idProp = root.GetProperty("id");
        if (idProp.ValueKind == JsonValueKind.Number)
        {
            if (Pending.TryRemove(idProp.GetUInt64(), out var tcs))
            {
                tcs.SetResult(root.Clone()); // returns immediately (RunContinuationsAsynchronously)
            }
            return;
        }

        Instance.EventReceived?.Invoke(root.Clone());
    }
}
