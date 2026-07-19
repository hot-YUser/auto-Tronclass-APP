# Fable 交接 — 「自動 Tronclass」UI

> 這份是給 UI 設計/實作階段的交接。**你負責把整個 App 的 UI 做成精緻優雅的成品**。地基（FFI 縫、假 core、
> App 身分、icon）已就緒；`ui/` 是**極簡起手**，等你把它長成完整四分頁 App。
>
> **狀態：已完成——四分頁 App 已建成、驗收通過並併回本 repo（`ui/` 與 `core/` 同 repo）。本文保留作交接／
> 「按鈕→命令→反應」契約參考。**

## 你要做什麼
把 **.NET MAUI 11** 的「自動 Tronclass」做成**小巧精緻且極致優雅**的完整 UI。你**只做 UI 層**：綁定
`ICore` 介面（送命令、收事件），**不需要也不會碰任何領域內幕**（core 怎麼算是黑箱）。專案已附 `MockCore`
——它會發出**真實時間線的假事件**，你對著它開發與預覽；之後我方把 `NativeCore`（真 Rust core）接上，你的
UI 一行都不用改。

## App 是什麼（誠實交代）
給校園 LMS「TronClass」用的**個人自動化工具**——自動點名簽到 ＋ LLM 自動答題，供使用者對**自己的**帳號/課程
使用。只在 **GitHub Release** 發佈、不上架商店。顯示名固定 **「自動 Tronclass」**。icon 已附
（`ui/Resources/AppIcon/appicon.png`，深底 neon-glitch 方標）——**視覺基調可延伸這個霓虹/暗色科技感**，
但你有完全的設計自由。

## 北極星
**小巧精緻且極致優雅**——不臃腫、不投機抽象、能不加的依賴就不加、一行能解決不寫五十行。

## 從哪裡開始（現況）
- `ui/Interop/ICore.cs`：你綁定的介面（`BootAsync`、`SendAsync(cmd, fields)`、`event EventReceived`、
  `LastCaps/LastProviders/LastAccounts/LastVaultState` 快照）。
- `ui/Interop/MockCore.cs`：**你的開發資料源**。腳本化一輪完整時間線：開機 `Caps/VaultState/Accounts` →
  `StartMonitoring` 後 `AccountStatus online` → 點名 `RollcallDetected → Countdown(15→0) → SignedIn`（per-account）→
  答題 `QuizPrepared(1 衝突) → ReasoningChunk 串流 → Countdown → QuizSubmitted`。要改情境就改這檔。
- `ui/Interop/NativeCore.cs`：真 core 實作（藏在 `ICore` 後，你不用碰）。上線＝把 `MauiProgram.cs` 那行
  `AddSingleton<ICore, MockCore>()` 換成 `NativeCore`。
- `ui/MainPage.cs`：**丟棄式 placeholder**，只證明縫可動。**刪掉它**，換成你的四分頁 shell。
- `ui/MauiProgram.cs` / `App.xaml.cs`：DI 註冊與進入點（`App(MainPage)` 目前注入 placeholder；改成注入你的 shell）。

## 資訊架構（結構定，視覺你決定）
完整職責/導覽見 **`docs/60-ui-ia.md`**。底部**四分頁：首頁 / 點名 / 答題 / 帳號**；**設定不是第五頁**，
從「帳號」頁入口點開。
- **首頁**：監控總開關(`StartMonitoring`/`StopMonitoring`)、被監控帳號摘要(`AccountStatus`)、目前狀態
  (`StateChanged.state`)、**下一堂課(`NextClass`；`LastNextClass`==null 就整塊隱藏，不留佔位)**、近期活動、
  背景可用性(`Caps.background_monitoring`；iOS 顯示「僅前景」)。
- **點名 / 答題**：近期紀錄 + 進行中清單（合併後一活動一列）→ 點進去是各自詳細頁。
- **帳號（重頭戲）**：同時監控多帳號（可跨班級/機構）、各自新增/切換/刪除；登入失敗可改「瀏覽器 cookie 登入」
  (`ImportCookies`)；**設定入口在此**。

## 英雄時刻 ＝ 置中小 modal 彈窗
有時限流程觸發時彈出快速操作——點名：立即簽(`SignNow`)/暫緩(`DeferSignIn`)；答題：送出(`SubmitNow`)/
暫緩(`HoldAnswer`)/捨棄(`DiscardAnswer`)。彈窗「詳細」深連到對應分頁的那個活動。

## 有時限流程（務必尊重）
**倒數由 core 持有**，你只渲染 `Countdown{scope, id_, remaining_secs}`。
- **點名**：`RollcallDetected` → 15% 全班簽到率防假門檻 → 15 秒倒數 → 自動簽；`PendingSignIn` 之後可補簽；
  確認 `on_call_fine` 後才 `SignedIn`。
- **答題**：`QuizPrepared`（備答，未送）→ **per-account 衝突：既有答案≠LLM 要高亮、絕不靜默覆蓋**、經
  `SetAnswer` 定案 → 15 秒倒數 → 自動送；`ReasoningChunk` 是 LLM 推理串流（逐字顯示）；`AnswerUpdated`/`QuizSubmitted`。

## 多帳號合併模型
每帳號各自獨立監控；活動合併鍵 ＝ (`base_url` + 活動類型 + 活動ID)，同鍵跨帳號併成「一活動 ＋ 參與帳號集合」，
**不同 `base_url` 即使 ID 同也不合併**；共用計算只跑一次，但**每帳號各自保留/可覆寫自己的答案、各自簽到/送出**
——所以 `SignedIn`/`QuizSubmitted`/`AnswerUpdated`/`QuizPrepared.per_account` 都帶 `account_id`。

## 能力旗標（`Caps` 事件）
`background_monitoring / self_update / biometric_unlock / qr_teacher_assist / ocr_captcha`——把按鈕 enabled 綁旗標。
**UI 裡零平台判斷**（不准 `if iOS`）；平台差異一律由 core 經 `Caps` 告訴你。

## 保險庫優先 / 驗證碼
任何操作前先 `Unlock`（首次 `CreateVault`）；生物辨識走 `UnlockWithKeystore`（由 `Caps.biometric_unlock`
決定顯不顯示）。驗證碼：收到 `CaptchaChallenge{account_id, image_b64}` → 顯示圖給使用者輸入 → `SubmitCaptcha`
（**UI 不做 OCR**）。

## 契約
命令與事件全表 ＋ 信封（`id`/`event`）規則見 **`docs/20-contract.md`**（已對齊真 core 的實際 wire）。你透過
`ICore.SendAsync("<Cmd>", ("欄位", 值)…)` 送命令、訂 `ICore.EventReceived` 收事件（**記得 marshal 回 UI thread**：
`MainThread.BeginInvokeOnMainThread`）；`Last*` 快照可即時 render。

## 每個按鈕做什麼（動作 → 命令 → 反應）
你只要記住「按鈕送命令、事件回來更新畫面」。以下是完整對照（`MockCore` 對每一條都會發出可見的假事件，
所以你按下去就看得到反應、可直接預覽）：

| 按鈕 / 動作 | `SendAsync` 送出 | 收到事件 → UI 反應 |
|---|---|---|
| 首次建立保險庫 | `CreateVault(master_password)` | `VaultState{unlocked:true}` → 進主畫面 |
| 解鎖（密碼 / 生物辨識） | `Unlock(master_password)` / `UnlockWithKeystore` | `VaultState{unlocked:true}` |
| 鎖定 | `LockVault` | `VaultState{unlocked:false}` |
| 開始 / 停止監控 | `StartMonitoring` / `StopMonitoring` | `StateChanged{state}`（+ 之後 Detected/Countdown/…） |
| 新增帳號 | `AddAccount(label,school,username,password)` | `Accounts`（清單更新） |
| 切換 / 刪除帳號 | `SwitchAccount` / `DeleteAccount(account_id)` | `Accounts`（active/清單更新） |
| 登入帳號 | `Login(account_id)` | `LoginResult{ok}` + `AccountStatus{online}` |
| 瀏覽器 cookie 登入（後備） | `ImportCookies(account_id,cookies_json)` | `AccountStatus{online}` |
| 輸入驗證碼 | `SubmitCaptcha(account_id,text)` | `AccountStatus{online}`（續登入） |
| 點名 · 立即簽 | `SignNow(rollcall_id)` | `SignedIn`（每個參與帳號各一） |
| 點名 · 暫緩 | `DeferSignIn(rollcall_id)` | `PendingSignIn`（之後可補簽） |
| 答題 · 手改某題（解衝突） | `SetAnswer(quiz_id,subject_id,answer)` | `AnswerUpdated{conflict:false}` |
| 答題 · 立即送 | `SubmitNow(quiz_id)` | `QuizSubmitted`（每帳號） |
| 答題 · 暫緩 / 捨棄 | `HoldAnswer` / `DiscardAnswer(quiz_id)` | 停自動送 / 不送（`LogLine`） |
| 改設定 / 設 LLM 金鑰 | `UpdateConfig(patch)` / `SetLlmKey(key)` | `Reply{ok}` |

被 core 主動推、你只負責「渲染」的（非按鈕）：`Caps`→綁按鈕 enabled、`NextClass`→首頁卡（null 就隱藏）、`Countdown`→倒數、
`RollcallDetected`/`QuizPrepared`→清單＋英雄彈窗、`ReasoningChunk`→推理串流、`Error`→錯誤提示、`Tick`→心跳。
**完整欄位見 `docs/20-contract.md`；行為在 `MockCore.cs` 都能實跑預覽。**

## 技術 / 如何跑
- **.NET MAUI 11 preview**；heads＝Windows + Android（iOS 之後）。shell/導覽是 **greenfield**（四分頁 shell 你來建）。
- `ui/` 與 Rust 核心 `core/` **同一個 repo**；UI 只透過 `Interop/ICore` 用核心，不碰領域內幕。
- 建置：直接 `dotnet build ui/Ui.csproj -f net11.0-windows10.0.19041.0`（Android: `-f net11.0-android`，模擬器 `tron_x64`）。
  **開發預覽用 `MockCore`，連原生庫都不需要**。要重編原生庫＝`./build-core.ps1`（見 `README.md`）。
- 交付：完整四分頁 UI ＋ 英雄彈窗 ＋ vault/設定/新增帳號流程，跑在 `MockCore` 上可展示全部畫面與有時限流程。
  **設計全知、建造增量**：帶著全部功能認知一次切對架構，功能一片片長。
