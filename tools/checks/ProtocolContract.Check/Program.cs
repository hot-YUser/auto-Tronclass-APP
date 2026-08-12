using System.Text.Json;
using Ui;

var fixturePath = Path.Combine(AppContext.BaseDirectory, "quiz_prepared_v1.json");
using var document = JsonDocument.Parse(File.ReadAllText(fixturePath));
Assert(QuizPreparedContract.TryParse(document.RootElement, out var quiz, out var error), error);
Assert(quiz.ActivityToken == "fixture-quiz-prepared-v1", "activity_token");
Assert(quiz.QuizId == "32877", "quiz_id");
Assert(quiz.Course == "行銷管理", "course");
Assert(quiz.Activity is { ExternalId: "32877", Source: "exam", CourseId: "55379", Course: "行銷管理" }, "activity fields");
Assert(quiz.ConflictCount == 2, "conflict_count");
Assert(quiz.Accounts.Count == 2, "account count");
Assert(quiz.ExpectedAccounts.Count == 2 && quiz.ExpectedAccounts.All(account => account.State == "ready"), "expected accounts");

foreach (var account in quiz.Accounts)
{
    Assert(account.AccountId is "a1" or "a2", "account id");
    Assert(account.InstanceId == $"attempt-{account.AccountId}", "instance_id");
    Assert(account.Questions.Count == 2, "question count");
    var first = account.Questions[0];
    Assert(first.SubjectId == "1" && first.Type == "short_answer" && first.AnswerType == "short_answer", "first question metadata");
    Assert(first.Stem == "台灣最高的山是哪一座？" && first.Answer.Kind == "text" && first.Answer.Value == "玉山", "first answer");
    Assert(first.ExistingAnswer?.Value == "雪山" && first.DisplayAnswer == "玉山", "existing/display answer");
    Assert(first.Source == "llm" && first.Conflict, "first source/conflict");
    Assert(first.Options.Count == 0, "first options");

    var second = account.Questions[1];
    Assert(second.SubjectId == "2" && second.Answer.Value == "H2O", "second answer");
    Assert(second.ExistingAnswer is null && !second.Conflict, "second existing/conflict");
}

foreach (var invalidAnswer in new[]
{
    "{\"kind\":\"options\",\"option_ids\":[]}",
    "{\"kind\":\"options\",\"option_ids\":[\" \" ]}",
    "{\"kind\":\"blanks\",\"values\":\"wrong\"}",
    "{\"kind\":\"text\",\"value\":\" \"}",
    "{\"kind\":\"vote\",\"letters\":[]}",
    "{\"kind\":\"unknown\"}",
})
{
    var root = JsonSerializer.Deserialize<Dictionary<string, object?>>(File.ReadAllText(fixturePath))!;
    var accounts = (JsonElement)root["per_account"]!;
    var accountList = JsonSerializer.Deserialize<List<Dictionary<string, object?>>>(accounts.GetRawText())!;
    var questions = (JsonElement)accountList[0]["questions"]!;
    var questionList = JsonSerializer.Deserialize<List<Dictionary<string, object?>>>(questions.GetRawText())!;
    questionList[0]["answer"] = JsonSerializer.Deserialize<JsonElement>(invalidAnswer);
    accountList[0]["questions"] = questionList;
    root["per_account"] = accountList;
    using var malformed = JsonDocument.Parse(JsonSerializer.Serialize(root));
    Assert(!QuizPreparedContract.TryParse(malformed.RootElement, out _, out _), $"malformed answer must fail closed: {invalidAnswer}");
}

foreach (var source in new[] { "courseware-quiz", "vote", "homework" })
{
    using var mutable = JsonDocument.Parse(File.ReadAllText(fixturePath));
    var root = JsonSerializer.Deserialize<Dictionary<string, object?>>(mutable.RootElement.GetRawText())!;
    var activity = JsonSerializer.Deserialize<Dictionary<string, object?>>(((JsonElement)root["activity"]!).GetRawText())!;
    activity["source"] = source;
    root["activity"] = activity;
    var accounts = JsonSerializer.Deserialize<List<Dictionary<string, object?>>>(((JsonElement)root["per_account"]!).GetRawText())!;
    foreach (var account in accounts) account["instance_id"] = "";
    root["per_account"] = accounts;
    using var valid = JsonDocument.Parse(JsonSerializer.Serialize(root));
    Assert(QuizPreparedContract.TryParse(valid.RootElement, out var parsed, out var sourceError), $"{source}: {sourceError}");
    Assert(parsed.Accounts.All(account => account.InstanceId == ""), $"{source} empty instance");
}

foreach (var source in new[] { "exam", "classroom-exam", "questionnaire" })
{
    using var mutable = JsonDocument.Parse(File.ReadAllText(fixturePath));
    var root = JsonSerializer.Deserialize<Dictionary<string, object?>>(mutable.RootElement.GetRawText())!;
    var activity = JsonSerializer.Deserialize<Dictionary<string, object?>>(((JsonElement)root["activity"]!).GetRawText())!;
    activity["source"] = source;
    root["activity"] = activity;
    var accounts = JsonSerializer.Deserialize<List<Dictionary<string, object?>>>(((JsonElement)root["per_account"]!).GetRawText())!;
    accounts[0]["instance_id"] = "";
    root["per_account"] = accounts;
    using var invalid = JsonDocument.Parse(JsonSerializer.Serialize(root));
    Assert(!QuizPreparedContract.TryParse(invalid.RootElement, out _, out _), $"{source} requires instance");
}

foreach (var requiredField in new[] { "quiz_id", "activity", "expected_accounts", "conflict_count" })
{
    using var mutable = JsonDocument.Parse(File.ReadAllText(fixturePath));
    var root = JsonSerializer.Deserialize<Dictionary<string, JsonElement>>(mutable.RootElement.GetRawText())!;
    root.Remove(requiredField);
    using var malformed = JsonDocument.Parse(JsonSerializer.Serialize(root));
    Assert(!QuizPreparedContract.TryParse(malformed.RootElement, out _, out _), $"missing {requiredField} must fail closed");
}

// ---- Quiz 完成述詞(terminal set):只有 Submitted 或 AttemptState ∈ {failed, gone} 是終端 ----
Assert(QuizCompletion.IsTerminal(null, submitted: true), "submitted must be terminal");
Assert(QuizCompletion.IsTerminal("failed", submitted: false), "failed must be terminal");
Assert(QuizCompletion.IsTerminal("gone", submitted: false), "gone must be terminal");
foreach (var state in new[] { "waiting", "preparing", "ready", "submitting" })
    Assert(!QuizCompletion.IsTerminal(state, submitted: false), $"{state} must block completion");

// 帳號層:AttemptState/Submitted 經由同一述詞
var terminalAcc = new QuizAccountVm { AccountId = "a" };
terminalAcc.AttemptState = "failed";
Assert(terminalAcc.IsTerminal, "failed account is terminal");
terminalAcc.AttemptState = "ready";
Assert(!terminalAcc.IsTerminal, "ready account blocks");
terminalAcc.SubmitResult = "ok";
Assert(terminalAcc.IsTerminal, "submitted account is terminal");

static QuizVm Quiz(params (string Id, string State, bool Submitted)[] accounts)
{
    var vm = new QuizVm { ActivityToken = "check", Id = "q" };
    foreach (var (id, state, submitted) in accounts)
    {
        vm.ExpectedAccountIds.Add(id);
        var acc = new QuizAccountVm { AccountId = id };
        acc.AttemptState = state;
        if (submitted) acc.SubmitResult = "ok";
        vm.PerAccount.Add(acc);
    }
    return vm;
}

// all-failed(failed + gone)→ 完成;狀態文字不得宣稱成功送出
var allFailed = Quiz(("a", "failed", false), ("b", "gone", false));
Assert(allFailed.IsComplete, "all-failed must complete");
allFailed.Status = "done";
Assert(allFailed.StatusTag == "已結束", "all-failed tag must not claim 已送出");
Assert(allFailed.StatusText == "活動已結束 · 未送出作答", "all-failed text must not claim submission");

// ready + failed:ready 仍在路上,不得完成
Assert(!Quiz(("a", "ready", false), ("b", "failed", false)).IsComplete, "ready+failed must not complete");

// preparing 不得完成(即使另一帳號已送出)
Assert(!Quiz(("a", "preparing", false), ("b", "ready", true)).IsComplete, "preparing must not complete");

// waiting / submitting 同樣阻擋
Assert(!Quiz(("a", "waiting", false), ("b", "gone", false)).IsComplete, "waiting must not complete");
Assert(!Quiz(("a", "submitting", false), ("b", "gone", false)).IsComplete, "submitting must not complete");

// 全帳號送出 → 完成
Assert(Quiz(("a", "ready", true), ("b", "ready", true)).IsComplete, "all submitted must complete");

Console.WriteLine("ProtocolContract.Check：全部通過");

static void Assert(bool condition, string message)
{
    if (!condition) throw new InvalidOperationException($"契約檢查失敗：{message}");
}
