namespace Ui;

using System.Globalization;

/// <summary>設定頁的純邏輯；無 MAUI 相依，可由 console contract check 直接編譯。</summary>
internal static class SettingsSync
{
    /// <summary>倒數必須落在 core 的 1..=86400 秒契約內。</summary>
    public static bool TryParseCountdown(string? text, out int seconds)
    {
        seconds = 0;
        if (string.IsNullOrWhiteSpace(text)) return false;
        if (!int.TryParse(text.Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out seconds)) return false;
        return seconds is >= 1 and <= 86_400;
    }

    /// <summary>防假點名門檻 0..100 的共享解析：InvariantCulture、嚴格 finite、JSON decimal 語意。</summary>
    public static bool TryParseGate(string? text, out double value)
    {
        value = 0;
        if (string.IsNullOrWhiteSpace(text)) return false;
        if (!double.TryParse(text.Trim(), NumberStyles.Float, CultureInfo.InvariantCulture, out value)) return false;
        if (!double.IsFinite(value)) return false;
        return value is >= 0 and <= 100;
    }

    /// <summary>門檻的共享格式化：InvariantCulture、可 round-trip。</summary>
    public static string FormatGate(double value) =>
        value.ToString("G17", CultureInfo.InvariantCulture);

    /// <summary>LLM max_tokens 0..1_000_000 共享解析：InvariantCulture，不接受群組分隔。</summary>
    public static bool TryParseMaxTokens(string? text, out int value)
    {
        value = 0;
        if (string.IsNullOrWhiteSpace(text)) return false;
        if (!int.TryParse(text.Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out value)) return false;
        return value is >= 0 and <= 1_000_000;
    }

    /// <summary>門檻停用(core 存 0)時畫面顯示的預設值；其餘以共享格式化呈現。</summary>
    public static string CanonicalGateText(double attendanceGatePercent) =>
        attendanceGatePercent > 0 ? FormatGate(attendanceGatePercent) : "15";

    /// <summary>QR 遠端位址的最小 hint：僅檢查必填與 http(s) 絕對 URL，正規化留給 core。</summary>
    public static bool TryHintQrRemoteBaseUrl(string? text, out string error)
    {
        error = "";
        var trimmed = (text ?? "").Trim();
        if (trimmed.Length == 0) { error = "遠端位址不得為空"; return false; }
        if (!Uri.TryCreate(trimmed, UriKind.Absolute, out var uri)) { error = "遠端位址格式不正確"; return false; }
        if (uri.Scheme != Uri.UriSchemeHttps && uri.Scheme != Uri.UriSchemeHttp) { error = "僅支援 http/https"; return false; }
        if (string.IsNullOrEmpty(uri.Host)) { error = "必須包含 host"; return false; }
        return true;
    }
}

/// <summary>
/// 單張設定卡的初始化／dirty 狀態。第一次核心回填可初始化未碰觸控制項；使用者若先
/// 輸入，第一個遲到的 Settings 事件也不得覆寫。成功儲存後清除 dirty/touched。
/// </summary>
internal sealed class SettingsCardSync
{
    bool _initialized;
    bool _touched;

    public bool IsDirty { get; private set; }
    public bool ShouldPopulate => !_touched && !IsDirty;

    public void MarkEdited()
    {
        _touched = true;
        IsDirty = _initialized;
    }

    public void Populated()
    {
        _initialized = true;
        _touched = false;
        IsDirty = false;
    }

    public void Saved() => Populated();
}
