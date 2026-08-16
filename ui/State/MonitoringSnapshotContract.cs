using System.Globalization;
using System.Text.Json;
using TronClass.Interop;

namespace Ui;

public sealed record WireErrorContract(string Code, string Message)
{
    public static WireErrorContract? Optional(JsonElement parent, string name)
    {
        var value = ContractJson.Required(parent, name);
        if (value.ValueKind == JsonValueKind.Null) return null;
        WireShape.RequireObject(value, name, "code", "message");
        return new(ContractJson.String(value, "code"), ContractJson.String(value, "message"));
    }
}

public sealed record NoticeContract(string Code, string Message, string? BackupPath)
{
    public static NoticeContract? Optional(JsonElement parent)
    {
        var value = ContractJson.Required(parent, "config_notice");
        if (value.ValueKind == JsonValueKind.Null) return null;
        WireShape.RequireObject(value, "config_notice", "code", "message", "backup_path");
        return new(
            ContractJson.String(value, "code"),
            ContractJson.String(value, "message"),
            ContractJson.NullableString(value, "backup_path"));
    }
}

public sealed record PlatformBlockContract(string Reason, string ObservedAtUtc)
{
    public static PlatformBlockContract? Optional(JsonElement parent)
    {
        var value = ContractJson.Required(parent, "platform_block");
        if (value.ValueKind == JsonValueKind.Null) return null;
        WireShape.RequireObject(value, "platform_block", "reason", "observed_at_utc");
        return new(
            ContractJson.String(value, "reason"),
            ContractJson.Utc(value, "observed_at_utc"));
    }
}

public sealed record DetectorSelectionSpec(bool IsAuto, string? AccountId = null) : IWireValue
{
    public static DetectorSelectionSpec Auto { get; } = new(true);
    public static DetectorSelectionSpec Preferred(string accountId) => new(false, accountId);

    public static DetectorSelectionSpec FromJson(JsonElement value)
    {
        var kind = ContractJson.String(value, "kind");
        if (kind == "auto")
        {
            WireShape.RequireObject(value, "detector_selection", "kind");
            return Auto;
        }
        if (kind == "preferred")
        {
            WireShape.RequireObject(value, "detector_selection", "kind", "account_id");
            return Preferred(ContractJson.NonEmpty(value, "account_id"));
        }
        throw new FormatException($"未知 detector kind：{kind}");
    }

    public void WriteTo(Utf8JsonWriter writer)
    {
        writer.WriteStartObject();
        writer.WriteString("kind", IsAuto ? "auto" : "preferred");
        if (!IsAuto) writer.WriteString("account_id", AccountId ?? throw new InvalidOperationException("缺少 detector account_id。"));
        writer.WriteEndObject();
    }
}

public sealed record TargetRefContract(TargetIdSpec Target, string Name)
{
    public static TargetRefContract FromJson(JsonElement value)
    {
        WireShape.RequireObject(value, "target_ref", "target", "name");
        return new(
            TargetIdSpec.FromJson(ContractJson.Required(value, "target")),
            ContractJson.String(value, "name"));
    }
}

public sealed record AccountSnapshotContract(
    string AccountId,
    string Label,
    string SchoolRef,
    string Username,
    string Role,
    string? TeacherCourseId,
    string LoginState,
    WireErrorContract? LoginError,
    bool LoginInFlight,
    TargetRefContract[] InUseTargets)
{
    public static AccountSnapshotContract FromJson(JsonElement value)
    {
        WireShape.RequireObject(value, "account", "account_id", "label", "school_ref", "username",
            "role", "teacher_course_id", "login_state", "login_error", "login_in_flight", "in_use_targets");
        var role = ContractJson.OneOf(value, "role", "student", "teacher");
        var teacherCourseId = ContractJson.NullableString(value, "teacher_course_id");
        if (role == "student" && teacherCourseId is not null)
            throw new FormatException("學生 account 不得帶 teacher_course_id。");
        return new(
            ContractJson.NonEmpty(value, "account_id"),
            ContractJson.String(value, "label"),
            ContractJson.String(value, "school_ref"),
            ContractJson.String(value, "username"),
            role,
            teacherCourseId,
            ContractJson.OneOf(value, "login_state", "stored", "logging_in", "online", "error"),
            WireErrorContract.Optional(value, "login_error"),
            ContractJson.Bool(value, "login_in_flight"),
            ContractJson.Array(value, "in_use_targets", TargetRefContract.FromJson));
    }
}

public sealed record ManualOverrideContract(bool ForceOpen, string? ExpiresAtUtc)
{
    public static ManualOverrideContract? Optional(JsonElement parent)
    {
        var value = ContractJson.Required(parent, "manual_override");
        if (value.ValueKind == JsonValueKind.Null) return null;
        WireShape.RequireObject(value, "manual_override", "force_open", "expires_at_utc");
        var expires = ContractJson.NullableString(value, "expires_at_utc");
        if (expires is not null) ContractJson.ValidateUtc(expires, "expires_at_utc");
        return new(ContractJson.Bool(value, "force_open"), expires);
    }
}

public sealed record DetectorContract(string AccountId, bool IsFallback)
{
    public static DetectorContract? Optional(JsonElement parent)
    {
        var value = ContractJson.Required(parent, "detector");
        if (value.ValueKind == JsonValueKind.Null) return null;
        WireShape.RequireObject(value, "detector", "account_id", "is_fallback");
        return new(ContractJson.NonEmpty(value, "account_id"), ContractJson.Bool(value, "is_fallback"));
    }
}

public sealed record GroupDefinitionContract(
    string[] MemberAccountIds,
    string[] CourseIds,
    DetectorSelectionSpec DetectorSelection)
{
    public static GroupDefinitionContract? Optional(JsonElement parent)
    {
        var value = ContractJson.Required(parent, "group_definition");
        if (value.ValueKind == JsonValueKind.Null) return null;
        WireShape.RequireObject(value, "group_definition", "member_account_ids", "course_ids", "detector_selection");
        return new(
            ContractJson.StringArray(value, "member_account_ids"),
            ContractJson.StringArray(value, "course_ids"),
            DetectorSelectionSpec.FromJson(ContractJson.Required(value, "detector_selection")));
    }
}

public sealed record CourseContract(string CourseId, string Name)
{
    public static CourseContract FromJson(JsonElement value)
    {
        WireShape.RequireObject(value, "course", "course_id", "name");
        return new(ContractJson.NonEmpty(value, "course_id"), ContractJson.String(value, "name"));
    }
}

public sealed record AccountResultContract(
    string AccountId,
    string Phase,
    string? ActivityKind,
    string? CourseName,
    string? UpdatedAtUtc,
    WireErrorContract? Error)
{
    public static AccountResultContract FromJson(JsonElement value)
    {
        WireShape.RequireObject(value, "account_result", "account_id", "phase", "activity_kind",
            "course_name", "updated_at_utc", "error");
        var updated = ContractJson.NullableString(value, "updated_at_utc");
        if (updated is not null) ContractJson.ValidateUtc(updated, "updated_at_utc");
        return new(
            ContractJson.NonEmpty(value, "account_id"),
            ContractJson.OneOf(value, "phase", "idle", "pending", "authorized", "succeeded", "failed", "unknown_after_restart"),
            ContractJson.NullableString(value, "activity_kind"),
            ContractJson.NullableString(value, "course_name"),
            updated,
            WireErrorContract.Optional(value, "error"));
    }
}

public sealed record TargetSnapshotContract(
    TargetIdSpec Target,
    string Name,
    string RuntimeState,
    ScheduleBindingSpec Schedule,
    bool ScheduleOpen,
    string? NextBoundaryUtc,
    ManualOverrideContract? ManualOverride,
    DetectorContract? Detector,
    GroupDefinitionContract? GroupDefinition,
    CourseContract[] Courses,
    string[] InUseAccountIds,
    AccountResultContract[] AccountResults,
    bool CanStart,
    bool CanStop,
    bool CanEditSchedule,
    string? DisabledReason,
    WireErrorContract? Error)
{
    public static TargetSnapshotContract FromJson(JsonElement value)
    {
        WireShape.RequireObject(value, "target_snapshot", "target", "name", "runtime_state", "schedule",
            "schedule_open", "next_boundary_utc", "manual_override", "detector", "group_definition",
            "courses", "in_use_account_ids", "account_results", "can_start", "can_stop",
            "can_edit_schedule", "disabled_reason", "error");
        var next = ContractJson.NullableString(value, "next_boundary_utc");
        if (next is not null) ContractJson.ValidateUtc(next, "next_boundary_utc");
        return new(
            TargetIdSpec.FromJson(ContractJson.Required(value, "target")),
            ContractJson.String(value, "name"),
            ContractJson.OneOf(value, "runtime_state", "scheduled_off", "manual_off", "starting",
                "monitoring", "stopping", "suppressed_by_group", "platform_blocked", "error"),
            ScheduleBindingSpec.FromJson(ContractJson.Required(value, "schedule")),
            ContractJson.Bool(value, "schedule_open"),
            next,
            ManualOverrideContract.Optional(value),
            DetectorContract.Optional(value),
            GroupDefinitionContract.Optional(value),
            ContractJson.Array(value, "courses", CourseContract.FromJson),
            ContractJson.StringArray(value, "in_use_account_ids"),
            ContractJson.Array(value, "account_results", AccountResultContract.FromJson),
            ContractJson.Bool(value, "can_start"),
            ContractJson.Bool(value, "can_stop"),
            ContractJson.Bool(value, "can_edit_schedule"),
            ContractJson.NullableString(value, "disabled_reason"),
            WireErrorContract.Optional(value, "error"));
    }
}

public sealed record MergePromptContract(
    string ComponentId,
    string[] GroupIds,
    string Coverage,
    string? DetectorAccountId,
    uint DetectorCount,
    string? Warning,
    bool Acknowledged)
{
    public static MergePromptContract FromJson(JsonElement value)
    {
        WireShape.RequireObject(value, "merge_prompt", "component_id", "group_ids", "coverage",
            "detector_account_id", "detector_count", "warning", "acknowledged");
        var detectorCount = ContractJson.UInt64(value, "detector_count");
        if (detectorCount == 0 || detectorCount > uint.MaxValue)
            throw new FormatException("detector_count 超出範圍。");
        return new(
            ContractJson.NonEmpty(value, "component_id"),
            ContractJson.StringArray(value, "group_ids"),
            ContractJson.OneOf(value, "coverage", "single_detector", "multiple_detectors_required"),
            ContractJson.NullableString(value, "detector_account_id"),
            (uint)detectorCount,
            ContractJson.NullableString(value, "warning"),
            ContractJson.Bool(value, "acknowledged"));
    }
}

public sealed record MonitoringSnapshotContract(
    byte SchemaVersion,
    ulong ConfigRevision,
    ulong ScheduleRevision,
    ulong PlanRevision,
    ulong? ClockRevision,
    string SessionState,
    bool AllSuspended,
    PlatformBlockContract? PlatformBlock,
    bool CanStopAll,
    bool CanResume,
    string? GlobalDisabledReason,
    WeeklyScheduleSpec GlobalSchedule,
    TimeZoneSpec TimeZone,
    string WakeMode,
    AccountSnapshotContract[] Accounts,
    TargetSnapshotContract[] Targets,
    MergePromptContract[] MergePrompts,
    NoticeContract? ConfigNotice)
{
    public static MonitoringSnapshotContract Parse(JsonElement eventOrSnapshot)
    {
        var value = eventOrSnapshot;
        if (value.TryGetProperty("snapshot", out var nested)) value = nested;
        WireShape.RequireObject(value, "MonitoringSnapshot", "schema_version", "config_revision",
            "schedule_revision", "plan_revision", "clock_revision", "session_state", "all_suspended",
            "platform_block", "can_stop_all", "can_resume", "global_disabled_reason", "global_schedule",
            "time_zone", "wake_mode", "accounts", "targets", "merge_prompts", "config_notice");
        var schema = ContractJson.UInt64(value, "schema_version");
        if (schema != 1) throw new FormatException($"不支援 MonitoringSnapshot schema {schema}。");
        var clock = ContractJson.Required(value, "clock_revision");
        if (clock.ValueKind != JsonValueKind.Null &&
            (clock.ValueKind != JsonValueKind.Number || !clock.TryGetUInt64(out _)))
            throw new FormatException("clock_revision 必須是 null 或非負整數。");
        return new(
            1,
            ContractJson.UInt64(value, "config_revision"),
            ContractJson.UInt64(value, "schedule_revision"),
            ContractJson.UInt64(value, "plan_revision"),
            clock.ValueKind == JsonValueKind.Null ? null : clock.GetUInt64(),
            ContractJson.OneOf(value, "session_state", "idle", "starting", "running", "stopping", "platform_blocked", "error"),
            ContractJson.Bool(value, "all_suspended"),
            PlatformBlockContract.Optional(value),
            ContractJson.Bool(value, "can_stop_all"),
            ContractJson.Bool(value, "can_resume"),
            ContractJson.NullableString(value, "global_disabled_reason"),
            WeeklyScheduleSpec.FromJson(ContractJson.Required(value, "global_schedule")),
            TimeZoneSpec.FromJson(ContractJson.Required(value, "time_zone")),
            ContractJson.OneOf(value, "wake_mode", "foreground_only", "exact", "inexact_user_action_required", "unavailable"),
            ContractJson.Array(value, "accounts", AccountSnapshotContract.FromJson),
            ContractJson.Array(value, "targets", TargetSnapshotContract.FromJson),
            ContractJson.Array(value, "merge_prompts", MergePromptContract.FromJson),
            NoticeContract.Optional(value));
    }

    public AccountSnapshotContract? Account(string accountId) =>
        Accounts.FirstOrDefault(account => account.AccountId == accountId);

    public TargetSnapshotContract? Target(TargetIdSpec target) =>
        Targets.FirstOrDefault(candidate => candidate.Target == target);
}

public sealed record GroupInputWire(
    string Name,
    string[] MemberAccountIds,
    string[] CourseIds,
    DetectorSelectionSpec Detector,
    ScheduleBindingSpec Schedule) : IWireValue
{
    public void WriteTo(Utf8JsonWriter writer)
    {
        writer.WriteStartObject();
        writer.WriteString("name", Name.Trim());
        ContractJson.WriteStrings(writer, "member_account_ids", MemberAccountIds);
        ContractJson.WriteStrings(writer, "course_ids", CourseIds);
        writer.WritePropertyName("detector");
        Detector.WriteTo(writer);
        writer.WritePropertyName("schedule");
        Schedule.WriteTo(writer);
        writer.WriteEndObject();
    }
}

static class ContractJson
{
    public static JsonElement Required(JsonElement value, string name) => WireShape.Required(value, name);

    public static string String(JsonElement value, string name) => WireShape.RequiredString(value, name);

    public static string NonEmpty(JsonElement value, string name)
    {
        var text = String(value, name);
        return !string.IsNullOrWhiteSpace(text)
            ? text
            : throw new FormatException($"{name} 不得為空。");
    }

    public static string OneOf(JsonElement value, string name, params string[] allowed)
    {
        var text = String(value, name);
        return allowed.Contains(text, StringComparer.Ordinal)
            ? text
            : throw new FormatException($"未知 {name}：{text}");
    }

    public static bool Bool(JsonElement value, string name)
    {
        var property = Required(value, name);
        return property.ValueKind switch
        {
            JsonValueKind.True => true,
            JsonValueKind.False => false,
            _ => throw new FormatException($"{name} 必須是布林值。"),
        };
    }

    public static ulong UInt64(JsonElement value, string name)
    {
        var property = Required(value, name);
        return property.ValueKind == JsonValueKind.Number && property.TryGetUInt64(out var number)
            ? number
            : throw new FormatException($"{name} 必須是非負整數。");
    }

    public static string? NullableString(JsonElement value, string name)
    {
        var property = Required(value, name);
        return property.ValueKind switch
        {
            JsonValueKind.Null => null,
            JsonValueKind.String => property.GetString(),
            _ => throw new FormatException($"{name} 必須是字串或 null。"),
        };
    }

    public static string Utc(JsonElement value, string name)
    {
        var text = String(value, name);
        ValidateUtc(text, name);
        return text;
    }

    public static void ValidateUtc(string text, string name)
    {
        if (!text.EndsWith('Z') ||
            !DateTimeOffset.TryParseExact(
                text,
                ["yyyy-MM-dd'T'HH:mm:ss'Z'", "yyyy-MM-dd'T'HH:mm:ss.FFFFFFF'Z'"],
                CultureInfo.InvariantCulture,
                DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal,
                out _))
            throw new FormatException($"{name} 必須是 RFC3339 UTC Z timestamp。");
    }

    public static T[] Array<T>(JsonElement value, string name, Func<JsonElement, T> parser)
    {
        var array = Required(value, name);
        if (array.ValueKind != JsonValueKind.Array) throw new FormatException($"{name} 必須是陣列。");
        var result = new T[array.GetArrayLength()];
        var index = 0;
        foreach (var item in array.EnumerateArray()) result[index++] = parser(item);
        return result;
    }

    public static string[] StringArray(JsonElement value, string name) =>
        Array(value, name, item => item.ValueKind == JsonValueKind.String
            ? item.GetString()!
            : throw new FormatException($"{name} 只能包含字串。"));

    public static void WriteStrings(Utf8JsonWriter writer, string name, IEnumerable<string> values)
    {
        writer.WriteStartArray(name);
        foreach (var value in values) writer.WriteStringValue(value);
        writer.WriteEndArray();
    }
}
