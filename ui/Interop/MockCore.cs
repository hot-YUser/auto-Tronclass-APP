#if USE_MOCK_CORE
using System.Text.Json;
using System.Text.Json.Nodes;

namespace TronClass.Interop;

/// <summary>Debug fixture-driven core；與 Rust/C# parser 共用 monitoring_snapshot_v1.json。</summary>
public sealed class MockCore : ICore
{
    public event Action<JsonElement>? EventReceived;

    public JsonElement? LastCaps { get; private set; }
    public JsonElement? LastProviders { get; private set; }
    public JsonElement? LastMonitoringSnapshot { get; private set; }
    public JsonElement? LastVaultState { get; private set; }
    public JsonElement? LastNextClass { get; private set; }

    JsonObject? _snapshot;
    int _nextAccount = 1;
    bool _booted;

    public async Task BootAsync(string dataDir)
    {
        if (_booted) return;
        await using var stream = await FileSystem.Current.OpenAppPackageFileAsync(
            "contract/monitoring_snapshot_v1.json");
        using var reader = new StreamReader(stream);
        _snapshot = JsonNode.Parse(await reader.ReadToEndAsync())?.AsObject()
                    ?? throw new InvalidOperationException("Mock snapshot fixture 無效。");
        _booted = true;

        Emit(new JsonObject
        {
            ["id"] = null,
            ["event"] = "Caps",
            ["caps"] = new JsonObject
            {
                ["background_monitoring"] = true,
                ["self_update"] = false,
                ["qr_teacher_assist"] = true,
                ["ocr_captcha"] = false,
            },
        });
        Emit(new JsonObject
        {
            ["id"] = null,
            ["event"] = "VaultState",
            ["exists"] = true,
            ["unlocked"] = true,
        });
        Emit(new JsonObject
        {
            ["id"] = null,
            ["event"] = "Providers",
            ["default_key"] = "thu",
            ["schools"] = new JsonArray
            {
                (JsonNode)new JsonObject
                {
                    ["key"] = "thu",
                    ["label"] = "東海大學",
                    ["base_url"] = "https://ilearn.thu.edu.tw",
                },
                (JsonNode)new JsonObject
                {
                    ["key"] = "demo",
                    ["label"] = "示範平台",
                    ["base_url"] = "https://demo.example.edu",
                },
            },
        });
        EmitSettings();
        EmitSnapshot();
    }

    public async Task<JsonElement> SendAsync(
        string cmd,
        params (string Key, object? Value)[] fields)
    {
        if (!_booted || _snapshot is null)
            return Reply(false, "not initialized");
        var command = JsonNode.Parse(JsonWire.SerializeCommand(0, cmd, fields))!.AsObject();
        switch (cmd)
        {
            case "GetMonitoringSnapshot":
                return Reply(true, data: new JsonObject
                {
                    ["snapshot"] = _snapshot.DeepClone(),
                });

            case "AddAccount":
                return AddAccount(command);

            case "Login":
            case "ImportCookies":
                return await VerifyAccount(command);

            case "DeleteAccount":
                return DeleteAccount(command);

            case "CreateGroup":
                return CreateGroup(command);

            case "UpdateGroup":
                return UpdateGroup(command);

            case "DeleteGroup":
                return DeleteGroup(command);

            case "MergeGroups":
                return MergeGroups(command);

            case "ListCommonCourses":
                return Reply(true, data: new JsonObject
                {
                    ["courses"] = new JsonArray
                    {
                        (JsonNode)new JsonObject { ["course_id"] = "course-zh", ["name"] = "大學中文" },
                        (JsonNode)new JsonObject { ["course_id"] = "course-common", ["name"] = "共同示範課程" },
                    },
                    ["account_errors"] = new JsonArray(),
                });

            case "SetTargetSchedule":
                return SetTargetSchedule(command);

            case "SetMonitoringPreferences":
                _snapshot["global_schedule"] = command["global_schedule"]!.DeepClone();
                _snapshot["time_zone"] = command["time_zone"]!.DeepClone();
                BumpDefinition(schedule: true);
                EmitSnapshot();
                return Reply(true);

            case "ApplyScheduleClock":
                ApplyClock(command);
                EmitSnapshot();
                return Reply(true);

            case "StartTarget":
                SetTargetRuntime(command["target"]!.AsObject(), "monitoring", forceOpen: true);
                EmitSnapshot();
                return Reply(true);

            case "StopTarget":
                SetTargetRuntime(command["target"]!.AsObject(), "manual_off", forceOpen: false);
                EmitSnapshot();
                return Reply(true);

            case "StopAllMonitoring":
                _snapshot["all_suspended"] = true;
                _snapshot["can_stop_all"] = false;
                _snapshot["can_resume"] = true;
                _snapshot["session_state"] = "idle";
                foreach (var target in Targets())
                {
                    target["runtime_state"] = "manual_off";
                    target["can_start"] = false;
                    target["can_stop"] = false;
                }
                BumpPlan();
                EmitSnapshot();
                return Reply(true);

            case "ResumeScheduledMonitoring":
                _snapshot["all_suspended"] = false;
                _snapshot["can_stop_all"] = true;
                _snapshot["can_resume"] = false;
                _snapshot["session_state"] = "running";
                foreach (var target in Targets())
                {
                    target["manual_override"] = null;
                    target["runtime_state"] = target["schedule_open"]?.GetValue<bool>() == true
                        ? target["target"]?["kind"]?.GetValue<string>() == "account"
                            ? "suppressed_by_group"
                            : "monitoring"
                        : "scheduled_off";
                    target["can_start"] = target["runtime_state"]?.GetValue<string>() != "monitoring";
                    target["can_stop"] = target["runtime_state"]?.GetValue<string>() == "monitoring";
                }
                BumpPlan();
                EmitSnapshot();
                return Reply(true);

            case "AcknowledgeTemporaryMerge":
                foreach (var prompt in _snapshot["merge_prompts"]!.AsArray().Select(node => node!.AsObject()))
                    if (prompt["component_id"]?.GetValue<string>() == Text(command, "component_id"))
                        prompt["acknowledged"] = true;
                BumpPlan();
                EmitSnapshot();
                return Reply(true);

            case "SuspendForPlatformLimit":
                _snapshot["platform_block"] = new JsonObject
                {
                    ["reason"] = Text(command, "reason"),
                    ["observed_at_utc"] = DateTimeOffset.UtcNow.ToString("O"),
                };
                _snapshot["session_state"] = "platform_blocked";
                foreach (var target in Targets()) target["runtime_state"] = "platform_blocked";
                BumpPlan();
                EmitSnapshot();
                return Reply(true);

            case "ClearPlatformLimit":
                _snapshot["platform_block"] = null;
                _snapshot["session_state"] = "idle";
                BumpPlan();
                EmitSnapshot();
                return Reply(true);

            case "UpdateConfig":
            case "SetLlmKey":
                EmitSettings();
                return Reply(true);

            case "SubmitCaptcha":
            case "SignNow":
            case "DeferSignIn":
            case "SubmitNow":
            case "HoldAnswer":
            case "DiscardAnswer":
            case "SetAnswer":
            case "Shutdown":
                return Reply(true);

            default:
                return Reply(false, $"Mock 不支援命令：{cmd}");
        }
    }

    JsonElement AddAccount(JsonObject command)
    {
        var id = $"mock-{_nextAccount++:x}";
        var teacher = command["is_teacher"]?.GetValue<bool>() == true;
        var account = new JsonObject
        {
            ["account_id"] = id,
            ["label"] = Text(command, "label"),
            ["school_ref"] = Text(command, "school"),
            ["username"] = Text(command, "username"),
            ["role"] = teacher ? "teacher" : "student",
            ["teacher_course_id"] = command["course_id"]?.DeepClone(),
            ["login_state"] = "stored",
            ["login_error"] = null,
            ["login_in_flight"] = false,
            ["in_use_targets"] = new JsonArray(),
        };
        _snapshot!["accounts"]!.AsArray().Add((JsonNode)account);
        if (!teacher) _snapshot["targets"]!.AsArray().Add((JsonNode)NewPersonalTarget(id, Text(command, "label")));
        BumpDefinition(schedule: !teacher);
        EmitSnapshot();
        return Reply(true, data: new JsonObject { ["account_id"] = id });
    }

    async Task<JsonElement> VerifyAccount(JsonObject command)
    {
        var account = FindAccount(Text(command, "account_id"));
        if (account is null) return Reply(false, "no such account");
        account["login_state"] = "logging_in";
        account["login_error"] = null;
        account["login_in_flight"] = true;
        EmitSnapshot();
        await Task.Delay(650);
        var fails = account["account_id"]?.GetValue<string>() == "student-b" ||
                    account["username"]?.GetValue<string>()?.Contains("fail", StringComparison.OrdinalIgnoreCase) == true;
        account["login_state"] = fails ? "error" : "online";
        account["login_error"] = fails
            ? new JsonObject { ["code"] = "login_failed", ["message"] = "示範：帳號或密碼錯誤" }
            : null;
        account["login_in_flight"] = false;
        EmitSnapshot();
        return Reply(!fails, fails ? "示範：帳號或密碼錯誤" : null);
    }

    JsonElement DeleteAccount(JsonObject command)
    {
        var id = Text(command, "account_id");
        RemoveWhere(_snapshot!["accounts"]!.AsArray(), node =>
            node!["account_id"]?.GetValue<string>() == id);
        RemoveWhere(_snapshot["targets"]!.AsArray(), node =>
            node!["target"]?["kind"]?.GetValue<string>() == "account" &&
            node["target"]?["account_id"]?.GetValue<string>() == id);
        foreach (var target in Targets().Where(target => target["group_definition"] is not null).ToArray())
        {
            var members = target["group_definition"]!["member_account_ids"]!.AsArray();
            RemoveWhere(members, node => node?.GetValue<string>() == id);
            if (members.Count == 0) _snapshot["targets"]!.AsArray().Remove(target);
        }
        BumpDefinition(schedule: true);
        EmitSnapshot();
        return Reply(true);
    }

    JsonElement CreateGroup(JsonObject command)
    {
        var input = command["group"]!.AsObject();
        var id = $"mock-group-{_nextAccount++:x}";
        _snapshot!["targets"]!.AsArray().Add((JsonNode)NewGroupTarget(id, input));
        BumpDefinition(schedule: true);
        EmitSnapshot();
        return Reply(true, data: new JsonObject { ["group_id"] = id });
    }

    JsonElement UpdateGroup(JsonObject command)
    {
        var target = FindTarget(new JsonObject
        {
            ["kind"] = "group",
            ["group_id"] = Text(command, "group_id"),
        });
        if (target is null) return Reply(false, "no such group");
        ApplyGroupInput(target, command["group"]!.AsObject());
        BumpDefinition(schedule: true);
        EmitSnapshot();
        return Reply(true);
    }

    JsonElement DeleteGroup(JsonObject command)
    {
        var id = Text(command, "group_id");
        RemoveWhere(_snapshot!["targets"]!.AsArray(), node =>
            node!["target"]?["kind"]?.GetValue<string>() == "group" &&
            node["target"]?["group_id"]?.GetValue<string>() == id);
        BumpDefinition(schedule: true);
        EmitSnapshot();
        return Reply(true);
    }

    JsonElement MergeGroups(JsonObject command)
    {
        var ids = command["group_ids"]!.AsArray().Select(node => node!.GetValue<string>()).ToHashSet(StringComparer.Ordinal);
        RemoveWhere(_snapshot!["targets"]!.AsArray(), node =>
            node!["target"]?["kind"]?.GetValue<string>() == "group" &&
            ids.Contains(node["target"]?["group_id"]?.GetValue<string>() ?? ""));
        _snapshot["targets"]!.AsArray().Add((JsonNode)NewGroupTarget($"mock-merged-{_nextAccount++:x}", command["group"]!.AsObject()));
        BumpDefinition(schedule: true);
        EmitSnapshot();
        return Reply(true);
    }

    JsonElement SetTargetSchedule(JsonObject command)
    {
        var target = FindTarget(command["target"]!.AsObject());
        if (target is null) return Reply(false, "no such target");
        target["schedule"] = command["schedule"]!.DeepClone();
        target["schedule_open"] = false;
        target["next_boundary_utc"] = null;
        BumpDefinition(schedule: true);
        EmitSnapshot();
        return Reply(true);
    }

    void ApplyClock(JsonObject command)
    {
        _snapshot!["clock_revision"] = command["clock_revision"]!.DeepClone();
        foreach (var entry in command["targets"]!.AsArray().Select(node => node!.AsObject()))
        {
            var target = FindTarget(entry["target"]!.AsObject());
            if (target is null) continue;
            target["schedule_open"] = entry["is_open"]!.DeepClone();
            target["next_boundary_utc"] = entry["next_boundary_utc"]?.DeepClone();
        }
        BumpPlan();
    }

    void SetTargetRuntime(JsonObject id, string state, bool forceOpen)
    {
        var target = FindTarget(id);
        if (target is null) return;
        target["runtime_state"] = state;
        target["manual_override"] = new JsonObject
        {
            ["force_open"] = forceOpen,
            ["expires_at_utc"] = null,
        };
        target["can_start"] = state != "monitoring";
        target["can_stop"] = state == "monitoring";
        _snapshot!["session_state"] = state == "monitoring" ? "running" : "idle";
        _snapshot["can_stop_all"] = Targets().Any(item => item["runtime_state"]?.GetValue<string>() == "monitoring");
        BumpPlan();
    }

    JsonObject NewPersonalTarget(string accountId, string name) => new()
    {
        ["target"] = new JsonObject { ["kind"] = "account", ["account_id"] = accountId },
        ["name"] = name,
        ["runtime_state"] = "scheduled_off",
        ["schedule"] = new JsonObject { ["kind"] = "disabled" },
        ["schedule_open"] = false,
        ["next_boundary_utc"] = null,
        ["manual_override"] = null,
        ["detector"] = null,
        ["group_definition"] = null,
        ["courses"] = new JsonArray(),
        ["in_use_account_ids"] = new JsonArray(),
        ["account_results"] = new JsonArray(),
        ["can_start"] = true,
        ["can_stop"] = false,
        ["can_edit_schedule"] = true,
        ["disabled_reason"] = null,
        ["error"] = null,
    };

    JsonObject NewGroupTarget(string groupId, JsonObject input)
    {
        var target = new JsonObject
        {
            ["target"] = new JsonObject { ["kind"] = "group", ["group_id"] = groupId },
            ["runtime_state"] = "scheduled_off",
            ["schedule_open"] = false,
            ["next_boundary_utc"] = null,
            ["manual_override"] = null,
            ["detector"] = null,
            ["courses"] = new JsonArray(),
            ["in_use_account_ids"] = new JsonArray(),
            ["account_results"] = new JsonArray(),
            ["can_start"] = true,
            ["can_stop"] = false,
            ["can_edit_schedule"] = true,
            ["disabled_reason"] = null,
            ["error"] = null,
        };
        ApplyGroupInput(target, input);
        return target;
    }

    static void ApplyGroupInput(JsonObject target, JsonObject input)
    {
        target["name"] = input["name"]!.DeepClone();
        target["schedule"] = input["schedule"]!.DeepClone();
        target["group_definition"] = new JsonObject
        {
            ["member_account_ids"] = input["member_account_ids"]!.DeepClone(),
            ["course_ids"] = input["course_ids"]!.DeepClone(),
            ["detector_selection"] = input["detector"]!.DeepClone(),
        };
        var courses = new JsonArray();
        foreach (var courseId in input["course_ids"]!.AsArray())
            courses.Add((JsonNode)new JsonObject
            {
                ["course_id"] = courseId!.GetValue<string>(),
                ["name"] = courseId.GetValue<string>(),
            });
        target["courses"] = courses;
    }

    JsonObject? FindAccount(string id) => _snapshot!["accounts"]!.AsArray()
        .Select(node => node!.AsObject())
        .FirstOrDefault(account => account["account_id"]?.GetValue<string>() == id);

    JsonObject? FindTarget(JsonObject id) => Targets().FirstOrDefault(target =>
        TargetKey(target["target"]!.AsObject()) == TargetKey(id));

    IEnumerable<JsonObject> Targets() => _snapshot!["targets"]!.AsArray()
        .Select(node => node!.AsObject());

    static string TargetKey(JsonObject target)
    {
        var kind = Text(target, "kind");
        return kind == "account"
            ? $"account:{Text(target, "account_id")}"
            : $"group:{Text(target, "group_id")}";
    }

    void BumpDefinition(bool schedule)
    {
        _snapshot!["config_revision"] = Number(_snapshot, "config_revision") + 1;
        if (schedule) _snapshot["schedule_revision"] = Number(_snapshot, "schedule_revision") + 1;
        BumpPlan();
    }

    void BumpPlan() => _snapshot!["plan_revision"] = Number(_snapshot, "plan_revision") + 1;

    static long Number(JsonObject value, string name) => value[name]!.GetValue<long>();
    static string Text(JsonObject value, string name) => value[name]?.GetValue<string>() ?? "";

    static void RemoveWhere(JsonArray array, Func<JsonNode?, bool> predicate)
    {
        for (var index = array.Count - 1; index >= 0; index--)
            if (predicate(array[index])) array.RemoveAt(index);
    }

    void EmitSettings() => Emit(new JsonObject
    {
        ["id"] = null,
        ["event"] = "Settings",
        ["settings"] = new JsonObject
        {
            ["countdown_secs"] = 15,
            ["attendance_gate_percent"] = 0.0,
            ["llm_endpoint"] = "https://api.openai.com/v1",
            ["llm_model"] = "gpt-4o-mini",
            ["llm_max_tokens"] = 4096,
            ["resubmit_for_correct"] = false,
            ["enable_llm_tools"] = true,
            ["has_llm_key"] = true,
        },
    });

    void EmitSnapshot() => Emit(new JsonObject
    {
        ["id"] = null,
        ["event"] = "MonitoringSnapshot",
        ["snapshot"] = _snapshot!.DeepClone(),
    });

    void Emit(JsonObject payload)
    {
        var element = Element(payload);
        switch (payload["event"]?.GetValue<string>())
        {
            case "Caps": LastCaps = element; break;
            case "Providers": LastProviders = element; break;
            case "MonitoringSnapshot": LastMonitoringSnapshot = element; break;
            case "VaultState": LastVaultState = element; break;
            case "NextClass": LastNextClass = element; break;
        }
        EventReceived?.Invoke(element);
    }

    static JsonElement Reply(bool ok, string? error = null, JsonObject? data = null)
    {
        var payload = new JsonObject
        {
            ["id"] = 0,
            ["event"] = "Reply",
            ["ok"] = ok,
        };
        if (error is not null) payload["error"] = error;
        if (data is not null) payload["data"] = data;
        return Element(payload);
    }

    static JsonElement Element(JsonNode node)
    {
        using var document = JsonDocument.Parse(node.ToJsonString());
        return document.RootElement.Clone();
    }
}
#endif
