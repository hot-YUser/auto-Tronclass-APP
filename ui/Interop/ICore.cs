using System.Text.Json;

namespace TronClass.Interop;

/// <summary>
/// The one seam the UI binds to. <see cref="NativeCore"/> drives the real Rust core over the FFI;
/// <see cref="MockCore"/> scripts a realistic event timeline so the UI can be built and previewed
/// WITHOUT the native library. Swap the two with one line in <c>MauiProgram</c>.
///
/// Commands are fire-and-await: <see cref="SendAsync"/> returns the id-correlated reply. Unsolicited
/// events arrive on <see cref="EventReceived"/> — raised on a worker thread, so marshal to the UI
/// thread yourself (<c>MainThread.BeginInvokeOnMainThread</c>). The <c>Last*</c> snapshots hold the
/// most recent of each state event so a screen can render immediately on appearing.
/// </summary>
public interface ICore
{
    Task BootAsync(string dataDir);
    Task<JsonElement> SendAsync(string cmd, params (string Key, object? Value)[] fields);
    event Action<JsonElement>? EventReceived;

    JsonElement? LastCaps { get; }
    JsonElement? LastProviders { get; }
    JsonElement? LastAccounts { get; }
    JsonElement? LastVaultState { get; }
    JsonElement? LastNextClass { get; } // null ⇒ no upcoming class → hide the Home "下一堂課" card
}
