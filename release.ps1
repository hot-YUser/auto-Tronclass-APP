#!/usr/bin/env pwsh
# ═══════════════════════════════════════════════════════════════════════════════════════════
# 一鍵發版建置器（v2）：clean → 建原生核心 → 建雙 head → 簽 APK → 打包 → **實跑驗證 release
# self-contained Ui.exe 開得起來** → 把資產放進 dist\。**本腳本只建置+驗證+備妥，不發布**
# （`gh release create` 是使用者決定、另外一步）。用途＝把發版流程固定化，降 token、加速、
# 並把踩過的雷變成硬性 gate。工具鏈路徑見記憶 v2-build-env-setup。
#
#   ./release.ps1 -Tag v2.0.0-alpha.4                  # 兩平台都建
#   ./release.ps1 -Tag v2.0.0-alpha.4 -SkipAndroid     # 只建 Windows
#
# 前置：先手動 bump ui/Ui.csproj 的 <ApplicationVersion>（Android versionCode，每版 +1）；
#       keystore.properties（gitignored）需含 storeFile/storePassword/keyAlias/keyPassword。
# ═══════════════════════════════════════════════════════════════════════════════════════════
param(
    [Parameter(Mandatory)] [string]$Tag,   # 例 v2.0.0-alpha.4
    [switch]$SkipAndroid,
    [switch]$SkipWindows
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

# ── 工具鏈（此機；見 v2-build-env-setup）──
$dotnet = Join-Path $env:LOCALAPPDATA "Microsoft\dotnet\dotnet.exe"   # net11 preview、user-local
if (-not (Test-Path $dotnet)) { $dotnet = "dotnet" }
$env:PATH = "$env:USERPROFILE\.cargo\bin;$(Split-Path $dotnet);$env:PATH"
$env:JAVA_HOME = "C:\Program Files\Android\openjdk\jdk-21.0.8"
$env:ANDROID_HOME = "${env:ProgramFiles(x86)}\Android\android-sdk"
$env:ANDROID_NDK_HOME = Join-Path $env:ANDROID_HOME "ndk\27.2.12479018"
$env:DOTNET_CLI_TELEMETRY_OPTOUT = "1"

$winTfm  = "net11.0-windows10.0.19041.0"
$winName = "AutoTronclass-$Tag-windows-x64-portable"
$apkName = "AutoTronclass-$Tag-android.apk"
$dist = Join-Path $root "dist"; New-Item -ItemType Directory -Force $dist | Out-Null
function Step($m){ Write-Host "`n=== $m ===" -ForegroundColor Cyan }
Add-Type -AssemblyName System.IO.Compression.FileSystem

# ── 1. 清乾淨（alpha.3 之殤：throwaway PublishTrimmed 汙染 publish 目錄 → 壞 self-contained）──
Step "clean ui/bin + ui/obj"
foreach($d in "ui\bin","ui\obj"){ if(Test-Path $d){ [System.IO.Directory]::Delete((Resolve-Path $d),$true) } }

# ── 2. 原生核心（opt-z release；build-core.ps1 出 win dll，-Head android 出 .so）──
Step "build native core — windows dll"
& (Join-Path $root "build-core.ps1")
if (-not $SkipAndroid) { Step "build native core — android .so"; & (Join-Path $root "build-core.ps1") -Head android }

# ── 3. Windows portable（self-contained）+ 兩道 gate ──
if (-not $SkipWindows) {
    Step "publish Windows portable (self-contained)"
    & $dotnet publish ui/Ui.csproj -f $winTfm -c Release -r win-x64 --self-contained -p:PackageMode=portable -p:WindowsAppSDKSelfContained=true
    if ($LASTEXITCODE -ne 0) { throw "windows publish 失敗" }
    $pub = "ui\bin\Release\$winTfm\win-x64\publish"

    # GATE A：trim-stub 守門——S.R.IS.dll 必須是完整 facade（~112KB），不是被 trim 的 32KB。
    $sris = (Get-Item "$pub\System.Runtime.InteropServices.dll").Length
    if ($sris -lt 90000) { throw "System.Runtime.InteropServices.dll 只有 $sris bytes（疑 trim 汙染）— 中止，別發壞包" }

    # GATE B：**實跑 self-contained Ui.exe 必須開得起來**（Debug≠Release；alpha.3 就是漏了這關）。
    Step "smoke-test: release Ui.exe 必須開得起來"
    $p = Start-Process "$pub\Ui.exe" -PassThru -WorkingDirectory (Resolve-Path $pub)
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while (-not $p.HasExited -and $p.MainWindowHandle -eq 0 -and $sw.Elapsed.TotalSeconds -lt 90) { Start-Sleep -Milliseconds 250; $p.Refresh() }
    Start-Sleep 3
    if ($p.HasExited) { throw ("release Ui.exe 啟動即崩（exit 0x{0:X8}）— 中止，別發壞包" -f $p.ExitCode) }
    Stop-Process -Id $p.Id -Force
    Write-Host ("  ✓ Ui.exe 開得起來（視窗 {0:N1}s；冷啟首次含 Defender 掃檔較久）" -f $sw.Elapsed.TotalSeconds) -ForegroundColor Green

    Step "zip Windows portable"
    $stage = Join-Path $dist $winName
    if(Test-Path $stage){ [System.IO.Directory]::Delete((Resolve-Path $stage),$true) }
    Copy-Item $pub $stage -Recurse -Force
    $zip = Join-Path $dist "$winName.zip"
    if(Test-Path $zip){ [System.IO.File]::Delete((Resolve-Path $zip).Path) }
    [System.IO.Compression.ZipFile]::CreateFromDirectory((Resolve-Path $stage), (Join-Path $dist "$winName.zip"), [System.IO.Compression.CompressionLevel]::Optimal, $true)
    [System.IO.Directory]::Delete((Resolve-Path $stage),$true)
    Write-Host ("  ✓ dist\$winName.zip ({0:N0} MB)" -f ((Get-Item $zip).Length/1MB)) -ForegroundColor Green
}

# ── 4. 簽章 Android APK（keystore.properties；keystore 傳絕對路徑，否則 XA4310）──
if (-not $SkipAndroid) {
    Step "publish signed Android APK"
    if (-not (Test-Path "keystore.properties")) { throw "缺 keystore.properties（簽章用）" }
    $kp = @{}; Get-Content "keystore.properties" | Where-Object {$_ -match '^\w+='} | ForEach-Object { $k,$v = $_ -split '=',2; $kp[$k.Trim()]=$v.Trim() }
    $ksAbs = (Resolve-Path $kp.storeFile).Path
    & $dotnet publish ui/Ui.csproj -f net11.0-android -c Release -p:AndroidKeyStore=true -p:AndroidSigningKeyStore="$ksAbs" -p:AndroidSigningStorePass=$($kp.storePassword) -p:AndroidSigningKeyAlias=$($kp.keyAlias) -p:AndroidSigningKeyPass=$($kp.keyPassword)
    if ($LASTEXITCODE -ne 0) { throw "android publish 失敗" }
    $apk = Get-ChildItem "ui\bin\Release\net11.0-android\publish" -Filter "*-Signed.apk" | Select-Object -First 1
    if (-not $apk) { throw "沒產出簽章 APK" }
    Copy-Item $apk.FullName (Join-Path $dist $apkName) -Force
    Write-Host ("  ✓ dist\$apkName ({0:N0} MB)" -f ($apk.Length/1MB)) -ForegroundColor Green
}

# ── 完成：資產備妥，尚未發布 ──
Step "資產備妥於 dist\（尚未發布）"
Get-ChildItem $dist -Filter "*$Tag*" -File | ForEach-Object { "  {0}  {1:N0} MB" -f $_.Name, ($_.Length/1MB) }
Write-Host "`n下一步（使用者決定後）：" -ForegroundColor Yellow
Write-Host "  gh release create $Tag --repo hot-YUser/auto-Tronclass-APP --prerelease --target main ``"
Write-Host "    --title `"$Tag`" --notes-file dist/RELEASE_NOTES-$Tag.md ``"
Write-Host "    dist/$winName.zip dist/$apkName"
