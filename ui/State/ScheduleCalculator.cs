using System.Globalization;
using System.Text.Json;
using TronClass.Interop;

namespace Ui;

public enum ScheduleBindingKind
{
    Disabled,
    InheritGlobal,
    Custom,
}
public sealed record TargetIdSpec(string Kind, string Id) : IWireValue
{
    public static TargetIdSpec FromJson(JsonElement value)
    {
        if (value.ValueKind != JsonValueKind.Object) throw new FormatException("target 必須是物件。");
        var kind = WireShape.RequiredString(value, "kind");
        return kind switch
        {
            "account" => Parse(value, kind, "account_id"),
            "group" => Parse(value, kind, "group_id"),
            _ => throw new FormatException($"未知 target kind：{kind}"),
        };
    }

    static TargetIdSpec Parse(JsonElement value, string kind, string idName)
    {
        WireShape.RequireObject(value, "target", "kind", idName);
        var id = WireShape.RequiredString(value, idName);
        if (string.IsNullOrWhiteSpace(id)) throw new FormatException($"{idName} 不得為空。");
        return new(kind, id);
    }

    public void WriteTo(Utf8JsonWriter writer)
    {
        writer.WriteStartObject();
        writer.WriteString("kind", Kind);
        writer.WriteString(Kind switch
        {
            "account" => "account_id",
            "group" => "group_id",
            _ => throw new InvalidOperationException($"未知 target kind：{Kind}"),
        }, Id);
        writer.WriteEndObject();
    }
}
public sealed record ScheduleClockEntryWire(TargetIdSpec Target, ScheduleEvaluation Evaluation) : IWireValue
{
    public void WriteTo(Utf8JsonWriter writer)
    {
        writer.WriteStartObject();
        writer.WritePropertyName("target");
        Target.WriteTo(writer);
        writer.WriteBoolean("is_open", Evaluation.IsOpen);
        WriteNullableString(writer, "window_key", Evaluation.WindowKey);
        WriteNullableInstant(writer, "current_window_start_utc", Evaluation.CurrentWindowStartUtc);
        WriteNullableInstant(writer, "next_boundary_utc", Evaluation.NextBoundaryUtc);
        if (Evaluation.NextIsOpen is { } nextIsOpen) writer.WriteBoolean("next_is_open", nextIsOpen);
        else writer.WriteNull("next_is_open");
        WriteNullableString(writer, "clock_error", Evaluation.ClockError);
        writer.WriteEndObject();
    }

    static void WriteNullableInstant(Utf8JsonWriter writer, string name, DateTimeOffset? value) =>
        WriteNullableString(writer, name, value is null ? null : UtcText(value.Value));

    static void WriteNullableString(Utf8JsonWriter writer, string name, string? value)
    {
        if (value is null) writer.WriteNull(name);
        else writer.WriteString(name, value);
    }

    internal static string UtcText(DateTimeOffset value) => value.ToUniversalTime()
        .ToString("yyyy-MM-dd'T'HH:mm:ss.fffffff'Z'", CultureInfo.InvariantCulture);
}

public sealed class ScheduleClockEntriesWire(IReadOnlyList<ScheduleClockEntryWire> entries) : IWireValue
{
    public void WriteTo(Utf8JsonWriter writer)
    {
        writer.WriteStartArray();
        foreach (var entry in entries) entry.WriteTo(writer);
        writer.WriteEndArray();
    }
}

public sealed record TimeWindowSpec(int StartMinute, int EndMinute) : IWireValue
{
    public static TimeWindowSpec FromJson(JsonElement value)
    {
        WireShape.RequireObject(value, "time window", "start_minute", "end_minute");
        return new(
            WireShape.RequiredInt(value, "start_minute"),
            WireShape.RequiredInt(value, "end_minute"));
    }

    public void WriteTo(Utf8JsonWriter writer)
    {
        writer.WriteStartObject();
        writer.WriteNumber("start_minute", StartMinute);
        writer.WriteNumber("end_minute", EndMinute);
        writer.WriteEndObject();
    }
}

public sealed class WeeklyScheduleSpec : IWireValue
{
    static readonly TimeWindowSpec[] NoWindows = [];

    public WeeklyScheduleSpec(
        TimeWindowSpec[]? monday = null,
        TimeWindowSpec[]? tuesday = null,
        TimeWindowSpec[]? wednesday = null,
        TimeWindowSpec[]? thursday = null,
        TimeWindowSpec[]? friday = null,
        TimeWindowSpec[]? saturday = null,
        TimeWindowSpec[]? sunday = null)
    {
        Monday = monday ?? NoWindows;
        Tuesday = tuesday ?? NoWindows;
        Wednesday = wednesday ?? NoWindows;
        Thursday = thursday ?? NoWindows;
        Friday = friday ?? NoWindows;
        Saturday = saturday ?? NoWindows;
        Sunday = sunday ?? NoWindows;
    }

    public TimeWindowSpec[] Monday { get; }
    public TimeWindowSpec[] Tuesday { get; }
    public TimeWindowSpec[] Wednesday { get; }
    public TimeWindowSpec[] Thursday { get; }
    public TimeWindowSpec[] Friday { get; }
    public TimeWindowSpec[] Saturday { get; }
    public TimeWindowSpec[] Sunday { get; }

    public bool IsEmpty => Monday.Length == 0 && Tuesday.Length == 0 && Wednesday.Length == 0 &&
                           Thursday.Length == 0 && Friday.Length == 0 && Saturday.Length == 0 &&
                           Sunday.Length == 0;

    public ReadOnlySpan<TimeWindowSpec> Day(int mondayBasedDay) => mondayBasedDay switch
    {
        0 => Monday,
        1 => Tuesday,
        2 => Wednesday,
        3 => Thursday,
        4 => Friday,
        5 => Saturday,
        6 => Sunday,
        _ => throw new ArgumentOutOfRangeException(nameof(mondayBasedDay)),
    };

    public void Validate()
    {
        var intervals = new List<(int Start, int End)>();
        for (var day = 0; day < 7; day++)
        {
            foreach (var window in Day(day))
            {
                if (window.StartMinute is < 0 or >= 1440)
                    throw new FormatException("start_minute 必須介於 0 與 1439。");
                if (window.EndMinute is < 0 or > 1440)
                    throw new FormatException("end_minute 必須介於 0 與 1440。");
                if (window.StartMinute == window.EndMinute)
                    throw new FormatException("時間窗不得為空。");

                var start = day * 1440 + window.StartMinute;
                var end = day * 1440 + window.EndMinute;
                if (window.StartMinute > window.EndMinute) end += 1440;
                if (end <= 10080)
                {
                    intervals.Add((start, end));
                }
                else
                {
                    intervals.Add((start, 10080));
                    intervals.Add((0, end - 10080));
                }
            }
        }
        intervals.Sort(static (left, right) => left.Start.CompareTo(right.Start));
        for (var index = 1; index < intervals.Count; index++)
            if (intervals[index - 1].End > intervals[index].Start)
                throw new FormatException("每週時間窗不得重疊。");
    }

    public static WeeklyScheduleSpec FromJson(JsonElement value)
    {
        WireShape.RequireObject(value, "weekly schedule",
            "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday");
        var schedule = new WeeklyScheduleSpec(
            ParseDay(value, "monday"), ParseDay(value, "tuesday"),
            ParseDay(value, "wednesday"), ParseDay(value, "thursday"),
            ParseDay(value, "friday"), ParseDay(value, "saturday"), ParseDay(value, "sunday"));
        schedule.Validate();
        return schedule;
    }

    static TimeWindowSpec[] ParseDay(JsonElement value, string name)
    {
        var array = WireShape.Required(value, name);
        if (array.ValueKind != JsonValueKind.Array) throw new FormatException($"{name} 必須是陣列。");
        var result = new TimeWindowSpec[array.GetArrayLength()];
        var index = 0;
        foreach (var item in array.EnumerateArray()) result[index++] = TimeWindowSpec.FromJson(item);
        return result;
    }

    public void WriteTo(Utf8JsonWriter writer)
    {
        writer.WriteStartObject();
        WriteDay(writer, "monday", Monday);
        WriteDay(writer, "tuesday", Tuesday);
        WriteDay(writer, "wednesday", Wednesday);
        WriteDay(writer, "thursday", Thursday);
        WriteDay(writer, "friday", Friday);
        WriteDay(writer, "saturday", Saturday);
        WriteDay(writer, "sunday", Sunday);
        writer.WriteEndObject();
    }

    static void WriteDay(Utf8JsonWriter writer, string name, TimeWindowSpec[] windows)
    {
        writer.WriteStartArray(name);
        foreach (var window in windows) window.WriteTo(writer);
        writer.WriteEndArray();
    }
}

public sealed record ScheduleBindingSpec(ScheduleBindingKind Kind, WeeklyScheduleSpec? Weekly = null) : IWireValue
{
    public static ScheduleBindingSpec Disabled { get; } = new(ScheduleBindingKind.Disabled);
    public static ScheduleBindingSpec InheritGlobal { get; } = new(ScheduleBindingKind.InheritGlobal);
    public static ScheduleBindingSpec Custom(WeeklyScheduleSpec weekly) => new(ScheduleBindingKind.Custom, weekly);

    public static ScheduleBindingSpec FromJson(JsonElement value)
    {
        if (value.ValueKind != JsonValueKind.Object) throw new FormatException("schedule 必須是物件。");
        var kind = WireShape.RequiredString(value, "kind");
        return kind switch
        {
            "disabled" => RequireSimple(value, ScheduleBindingKind.Disabled),
            "inherit_global" => RequireSimple(value, ScheduleBindingKind.InheritGlobal),
            "custom" => ParseCustom(value),
            _ => throw new FormatException($"未知 schedule kind：{kind}"),
        };
    }

    static ScheduleBindingSpec RequireSimple(JsonElement value, ScheduleBindingKind kind)
    {
        WireShape.RequireObject(value, "schedule", "kind");
        return new(kind);
    }

    static ScheduleBindingSpec ParseCustom(JsonElement value)
    {
        WireShape.RequireObject(value, "custom schedule", "kind", "weekly");
        return Custom(WeeklyScheduleSpec.FromJson(WireShape.Required(value, "weekly")));
    }

    public void WriteTo(Utf8JsonWriter writer)
    {
        writer.WriteStartObject();
        writer.WriteString("kind", Kind switch
        {
            ScheduleBindingKind.Disabled => "disabled",
            ScheduleBindingKind.InheritGlobal => "inherit_global",
            ScheduleBindingKind.Custom => "custom",
            _ => throw new InvalidOperationException("未知 schedule kind。"),
        });
        if (Kind == ScheduleBindingKind.Custom)
        {
            writer.WritePropertyName("weekly");
            (Weekly ?? throw new InvalidOperationException("Custom schedule 缺少 weekly。")).WriteTo(writer);
        }
        writer.WriteEndObject();
    }
}

public sealed record TimeZoneSpec(bool IsDevice, string? IanaId = null) : IWireValue
{
    public static TimeZoneSpec Device { get; } = new(true);
    public static TimeZoneSpec Named(string ianaId) => new(false, ianaId);

    public static TimeZoneSpec FromJson(JsonElement value)
    {
        if (value.ValueKind != JsonValueKind.Object) throw new FormatException("time_zone 必須是物件。");
        return WireShape.RequiredString(value, "kind") switch
        {
            "device" => ParseDevice(value),
            "named" => ParseNamed(value),
            var kind => throw new FormatException($"未知 time_zone kind：{kind}"),
        };
    }

    static TimeZoneSpec ParseDevice(JsonElement value)
    {
        WireShape.RequireObject(value, "device time zone", "kind");
        return Device;
    }

    static TimeZoneSpec ParseNamed(JsonElement value)
    {
        WireShape.RequireObject(value, "named time zone", "kind", "iana_id");
        var id = WireShape.RequiredString(value, "iana_id");
        if (string.IsNullOrWhiteSpace(id)) throw new FormatException("iana_id 不得為空。");
        return Named(id);
    }

    public void WriteTo(Utf8JsonWriter writer)
    {
        writer.WriteStartObject();
        writer.WriteString("kind", IsDevice ? "device" : "named");
        if (!IsDevice) writer.WriteString("iana_id", IanaId ?? throw new InvalidOperationException("缺少 IANA ID。"));
        writer.WriteEndObject();
    }
}

public sealed record ScheduleEvaluation(
    bool IsOpen,
    string? WindowKey,
    DateTimeOffset? CurrentWindowStartUtc,
    DateTimeOffset? NextBoundaryUtc,
    bool? NextIsOpen,
    string? ClockError)
{
    public static ScheduleEvaluation Closed(string? error = null) => new(false, null, null, null, null, error);
}

/// <summary>具名時區與 DST 的唯一排程時鐘。Rust 只接收此處算好的 UTC 邊界。</summary>
public static class ScheduleCalculator
{
    sealed record Occurrence(DateTime LocalDate, int Day, int Window, DateTimeOffset Start, DateTimeOffset End);

    public static ScheduleEvaluation Evaluate(
        ScheduleBindingSpec binding,
        WeeklyScheduleSpec globalSchedule,
        TimeZoneSpec timeZone,
        DateTimeOffset evaluatedAtUtc)
    {
        var weekly = binding.Kind switch
        {
            ScheduleBindingKind.Disabled => null,
            ScheduleBindingKind.InheritGlobal => globalSchedule,
            ScheduleBindingKind.Custom => binding.Weekly ?? throw new FormatException("Custom schedule 缺少 weekly。"),
            _ => throw new FormatException("未知 schedule kind。"),
        };
        if (weekly is null || weekly.IsEmpty) return ScheduleEvaluation.Closed();
        if (!TryResolveTimeZone(timeZone, out var zone))
            return ScheduleEvaluation.Closed("invalid_time_zone");
        return Evaluate(weekly, zone!, evaluatedAtUtc);
    }

    public static ScheduleEvaluation Evaluate(
        WeeklyScheduleSpec weekly,
        TimeZoneInfo timeZone,
        DateTimeOffset evaluatedAtUtc)
    {
        weekly.Validate();
        var now = evaluatedAtUtc.ToUniversalTime();
        var localDate = TimeZoneInfo.ConvertTime(now, timeZone).Date;
        var occurrences = new List<Occurrence>(32);

        // One prior week covers a currently-open cross-midnight window; two future weeks guarantee
        // a next start even when the only weekly window just ended.
        for (var offset = -8; offset <= 14; offset++)
        {
            var date = localDate.AddDays(offset);
            var day = MondayBasedDay(date.DayOfWeek);
            var windows = weekly.Day(day);
            for (var index = 0; index < windows.Length; index++)
            {
                var window = windows[index];
                var startLocal = AtMinute(date, window.StartMinute);
                var endDate = window.EndMinute == 1440 || window.StartMinute > window.EndMinute
                    ? date.AddDays(1)
                    : date;
                var endMinute = window.EndMinute == 1440 ? 0 : window.EndMinute;
                var endLocal = AtMinute(endDate, endMinute);
                var start = ResolveBoundary(timeZone, startLocal, startBoundary: true);
                var end = ResolveBoundary(timeZone, endLocal, startBoundary: false);
                if (end > start) occurrences.Add(new(date, day, index, start, end));
            }
        }
        occurrences.Sort(static (left, right) => left.Start.CompareTo(right.Start));

        foreach (var occurrence in occurrences)
        {
            if (occurrence.Start <= now && now < occurrence.End)
            {
                return new(
                    true,
                    WindowKey(occurrence),
                    occurrence.Start,
                    occurrence.End,
                    false,
                    null);
            }
        }
        var next = occurrences.FirstOrDefault(occurrence => occurrence.Start > now);
        return next is null
            ? ScheduleEvaluation.Closed()
            : new(false, null, null, next.Start, true, null);
    }

    public static bool TryResolveTimeZone(TimeZoneSpec spec, out TimeZoneInfo? timeZone)
    {
        if (spec.IsDevice)
        {
            timeZone = TimeZoneInfo.Local;
            return true;
        }
        try
        {
            timeZone = TimeZoneInfo.FindSystemTimeZoneById(spec.IanaId ?? "");
            return true;
        }
        catch (TimeZoneNotFoundException)
        {
            timeZone = null;
            return false;
        }
        catch (InvalidTimeZoneException)
        {
            timeZone = null;
            return false;
        }
    }

    public static string? NormalizeIanaId(string systemTimeZoneId)
    {
        if (string.IsNullOrWhiteSpace(systemTimeZoneId)) return null;
        if (TimeZoneInfo.TryConvertWindowsIdToIanaId(systemTimeZoneId, out var iana)) return iana;
        try
        {
            _ = TimeZoneInfo.FindSystemTimeZoneById(systemTimeZoneId);
            return systemTimeZoneId;
        }
        catch (TimeZoneNotFoundException) { return null; }
        catch (InvalidTimeZoneException) { return null; }
    }

    static DateTime AtMinute(DateTime date, int minute) =>
        DateTime.SpecifyKind(date.Date.AddMinutes(minute), DateTimeKind.Unspecified);

    static DateTimeOffset ResolveBoundary(TimeZoneInfo zone, DateTime local, bool startBoundary)
    {
        for (var shifted = 0; zone.IsInvalidTime(local); shifted++)
        {
            if (shifted >= 180) throw new InvalidTimeZoneException("DST gap 超過三小時。");
            local = local.AddMinutes(1);
        }
        if (zone.IsAmbiguousTime(local))
        {
            var candidates = zone.GetAmbiguousTimeOffsets(local)
                .Select(offset => new DateTimeOffset(local, offset).ToUniversalTime())
                .OrderBy(value => value)
                .ToArray();
            return startBoundary ? candidates[0] : candidates[^1];
        }
        return new DateTimeOffset(TimeZoneInfo.ConvertTimeToUtc(local, zone), TimeSpan.Zero);
    }

    static int MondayBasedDay(DayOfWeek day) => day == DayOfWeek.Sunday ? 6 : (int)day - 1;

    static string WindowKey(Occurrence occurrence) => string.Create(
        CultureInfo.InvariantCulture,
        $"{occurrence.LocalDate:yyyy-MM-dd}/{occurrence.Day}/{occurrence.Window}/{occurrence.Start:yyyyMMddTHHmmss'Z'}");
}

internal static class WireShape
{
    public static JsonElement Required(JsonElement value, string name)
    {
        if (!value.TryGetProperty(name, out var property)) throw new FormatException($"缺少 {name}。");
        return property;
    }

    public static string RequiredString(JsonElement value, string name)
    {
        var property = Required(value, name);
        return property.ValueKind == JsonValueKind.String
            ? property.GetString()!
            : throw new FormatException($"{name} 必須是字串。");
    }

    public static int RequiredInt(JsonElement value, string name)
    {
        var property = Required(value, name);
        return property.ValueKind == JsonValueKind.Number && property.TryGetInt32(out var number)
            ? number
            : throw new FormatException($"{name} 必須是整數。");
    }

    public static void RequireObject(JsonElement value, string label, params string[] fields)
    {
        if (value.ValueKind != JsonValueKind.Object) throw new FormatException($"{label} 必須是物件。");
        var allowed = new HashSet<string>(fields, StringComparer.Ordinal);
        foreach (var property in value.EnumerateObject())
            if (!allowed.Remove(property.Name)) throw new FormatException($"{label} 含未知欄位 {property.Name}。");
        if (allowed.Count != 0) throw new FormatException($"{label} 缺少欄位 {allowed.First()}。");
    }
}
