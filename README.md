# auto-Tronclass-APP（v2）

> [!WARNING]
> v2 是以 Rust 核心與 .NET MAUI GUI 重寫的實驗性版本，尚不是「任何學校、任何活動都保證可用」的產品。請只在你本人有權限、且符合學校與課程規範的帳號與租戶上使用；自動簽到或自動作答可能違反校規、課程規則或當地法律。你必須自行取得授權並承擔使用結果。

v1 行為基準保留在 [auto-rollcall-thu-tronclass](https://github.com/hot-YUser/auto-rollcall-thu-tronclass)。

## 目前支援範圍

- Windows x64：可攜式資料夾（portable）與開發用的 setup/MSIX 建置路徑；App 存活時執行排程。
- Android：arm64-v8a 與 x86_64 APK；精準鬧鐘喚醒後使用 `dataSync` 前景服務。
- 學生可建立個人 target 與同租戶群組；每個 target 有獨立時間表、手動開始／停止，群組偵測後由各成員自己的 session 執行。
- Rust native core 與 MAUI UI 只以版本化命令及封閉 `MonitoringSnapshot` 溝通；活動使用不透明 `activity_token`，避免只靠租戶內裸 ID。
- 點名與測驗流程仍取決於目標 TronClass 租戶的實際 API、權限與活動設定。QR、教師輔助、LLM、各題型和特殊租戶行為不可在沒有實機契約測試時宣稱「完整支援」。

## 使用流程

1. 從 GitHub Releases 下載對應平台產物，或依下方建置步驟自行建置。
2. 在「帳號」頁新增帳號；新增後會立即驗證，失敗帳號仍會保留供重新驗證或 Cookie 登入。不要把密碼、Cookie、vault 或 API key 貼到 Issue、截圖或聊天中。
3. 在「監控」建立同租戶學生群組，為個人／群組選擇「停用、跟隨全局、自訂」時間表；全局時間表與 Device／Named IANA 時區位於「設定」。
4. 各 target 可個別立即開始／停止；群組重疊時依畫面選擇暫時去重或永久合併。點名、測驗、錯誤與警告會出現在監控、點名／答題分頁及日誌。
5. 「一鍵停止全部」會跨重啟保留；「恢復照時間表」會清除手動 override 並重新計算排程。

## Windows

### 使用 portable zip

將 Releases 的 `AutoTronclass-<tag>-windows-x64-portable.zip` 完整解壓縮至使用者有寫入權限的資料夾，再執行其中的 `Ui.exe`。zip 內的 `.portable` 標記使資料寫入 exe 旁的 `Data\\`；刪除整個解壓縮資料夾即可移除這份 portable 資料（仍請先備份）。不要從 zip 內直接執行，也不要把資料夾放在需要系統管理員權限的唯讀位置。

### setup / MSIX

setup/MSIX 是目前的發行或開發路徑，請以 Releases 的說明為準。未受信任的自簽憑證不代表官方發布；安裝前應檢查檔案來源、版本與簽章。開發者可執行 `tools/package-windows.ps1 -Mode msix -DevelopmentOnly` 產生開發用套件，但這不是可直接對外發布的商業簽章。

### Windows 資料與秘密

- portable：`<Ui.exe 所在資料夾>\\Data\\`。
- 安裝/開發：`%LOCALAPPDATA%\\AutoTronclass\\Data\\`。
- 保險庫（vault）與設定由 Rust 保存；裝置金鑰在 Windows 使用目前使用者的 DPAPI（CurrentUser）保護。設定以原子方式寫入；只對精確舊 schema 先備份再重設，毀損或未來 schema 保持原檔並拒絕啟動。
- portable 只代表資料跟著資料夾走，不代表可跨電腦或跨 Windows 使用者解密。DPAPI 仍綁定原裝置/使用者；搬移前請使用應用程式支援的匯出/復原流程，不要手動複製秘密檔期待能解密。

## Android

Android 使用平台沙盒資料夾與 Android Keystore 產生的 AES-GCM 裝置金鑰；應用程式不把 vault／金鑰加入 Android 自動備份。只有授予「精準鬧鐘」特殊權限時，未來排程邊界才能自動啟動前景服務；未授權時只發可點擊通知。Force-stop 會停用鬧鐘與 receiver，必須重新開啟 App；API 35+ 重新開機若已落在有效時段內也只通知，不由 `BOOT_COMPLETED` 直接啟動 `dataSync`。

Android API 35 以上的 `dataSync` 前景服務有系統配額：每個滾動 24 小時最多執行 6 小時。達到時限時只停止新偵測、保存 platform block、立即結束服務；已取得 mutation 許可的請求不會被宣稱已撤回。使用者回到前景或後續精準鬧鐘成功完成 cold-start clock handshake 後才清除 block。這是平台限制，不能繞過。詳見 [Android 前景服務時限文件](https://developer.android.com/develop/background-work/services/fgs/timeout) 與 [前景服務類型文件](https://developer.android.com/develop/background-work/services/fgs/service-types)。

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

`core/src/assets/quiz_prepared_v1.json` 與 `core/src/assets/monitoring_snapshot_v1.json` 是 Rust、C# parser、Native cache 與 Debug Mock 共用的 golden fixtures。跨邊界變更必須同時更新核心、UI、Mock／檢查器與測試。

## 從原始碼建置與測試

需求：PowerShell 7.4+（發版腳本 `#requires`）、Rust 1.97.1（root `rust-toolchain.toml` 固定，rustup 自動採用）、`cargo-ndk` 4.1.2（Android 建置用）、精確的 .NET SDK 11.0.100-preview.7.26381.103（`tools/release.ps1` 開頭 gate 精確版本）、MAUI workload：`maui-windows` 與 `maui-android` 都要裝（即使只建 Windows，multi-target restore 也需要 `maui-android`；`tools/release.ps1` 會驗證兩者 manifest 精確版本）；Android 建置另需 Android SDK/NDK（精確 27.2.12479018，`build-core.ps1` 解析 `source.properties` 檢查）、JDK 及對應 workload。Rust channel 以 `rust-toolchain.toml` 為唯一規範；其餘工具鏈 pins（.NET SDK、MAUI workload set/manifest、NDK、cargo-ndk）以 `tools/toolchain.json` 為單一規範，CI 與發版腳本都從它讀取，請勿在其他地方重複硬編。工具鏈路徑可用環境變數指定，請勿把金鑰寫入腳本或提交。

```powershell
# Rust 核心格式、單元/整合測試與 lint
cargo fmt --manifest-path core/Cargo.toml --all -- --check
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
# 只驗算版本（DisplayVersion/versionCode/Windows 數值版本），不建置：
./tools/release.ps1 -Tag v2.0.0-alpha.XX -ValidateOnly
# 查看 exact HEAD target、gates 與資產計畫，不建置、不寫 dist、不發布：
./tools/release.ps1 -Tag v2.0.0-alpha.XX -PlanOnly
```

`tools/release.ps1` 需要 Android 簽章資訊與私有 keystore 才能做完整 APK 發行；私鑰不在 Repository。若只做 Windows 開發驗證，可依腳本參數跳過 Android，但跳過的 head 不得被當成完整雙平台發行證據。`tools/checks/DeviceKey.Check` 是無第三方依賴的 Windows 裝置金鑰／遷移與 NativeCore 生命週期 runnable check；CI 與發版也會執行協定 contract check（`ProtocolContract.Check`）與設定頁未儲存編輯保護檢查（`UiSettings.Check`）。

發版閘：git 工作樹必須乾淨（尊重 .gitignore）、`-Tag` 必須是嚴格 SemVer（`v?M.m.p[-alpha|beta|rc.N]`）並由共享公式計算 DisplayVersion／Android versionCode／Windows 數值版本（不依賴 Ui.csproj 手動版本欄位）；預設不要求 git tag 已存在，`-RequireTaggedHead` 才驗證 tag 指向 HEAD。腳本最後只印出待人工執行的 `gh release create`，`--target` 固定為本次建置記錄的 exact HEAD SHA；`-PlanOnly` 可在不建置／不寫入 `dist` 的情況下檢查 target、gates 與資產清單。CI 的 Windows job 會以 `v0.0.0-alpha.0` 跑一次 unsigned release dry-run（簽章密鑰不進 CI）。

## 發行驗證與簽章

正式產物必須由 `tools/release.ps1` 產生並通過：Rust rustfmt/test/clippy、跨平台 production-linked checks、native marker（hash/mtime）、Windows publish native hash、隔離資料 smoke test、APK `apksigner verify`、APK 內兩個 ABI 的 native hash 比對，以及完整資產存在性檢查。腳本另以 `aapt` 核對 APK 的 versionName/versionCode、以 `FileVersionInfo` 核對 Windows `Ui.exe` 的 File/ProductVersion，並產出 `build-metadata.json`（tag/版本/commit/工具鏈）與 `SHA256SUMS.txt`；兩者會和平台產物一起列入待上傳的 GitHub Release 資產。未通過任何一項都不應發布。

目前公開 Android 憑證指紋固定為：

```text
SHA-256: 50F019154AFDFA4CB464339CF7F6D62DD603FAA68136E97FF01E8D7C09FD2CF7
Subject: CN=Auto TronClass, OU=Dev, O=hot-YUser, C=TW
```

指紋只驗證公開憑證，不包含私鑰。若要輪替簽章金鑰，必須在獨立變更中更新 `tools/android-signing.json`、發布說明與升級策略；不能以環境變數靜默改成另一把未審查的金鑰。

## 目前限制

Windows 目前是前景程序模型；關閉視窗就停止監控，不提供 tray／開機常駐。Android 自動喚醒依賴使用者授予精準鬧鐘特殊權限，且仍受 Force-stop、API 35 reboot 限制、`dataSync` 六小時配額與程序生命週期約束。完整 Android release 另依賴本機私有 keystore 與密碼配對；秘密不在 Repository，因此 CI 只驗證未簽 Windows 發行路徑。

## 授權

本專案採 [AGPL-3.0-or-later](LICENSE)。原始 v1/上游授權與著作權聲明仍依 LICENSE 內容保留。再次提醒：授權不等於你取得學校、教師、課程或第三方服務的使用許可；請先取得授權並遵守適用規範。
