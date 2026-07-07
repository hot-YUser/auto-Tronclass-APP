using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using TronClass.Interop;

// Drives the same three proofs as the Rust seam test, but across the C# P/Invoke boundary:
// good creds → LoginResult ok, bad creds → ok:false (loud, not silent), plus the unsolicited
// StateChanged and heartbeat Tick pushed up through the callback.

static class Program
{
    static readonly List<string> Events = new();
    static readonly object Gate = new();

    [UnmanagedCallersOnly(CallConvs = new[] { typeof(CallConvCdecl) })]
    static unsafe void OnEvent(byte* ptr, nuint len)
    {
        // Copy synchronously — the buffer is only valid for this call. This runs on a Rust
        // tokio worker thread, so it must be quick and thread-safe.
        var json = Encoding.UTF8.GetString(new ReadOnlySpan<byte>(ptr, (int)len));
        lock (Gate) Events.Add(json);
    }

    static unsafe int Main(string[] args)
    {
        var baseUrl = args.Length > 0 ? args[0] : "http://127.0.0.1:8779";
        var handle = NativeMethods.core_init(&OnEvent);

        void Send(int id, string user, string pass)
        {
            var cmd = JsonSerializer.Serialize(new
            {
                id,
                cmd = "Login",
                base_url = baseUrl,
                username = user,
                password = pass,
            });
            var bytes = Encoding.UTF8.GetBytes(cmd);
            fixed (byte* p = bytes) NativeMethods.core_send(handle, p, (nuint)bytes.Length);
        }

        Send(1, "test", "secret"); // → ok:true
        Send(2, "test", "wrong");  // → ok:false

        bool ok1 = false, ok2Seen = false, ok2 = false, sawState = false, sawTick = false;
        var deadline = DateTime.UtcNow.AddSeconds(20);
        while (DateTime.UtcNow < deadline)
        {
            Thread.Sleep(100);
            List<string> snapshot;
            lock (Gate) snapshot = new(Events);
            foreach (var ev in snapshot)
            {
                var root = JsonDocument.Parse(ev).RootElement;
                switch (root.GetProperty("event").GetString())
                {
                    case "StateChanged": sawState = true; break;
                    case "Tick": sawTick = true; break;
                    case "LoginResult":
                        var id = root.GetProperty("id").GetInt32();
                        var ok = root.GetProperty("ok").GetBoolean();
                        if (id == 1) ok1 = ok;
                        if (id == 2) { ok2Seen = true; ok2 = ok; }
                        break;
                }
            }
            if (ok1 && ok2Seen && sawState && sawTick) break;
        }

        NativeMethods.core_free(handle);

        Console.WriteLine($"login#1 ok={ok1}  login#2 ok={ok2} (seen={ok2Seen})  stateChanged={sawState}  tick={sawTick}");
        var pass = ok1 && ok2Seen && !ok2 && sawState && sawTick;
        Console.WriteLine(pass ? "SEAM SMOKE PASS" : "SEAM SMOKE FAIL");
        return pass ? 0 : 1;
    }
}
