namespace Ui;

/// <summary>設定頁的純邏輯；無 MAUI 相依，可由 console contract check 直接編譯。</summary>
internal static class SettingsSync
{
    /// <summary>倒數必須落在 core 的 1..=86400 秒契約內。</summary>
    public static bool TryParseCountdown(string? text, out int seconds) =>
        int.TryParse(text, out seconds) && seconds is >= 1 and <= 86_400;

    /// <summary>門檻停用(core 存 0)時畫面顯示的預設值。</summary>
    public static string CanonicalGateText(double attendanceGatePercent) =>
        (attendanceGatePercent > 0 ? attendanceGatePercent : 15).ToString("0.#");
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
