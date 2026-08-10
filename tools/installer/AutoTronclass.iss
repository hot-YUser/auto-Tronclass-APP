; ═══════════════════════════════════════════════════════════════════════════════════════════
; 自動 Tronclass — Inno Setup 安裝檔（per-user、完全免系統管理員/UAC、免在系統裝任何憑證）。
; 由 release.ps1 呼叫 ISCC 建置，三個 /D 定義帶入：
;   /DMyAppVersion=<tag>   例 v2.0.0-alpha.6（也決定輸出檔名）
;   /DPubDir=<publish>     self-contained 發佈資料夾（與 portable 同來源，但不含 .portable 標記）
;   /DOutDir=<dist>        setup.exe 產出位置
; 未簽章 → 首次執行會有 SmartScreen「不明發行者」提示（按「仍要執行」即可，不涉及任何憑證）。
; ═══════════════════════════════════════════════════════════════════════════════════════════

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0-dev"
#endif
#ifndef PubDir
  #error 需以 /DPubDir=<self-contained 發佈資料夾> 呼叫
#endif
#ifndef OutDir
  #define OutDir "."
#endif

[Setup]
; AppId 固定不變 → 之後版本會就地升級覆蓋、不會裝成多份。★ 切勿更動這個 GUID。
AppId={{7E9C2B14-3A6D-4F82-9C0E-1D5A8B7F4C33}
AppName=自動 Tronclass
AppVersion={#MyAppVersion}
AppPublisher=hot-YUser
AppPublisherURL=https://github.com/hot-YUser/auto-Tronclass-APP
; ── per-user：裝到 %LOCALAPPDATA%\Programs，完全免 UAC ──
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\AutoTronclass
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
; ── 小巧：LZMA2/max + solid，壓到最小 ──
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
OutputDir={#OutDir}
OutputBaseFilename=AutoTronclass-{#MyAppVersion}-windows-x64-setup
UninstallDisplayName=自動 Tronclass
UninstallDisplayIcon={app}\Ui.exe

[Files]
; 整個 self-contained 發佈輸出。不含 .portable 標記 → App 會走 %LOCALAPPDATA%\AutoTronclass\Data。
Source: "{#PubDir}\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion

[Tasks]
Name: "desktopicon"; Description: "建立桌面捷徑"; GroupDescription: "附加捷徑："; Flags: unchecked

[Icons]
Name: "{autoprograms}\自動 Tronclass"; Filename: "{app}\Ui.exe"
Name: "{autodesktop}\自動 Tronclass"; Filename: "{app}\Ui.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\Ui.exe"; Description: "立即啟動 自動 Tronclass"; Flags: nowait postinstall skipifsilent

[Code]
// 解除安裝時，問使用者要不要一併刪掉資料（帳號/設定/金鑰在 %LOCALAPPDATA%\AutoTronclass）。
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  DataDir: String;
begin
  if CurUninstallStep = usUninstall then
  begin
    DataDir := ExpandConstant('{localappdata}\AutoTronclass');
    // 靜默解除安裝(/SILENT、自動化)一律保留資料，避免誤刪帳號；只有互動式解除安裝才詢問。
    if DirExists(DataDir) and (not UninstallSilent) then
    begin
      if MsgBox('要一併刪除你的帳號、設定與金鑰嗎？' + #13#10 +
                '(' + DataDir + ')' + #13#10 + #13#10 +
                '選「否」會保留資料，日後重裝可沿用。',
                mbConfirmation, MB_YESNO) = IDYES then
        DelTree(DataDir, True, True, True);
    end;
  end;
end;
