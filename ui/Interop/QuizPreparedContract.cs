using System.Text.Json;

namespace Ui;

/// <summary>
/// Rust core 與 UI 共用的 QuizPrepared v1 wire parser。
/// 解析集中在這裡，避免頁面層各自猜測欄位型別而悄悄接受破損事件。
/// </summary>
public static class QuizPreparedContract
{
    public static bool TryParse(JsonElement element, out PreparedQuiz quiz, out string error)
    {
        quiz = null!;
        error = "";
        if (element.ValueKind != JsonValueKind.Object)
            return Fail("QuizPrepared 必須是 JSON 物件", out error);
        if (Str(element, "event") != "QuizPrepared")
            return Fail("QuizPrepared event 欄位不正確", out error);
        if (!element.TryGetProperty("schema_version", out var version) ||
            version.ValueKind != JsonValueKind.Number || !version.TryGetInt32(out var versionValue) || versionValue != 1)
            return Fail("QuizPrepared schema_version 不受支援", out error);

        var activityToken = Str(element, "activity_token");
        if (string.IsNullOrWhiteSpace(activityToken))
            return Fail("QuizPrepared 缺少 activity_token", out error);
        var quizId = RequiredStr(element, "quiz_id");
        var course = RequiredStr(element, "course");
        if (string.IsNullOrWhiteSpace(quizId) || course is null)
            return Fail("QuizPrepared 缺少 quiz_id 或 course", out error);

        if (!element.TryGetProperty("activity", out var activityElement) || activityElement.ValueKind != JsonValueKind.Object)
            return Fail("QuizPrepared 缺少 activity 物件", out error);
        var externalId = RequiredStr(activityElement, "external_id");
        var source = RequiredStr(activityElement, "source");
        var courseId = RequiredStr(activityElement, "course_id");
        var activityCourse = RequiredStr(activityElement, "course");
        if (string.IsNullOrWhiteSpace(externalId) || string.IsNullOrWhiteSpace(source) ||
            courseId is null || activityCourse is null)
            return Fail("QuizPrepared activity 欄位不完整", out error);
        var activity = new ActivityInfo(externalId, source, courseId, activityCourse);

        if (!element.TryGetProperty("per_account", out var accounts) || accounts.ValueKind != JsonValueKind.Array)
            return Fail("QuizPrepared 缺少 per_account 陣列", out error);
        var parsedAccounts = new List<PreparedAccount>();
        var accountIds = new HashSet<string>(StringComparer.Ordinal);
        foreach (var account in accounts.EnumerateArray())
        {
            if (account.ValueKind != JsonValueKind.Object)
                return Fail("QuizPrepared per_account 含有非物件", out error);
            var accountId = Str(account, "account_id");
            if (string.IsNullOrWhiteSpace(accountId))
                return Fail("QuizPrepared account 缺少 account_id", out error);
            if (!accountIds.Add(accountId))
                return Fail($"QuizPrepared 重複 account_id：{accountId}", out error);
            if (!account.TryGetProperty("instance_id", out var instanceElement) ||
                instanceElement.ValueKind != JsonValueKind.String || string.IsNullOrWhiteSpace(instanceElement.GetString()))
                return Fail($"QuizPrepared 的 {accountId} 缺少 instance_id", out error);
            var instanceId = instanceElement.GetString()!;
            if (!account.TryGetProperty("questions", out var questions) || questions.ValueKind != JsonValueKind.Array)
                return Fail($"QuizPrepared 的 {accountId} 缺少 questions 陣列", out error);

            var parsedQuestions = new List<PreparedQuestion>();
            var subjectIds = new HashSet<string>(StringComparer.Ordinal);
            foreach (var question in questions.EnumerateArray())
            {
                if (question.ValueKind != JsonValueKind.Object)
                    return Fail($"QuizPrepared 的 {accountId} 含有非物件題目", out error);
                var subjectId = Str(question, "subject_id");
                if (string.IsNullOrWhiteSpace(subjectId))
                    return Fail($"QuizPrepared 的 {accountId} 題目缺少 subject_id", out error);
                if (!subjectIds.Add(subjectId))
                    return Fail($"QuizPrepared 的 {accountId} 重複 subject_id：{subjectId}", out error);
                if (!question.TryGetProperty("answer", out var answerElement) || AnswerWire.FromJson(answerElement) is not { } answer)
                    return Fail($"QuizPrepared 的 {subjectId} 缺少合法型別答案", out error);

                var options = new List<QuestionOptionVm>();
                if (question.TryGetProperty("options", out var optionElements))
                {
                    if (optionElements.ValueKind != JsonValueKind.Array)
                        return Fail($"QuizPrepared 的 {subjectId} options 必須是陣列", out error);
                    foreach (var option in optionElements.EnumerateArray())
                    {
                        if (option.ValueKind != JsonValueKind.Object)
                            return Fail($"QuizPrepared 的 {subjectId} 含有非物件選項", out error);
                        options.Add(new QuestionOptionVm(Str(option, "id") ?? "", Str(option, "text") ?? ""));
                    }
                }

                AnswerWire? existingAnswer = null;
                if (question.TryGetProperty("existing_answer", out var existingElement) && existingElement.ValueKind != JsonValueKind.Null)
                {
                    existingAnswer = AnswerWire.FromJson(existingElement);
                    if (existingAnswer is null)
                        return Fail($"QuizPrepared 的 {subjectId} existing_answer 型別無效", out error);
                }
                var type = RequiredStr(question, "type");
                var answerType = RequiredStr(question, "answer_type");
                var stem = RequiredStr(question, "stem");
                var answerSource = RequiredStr(question, "source");
                if (string.IsNullOrWhiteSpace(type) || string.IsNullOrWhiteSpace(answerType) || stem is null ||
                    string.IsNullOrWhiteSpace(answerSource) || !question.TryGetProperty("conflict", out var conflictElement) ||
                    conflictElement.ValueKind is not (JsonValueKind.True or JsonValueKind.False))
                    return Fail($"QuizPrepared 的 {subjectId} 欄位不完整", out error);
                parsedQuestions.Add(new PreparedQuestion(
                    subjectId,
                    Str(question, "parent_id"),
                    type,
                    answerType,
                    stem,
                    options,
                    answer,
                    existingAnswer,
                    Str(question, "display_answer") ?? answer.Display,
                    answerSource,
                    conflictElement.GetBoolean()));
            }
            parsedAccounts.Add(new PreparedAccount(accountId, instanceId, parsedQuestions));
        }

        if (!element.TryGetProperty("conflict_count", out var conflictCountElement) ||
            conflictCountElement.ValueKind != JsonValueKind.Number || !conflictCountElement.TryGetInt32(out var conflictCount) ||
            conflictCount < 0)
            return Fail("QuizPrepared conflict_count 必須是非負整數", out error);
        quiz = new PreparedQuiz(activityToken, quizId, course, activity, parsedAccounts, conflictCount);
        return true;
    }

    static bool Fail(string message, out string error)
    {
        error = message;
        return false;
    }

    static string? Str(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;

    static string? RequiredStr(JsonElement element, string name) => Str(element, name);}

public sealed record PreparedQuiz(
    string ActivityToken,
    string QuizId,
    string Course,
    ActivityInfo? Activity,
    IReadOnlyList<PreparedAccount> Accounts,
    int ConflictCount);

public sealed record ActivityInfo(string ExternalId, string Source, string CourseId, string Course);

public sealed record PreparedAccount(string AccountId, string InstanceId, IReadOnlyList<PreparedQuestion> Questions);

public sealed record PreparedQuestion(
    string SubjectId,
    string? ParentId,
    string Type,
    string AnswerType,
    string Stem,
    IReadOnlyList<QuestionOptionVm> Options,
    AnswerWire Answer,
    AnswerWire? ExistingAnswer,
    string DisplayAnswer,
    string Source,
    bool Conflict);
