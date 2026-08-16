using System.Diagnostics;
using System.Globalization;
using System.Text.Json;
using TronClass.Interop;

namespace Ui;



sealed record ScheduleTargetDefinition(TargetIdSpec Target, ScheduleBindingSpec Schedule)
{
    public static ScheduleTargetDefinition FromJson(JsonElement value)
    {
        if (value.ValueKind != JsonValueKind.Object) throw new FormatException("target snapshot 必須是物件。");
        return new(
            TargetIdSpec.FromJson(WireShape.Required(value, "target")),
            ScheduleBindingSpec.FromJson(WireShape.Required(value, "schedule")));
    }
}

sealed record ScheduleDefinitionSnapshot(
    ulong ConfigRevision,
    ulong ScheduleRevision,
    ulong? ClockRevision,
    WeeklyScheduleSpec GlobalSchedule,
    TimeZoneSpec TimeZone,
    ScheduleTargetDefinition[] Targets)
{
    public static ScheduleDefinitionSnapshot FromEventOrSnapshot(JsonElement value)
    {
        if (value.TryGetProperty("snapshot", out var nested)) value = nested;
        if (value.ValueKind != JsonValueKind.Object) throw new FormatException("MonitoringSnapshot 必須是物件。");
        var targetsValue = WireShape.Required(value, "targets");
        if (targetsValue.ValueKind != JsonValueKind.Array) throw new FormatException("targets 必須是陣列。");
        var targets = new ScheduleTargetDefinition[targetsValue.GetArrayLength()];
        var index = 0;
        foreach (var target in targetsValue.EnumerateArray()) targets[index++] = ScheduleTargetDefinition.FromJson(target);
        var clock = WireShape.Required(value, "clock_revision");
        return new(
            RequiredUInt64(value, "config_revision"),
            RequiredUInt64(value, "schedule_revision"),
            clock.ValueKind == JsonValueKind.Null ? null : clock.GetUInt64(),
            WeeklyScheduleSpec.FromJson(WireShape.Required(value, "global_schedule")),
            TimeZoneSpec.FromJson(WireShape.Required(value, "time_zone")),
            targets);
    }

    static ulong RequiredUInt64(JsonElement value, string name)
    {
        var property = WireShape.Required(value, name);
        return property.ValueKind == JsonValueKind.Number && property.TryGetUInt64(out var number)
            ? number
            : throw new FormatException($"{name} 必須是非負整數。");
    }
}

/// <summary>
/// 序列化 snapshot definition 與 UTC clock 發布。Cold boot 在送出 matching clock 前保持 core 的
/// automatic targets fail-closed；排程邊界、App resume、裝置時區或系統時間跳動都走同一條重算路徑。
/// </summary>
public sealed class ScheduleCoordinator : IDisposable
{
    readonly ICore _core;
    readonly SemaphoreSlim _publishGate = new(1, 1);
    readonly Timer _boundaryTimer;
    readonly Timer _environmentTimer;
    readonly object _snapshotGate = new();
    JsonElement? _latestSnapshot;
    ulong _nextClockRevision;
    ulong _publishedConfigRevision = ulong.MaxValue;
    ulong _publishedScheduleRevision = ulong.MaxValue;
    string _localZoneId = TimeZoneInfo.Local.Id;
    TimeSpan _localOffset = TimeZoneInfo.Local.GetUtcOffset(DateTimeOffset.Now);
    DateTimeOffset _lastWallClock = DateTimeOffset.UtcNow;
    long _lastMonotonic = Stopwatch.GetTimestamp();
    int _disposed;

    public ScheduleCoordinator(ICore core)
    {
        _core = core;
        _core.EventReceived += OnCoreEvent;
        _boundaryTimer = new Timer(static state => ((ScheduleCoordinator)state!).QueuePublish(force: true), this,
            Timeout.InfiniteTimeSpan, Timeout.InfiniteTimeSpan);
        _environmentTimer = new Timer(static state => ((ScheduleCoordinator)state!).CheckEnvironment(), this,
            TimeSpan.FromMinutes(1), TimeSpan.FromMinutes(1));
    }

    public event Action<string>? Diagnostic;

    public async Task BootAsync(string dataDir)
    {
        await _core.BootAsync(dataDir).ConfigureAwait(false);
        var cached = _core.LastMonitoringSnapshot;
        if (cached is null)
        {
            var reply = await _core.SendAsync("GetMonitoringSnapshot").ConfigureAwait(false);
            if (!ReplyOk(reply)) throw new InvalidOperationException(ReplyError(reply));
            cached = WireShape.Required(WireShape.Required(reply, "data"), "snapshot").Clone();
        }
        RememberSnapshot(cached.Value);
        await PublishLatestAsync(force: true).ConfigureAwait(false);
    }

    public async Task OnResumeAsync()
    {
        var reply = await _core.SendAsync("GetMonitoringSnapshot").ConfigureAwait(false);
        if (!ReplyOk(reply)) throw new InvalidOperationException(ReplyError(reply));
        RememberSnapshot(WireShape.Required(WireShape.Required(reply, "data"), "snapshot"));
        await PublishLatestAsync(force: true).ConfigureAwait(false);
    }

    public Task RecalculateAsync() => PublishLatestAsync(force: true);

    void OnCoreEvent(JsonElement coreEvent)
    {
        if (!coreEvent.TryGetProperty("event", out var eventName) ||
            eventName.GetString() != "MonitoringSnapshot") return;
        try
        {
            var definition = ScheduleDefinitionSnapshot.FromEventOrSnapshot(coreEvent);
            RememberSnapshot(coreEvent);
            if (definition.ConfigRevision != _publishedConfigRevision ||
                definition.ScheduleRevision != _publishedScheduleRevision)
                QueuePublish(force: false);
        }
        catch (Exception error)
        {
            Diagnostic?.Invoke($"排程快照無效：{error.Message}");
        }
    }

    void RememberSnapshot(JsonElement snapshot)
    {
        lock (_snapshotGate) _latestSnapshot = snapshot.Clone();
    }

    void QueuePublish(bool force) => _ = PublishObservedAsync(force);

    async Task PublishObservedAsync(bool force)
    {
        try { await PublishLatestAsync(force).ConfigureAwait(false); }
        catch (Exception error) { Diagnostic?.Invoke($"排程時鐘發布失敗：{error.Message}"); }
    }

    async Task PublishLatestAsync(bool force)
    {
        ObjectDisposedException.ThrowIf(Volatile.Read(ref _disposed) != 0, this);
        await _publishGate.WaitAsync().ConfigureAwait(false);
        try
        {
            JsonElement snapshot;
            lock (_snapshotGate)
            {
                if (_latestSnapshot is null) throw new InvalidOperationException("尚未收到 MonitoringSnapshot。");
                snapshot = _latestSnapshot.Value.Clone();
            }
            var definition = ScheduleDefinitionSnapshot.FromEventOrSnapshot(snapshot);
            if (!force && definition.ConfigRevision == _publishedConfigRevision &&
                definition.ScheduleRevision == _publishedScheduleRevision) return;

            _nextClockRevision = Math.Max(_nextClockRevision, definition.ClockRevision ?? 0);
            if (_nextClockRevision == ulong.MaxValue) throw new InvalidOperationException("clock revision exhausted");
            var clockRevision = ++_nextClockRevision;
            var evaluatedAt = DateTimeOffset.UtcNow;
            var entries = new ScheduleClockEntryWire[definition.Targets.Length];
            DateTimeOffset? earliestBoundary = null;
            for (var index = 0; index < definition.Targets.Length; index++)
            {
                var target = definition.Targets[index];
                var evaluation = ScheduleCalculator.Evaluate(
                    target.Schedule, definition.GlobalSchedule, definition.TimeZone, evaluatedAt);
                entries[index] = new(target.Target, evaluation);
                if (evaluation.NextBoundaryUtc is { } boundary &&
                    (earliestBoundary is null || boundary < earliestBoundary)) earliestBoundary = boundary;
            }

            var reply = await _core.SendAsync(
                "ApplyScheduleClock",
                ("clock_revision", clockRevision),
                ("config_revision", definition.ConfigRevision),
                ("schedule_revision", definition.ScheduleRevision),
                ("evaluated_at_utc", ScheduleClockEntryWire.UtcText(evaluatedAt)),
                ("targets", new ScheduleClockEntriesWire(entries))).ConfigureAwait(false);
            if (!ReplyOk(reply)) throw new InvalidOperationException(ReplyError(reply));

            _publishedConfigRevision = definition.ConfigRevision;
            _publishedScheduleRevision = definition.ScheduleRevision;
            ArmBoundaryTimer(earliestBoundary, evaluatedAt, entries.Any(entry => entry.Evaluation.IsOpen));
        }
        finally
        {
            _publishGate.Release();
        }
    }

    void ArmBoundaryTimer(DateTimeOffset? boundary, DateTimeOffset evaluatedAt, bool activeNow)
    {
#if ANDROID
        var wakeMode = AndroidScheduleAlarms.Schedule(
            global::Android.App.Application.Context,
            boundary,
            activeNow);
        _ = ReportWakeModeAsync(wakeMode);
#endif
        if (boundary is null)
        {
            _boundaryTimer.Change(Timeout.InfiniteTimeSpan, Timeout.InfiniteTimeSpan);
            return;
        }
        var due = boundary.Value - evaluatedAt + TimeSpan.FromMilliseconds(100);
        if (due < TimeSpan.Zero) due = TimeSpan.Zero;
        _boundaryTimer.Change(due, Timeout.InfiniteTimeSpan);
    }

#if ANDROID
    async Task ReportWakeModeAsync(string wakeMode)
    {
        try
        {
            var reply = await _core.SendAsync(
                "ClearPlatformLimit",
                ("reason", $"wake_mode:{wakeMode}")).ConfigureAwait(false);
            if (!ReplyOk(reply)) Diagnostic?.Invoke($"無法更新 Android wake mode：{ReplyError(reply)}");
        }
        catch (Exception error)
        {
            Diagnostic?.Invoke($"無法更新 Android wake mode：{error.Message}");
        }
    }
#endif

    void CheckEnvironment()
    {
        if (Volatile.Read(ref _disposed) != 0) return;
        var now = DateTimeOffset.UtcNow;
        var ticks = Stopwatch.GetTimestamp();
        var elapsed = TimeSpan.FromSeconds((ticks - _lastMonotonic) / (double)Stopwatch.Frequency);
        var wallElapsed = now - _lastWallClock;
        var zoneId = TimeZoneInfo.Local.Id;
        var offset = TimeZoneInfo.Local.GetUtcOffset(now);
        var changed = zoneId != _localZoneId || offset != _localOffset ||
                      Math.Abs((wallElapsed - elapsed).TotalSeconds) > 5;
        _localZoneId = zoneId;
        _localOffset = offset;
        _lastWallClock = now;
        _lastMonotonic = ticks;
        if (changed) QueuePublish(force: true);
    }

    static bool ReplyOk(JsonElement reply) =>
        reply.TryGetProperty("ok", out var ok) && ok.ValueKind == JsonValueKind.True;

    static string ReplyError(JsonElement reply) =>
        reply.TryGetProperty("error", out var error) && error.ValueKind == JsonValueKind.String
            ? error.GetString() ?? "核心拒絕排程時鐘。"
            : "核心拒絕排程時鐘。";

    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        _core.EventReceived -= OnCoreEvent;
        _boundaryTimer.Dispose();
        _environmentTimer.Dispose();
        _publishGate.Dispose();
    }
}
