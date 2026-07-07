# AGENTS.md — auto-tronclass-rollcall-answer

## 如果你是要實作本專案的 AI：先讀這一份

你沒有任何先前脈絡。**這個 repository 就是你的全世界。** 你需要的一切都在 `docs/`。
不要去找「上一個版本」、對話紀錄、或外部筆記——那些對你都不存在，你也不需要。照這裡的規格從零建造。

這是一次 **clean-room、從零重寫**。沒有任何舊碼要移植。`docs/` 是一份**規格**（軟體必須做到什麼 +
目標架構），不是對任何既有程式的描述。用 Rust + .NET MAUI **自己設計出優雅的實作**。

## 這個專案是什麼

TronClass（校園 LMS；各校自取名 iLearn/iClass/TronClass，但同一套 API）的跨平台客戶端，自動化兩件事：
**點名簽到（rollcall）** 與 **LLM 輔助答題（auto-answer）**。單一 UI codebase 跑
Windows / macOS / Linux / Android / iOS 原生。

北極星：**精巧優雅。** 沒有臃腫、沒有投機抽象、能不加的依賴就不加。一個功能或一層若賺不到它的存在，
它就不該存在。這是硬要求，不是偏好。

## 技術選型（已定案，不要重新辯論）

- **核心**：Rust `cdylib`。所有領域邏輯——HTTP、登入、學校 registry、四型點名、雷達解算、LLM 答題、
  QR、設定、狀態、祕密保管、監控迴圈、倒數計時——全在這。完全 headless，用假伺服器即可測。
- **UI**：.NET MAUI（Win/macOS/iOS/Android 原生；Linux + WASM 走 **Avalonia MAUI backend**，
  同一份 UI code、零重寫）。需要 .NET MAUI 11（GA 前為 Preview，約 2026 稍晚）+ Avalonia 12。
  UI 是**笨視圖**：送命令、渲染事件，**零領域邏輯**（不准有 `if TronClass…`，也不准有 `if iOS…`）。
- **FFI 縫**：行程內（P/Invoke 進 cdylib），**不是** IPC。型別綁定用 **csbindgen** 生成。介面窄且粗粒度：
  一條命令通道（UI→core）+ 一個事件 callback（core→UI），見 `docs/20-contract.md`。真正的難點是
  **async + 串流事件過 FFI 邊界**——先證明這件事（見建造順序）。

## 閱讀順序

1. `docs/00-overview.md` — 範圍、目標平台、non-goals
2. `docs/10-architecture.md` — core/UI 切分、FFI、生命週期、祕密保管
3. `docs/20-contract.md` — 命令/事件 schema、能力旗標、兩個有時限的流程
4. `docs/30-domain-rollcall.md`、`31-domain-autoanswer.md`、`32-domain-qr.md` — 逆向來的 TronClass 領域（真實 API 事實）
5. `docs/40-providers.md` — 學校
6. `docs/50-build-order.md` — **從這裡開始決定先做什麼**
7. `docs/90-conventions.md` — 下面的硬規矩，完整版

## 怎麼建：設計全知、建造增量

全部功能集已知且已規格化——所以架構可以**一次切對**。但**不要在出貨前把全部功能都實作完**。
分片建造（見 `docs/50-build-order.md`），每片都落在已經對的架構上。先做 **walking skeleton**：
一顆按鈕 → Rust core 真的做一次登入 → UI 顯示結果。這一個骨架要**同時**證明三個最危險的東西：
async 過 FFI、事件 callback、平台把行程吊活。證明了才加功能。

## 定死 vs 你的裁量

- **定死（照做）**：FFI 契約、命令/事件 schema、能力旗標、領域 API 事實，以及兩個有時限流程
  （自動送答、點名簽到）的硬 UX 不變量。這些做錯會**不可逆地**遺失資料或驚嚇使用者。
- **你設計**：所有視覺——版面、字體、動效、文案。`docs/` 給你畫面清單與每個畫面的職責，長相是你的。
  **別去找 pixel 規格，故意沒有。**

## 硬規矩（why 見 docs/90-conventions.md）

- **Clean-room**：照規格實作。不要去找、不要抄任何既有實作。你的優雅，靠的就是不繼承任何人的積垢。
- **QR 誠實**：純學生端偽造 QR `data` token **尚未被發現**（不是「不可能」）。永遠不要寫
  「不可能／不可偽造」。永遠不要宣稱某個具體的人解出來了——若要提及，一律是「傳聞／都市傳說」。
  唯一自動 QR 路徑是**教師輔助**。永遠不要把手動貼上/掃描包裝成「自動」路徑。
  `docs/32-domain-qr.md` 的負面結果地圖是本專案的長期研究遺產——**保留它**。
- **學校一律平等**：任何學校都不寫死、不特權。所有學校名單一律從 provider registry
  （`docs/40-providers.md`）生成。code 與 docs 皆「列全或都不列」。
- **祕密絕不落明碼**：帳密與 LLM 金鑰進**加密保險庫**，各平台分層解鎖（見架構）。
  永不明碼落地；log／匯出一律遮蔽。
- **iOS 是二等公民，且誠實交代**：同一份 UI code，但背景監控、自動更新、易安裝在 iOS 上都做不到。
  用能力旗標暴露可用性；永遠不要假裝各平台對等。
- **授權**：AGPL-3.0-or-later，出於信念的主動選擇。

就這樣。下一站：`docs/50-build-order.md`。
