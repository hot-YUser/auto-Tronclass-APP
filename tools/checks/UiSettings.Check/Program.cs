using Ui;

// 設定頁未儲存編輯保護的純邏輯(SettingsPage 共用)。

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

Console.WriteLine("UiSettings.Check：全部通過");

static void Assert(bool condition, string message)
{
    if (!condition) throw new InvalidOperationException($"設定邏輯檢查失敗：{message}");
}
