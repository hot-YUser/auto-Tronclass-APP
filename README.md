# auto-Tronclass-APP（v2）

> [!WARNING]
> v2 是以 Rust 核心與 .NET MAUI GUI 重寫的實驗性版本，尚不是「任何學校、任何活動都保證可用」的產品。請只在你本人有權限、且符合學校與課程規範的帳號與租戶上使用；自動簽到或自動作答可能違反校規、課程規則或當地法律。你必須自行取得授權並承擔使用結果。

v1 行為基準保留在 [auto-rollcall-thu-tronclass](https://github.com/hot-YUser/auto-rollcall-thu-tronclass)。

## 目前支援範圍

- Windows x64：可攜式資料夾（portable）與開發用的 setup/MSIX 建置路徑。
- Android：arm64-v8a 與 x86_64 APK；監控時使用 Android `dataSync` 前景服務。
- Rust native core 與 MAUI UI 透過版本化 JSON/FFI 事件溝通；活動使用不透明 `activity_token`，避免只靠租戶內裸 ID。
- 點名與測驗流程仍取決於目標 TronClass 租戶的實際 API、權限與活動設定。QR、教師輔助、LLM、各題型和特殊租戶行為不可在沒有實機契約測試時宣稱「完整支援」。

## 使用流程

1. 從 GitHub Releases 下載對應平台產物，或依下方建置步驟自行建置。
2. 首次啟動，在「帳號」頁新增有權限的 TronClass 帳號；不要把密碼、Cookie、vault 或 API key 貼到 Issue、截圖或聊天中。
3. 在「設定」頁選擇監控時段、點名門檻與答題/LLM 選項。LLM 是可選功能；沒有有效金鑰時應略過答題，不會以空白答案提交。
4. 回到首頁按「開始監控」。點名、測驗、錯誤與警告會出現在首頁、點名/答題分頁及日誌；需要人工決定的活動可在詳細頁介入。
5. 需要停止時按「停止監控」。停止後應確認 UI 狀態回到待命；若 Android 曾觸發系統時限，重新開啟 App 後由使用者再次啟動。

## Windows

### 使用 portable zip

將 Releases 的 `AutoTronclass-<tag>-windows-x64-portable.zip` 完整解壓縮至使用者有寫入權限的資料夾，再執行其中的 `Ui.exe`。zip 內的 `.portable` 標記使資料寫入 exe 旁的 `Data\\`；刪除整個解壓縮資料夾即可移除這份 portable 資料（仍請先備份）。不要從 zip 內直接執行，也不要把資料夾放在需要系統管理員權限的唯讀位置。

### setup / MSIX

setup/MSIX 是目前的發行或開發路徑，請以 Releases 的說明為準。未受信任的自簽憑證不代表官方發布；安裝前應檢查檔案來源、版本與簽章。開發者可執行 `tools/package-windows.ps1 -Mode msix -DevelopmentOnly` 產生開發用套件，但這不是可直接對外發布的商業簽章。

### Windows 資料與秘密

- portable：`<Ui.exe 所在資料夾>\\Data\\`。
- 安裝/開發：`%LOCALAPPDATA%\\AutoTronclass\\Data\\`。
- 保險庫（vault）與設定由 Rust 保存；裝置金鑰在 Windows 使用目前使用者的 DPAPI（CurrentUser）保護，另以原子寫入與毀損隔離避免把壞檔當成首次啟動。
- portable 只代表資料跟著資料夾走，不代表可跨電腦或跨 Windows 使用者解密。DPAPI 仍綁定原裝置/使用者；搬移前請使用應用程式支援的匯出/復原流程，不要手動複製秘密檔期待能解密。

## Android

Android 使用平台沙盒資料夾與 Android Keystore 產生的 AES-GCM 裝置金鑰；應用程式不把 vault/金鑰加入 Android 自動備份。前景服務會顯示持續通知，程序死亡或服務逾時後不會偷偷恢復監控。

Android API 35 以上的 `dataSync` 前景服務有系統配額：每個滾動 24 小時最多執行 6 小時。達到時限時，服務會停止並通知使用者；請開啟 App、確認狀態，再由使用者手動重新啟動。這是平台限制，不是可以由本專案保證繞過的錯誤。詳見 [Android 前景服務時限文件](https://developer.android.com/develop/background-work/services/fgs/timeout) 與 [前景服務類型文件](https://developer.android.com/develop/background-work/services/fgs/service-types)。

## LLM 設定

LLM 只在你明確啟用自動答題並透過 UI 將合法的 provider 金鑰保存至加密 vault 時使用。預設可使用 NVIDIA NIM 相容端點；目前通用 OpenAI-compatible 端點只送標準欄位，NVIDIA 專用欄位只對確定的 NVIDIA host 啟用。金鑰禁止提交至 Git。

自動答題涉及真實成績活動，請先確認教師/學校允許。若題目、答案型別、租戶或 HTTP 狀態不符合契約，應保留可見錯誤並停止該次提交，而不是把錯誤當成成功。

## 架構

```text
MAUI Pages / ViewModels
        │ 事件、命令與版本化 JSON
        ▼
NativeCore（C# FFI 邊界、佇列化 callback、生命週期）
        │ C ABI
        ▼
Rust core（登入、持久化、監控、點名、測驗、LLM、redaction）
        │
        ├─ Windows: tronclass_core.dll
        └─ Android: libtronclass_core.so（arm64-v8a / x86_64）
```

`core/src/assets/quiz_prepared_v1.json` 是 Rust 與 C# 共用的 golden fixture。跨邊界變更必須同時更新核心、UI、Mock/檢查器與測試，不應只修改其中一端。

## 從原始碼建置與測試

需求：Rust stable、可用的 .NET SDK（目前專案 target .NET 11 preview）、Windows MAUI workload；Android 建置另需 Android SDK/NDK、JDK 及對應 workload。工具鏈路徑可用環境變數指定，請勿把金鑰寫入腳本或提交。

```powershell
# Rust 核心單元/整合測試與 lint
cargo test --manifest-path core/Cargo.toml --locked --all-targets --all-features
cargo clippy --manifest-path core/Cargo.toml --locked --all-targets --all-features -- -D warnings

# 產生目前平台的 native core
./tools/build-core.ps1 -Head windows
./tools/build-core.ps1 -Head android

# Windows 開發建置（實際 TFM 以目前 SDK 與 Ui.csproj 為準）
dotnet build ui/Ui.csproj -f net11.0-windows10.0.19041.0 -c Debug

# Android 開發建置
dotnet build ui/Ui.csproj -f net11.0-android -c Debug

# 發行流程：測試、雙 head、產物 hash/簽章/smoke gate
./tools/release.ps1 -Tag v2.0.0-alpha.XX
```

`tools/release.ps1` 需要 Android 簽章資訊與私有 keystore 才能做完整 APK 發行；私鑰不在 Repository。若只做 Windows 開發驗證，可依腳本參數跳過 Android，但跳過的 head 不得被當成完整雙平台發行證據。`tools/checks/DeviceKey.Check` 是無第三方依賴的 Windows 裝置金鑰/遷移 runnable check；CI 也應執行它與協定 contract check。

## 發行驗證與簽章

正式產物必須由 `tools/release.ps1` 產生並通過：Rust test/clippy、native marker（hash/mtime）、Windows publish native hash、隔離資料 smoke test、APK `apksigner verify`、APK 內兩個 ABI 的 native hash 比對，以及完整資產存在性檢查。未通過任何一項都不應發布。

目前公開 Android 憑證指紋固定為：

```text
SHA-256: 50F019154AFDFA4CB464339CF7F6D62DD603FAA68136E97FF01E8D7C09FD2CF7
Subject: CN=Auto TronClass, OU=Dev, O=hot-YUser, C=TW
```

指紋只驗證公開憑證，不包含私鑰。若要輪替簽章金鑰，必須在獨立變更中更新 `tools/android-signing.json`、發布說明與升級策略；不能以環境變數靜默改成另一把未審查的金鑰。

## 審查與目前限制

第一輪審查的逐項證據矩陣見 [`docs/review-remediation.md`](docs/review-remediation.md)。原始審查檔 [`第一輪審查.md`](第一輪審查.md) 是歷史紀錄，不會被本 README 重新改寫；矩陣只記錄目前可由提交、測試或程式碼重查證的狀態。沒有證據的 DeepSeek/Kimi 反駁、以及報告中未交付的 A–E 命題，都保持 `unverified`，不會被寫成共識。

## 授權

本專案採 [AGPL-3.0-or-later](LICENSE)。原始 v1/上游授權與著作權聲明仍依 LICENSE 內容保留。再次提醒：授權不等於你取得學校、教師、課程或第三方服務的使用許可；請先取得授權並遵守適用規範。
