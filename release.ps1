#!/usr/bin/env pwsh
# 一鍵發版建置器：測試 → 雙 head 原生核心 → Windows/Android 發行產物 → 驗證與打包。
# 本腳本只建置與驗證，不發布；缺少 APK 固定 fingerprint 或簽章工具時必定失敗。
#
#   ./release.ps1 -Tag v2.0.0-alpha.4
#   ./release.ps1 -Tag v2.0.0-alpha.4 -SkipAndroid
#
# Android 私鑰仍由 keystore.properties 提供；公開 SHA-256 憑證指紋固定在
# release/android-signing.json，不能以環境變數靜默換掉。

param(
    [Parameter(Mandatory)] [string]$Tag,
    [switch]$SkipAndroid,
    [switch]$SkipWindows,
    [switch]$SkipInstaller,
    [string]$ExpectedApkFingerprint = $env:ANDROID_APK_FINGERPRINT
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

if ($Tag -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$' -or $Tag -in '.', '..') {
    throw "Tag 只能由 1–64 個英數、點、底線或連字號組成，且不得是路徑片段：$Tag"
}
if ($SkipWindows -and $SkipAndroid) {
    throw "不能同時跳過 Windows 與 Android；至少必須驗證一個正式發行 head。"
}

function Step([string]$Message) {
    Write-Host "`n=== $Message ===" -ForegroundColor Cyan
}

function Invoke-Native {
    param(
        [Parameter(Mandatory)] [string]$FilePath,
        [Parameter(Mandatory)] [object[]]$Arguments,
        [Parameter(Mandatory)] [string]$FailureMessage
    )

    try {
        & $FilePath @Arguments
    }
    catch {
        throw "$FailureMessage（無法執行：$($_.Exception.Message)）"
    }
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        throw "$FailureMessage（退出碼 $exitCode）"
    }
}

function Read-NativeMarker {
    param(
        [Parameter(Mandatory)] [string]$Path,
        [Parameter(Mandatory)] [string]$ExpectedHead
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "缺少 $ExpectedHead 原生核心建置 marker：$Path"
    }
    try { $marker = Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json }
    catch { throw "$ExpectedHead 原生核心建置 marker 格式錯誤：$Path" }
    if ($marker.schema -ne 1 -or $marker.head -ne $ExpectedHead -or [string]::IsNullOrWhiteSpace([string]$marker.buildId)) {
        throw "$ExpectedHead 原生核心建置 marker 不完整：$Path"
    }
    foreach ($artifact in @($marker.artifacts)) {
        $artifactPath = Join-Path $root ([string]$artifact.path)
        if (-not (Test-Path -LiteralPath $artifactPath -PathType Leaf)) {
            throw "$ExpectedHead 原生核心 marker 指向的檔案不存在：$artifactPath"
        }
        $actualHash = (Get-FileHash -LiteralPath $artifactPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne ([string]$artifact.sha256).ToLowerInvariant()) {
            throw "$ExpectedHead 原生核心 hash 與 marker 不符：$artifactPath"
        }
        $actualMtime = (Get-Item -LiteralPath $artifactPath).LastWriteTimeUtc
        $markedMtime = [DateTime]::Parse([string]$artifact.mtimeUtc).ToUniversalTime()
        # 建置後可能有 linker/防毒程式觸碰檔案時間；只接受不早於 marker 的檔案，hash 仍須完全相同。
        if ($actualMtime -lt $markedMtime) {
            throw "$ExpectedHead 原生核心 mtime 早於 marker：$artifactPath"
        }
    }
    if (@($marker.artifacts).Count -eq 0) { throw "$ExpectedHead 原生核心 marker 沒有 artifact" }
    return $marker
}

function Normalize-Fingerprint([string]$Fingerprint) {
    if ([string]::IsNullOrWhiteSpace($Fingerprint)) { return "" }
    return (($Fingerprint -replace '(?i)^sha-?256\s*:\s*', '') -replace '[^0-9A-Fa-f]', '').ToUpperInvariant()
}

function Find-ApkSigner {
    $sdk = if (-not [string]::IsNullOrWhiteSpace($env:ANDROID_HOME)) { $env:ANDROID_HOME } else { $env:ANDROID_SDK_ROOT }
    if ([string]::IsNullOrWhiteSpace($sdk)) { return $null }
    $buildTools = Join-Path $sdk "build-tools"
    if (-not (Test-Path -LiteralPath $buildTools -PathType Container)) { return $null }
    return Get-ChildItem -LiteralPath $buildTools -Directory |
        Sort-Object Name -Descending |
        ForEach-Object { Join-Path $_.FullName "apksigner.bat" } |
        Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
        Select-Object -First 1
}

function Resolve-ExistingDirectory {
    param(
        [Parameter(Mandatory)] [string]$Label,
        [Parameter(Mandatory)] [object[]]$Candidates
    )
    foreach ($candidate in $Candidates) {
        if (-not [string]::IsNullOrWhiteSpace([string]$candidate) -and
            (Test-Path -LiteralPath ([string]$candidate) -PathType Container)) {
            return (Resolve-Path -LiteralPath ([string]$candidate)).Path
        }
    }
    throw "找不到 $Label；請先設定對應環境變數或安裝工具鏈。"
}

function Get-DirectoryFingerprint([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) { return "<missing>" }
    $records = Get-ChildItem -LiteralPath $Path -File -Recurse | Sort-Object FullName | ForEach-Object {
        $relative = [IO.Path]::GetRelativePath($Path, $_.FullName)
        $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
        "$relative|$($_.Length)|$hash"
    }
    $bytes = [Text.Encoding]::UTF8.GetBytes(($records -join "`n"))
    try {
        $sha = [Security.Cryptography.SHA256]::Create()
        return [Convert]::ToHexString($sha.ComputeHash($bytes))
    }
    finally {
        if ($sha) { $sha.Dispose() }
        [Security.Cryptography.CryptographicOperations]::ZeroMemory($bytes)
    }
}

function Get-ZipEntrySha256 {
    param(
        [Parameter(Mandatory)] [IO.Compression.ZipArchive]$Archive,
        [Parameter(Mandatory)] [string]$EntryName
    )
    $entry = $Archive.GetEntry($EntryName)
    if (-not $entry -or $entry.Length -le 0) { throw "APK 缺少或包含空的 $EntryName" }
    $stream = $entry.Open()
    try {
        $sha = [Security.Cryptography.SHA256]::Create()
        try { return [Convert]::ToHexString($sha.ComputeHash($stream)).ToLowerInvariant() }
        finally { $sha.Dispose() }
    }
    finally { $stream.Dispose() }
}

function Assert-ApkNativeHashes {
    param([Parameter(Mandatory)] $Marker, [Parameter(Mandatory)] [string]$ApkPath)
    $expected = @{}
    foreach ($artifact in @($Marker.artifacts)) {
        $path = ([string]$artifact.path).Replace('\', '/')
        if ($path -match '/jniLibs/(arm64-v8a|x86_64)/libtronclass_core\.so$') {
            $expected[$Matches[1]] = ([string]$artifact.sha256).ToLowerInvariant()
        }
    }
    foreach ($abi in 'arm64-v8a', 'x86_64') {
        if (-not $expected.ContainsKey($abi)) { throw "Android marker 缺少 $abi 原生核心" }
    }
    $archive = [IO.Compression.ZipFile]::OpenRead($ApkPath)
    try {
        foreach ($abi in 'arm64-v8a', 'x86_64') {
            $actual = Get-ZipEntrySha256 -Archive $archive -EntryName "lib/$abi/libtronclass_core.so"
            if ($actual -ne $expected[$abi]) {
                throw "APK 內 $abi 原生核心不是本次建置產物"
            }
        }
    }
    finally { $archive.Dispose() }
}

function Clear-HeadBuildOutput([string]$TargetFramework) {
    foreach ($path in @(
        (Join-Path $root "ui\bin\Release\$TargetFramework"),
        (Join-Path $root "ui\obj\Release\$TargetFramework")
    )) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Recurse -Force }
    }
}

function Assert-PublishedNativeHash {
    param([Parameter(Mandatory)] $Marker, [Parameter(Mandatory)] [string]$PublishedPath)
    if (-not (Test-Path -LiteralPath $PublishedPath -PathType Leaf)) {
        throw "Windows publish 缺少原生核心：$PublishedPath"
    }
    $expected = (@($Marker.artifacts) | Where-Object { $_.path -like "*tronclass_core.dll" } | Select-Object -First 1).sha256
    $actual = (Get-FileHash -LiteralPath $PublishedPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ([string]::IsNullOrWhiteSpace([string]$expected) -or $actual -ne ([string]$expected).ToLowerInvariant()) {
        throw "Windows publish 包含的原生核心不是本次建置產物：$PublishedPath"
    }
}

# ── 工具鏈 ──
$dotnet = Join-Path $env:LOCALAPPDATA "Microsoft\dotnet\dotnet.exe"
if (-not (Test-Path -LiteralPath $dotnet -PathType Leaf)) { $dotnet = "dotnet" }
$dotnetDir = Split-Path $dotnet -Parent
$env:PATH = "$env:USERPROFILE\.cargo\bin;$dotnetDir;$env:PATH"
$env:DOTNET_CLI_TELEMETRY_OPTOUT = "1"

if (-not $SkipAndroid) {
    $javaCandidates = @()
    if (-not [string]::IsNullOrWhiteSpace($env:JAVA_HOME)) { $javaCandidates += $env:JAVA_HOME }
    foreach ($base in @($env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:LOCALAPPDATA)) {
        if (-not [string]::IsNullOrWhiteSpace($base)) {
            $javaRoot = Join-Path $base "Android\openjdk"
            if (Test-Path -LiteralPath $javaRoot -PathType Container) {
                $javaCandidates += Get-ChildItem -LiteralPath $javaRoot -Directory | Sort-Object Name -Descending | ForEach-Object FullName
            }
        }
    }
    $env:JAVA_HOME = Resolve-ExistingDirectory -Label "JAVA_HOME（Android JDK）" -Candidates $javaCandidates

    $sdkCandidates = @()
    foreach ($value in @($env:ANDROID_HOME, $env:ANDROID_SDK_ROOT)) {
        if (-not [string]::IsNullOrWhiteSpace($value)) { $sdkCandidates += $value }
    }
    if (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) { $sdkCandidates += (Join-Path $env:LOCALAPPDATA "Android\sdk") }
    if (-not [string]::IsNullOrWhiteSpace(${env:ProgramFiles(x86)})) { $sdkCandidates += (Join-Path ${env:ProgramFiles(x86)} "Android\android-sdk") }
    $env:ANDROID_HOME = Resolve-ExistingDirectory -Label "Android SDK" -Candidates $sdkCandidates
    $env:ANDROID_SDK_ROOT = $env:ANDROID_HOME

    $ndkCandidates = @()
    if (-not [string]::IsNullOrWhiteSpace($env:ANDROID_NDK_HOME)) { $ndkCandidates += $env:ANDROID_NDK_HOME }
    $ndkRoot = Join-Path $env:ANDROID_HOME "ndk"
    if (Test-Path -LiteralPath $ndkRoot -PathType Container) {
        $ndkCandidates += Get-ChildItem -LiteralPath $ndkRoot -Directory | Sort-Object Name -Descending | ForEach-Object FullName
    }
    $env:ANDROID_NDK_HOME = Resolve-ExistingDirectory -Label "Android NDK" -Candidates $ndkCandidates
}

$core = Join-Path $root "core"
$buildCore = Join-Path $root "build-core.ps1"
$winTfm = "net11.0-windows10.0.19041.0"
$winName = "AutoTronclass-$Tag-windows-x64-portable"
$setupName = "AutoTronclass-$Tag-windows-x64-setup"
$apkName = "AutoTronclass-$Tag-android.apk"
$dist = Join-Path $root "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null
Add-Type -AssemblyName System.IO.Compression.FileSystem

# marker 放在暫存區，不會混入發行資產；每次執行使用新的 GUID。
$markerRoot = Join-Path ([IO.Path]::GetTempPath()) ("AutoTronclass-release-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $markerRoot | Out-Null
$winMarkerPath = Join-Path $markerRoot "windows.json"
$androidMarkerPath = Join-Path $markerRoot "android.json"

Step "cargo test"
Invoke-Native -FilePath "cargo" -Arguments @("test", "--manifest-path", "$core/Cargo.toml", "--all-targets", "--all-features") -FailureMessage "Rust cargo test 失敗"
Step "cargo clippy"
Invoke-Native -FilePath "cargo" -Arguments @("clippy", "--manifest-path", "$core/Cargo.toml", "--all-targets", "--all-features", "--", "-D", "warnings") -FailureMessage "Rust cargo clippy 失敗"

if (-not $SkipWindows) {
    # 兩個檢查直接連結 production source，防止 OS key 與 Rust↔C# wire contract 在發版前漂移。
    foreach ($check in @(
        @{ Name = "DeviceKey"; Path = (Join-Path $root "checks\DeviceKey.Check\DeviceKey.Check.csproj") },
        @{ Name = "ProtocolContract"; Path = (Join-Path $root "checks\ProtocolContract.Check\ProtocolContract.Check.csproj") }
    )) {
        if (-not (Test-Path -LiteralPath $check.Path -PathType Leaf)) {
            throw "缺少 $($check.Name) 可執行檢查：$($check.Path)"
        }
        Step "$($check.Name) 可執行檢查"
        # 檢查以已發布的 net10 LTS API 面建置；發行機只有 net11 preview 時允許向前執行。
        $oldRollForward = $env:DOTNET_ROLL_FORWARD
        $env:DOTNET_ROLL_FORWARD = "Major"
        try {
            Invoke-Native -FilePath $dotnet -Arguments @("run", "--project", $check.Path, "-c", "Release") -FailureMessage "$($check.Name) 可執行檢查失敗"
        }
        finally { $env:DOTNET_ROLL_FORWARD = $oldRollForward }
    }
}

# ── 原生核心：build-core 會先刪除精確輸出，並寫 hash/mtime/build marker ──
if (-not $SkipWindows) {
    Step "build native core — windows dll"
    & $buildCore -Head windows -BuildMarkerPath $winMarkerPath
    if ($LASTEXITCODE -ne 0) { throw "Windows 原生核心腳本失敗（退出碼 $LASTEXITCODE）" }
    $winMarker = Read-NativeMarker -Path $winMarkerPath -ExpectedHead "windows"
}
if (-not $SkipAndroid) {
    Step "build native core — android .so"
    & $buildCore -Head android -BuildMarkerPath $androidMarkerPath
    if ($LASTEXITCODE -ne 0) { throw "Android 原生核心腳本失敗（退出碼 $LASTEXITCODE）" }
    $androidMarker = Read-NativeMarker -Path $androidMarkerPath -ExpectedHead "android"
}

# ── Windows portable + smoke ──
if (-not $SkipWindows) {
    Step "publish Windows portable (self-contained)"
    Clear-HeadBuildOutput -TargetFramework $winTfm
    Invoke-Native -FilePath $dotnet -Arguments @(
        "publish", "ui/Ui.csproj", "-f", $winTfm, "-c", "Release", "-r", "win-x64", "--self-contained",
        "-p:PackageMode=portable", "-p:WindowsAppSDKSelfContained=true"
    ) -FailureMessage "Windows publish 失敗"
    $pub = Join-Path $root "ui\bin\Release\$winTfm\win-x64\publish"
    if (-not (Test-Path -LiteralPath $pub -PathType Container)) { throw "Windows publish 目錄不存在：$pub" }
    Assert-PublishedNativeHash -Marker $winMarker -PublishedPath (Join-Path $pub "tronclass_core.dll")

    $srisPath = Join-Path $pub "System.Runtime.InteropServices.dll"
    if (-not (Test-Path -LiteralPath $srisPath -PathType Leaf)) { throw "Windows publish 缺少 System.Runtime.InteropServices.dll" }
    $sris = (Get-Item -LiteralPath $srisPath).Length
    if ($sris -lt 90000) { throw "System.Runtime.InteropServices.dll 只有 $sris bytes（疑 trim 汙染）— 中止" }

    Step "smoke-test: release Ui.exe（獨立暫存資料）"
    $smokeRoot = Join-Path ([IO.Path]::GetTempPath()) ("AutoTronclass-smoke-" + [guid]::NewGuid().ToString("N"))
    $realData = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)) "AutoTronclass\Data"
    $realDataBefore = Get-DirectoryFingerprint -Path $realData
    New-Item -ItemType Directory -Force -Path $smokeRoot | Out-Null
    $smokeProcess = $null
    try {
        # 複製 publish 目錄的內容（而不是多一層 publish），讓 Ui.exe 與 .portable 同層。
        Copy-Item -Path (Join-Path $pub "*") -Destination $smokeRoot -Recurse -Force
        $smokeData = Join-Path $smokeRoot "Data"
        New-Item -ItemType Directory -Force -Path $smokeData | Out-Null
        Set-Content -LiteralPath (Join-Path $smokeRoot ".portable") -Encoding UTF8 -Value "release smoke isolated data"
        $smokeLocalAppData = Join-Path $smokeRoot "LocalAppData"
        $smokeAppData = Join-Path $smokeRoot "AppData"
        New-Item -ItemType Directory -Force -Path $smokeLocalAppData, $smokeAppData | Out-Null
        $smokeExe = Join-Path $smokeRoot "Ui.exe"
        $smokeProcess = Start-Process -FilePath $smokeExe -PassThru -WorkingDirectory $smokeRoot -Environment @{
            LOCALAPPDATA = $smokeLocalAppData
            APPDATA       = $smokeAppData
            USERPROFILE   = $smokeRoot
        }
        $sw = [Diagnostics.Stopwatch]::StartNew()
        while (-not $smokeProcess.HasExited -and $smokeProcess.MainWindowHandle -eq 0 -and $sw.Elapsed.TotalSeconds -lt 90) {
            Start-Sleep -Milliseconds 250
            $smokeProcess.Refresh()
        }
        Start-Sleep -Seconds 3
        if ($smokeProcess.HasExited) {
            throw ("release Ui.exe 啟動即崩（exit 0x{0:X8}）— 中止" -f $smokeProcess.ExitCode)
        }
        if ($smokeProcess.MainWindowHandle -eq 0) {
            throw "release Ui.exe 在 90 秒內沒有建立主視窗—中止"
        }
        $realDataAfter = Get-DirectoryFingerprint -Path $realData
        if ($realDataAfter -ne $realDataBefore) {
            throw "portable smoke 改寫了真實 LocalAppData 資料：$realData"
        }
        Write-Host ("  ✓ Ui.exe 開得起來；資料隔離於暫存目錄（{0:N1}s）" -f $sw.Elapsed.TotalSeconds) -ForegroundColor Green
    }
    finally {
        if ($smokeProcess -and -not $smokeProcess.HasExited) {
            Stop-Process -Id $smokeProcess.Id -Force -ErrorAction SilentlyContinue
        }
        if (Test-Path -LiteralPath $smokeRoot) {
            Remove-Item -LiteralPath $smokeRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    Step "zip Windows portable"
    $stage = Join-Path $dist $winName
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    Copy-Item -LiteralPath $pub -Destination $stage -Recurse -Force
    Set-Content -LiteralPath (Join-Path $stage ".portable") -Encoding UTF8 -Value "此檔存在＝真 portable：資料存在本資料夾的 Data\ 內；Windows DPAPI 仍綁定目前使用者與裝置。"
    $zip = Join-Path $dist "$winName.zip"
    if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
    [IO.Compression.ZipFile]::CreateFromDirectory((Resolve-Path $stage), $zip, [IO.Compression.CompressionLevel]::Optimal, $true)
    Remove-Item -LiteralPath $stage -Recurse -Force
    Write-Host ("  ✓ dist\$winName.zip ({0:N0} MB)" -f ((Get-Item -LiteralPath $zip).Length / 1MB)) -ForegroundColor Green

    if (-not $SkipInstaller) {
        $iscc = @(
            "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
            "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
            "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
        ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
        if (-not $iscc) { throw "找不到 ISCC.exe（請安裝 Inno Setup，或使用 -SkipInstaller）" }
        Step "build Inno per-user installer"
        $pubAbs = (Resolve-Path -LiteralPath $pub).Path
        Invoke-Native -FilePath $iscc -Arguments @(
            "/Qp", "/DMyAppVersion=$Tag", "/DPubDir=$pubAbs", "/DOutDir=$dist",
            (Join-Path $root "installer\AutoTronclass.iss")
        ) -FailureMessage "Inno 安裝檔建置失敗"
        $setup = Join-Path $dist "$setupName.exe"
        if (-not (Test-Path -LiteralPath $setup -PathType Leaf)) { throw "沒產出 setup.exe" }
        Write-Host ("  ✓ dist\$setupName.exe ({0:N0} MB)" -f ((Get-Item -LiteralPath $setup).Length / 1MB)) -ForegroundColor Green
    }
}

# ── Android：簽章、apksigner verify、固定 fingerprint ──
if (-not $SkipAndroid) {
    Step "publish signed Android APK"
    Clear-HeadBuildOutput -TargetFramework "net11.0-android"
    $signingConfigPath = Join-Path $root "release\android-signing.json"
    if (-not (Test-Path -LiteralPath $signingConfigPath -PathType Leaf)) { throw "缺少固定 Android 簽章設定：$signingConfigPath" }
    try { $signingConfig = Get-Content -Raw -LiteralPath $signingConfigPath | ConvertFrom-Json }
    catch { throw "Android 簽章設定不是有效 JSON：$signingConfigPath" }
    $pinnedFingerprint = Normalize-Fingerprint ([string]$signingConfig.certificateSha256)
    if ($signingConfig.schema -ne 1 -or $pinnedFingerprint -notmatch '^[0-9A-F]{64}$' -or [string]::IsNullOrWhiteSpace([string]$signingConfig.subject)) {
        throw "固定 Android 簽章設定不完整：$signingConfigPath"
    }
    $keystoreProperties = Join-Path $root "keystore.properties"
    if (-not (Test-Path -LiteralPath $keystoreProperties -PathType Leaf)) { throw "缺 keystore.properties（簽章用）" }
    $kp = @{}
    Get-Content -LiteralPath $keystoreProperties | Where-Object { $_ -match '^\s*\w+=' } | ForEach-Object {
        $k, $v = $_ -split '=', 2
        $kp[$k.Trim()] = $v.Trim()
    }
    foreach ($required in "storeFile", "storePassword", "keyAlias", "keyPassword") {
        if ([string]::IsNullOrWhiteSpace([string]$kp[$required])) { throw "keystore.properties 缺少 $required" }
    }
    $providedFingerprints = @($ExpectedApkFingerprint, $kp["expectedFingerprint"]) | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) }
    foreach ($provided in $providedFingerprints) {
        if ((Normalize-Fingerprint ([string]$provided)) -ne $pinnedFingerprint) {
            throw "keystore.properties／環境變數指定的 APK fingerprint 與 release/android-signing.json 不符"
        }
    }
    $expectedFingerprintNormalized = $pinnedFingerprint
    $ksPath = Join-Path $root ([string]$kp.storeFile)
    if (-not (Test-Path -LiteralPath $ksPath -PathType Leaf)) { throw "找不到簽章 keystore：$ksPath" }
    $ksAbs = (Resolve-Path -LiteralPath $ksPath).Path
    $androidPublish = Join-Path $root "ui\bin\Release\net11.0-android\publish"
    Invoke-Native -FilePath $dotnet -Arguments @(
        "publish", "ui/Ui.csproj", "-f", "net11.0-android", "-c", "Release",
        "-p:AndroidKeyStore=true", "-p:AndroidSigningKeyStore=$ksAbs",
        "-p:AndroidSigningStorePass=$($kp.storePassword)", "-p:AndroidSigningKeyAlias=$($kp.keyAlias)",
        "-p:AndroidSigningKeyPass=$($kp.keyPassword)"
    ) -FailureMessage "Android publish 失敗"
    $apk = Get-ChildItem -LiteralPath $androidPublish -Filter "*-Signed.apk" -File |
        Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
    if (-not $apk) { throw "沒產出簽章 APK" }
    $apkDest = Join-Path $dist $apkName
    if (Test-Path -LiteralPath $apkDest -PathType Leaf) { Remove-Item -LiteralPath $apkDest -Force }
    Copy-Item -LiteralPath $apk.FullName -Destination $apkDest -Force

    $apksigner = Find-ApkSigner
    if (-not $apksigner) { throw "找不到 apksigner（Android SDK build-tools 未安裝）" }
    $verifyOutput = & $apksigner verify --verbose --print-certs $apkDest 2>&1
    $verifyExitCode = $LASTEXITCODE
    if ($verifyExitCode -ne 0) { throw "apksigner verify 失敗（退出碼 $verifyExitCode）" }
    $verifyText = ($verifyOutput | ForEach-Object { $_.ToString() }) -join "`n"
    $digestMatch = [regex]::Match($verifyText, '(?im)certificate\s+SHA-256\s+digest:\s*([0-9A-F:]{32,})')
    if (-not $digestMatch.Success) { throw "apksigner 未回報 SHA-256 certificate fingerprint" }
    $actualFingerprint = Normalize-Fingerprint $digestMatch.Groups[1].Value
    if ($actualFingerprint -ne $expectedFingerprintNormalized) {
        throw "APK fingerprint 不符固定設定（實際 $actualFingerprint；預期 $expectedFingerprintNormalized）"
    }
    if ($verifyText -notmatch [regex]::Escape([string]$signingConfig.subject)) {
        throw "APK 憑證 subject 不符固定設定（預期 $($signingConfig.subject)）"
    }
    Assert-ApkNativeHashes -Marker $androidMarker -ApkPath $apkDest
    Write-Host ("  ✓ dist\$apkName ({0:N0} MB)，簽章 fingerprint 已核對" -f ($apk.Length / 1MB)) -ForegroundColor Green
}

Step "資產備妥於 dist\（尚未發布）"
$expectedAssets = @()
if (-not $SkipWindows) {
    $expectedAssets += Join-Path $dist "$winName.zip"
    if (-not $SkipInstaller) { $expectedAssets += Join-Path $dist "$setupName.exe" }
}
if (-not $SkipAndroid) { $expectedAssets += Join-Path $dist $apkName }
foreach ($asset in $expectedAssets) {
    if (-not (Test-Path -LiteralPath $asset -PathType Leaf)) { throw "缺少預期發行資產：$asset" }
    $item = Get-Item -LiteralPath $asset
    if ($item.Length -le 0) { throw "發行資產為空：$asset" }
    Write-Host ("  {0}  {1:N0} MB" -f $item.Name, ($item.Length / 1MB))
}
$notesPath = Join-Path $dist "RELEASE_NOTES-$Tag.md"
if (-not (Test-Path -LiteralPath $notesPath -PathType Leaf)) {
    $noteLines = @(
        "# Auto-Tronclass $Tag",
        "",
        "此檔由 release.ps1 產生；腳本只建置與驗證，不會發布 GitHub Release。",
        "",
        "## 已驗證資產",
        ""
    )
    $noteLines += ($expectedAssets | ForEach-Object { "- ``$([IO.Path]::GetFileName($_))``" })
    Set-Content -LiteralPath $notesPath -Encoding UTF8 -Value ($noteLines -join "`n")
}
if ((Get-Item -LiteralPath $notesPath).Length -le 0) { throw "release notes 為空：$notesPath" }
$assets = ($expectedAssets | ForEach-Object { "dist/$([IO.Path]::GetFileName($_))" }) -join " "
Write-Host "`n下一步（使用者決定後）：" -ForegroundColor Yellow
Write-Host "  gh release create $Tag --repo hot-YUser/auto-Tronclass-APP --prerelease --target main ``"
Write-Host "    --title `"$Tag`" --notes-file dist/RELEASE_NOTES-$Tag.md ``"
Write-Host "    $assets"

# 成功時清理本次暫存 marker；失敗時保留供稽核，不會碰工作區。
if (Test-Path -LiteralPath $markerRoot) {
    Remove-Item -LiteralPath $markerRoot -Recurse -Force -ErrorAction SilentlyContinue
}
