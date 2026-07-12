# 20 · 契約（Core ↔ UI Contract）

這一份是**定死的**：命令/事件 schema、能力旗標、兩個有時限流程的硬 UX 不變量。做錯這些會**不可逆地
遺失資料或驚嚇使用者**。UI 的視覺長相是你的自由，但**這裡的行為與資料形狀不是**。

下列命令/事件名與欄位＝**真 core 實際 wire**（對齊自程式碼 `engine.rs`/`monitor.rs`/`llm.rs`；`MockCore`
照這個發，Fable 照這個綁）。線型：`{ "id": <num|null>, "cmd"|"event": "…", …欄位 }`。

**信封（envelope）**：每則訊息都有 `id`。命令送出時帶一個遞增數字 `id`；core 以**同 `id` 的回覆**
（`Reply` 或 `LoginResult`，`id` 為該數字）完成該次呼叫（`ICore.SendAsync` 回傳的 Task）。**主動事件的
`id` 為 `null`**，帶 `event` 字串，走 `EventReceived`。

## 命令（UI → core）

```
// 每則命令 = { "id": <num>, "cmd": "<Name>", …欄位 }
Init { data_dir }                    // 啟動 core：載入設定/狀態，回 Providers/Accounts/VaultState/Caps/StateChanged
CreateVault { master_password }      // 首次建立保險庫
Unlock { master_password }           // 主密碼解鎖
UnlockWithKeystore {}                // 平台密鑰庫（生物辨識/免密碼）解鎖
LockVault {}                         // 鎖回保險庫
AddAccount { label, school, username, password }   // 新增帳號（school = 校代號或原始 base_url）
SwitchAccount { account_id }
DeleteAccount { account_id }
Login { account_id }                 // 登入單一帳號（回 LoginResult）
ImportCookies { account_id, cookies_json }         // 瀏覽器 cookie 登入後備
SubmitCaptcha { account_id, text }   // 回覆 CaptchaChallenge 的驗證碼
StartMonitoring {}  StopMonitoring {}
SignNow { rollcall_id }              // 立即簽（跳過剩餘倒數；簽該活動全部參與帳號）
DeferSignIn { rollcall_id }          // 先不簽 → PendingSignIn，之後可再 SignNow
SubmitNow { quiz_id }                // 立即送
HoldAnswer { quiz_id }               // 保留 LLM 答案但暫緩（停倒數）
DiscardAnswer { quiz_id }            // 捨棄、不送
SetAnswer { quiz_id, subject_id, answer }          // 手動改某題（衝突定案）
SetLlmKey { key }                    // 設定 LLM 金鑰
UpdateConfig { patch }               // typed 設定 patch
Shutdown {}
```

粒度要粗：一個「開始監控」而不是十個微命令。**穿過縫的命令種類越少越好。**

## 事件（core → UI，callback 推上來）

```
// 每則事件 = { "id": null, "event": "<Name>", …欄位 }；回覆 = { "id": <num>, "event": "Reply"|"LoginResult", … }
Reply { id, ok, error }                              // 命令的 id 相關回覆（error 為 null 即成功）
LoginResult { id, ok, detail? , reason? }            // Login 的 id 相關回覆
Tick { n }                                           // 每秒心跳（process-alive 證明）
Caps { caps }                                        // 能力旗標（見下）；啟動即發，變動可再發
StateChanged { state }                               // starting / idle / monitoring / login_failed …
Providers { default_key, schools[] }                 // 學校登錄表（schools[] = {key,label,base_url,aliases,notes}）
Accounts { active, accounts[] }                      // 帳號清單（accounts[] = {id,label,school_ref,username,device_id,is_teacher,course_id}）
AccountStatus { account_id, state, error? }          // 單帳號 online / login_failed
VaultState { exists, unlocked }
CaptchaChallenge { account_id, image_b64 }           // 圖形驗證碼 → 顯示給使用者 → SubmitCaptcha
LogLine { level, text }                              // 已遮蔽的日誌行
Error { severity, code, message }                    // 永不靜默吞錯；一律成事件上來

// 點名
RollcallDetected { rollcall_id, base_url, kind, course, attendance_rate, accounts[] }
PendingSignIn { rollcall_id }                        // 達門檻但使用者選先不簽（可隨時補簽）
Countdown { scope, id_, remaining_secs }             // scope="rollcall"|"quiz"；id_=活動id（避開信封 id）；core 持有計時器
SignedIn { rollcall_id, account_id, course, method } // 已確認 on_call_fine（per-account）

// 答題
QuizPrepared { quiz_id, course, per_account[{account_id, questions[]}], conflict_count }
ReasoningChunk { quiz_id, subject_id, text }         // LLM reasoning 串流（可展開觀看）
AnswerUpdated { quiz_id, account_id, subject_id, source, conflict }  // 某題答案變動（LLM 或使用者）
QuizSubmitted { quiz_id, account_id, result }        // 送出結果（per-account）
```

**硬規矩：錯誤永不靜默。** LLM 連不上、登入失敗、送出被伺服器擋——一律以 `Error` 事件（帶可讀原因）
上來，UI 顯示；**LLM 失敗時寧可不送，也不送空白答案**。

## 能力旗標（Capabilities）

core 判定、UI 綁定。至少包含：

```
struct Caps {
  background_monitoring: bool,   // 螢幕關著能否續跑（桌面/Android true、iOS false）
  self_update: bool,             // 應用內自動更新（桌面/APK true、iOS false）
  biometric_unlock: bool,        // 平台密鑰庫可做生物辨識/免密碼解鎖
  qr_teacher_assist: bool,       // 是否配有教師帳號 → QR 型可自動化（見 32）
  ocr_captcha: bool,             // 圖形驗證碼本地辨識是否可用
  // …隨功能增補
}
```

## 有時限流程 A — 答題（auto-answer）

**預設行為與 v1 一致：備妥 → 給反悔窗 → 送出。** GUI 版把它視覺化，並加入衝突處理與串流。狀態機：

1. **偵測**到進行中測驗 → core 取題、對每題決策（server 洩漏正解→replay；否則交 LLM）。
2. **LLM 作答**中：發 `ReasoningChunk`（使用者可展開看即時 reasoning stream）。
3. **衝突檢查**：若**使用者對某題已有作答**，而 LLM 給的答案**不同** → 該題標 `conflict=true`。
   - `QuizPrepared` 帶 `conflict_count`；UI **高亮**衝突題，**要使用者做最終抉擇**（`SetAnswer` 選定）。
   - **絕不靜默用 LLM 答案覆蓋使用者既有答案。**
4. **無衝突**（或衝突已解決）後：core 起 **15 秒倒數**（發 `Countdown`），到點**自動送出**。
   期間使用者三選一：
   - `SubmitNow` → 立即送。
   - `HoldAnswer` → **保留 LLM 答案但暫緩**（停倒數、不自動送；之後可再 `SubmitNow`）。
   - `DiscardAnswer` → 捨棄 LLM 答案、不送。
5. 送出（`QuizSubmitted`）。若該活動允許重作且複閱洩漏正解，可再送一次正解保滿分（見 `31`）。

**倒數計時由 core 持有**（單一真實來源、全平台行為一致、可測），UI 只渲染 `remaining_secs`/`deadline`。
15 秒為預設，屬設定可調。

## 有時限流程 B — 點名（rollcall）

1. **偵測**到點名（`RollcallDetected`，帶型別/課程/全班簽到率）。
2. **15% 防假點名門檻**：全班簽到率未達 15% 時**不出手**（避免老師誤觸的空點名把你簽進去）。
   門檻可由設定或旗標關閉（進階）。達門檻才續。
3. **15 秒倒數**（`Countdown`），到點**自動簽到**。期間使用者：
   - `SignNow` → 立即簽。
   - `DeferSignIn` → **先不簽** → 轉 `PendingSignIn`（狀態留著，使用者**之後隨時**可再 `SignNow` 補完）。
4. 簽到後**回查確認 `on_call_fine`** 才發 `SignedIn`（見 `30`）。

## 定死 vs 你的裁量（再申明）

- **定死（照做）**：上面所有命令/事件語意、能力旗標、A/B 兩流程的每一步與其不變量
  （複閱前必先呈現、衝突不覆蓋、倒數 core 持有、defer 可補、送出前回查、LLM 失敗不送空白）。
- **你設計**：畫面版面、字體、動效、文案、以及「reasoning stream 怎麼展開」「衝突怎麼高亮」的視覺呈現。
  `docs/` 給畫面清單與職責（見 `50` 與各領域 doc），**沒有 pixel 規格，是刻意的**。

## 畫面清單（職責級，視覺交你）

登入/帳號、監控儀表板（即時狀態 + 能力旗標）、點名事件卡（含 15% 與倒數與 defer）、
答題複閱頁（逐題 LLM 答案 + 衝突高亮 + 可展開 reasoning + 倒數 + 送/暫緩/捨棄）、設定頁、日誌頁。
其中**只有點名事件卡與答題複閱頁**是有時限、替使用者出手、難復原的——精度只欠它們（見 `50`）。其餘為
一般 CRUD/顯示，給一句話意圖即可。
