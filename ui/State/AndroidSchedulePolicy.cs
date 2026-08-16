namespace Ui;

public enum AlarmBoundaryAction
{
    StartForegroundService,
    NotifyUser,
}

public readonly record struct BootScheduleAction(
    bool ScheduleFutureBoundary,
    bool NotifyUser,
    bool StartForegroundService);

/// <summary>不依賴 Android binding 的平台限制決策；receiver 與 production-linked check 共用。</summary>
public static class AndroidSchedulePolicy
{
    public static AlarmBoundaryAction AtBoundary(bool canScheduleExact) => canScheduleExact
        ? AlarmBoundaryAction.StartForegroundService
        : AlarmBoundaryAction.NotifyUser;

    public static BootScheduleAction AfterBoot(bool hasFutureBoundary, bool activeNow)
    {
        if (!hasFutureBoundary)
            return new(false, true, false);
        return new(true, activeNow, false);
    }

    public static string WakeMode(bool atLeastApi31, bool canScheduleExact) =>
        !atLeastApi31 || canScheduleExact ? "exact" : "inexact_user_action_required";
}
