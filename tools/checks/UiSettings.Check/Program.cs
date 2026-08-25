using System.Text.Json;
using TronClass.Interop;
using Ui;

// 設定頁未儲存編輯保護與輸入契約的純邏輯(SettingsPage 共用)。

Assert(SettingsSync.TryParseCountdown("1", out var oneSecond) && oneSecond == 1,
    "countdown 1 必須接受");
Assert(SettingsSync.TryParseCountdown("86400", out var oneDay) && oneDay == 86_400,
    "countdown 86400 必須接受");
Assert(!SettingsSync.TryParseCountdown("0", out _), "countdown 0 必須拒絕");
Assert(!SettingsSync.TryParseCountdown("-1", out _), "countdown 負數必須拒絕");
Assert(!SettingsSync.TryParseCountdown("86401", out _), "countdown 超過一天必須拒絕");
Assert(!SettingsSync.TryParseCountdown("fast", out _), "countdown 非整數必須拒絕");

Assert(SettingsSync.CanonicalGateText(0) == "15", "gate 0 → 顯示 15");
Assert(SettingsSync.CanonicalGateText(15.0) == "15", "gate 15 → 15");
Assert(SettingsSync.CanonicalGateText(30.5) == "30.5", "gate 30.5 → 30.5");

var monitor = new SettingsCardSync();
Assert(monitor.ShouldPopulate, "初次核心事件必須填入空控制項");
monitor.MarkEdited();
Assert(!monitor.IsDirty, "初始化前的使用者輸入尚無快照可比較");
Assert(!monitor.ShouldPopulate, "初始化前的使用者輸入不得被遲到的核心事件覆寫");
monitor.Saved(); // 使用者保存成功後，核心事件可回填正規值
Assert(monitor.ShouldPopulate && !monitor.IsDirty, "成功保存後可接受後續核心同步");
monitor.MarkEdited();
Assert(monitor.IsDirty && !monitor.ShouldPopulate, "使用者編輯後不得被不相關事件覆寫");
monitor.Saved();
Assert(!monitor.IsDirty && monitor.ShouldPopulate, "儲存成功後 dirty 清除");

var llm = new SettingsCardSync();
llm.Populated();
llm.MarkEdited();
Assert(!llm.ShouldPopulate, "每張卡的 dirty 狀態彼此獨立");
monitor.Populated();
Assert(llm.IsDirty, "另一張卡回填不得清除 LLM 卡的 dirty");
llm.Saved();
Assert(llm.ShouldPopulate, "LLM 卡儲存後重新允許核心回填");

var newYork = TimeZoneInfo.FindSystemTimeZoneById("America/New_York");
var spring = new WeeklyScheduleSpec(sunday:
[
    new TimeWindowSpec(150, 240), // 02:30 不存在，必須移到 03:00
]);
var springResult = ScheduleCalculator.Evaluate(
    spring, newYork, new DateTimeOffset(2026, 3, 8, 6, 0, 0, TimeSpan.Zero));
Assert(!springResult.IsOpen, "spring gap 前尚未開啟");
Assert(springResult.NextBoundaryUtc == new DateTimeOffset(2026, 3, 8, 7, 0, 0, TimeSpan.Zero),
    "不存在的 02:30 邊界移到 gap 後第一個有效分鐘 03:00");

var fall = new WeeklyScheduleSpec(sunday:
[
    new TimeWindowSpec(90, 105), // 01:30→01:45 都重複
]);
var fallResult = ScheduleCalculator.Evaluate(
    fall, newYork, new DateTimeOffset(2026, 11, 1, 5, 40, 0, TimeSpan.Zero));
Assert(fallResult.IsOpen, "fall overlap 第一次 01:40 已在時間窗內");
Assert(fallResult.CurrentWindowStartUtc == new DateTimeOffset(2026, 11, 1, 5, 30, 0, TimeSpan.Zero),
    "重複開始取較早 UTC");
Assert(fallResult.NextBoundaryUtc == new DateTimeOffset(2026, 11, 1, 6, 45, 0, TimeSpan.Zero),
    "重複結束取較晚 UTC");

var sundayCrossMidnight = new WeeklyScheduleSpec(sunday:
[
    new TimeWindowSpec(1380, 60),
]);
var crossResult = ScheduleCalculator.Evaluate(
    sundayCrossMidnight, TimeZoneInfo.Utc,
    new DateTimeOffset(2026, 1, 5, 0, 30, 0, TimeSpan.Zero));
Assert(crossResult.IsOpen, "週日跨午夜時間窗涵蓋週一 00:30");

var global = new WeeklyScheduleSpec(monday: [new TimeWindowSpec(0, 60)]);
var inherited = ScheduleCalculator.Evaluate(
    ScheduleBindingSpec.InheritGlobal, global, TimeZoneSpec.Named("Etc/UTC"),
    new DateTimeOffset(2026, 1, 5, 0, 30, 0, TimeSpan.Zero));
Assert(inherited.IsOpen, "inherit_global 使用全局時間表");
var empty = ScheduleCalculator.Evaluate(
    ScheduleBindingSpec.InheritGlobal, new WeeklyScheduleSpec(), TimeZoneSpec.Device,
    DateTimeOffset.UtcNow);
Assert(!empty.IsOpen && empty.NextBoundaryUtc is null, "空時間表沒有自動時段");

var core = new ScheduleCoreFake();
using (var coordinator = new ScheduleCoordinator(core))
{
    await coordinator.BootAsync("unused");
    var command = core.LastCommand ?? throw new InvalidOperationException("未送出 ApplyScheduleClock。");
    Assert(command.GetProperty("clock_revision").GetUInt64() == 8,
        "cold boot 從 snapshot clock_revision + 1 發布");
    Assert(command.GetProperty("config_revision").GetUInt64() == 4 &&
           command.GetProperty("schedule_revision").GetUInt64() == 6,
        "clock 綁定 matching definition revisions");
    Assert(command.GetProperty("targets").GetArrayLength() == 1,
        "clock 必須完整覆蓋所有 target");
}

Assert(
    AndroidSchedulePolicy.AtBoundary(canScheduleExact: true) ==
    AlarmBoundaryAction.StartForegroundService,
    "exact boundary 才可自動啟動 FGS");
Assert(
    AndroidSchedulePolicy.AtBoundary(canScheduleExact: false) ==
    AlarmBoundaryAction.NotifyUser,
    "未授 exact boundary 只能通知使用者");
Assert(
    AndroidSchedulePolicy.AfterBoot(hasFutureBoundary: true, activeNow: true) ==
    new BootScheduleAction(true, true, false),
    "reboot 落在 active window：重排未來邊界、通知、絕不由 boot 啟 FGS");
Assert(
    AndroidSchedulePolicy.AfterBoot(hasFutureBoundary: false, activeNow: false) ==
    new BootScheduleAction(false, true, false),
    "已過 boundary：只通知並等待 App 重算");
Assert(
    AndroidSchedulePolicy.WakeMode(atLeastApi31: true, canScheduleExact: false) ==
    "inexact_user_action_required",
    "拒絕 exact access 時 wake_mode 必須 fail closed");

var handshakes = new ForegroundServiceHandshakeState();
var handshakeEffects = new List<string>();
var generation1 = handshakes.Begin();
var generation2 = handshakes.Begin();
Assert(generation2.Generation > generation1.Generation, "新 start 必須取得較新的 generation");
Assert(generation1.Cancellation.IsCancellationRequested, "gen2 必須取消 gen1");
Assert(!handshakes.TryStop(generation1, () => handshakeEffects.Add("gen1-fail-stop")),
    "gen1 fail/catch 不得停止 gen2");
Assert(!handshakes.TryRun(generation1, () => handshakeEffects.Add("gen1-notification")),
    "gen1 callback 不得更新 gen2 notification");
await AssertCanceled(
    () => handshakes.RunAsync(generation1, () =>
    {
        handshakeEffects.Add("gen1-platform-clear");
        return Task.CompletedTask;
    }),
    "gen1 stale platform-block command 必須在派送前取消");
Assert(handshakes.TrySucceed(generation2, () =>
{
    handshakeEffects.Add("gen2-success");
    handshakeEffects.Add("gen2-notification");
}), "gen2 success 必須可提交");
Assert(handshakes.IsReady(generation2), "gen2 success 後必須 ready");
Assert(handshakeEffects.SequenceEqual(["gen2-success", "gen2-notification"]),
    "gen1 fail/platform/notification 副作用必須全數無效，只有 gen2 success 可見");

var staleSuccess = handshakes.Begin();
var currentSuccess = handshakes.Begin();
Assert(!handshakes.TrySucceed(staleSuccess, () => handshakeEffects.Add("stale-success")),
    "stale success 不得標記新版 ready");
Assert(!handshakes.IsReady(currentSuccess), "stale success 後新版仍不得 ready");
Assert(handshakes.TrySucceed(currentSuccess, () => handshakeEffects.Add("current-success")),
    "current success 必須仍可提交");

var destroyed = handshakes.Begin();
handshakes.Destroy();
Assert(destroyed.Cancellation.IsCancellationRequested, "destroy 必須取消 current handshake");
Assert(!handshakes.IsCurrent(destroyed) && !handshakes.IsReady(destroyed),
    "destroy 後舊 lease 不得 current/ready");
Assert(!handshakes.TryRun(destroyed, () => handshakeEffects.Add("destroy-notification")) &&
       !handshakes.TryStop(destroyed, () => handshakeEffects.Add("destroy-stop")),
    "destroy 後排隊中的 notification/stop 必須無效");

var timedOut = handshakes.Begin();
var timeoutStops = 0;
Assert(handshakes.TryStopCurrent(() => timeoutStops++), "timeout 必須可 claim current stop");
Assert(timedOut.Cancellation.IsCancellationRequested, "timeout stop 必須先取消 handshake");
Assert(!handshakes.TryStopCurrent(() => timeoutStops++) && timeoutStops == 1,
    "同一代 stop 副作用只能執行一次");

Console.WriteLine("UiSettings.Check：全部通過");

static void Assert(bool condition, string message)
{
    if (!condition) throw new InvalidOperationException($"設定邏輯檢查失敗：{message}");
}

static async Task AssertCanceled(Func<Task> action, string message)
{
    try
    {
        await action();
    }
    catch (OperationCanceledException)
    {
        return;
    }
    throw new InvalidOperationException($"設定邏輯檢查失敗：{message}");
}

sealed class ScheduleCoreFake : ICore
{
    public event Action<JsonElement>? EventReceived { add { } remove { } }
    public JsonElement? LastCaps => null;
    public JsonElement? LastProviders => null;
    public JsonElement? LastVaultState => null;
    public JsonElement? LastNextClass => null;
    public JsonElement? LastMonitoringSnapshot { get; } = Parse(
        """
        {
          "id": null,
          "event": "MonitoringSnapshot",
          "snapshot": {
            "config_revision": 4,
            "schedule_revision": 6,
            "clock_revision": 7,
            "global_schedule": {
              "monday": [], "tuesday": [], "wednesday": [], "thursday": [],
              "friday": [], "saturday": [], "sunday": []
            },
            "time_zone": { "kind": "device" },
            "targets": [
              {
                "target": { "kind": "account", "account_id": "a" },
                "schedule": { "kind": "disabled" }
              }
            ]
          }
        }
        """);

    public JsonElement? LastCommand { get; private set; }

    public Task BootAsync(string dataDir) => Task.CompletedTask;

    public Task<JsonElement> SendAsync(string cmd, params (string Key, object? Value)[] fields)
    {
        if (cmd == "ApplyScheduleClock") LastCommand = JsonWire.Object(fields);
        return Task.FromResult(JsonWire.Object(
            ("id", 1),
            ("event", "Reply"),
            ("ok", true),
            ("data", null)));
    }

    static JsonElement Parse(string json)
    {
        using var document = JsonDocument.Parse(json);
        return document.RootElement.Clone();
    }
}
