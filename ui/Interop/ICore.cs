using System.Text.Json;

namespace TronClass.Interop;

/// <summary>
/// The one seam the UI binds to. <see cref="NativeCore"/> drives the real Rust core over the FFI;
/// <c>MockCore</c> (Debug 建置才編入) scripts a realistic event timeline so the UI can be built and
/// previewed WITHOUT the native library. Swap the two with one line in <c>MauiProgram</c>.
///
/// Commands are fire-and-await: <see cref="SendAsync"/> returns the id-correlated reply. Unsolicited
/// events arrive on <see cref="EventReceived"/> — raised on a worker thread, so marshal to the UI
/// thread yourself (<c>MainThread.BeginInvokeOnMainThread</c>).
/// <see cref="LastMonitoringSnapshot"/> holds the most recent snapshot for the schedule clock.
/// </summary>
public interface ICore
{
    Task BootAsync(string dataDir);
    Task<JsonElement> SendAsync(string cmd, params (string Key, object? Value)[] fields);
    event Action<JsonElement>? EventReceived;

    JsonElement? LastMonitoringSnapshot { get; }
}
