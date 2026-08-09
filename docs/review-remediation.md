# 第一輪審查修復矩陣

本文件是 `第一輪審查.md` 的現況對照，不是新的審查結論。`confirmed` 代表原報告中的問題已由程式碼重查證；`fixed` 代表目前工作樹已有對應修正；`verified` 只在有可重跑的測試、建置或人工檢查證據時使用；`remaining` 代表仍不能宣稱完成。若一列同時出現多個標記，後者是交付門檻：例如 `fixed / verification pending` 仍不能當成發布證據。

## P1 發版阻斷項目

| 項目 | 原報告結論 | 目前狀態 | 證據與下一步 |
|---|---|---|---|
| P1-1 逐帳號試卷 instance/答案 | confirmed | fixed / verified | `21b0a4f` 將 attempt 與 submitted subjects 分離保存；Rust 既有測試及 Windows/Android UI build 已重跑。仍需在發布前保留多租戶/晚到帳號回歸測試。 |
| P1-2 活動識別被壓成裸 ID | confirmed | fixed / verified | `43ee45a` 導入不可混淆的活動識別與型別化答案；UI 命令以 `activity_token` 尋找活動。需由 protocol contract check 覆蓋同 ID、不同租戶。 |
| P1-3 非 2xx 被當成功、部分提交進度遺失 | confirmed | fixed / locally verified | `525293b` 收斂非 2xx；本輪再以共用 `mutation_checked` 拒絕 2xx 的 `success:false`、`ok:false`、`is_success:false` 與非空 `error_code`，涵蓋一般測驗、classroom 與非 number 點名，且錯誤不洩漏回應 body。2026-08-10 `cargo test --all-targets --all-features`：117 passed / 10 live ignored / 0 failed；clippy 0 warning。 |
| P1-4 QuizPrepared 真實 core 與 C# 契約斷裂 | confirmed | fixed / locally verified | production emitter 已抽成同一路徑的 `quiz_prepared_event`，Rust 測試直接產生事件並與共用 v1 fixture 對照；`ProtocolContract.Check` 直接編譯 production `QuizPreparedContract`/`Models`，並驗證缺少必填欄位、未知或空 typed answer 一律 fail-closed。2026-08-10 runnable check、Windows/Android Release build 全綠；CI 尚待遠端執行。 |
| P1-5 數字型既有答案消失 | confirmed | fixed / verified | `716d70e` 保留數字型既有選項答案；Rust answer tests 已涵蓋該回歸。 |
| P1-6 Start×2 / Start→Stop orphan monitor | confirmed | fixed / verified | `a04262c` 以世代狀態機收斂 core；目前 UI 另有 boot-ready 與 transition guard。需把 Start/Stop 競態測試納入完整 all-features gate。 |
| P1-7 損毀資料被當首次啟動並覆寫 | confirmed | fixed / verified | `3cacbd8`、`f1a2a30` 保留毀損檔並原子保存；`checks/DeviceKey.Check` 已實跑首次啟動、重載、遷移、毀損 envelope/legacy preservation。仍需在 CI 穩定執行。 |
| P1-8 Android dataSync 時限與逾時崩潰 | confirmed | fixed / build verified / device pending | `836b0f3` 改為 NotSticky、OnTimeout、停止時關閉服務；本輪讓 timeout/OnDestroy 共用冪等 best-effort core stop，且通知例外不再阻止 `StopForeground`/`StopSelf`。2026-08-10 Android Release build 0 warning / 0 error；API 35+ 真機的 6 小時/24 小時 timeout 與 process-death 行為仍待實測。 |
| P1-9 舊 native core、smoke 污染真實資料 | confirmed | fixed / Windows release verified | `build-core.ps1`/`release.ps1` 已加入精確輸出清理、marker hash/mtime、publish hash、隔離 smoke、apksigner 與 APK ABI hash。2026-08-10 `dev-local-check4` Windows-only dry-run 通過：117 tests、clippy、兩個 runnable checks、fresh native marker、self-contained publish、5.6 秒實跑、隔離資料 fingerprint、111 MB portable ZIP；signed Android gate 尚未重跑。 |

## 其他高風險項目

| 項目 | 目前狀態 | 證據與限制 |
|---|---|---|
| Android 備份包含金鑰/vault | fixed / verified | `836b0f3` 停用敏感備份規則；仍需在 CI/Android manifest 產物檢查中固定驗證。 |
| unmanaged callback no-throw | fixed / locally verified | `NativeCore` callback 現在隔離例外、佇列事件並在 native callback 外分派；2026-08-10 Windows/Android Release build 均 0 warning / 0 error。re-entry/ordering 壓力測試仍可加強。 |
| Boot 失敗後無法重試 | fixed / verified | `NativeCore` 以 boot task 取代永久 `_booted`，失敗會重置；`AppState` 對 boot failure 顯示錯誤並保持不可啟動直到成功。需加一次明確的 failed-then-retry 測試。 |
| persistence mutation 非交易 | fixed / verified | `ed2230a` 導入無密交易日誌與一致性保存；既有 Rust persistence 測試需在 all-features gate 重跑。 |
| Capability 宣告與實作不符 | fixed / locally verified | self-update capability 已關閉，Mock 與 core 對齊；Rust capability test 與 `ProtocolContract.Check` 於 2026-08-10 通過。 |
| SettingsPage 訂閱生命週期 | fixed / locally verified | SettingsPage 改為 OnAppearing 訂閱、OnDisappearing 全數退訂並使用具名委派；2026-08-10 Windows/Android Release build 均通過。 |
| ISO-8601 parser 不完整 | fixed / verified | parser 現在驗證年月日、閏年、時間範圍、Z/±HH:MM/±HHMM 與 ±14:00 邊界；Rust monitor tests 目前 11 項通過。 |
| LLM body 綁死 NVIDIA/MiniMax 方言 | fixed / verified | `LlmProvider` 只在精確 NVIDIA host 傳 vendor fields；generic OpenAI-compatible request 共用 builder，3 個 provider tests 通過。 |
| README 是 v1 CLI 文件 | fixed | 本文件與 README 已改為 v2 Rust + MAUI GUI 說明；使用者操作、平台限制與證據仍以實際 release gate 為準。 |
| 測試與 CI 斷層 | fixed / verified | `.github/workflows/ci.yml` 已含 Rust test/clippy、Windows production contract/device-key check 與雙平台 build，官方 actions 以 SHA 固定；cargo-ndk 固定為 4.1.2，.NET 11 preview.6 與 MAUI workload set 亦精確固定。2026-08-10 [CI run 31325769517](https://github.com/hot-YUser/auto-Tronclass-APP/actions/runs/31325769517) 三個 jobs 全綠；完整 release artifact identity gate 仍由本機 `release.ps1` dry-run 提供。 |
| 裝置金鑰/秘密記憶體生命週期 | fixed in code / verification pending | Windows DPAPI、Android Keystore、外部 key 注入、毀損隔離及 DeviceKey runnable check 已具備；`AccountSecret`/LLM 字串的完整 zeroization 與跨程序失敗路徑仍需最後審閱。 |

## 多模型證據界線

- Kimi K3：第一輪兩次由供應商路由回傳 `503 auth_unavailable`，沒有可採用的審查內容；不能寫成 Kimi 共識。
- DeepSeek V4 Flash：第一輪角色由平台固定為 `high`，不是本文件可驗證的 `xhigh`；它提供的 late participant、ISO parser、provider 方言等線索，只有在主審重查並有測試後才列入上表。
- DeepSeek 所稱登入表單巢狀節點問題已被主審重查後駁回，不列為 bug。
- 報告中送回 DeepSeek 的 A–E 反駁沒有在有界時間內交付；它們保持 `unverified`，不計為確認，也不計為否定。任何後續模型回覆都必須保存獨立上下文、原始錯誤與可重跑證據，才能更新此矩陣。

## 發布前必要證據

1. `cargo test --all-targets --all-features` 與 `cargo clippy --all-targets --all-features -- -D warnings`。
2. Windows/Android UI build（包含最新 NativeCore、SettingsPage、服務與 fixture 變更）。
3. DeviceKey runnable check、跨邊界 protocol contract check、Start/Stop/partial-submit 回歸測試。
4. `release.ps1` 完整或明確記錄的受限 dry-run：native marker、publish hash、隔離 smoke、APK `apksigner`、固定憑證指紋與兩個 ABI hash。
5. 逐項將本表的 `verification pending` 轉成有命令、日期、commit 與輸出的 `verified`；沒有輸出就保持 pending。
