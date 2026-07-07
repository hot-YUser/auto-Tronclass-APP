# 10 · 架構（Architecture）

## 一句話

**所有領域邏輯在一個 Rust 核心；UI 是一層笨視圖。兩者之間只有一條窄縫。**

```
┌──────────── .NET MAUI / Avalonia UI（笨視圖層）────────────┐
│  只做三件事：送命令、收事件、畫畫面。零領域邏輯。         │
│  不准有 `if TronClass…`，也不准有 `if iOS…`。            │
└───────────────────────────┬───────────────────────────────┘
                            │  FFI（行程內 P/Invoke）
                            │  UI→core：命令通道
                            │  core→UI：事件 callback（含串流）
┌───────────────────────────┴───────────────────────────────┐
│  Rust core（cdylib）＝一台事件/命令狀態機                  │
│  http · 登入 · 學校registry · 點名×4 · 雷達解算 · LLM答題  │
│  · QR · 設定 · 祕密保管 · 狀態/持久化 · 遮蔽 · 監控迴圈    │
│  · 倒數計時。全 headless，用假伺服器即可離線測。          │
└─────────────────────────────────────────────────────────────┘
```

## 為什麼這樣切

- **一條縫、可測到骨子裡。** core 是純狀態機：餵命令、斷言事件，就能把整條點名/答題流程離線跑完
  （對一個假 TronClass 伺服器）。UI 換掉也不影響 core 的正確性與測試。
- **UI 動盪被隔離。** UI 站在 .NET MAUI 11（GA 前為 Preview）+ Avalonia backend 上，會有 API churn；
  但那些不穩定**塌不到 core**。core + core 測試永遠穩。
- **未來換 UI 技術不用動 core。** 縫是語言無關的；哪天要換原生 Swift/Kotlin head 也只換 UI。

## UI 技術棧（已定案）

- **.NET MAUI** 一套 UI codebase。Win/macOS/iOS/Android 用 MAUI 原生控件（各平台自動最適觀感）。
- **Linux（與 WASM）經 Avalonia MAUI backend 算繪同一份 code**——不 fork MAUI、不加額外 target
  framework、不做魔改。需要 **.NET MAUI 11 + Avalonia 12**（backend GA 目標約與 .NET 11 穩定版同期）。
  在 GA 前，Linux/WASM head 視為「跟著 GA 走的 best-effort」。
- UI 視覺（版面、字體、動效、文案）**由實作者設計**；本 docs 只給畫面清單與各畫面職責（見 `20`），
  不給 pixel 規格。

## 那條 FFI 縫（最關鍵的工程決定）

- **行程內 FFI，不是 IPC。** UI 用 P/Invoke 直接載入 core 的 `cdylib`；**不**開第二個行程、不走
  pipe/socket/gRPC。單一 App、單一行程。
- **型別綁定用 csbindgen 生成**（.NET 團隊為 .NET 做、對 Native AOT 友善）。不手刻 marshalling，
  也不用反射式綁定（AOT 會炸）。
- **介面窄且粗粒度。** 概念上只有兩個方向：
  - **命令通道**：UI → core，粗粒度動作（見 `20-contract.md` 的命令 enum）。
  - **事件 callback**：core → UI，core 主動推事件（狀態變化、偵測到點名、答案備妥、reasoning 串流塊…）。
  型別化**不是**開 200 個方法的藉口；穿過縫的表面要小而穩。
- **真正的難點＝async + 串流過縫。** core 的 I/O（登入、輪詢、LLM）都是 async（tokio）；UI 在 UI thread。
  你要把「C# 的 `await`/Task ↔ Rust 的 future」對起來，還要有一條**反向通道**讓 core 把非請求觸發的
  事件（例如「偵測到點名」）與 **reasoning 串流塊**連續推上來。**這是 walking skeleton 唯一必須先證明
  的東西**（見 `50-build-order.md`）；marshalling 格式（型別 vs JSON 字串）相比之下是小事。

## Core 內部（模組概觀）

core 自己持有一個 **tokio runtime**，跑一條長命的**監控 task**與所有**倒數計時器**。模組（細節見各領域 doc）：

- **http**：帶登入後 session cookie 的 HTTP client；連線探測。
- **login**：與學校無關的登入主流程，**純依偵測到的頁面特徵分流**（有無驗證碼/哪種、SSO 探索、
  公有雲 email SPA、NetIQ 之類）——**分支以協定/特徵命名，絕不以學校命名**（見 `30` 與 `90`）。
- **providers**：學校 registry（`40-providers.md`）。
- **rollcall**：四型點名引擎 + 送後回查（`30-domain-rollcall.md`）。
- **radar solver**：WGS84 多點定位，**零外部數學套件**（`30`）。
- **answer**：測驗答題流程 + LLM 客戶端（`31-domain-autoanswer.md`）。
- **qr**：教師輔助 QR 路徑 + 研究儀器（`32-domain-qr.md`）。
- **config**：結構化設定（見下）。
- **secrets**：`SecretStore`（見下）。
- **state/persistence**：cookie/session 快取、日誌、設定、保險庫的落地。
- **redaction**：單一遮蔽走訪，祕密永不進日誌/匯出。
- **event bus + monitor loop + timers**：把上述串成一台狀態機，對 UI 只吐命令/事件。

## 能力旗標（Capability flags）

平台差異**只在 core 判定、只經旗標暴露**。core 啟動時算出一組布林（`background_monitoring`、
`self_update`、`biometric_unlock`、`qr_teacher_assist`…見 `20`），經 `CapabilityReport` 事件給 UI。
**UI 把按鈕的 enabled 綁到旗標，UI 裡一行平台判斷都沒有。** 要不要能用，是 core 說了算。

## 生命週期：領域存活 vs 行程存活

把兩件事分開：

- **領域存活**＝那條監控迴圈在跑（純 Rust，全平台同一份）。
- **行程存活**＝OS 讓不讓這個行程呼吸：桌面天生活著；**Android 用前景服務**（常駐通知「監控中」）
  吊著；**iOS 只有前景**（切背景就被凍/殺）。

同一條監控迴圈跑遍全平台，差別只在**行程存活由平台授予**。`background_monitoring` 旗標＝「螢幕關了
OS 還讓我活嗎」＝桌面/Android true、iOS false。UI 只是替各平台把「保活」這件事接上（Android 起前景
服務、桌面無需動作、iOS 顯示「僅前景可用」）。

## 祕密保管（跨平台，避免平台工具蔓延）

**以一個可攜加密庫為主幹，平台密鑰庫只當選配解鎖層。** core 對外只有一個 `SecretStore` trait
（`get/set/delete`），UI 與其他模組永遠只呼叫它、看不到平台。

1. **主幹（五平台同碼）**：一個**加密保險庫檔**，authenticated encryption（例如 XChaCha20-Poly1305），
   金鑰用 **Argon2id** 從解鎖來源推導。所有祕密（帳密、LLM 金鑰）只存在這裡，**永不明碼落地**。
2. **解鎖來源（分層降級）**：
   - 平台有可靠密鑰庫（iOS Keychain / Android Keystore / Windows / macOS）→ 把**保險庫的那一把金鑰**
     放進去，達成免密碼/生物辨識解鎖。**平台庫只保管一把 key，不散落 N 個祕密。**
   - 平台沒有可靠密鑰庫（headless Linux / 嵌入式）→ **降級成使用者主密碼**（Argon2id），不爆掉。

起步可以只做「加密庫 + 主密碼」（零平台特定碼）；生物辨識/免密碼解鎖是之後加的選配層。

## 設定（結構化，不是文字檔）

設定是**結構化資料 + 一個 Settings UI**，不是手改的文字檔。core 開 typed config get/set；UI 畫設定頁。
（手機上沒有記事本可改檔，桌面也不該逼使用者編輯 config 文字。）

## 執行緒與可測性

- core 的網路/運算在 tokio 上、不阻塞 UI thread；**事件由 UI 層 marshalling 回 UI thread** 再渲染。
- **core 完全 headless、可離線測**：對一個假 TronClass 伺服器，用命令驅動、斷言事件，覆蓋登入/四型
  點名/答題全流程。純決策邏輯（答題決策、雷達解算）直接單測。
