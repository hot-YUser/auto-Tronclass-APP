#!/usr/bin/env pwsh
#requires -Version 7.4
# 一鍵發版建置器：測試 → 雙 head 原生核心 → Windows/Android 發行產物 → 驗證與打包。
# 本腳本只建置與驗證，不發布；缺少 APK 固定 fingerprint 或簽章工具時必定失敗。
#
#   ./tools/release.ps1 -Tag v2.0.0-alpha.4
#   ./tools/release.ps1 -Tag v2.0.0-alpha.4 -SkipAndroid
#   ./tools/release.ps1 -Tag v2.0.0-alpha.4 -ValidateOnly   # 純版本驗算，不建置
#   ./tools/release.ps1 -Tag v2.0.0-alpha.4 -PlanOnly       # 純發版計畫，不建置／發布
#
# Tag 必須是嚴格 SemVer（v?M.m.p[-alpha|beta|rc.N]）；DisplayVersion／Android
# versionCode／Windows 數值版本一律由 Tag 依共享公式計算，不依賴 Ui.csproj 的手動欄位。
# Android 私鑰仍由 keystore.properties 提供；公開 SHA-256 憑證指紋固定在
# tools/android-signing.json，不能以環境變數靜默換掉。

param(
    [Parameter(Mandatory)] [string]$Tag,
    [switch]$SkipAndroid,
    [switch]$SkipWindows,
    [switch]$SkipInstaller,
    # 要求 git tag 已存在且指向 HEAD；預設允許 tag 尚未建立的發行前置建置。
    [switch]$RequireTaggedHead,
    # 純版本解析自測：驗算 Tag → DisplayVersion/versionCode/Windows 版本後直接結束，不改工作區。
    [switch]$ValidateOnly,
    # 純計畫：輸出 exact HEAD target、gates 與將上傳的資產後結束，不建置、不寫 dist、不發布。
    [switch]$PlanOnly,
    [string]$ExpectedApkFingerprint = $env:ANDROID_APK_FINGERPRINT
)

$ErrorActionPreference = "Stop"
# 腳本集中於 tools/；$root 維持 = repo root，讓 core/ui/dist/keystore 路徑不變。
$tools = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $tools
Set-Location $root

if ($SkipWindows -and $SkipAndroid) {
    throw "不能同時跳過 Windows 與 Android；至少必須驗證一個正式發行 head。"
}
if ($ValidateOnly -and $PlanOnly) {
    throw "不能同時使用 -ValidateOnly 與 -PlanOnly。"
}

function ConvertTo-ReleaseVersion {
    param([Parameter(Mandatory)] [string]$Tag)

    # 嚴格 SemVer：v?M.m.p[-alpha|beta|rc.N]（無前導零）。stage 排序 alpha=1、beta=2、rc=3、stable=9。
    $m = [regex]::Match($Tag, '^v?(0|[1-9]\d{0,1})\.(0|[1-9]\d{0,1})\.(0|[1-9]\d{0,1})(?:-(alpha|beta|rc)\.(0|[1-9]\d{0,2}))?$')
    if (-not $m.Success) {
        throw "Tag 必須是嚴格 SemVer（v?M.m.p[-alpha|beta|rc.N]，M<=20、m/p<=99、N<=999）：$Tag"
    }
    $major = [int]$m.Groups[1].Value
    $minor = [int]$m.Groups[2].Value
    $patch = [int]$m.Groups[3].Value
    $stage = if ($m.Groups[4].Success) { $m.Groups[4].Value } else { "stable" }
    $ordinal = if ($m.Groups[5].Success) { [int]$m.Groups[5].Value } else { 0 }
    if ($major -gt 20) { throw "Tag major 不得大於 20：$Tag" }
    if ($minor -gt 99 -or $patch -gt 99) { throw "Tag minor/patch 不得大於 99：$Tag" }
    if ($ordinal -gt 999) { throw "Tag prerelease ordinal 不得大於 999：$Tag" }
    $rank = switch ($stage) { "alpha" { 1 } "beta" { 2 } "rc" { 3 } "stable" { 9 } }
    # 共享公式：major*100_000_000 + minor*1_000_000 + patch*10_000 + rank*1_000 + ordinal；
    # Windows 數值版本以同一組數字為第四段（rank*1000+ordinal <= 9999，恆在 16-bit 範圍）。
    $versionCode = ($major * 100000000) + ($minor * 1000000) + ($patch * 10000) + ($rank * 1000) + $ordinal
    if ($versionCode -gt 2147483647) { throw "versionCode 超過 Android int32 上限：$versionCode" }
    return [pscustomobject]@{
        Tag            = $Tag
        Major          = $major
        Minor          = $minor
        Patch          = $patch
        Stage          = $stage
        Ordinal        = $ordinal
        Rank           = $rank
        DisplayVersion = $Tag -replace '^v', ''
        VersionCode    = $versionCode
        WindowsVersion = "$major.$minor.$patch.$($rank * 1000 + $ordinal)"
    }
}

$version = ConvertTo-ReleaseVersion -Tag $Tag

if ($ValidateOnly) {
    [ordered]@{
        tag            = $version.Tag
        displayVersion = $version.DisplayVersion
        versionCode    = $version.VersionCode
        windowsVersion = $version.WindowsVersion
        stage          = $version.Stage
        ordinal        = $version.Ordinal
    } | ConvertTo-Json | Write-Output
    exit 0
}

# ── 來源／計畫閘：記下 exact HEAD；真正建置另要求乾淨工作樹。 ──

Import-Module (Join-Path $tools "GitRawHelper.psm1") -Force

function Assert-RawBytesExact {
    param([string]$Phase)
    $maxPrint = 20
    $failures = [System.Collections.Generic.List[string]]::new()
    # Get-GitHeadEntries is fail-closed on unsupported modes/types and NUL/UTF-8/parse errors.
    $entries = Get-GitHeadEntries -RepoRoot $root
    $count = $entries.Count
    foreach ($e in $entries) {
        $rel = $e.Path
        $esc = Escape-GitPath $rel
        $abs = Join-Path $root $rel
        if (-not (Test-Path -LiteralPath $abs -PathType Leaf)) {
            $failures.Add("$esc (missing on disk)")
            continue
        }
        try { $raw = Get-FileGitBlobHash -LiteralPath $abs }
        catch { throw "[$Phase] raw-byte guard: hash failed for $esc : $($_.Exception.Message)" }
        if ($raw -ne $e.Sha) { $failures.Add($esc) }
        if ($failures.Count -ge $maxPrint) { break }
    }
    if ($failures.Count -gt 0) {
        $show = @($failures | Select-Object -First $maxPrint)
        foreach ($p in $show) { Write-Host "  ! [$Phase] $p" -ForegroundColor Yellow }
        if ($failures.Count -gt $maxPrint) { Write-Host "  ... and $($failures.Count - $maxPrint) more" -ForegroundColor Yellow }
        throw "[$Phase] raw-byte guard: differ ($($failures.Count)/$count); fix pwsh ./tools/check-crlf.ps1 -Fix"
    }
    Write-Host "  raw-byte guard [$Phase]: $count OK" -ForegroundColor Green
}

$headSha = [string](& git rev-parse HEAD 2>$null)
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($headSha)) { throw "無法取得 HEAD commit。" }
$headSha = $headSha.Trim()

if (-not $PlanOnly) {
    $gitStatus = @(& git status --porcelain 2>$null)
    if ($LASTEXITCODE -ne 0) { throw "無法執行 git status；請確認在 git 工作樹內執行。" }
    if ($gitStatus.Count -gt 0) {
        $redacted = @($gitStatus | ForEach-Object { ($_ -replace "^\s*\S+\s+", "").Trim() } | Where-Object { $_ -ne "" } | Select-Object -First 20)
        if ($redacted.Count -gt 0) {
            Write-Host ("  ! 工作樹不乾淨（節選，最多 20 筆，已去敏僅列路徑）：") -ForegroundColor Yellow
            foreach ($dirtyPath in $redacted) { Write-Host ("    - $dirtyPath") -ForegroundColor Yellow }
        }
        throw "git 工作樹不乾淨（$($gitStatus.Count) 筆變更）；發行前請先提交或清除變更。"
    }
}
if ($RequireTaggedHead) {
    # Resolve the tag namespace explicitly: a same-named branch must never satisfy this release gate.
    $tagRef = "refs/tags/$Tag"
    $tagCommit = [string](& git rev-parse --verify --quiet "$tagRef^{commit}" 2>$null)
    if ($LASTEXITCODE -ne 0) { throw "找不到 git tag $Tag（-RequireTaggedHead 要求 tag 已存在）。" }
    if ($tagCommit.Trim() -ne $headSha) {
        throw "git tag $Tag 指向 $($tagCommit.Trim())，不是 HEAD $headSha（-RequireTaggedHead）。"
    }
}

$winTfm = "net11.0-windows10.0.19041.0"
$winName = "AutoTronclass-$Tag-windows-x64-portable"
$setupName = "AutoTronclass-$Tag-windows-x64-setup"
$apkName = "AutoTronclass-$Tag-android.apk"
$dist = Join-Path $root "dist"
$notesPath = Join-Path $dist "RELEASE_NOTES-$Tag.md"
$metadataPath = Join-Path $dist "build-metadata.json"
$sumsPath = Join-Path $dist "SHA256SUMS.txt"

$expectedAssets = @()
if (-not $SkipWindows) {
    $expectedAssets += Join-Path $dist "$winName.zip"
    if (-not $SkipInstaller) { $expectedAssets += Join-Path $dist "$setupName.exe" }
}
if (-not $SkipAndroid) { $expectedAssets += Join-Path $dist $apkName }
$releaseAssets = @($expectedAssets + $metadataPath + $sumsPath)

$csharpChecks = @(
    @{ Name = "ProtocolContract"; Path = (Join-Path $tools "checks\ProtocolContract.Check\ProtocolContract.Check.csproj") },
    @{ Name = "CommandWire"; Path = (Join-Path $tools "checks\CommandWire.Check\CommandWire.Check.csproj") },
    @{ Name = "UiSettings"; Path = (Join-Path $tools "checks\UiSettings.Check\UiSettings.Check.csproj") }
)
if (-not $SkipWindows) {
    $csharpChecks += @{ Name = "DeviceKey"; Path = (Join-Path $tools "checks\DeviceKey.Check\DeviceKey.Check.csproj") }
}
$releaseCheckNames = @("Rustfmt", "RustTests", "RustClippy") + @($csharpChecks | ForEach-Object Name)

if ($PlanOnly) {
    [ordered]@{
        schema         = 1
        tag            = $version.Tag
        displayVersion = $version.DisplayVersion
        versionCode    = $version.VersionCode
        windowsVersion = $version.WindowsVersion
        target         = $headSha
        checks         = @($releaseCheckNames)
        assets         = @($releaseAssets | ForEach-Object { [IO.Path]::GetFileName($_) })
        notesFile      = [IO.Path]::GetFileName($notesPath)
    } | ConvertTo-Json -Depth 3 | Write-Output
    exit 0
}


function Step([string]$Message) {
    Write-Host "`n=== $Message ===" -ForegroundColor Cyan
}

Assert-RawBytesExact -Phase "start"


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

function Get-ToolVersion {
    param([Parameter(Mandatory)] [string]$Name)
    try { return ((& $Name --version 2>$null) | Select-Object -First 1) }
    catch { return "unknown" }
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

function Find-Aapt {
    $sdk = if (-not [string]::IsNullOrWhiteSpace($env:ANDROID_HOME)) { $env:ANDROID_HOME } else { $env:ANDROID_SDK_ROOT }
    if ([string]::IsNullOrWhiteSpace($sdk)) { return $null }
    $buildTools = Join-Path $sdk "build-tools"
    if (-not (Test-Path -LiteralPath $buildTools -PathType Container)) { return $null }
    return Get-ChildItem -LiteralPath $buildTools -Directory |
        Sort-Object Name -Descending |
        ForEach-Object { Join-Path $_.FullName "aapt.exe" } |
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
        if (-not (Test-Path -LiteralPath $path)) { continue }
        # Retry with build-server shutdown on lock (VBCSCompiler/R2R race on Windows).
        for ($r = 0; $r -lt 3; $r++) {
            try { Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction Stop; break }
            catch {
                if ($r -eq 2) { throw }
                try { & dotnet build-server shutdown 2>$null } catch {}
                Start-Sleep -Milliseconds (500 * ($r + 1))
            }
        }
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
# 所有工具鏈 pins 以 tools/toolchain.json 為單一規範（與 CI 相同），此處不重複硬編。
$toolchainPath = Join-Path $tools "toolchain.json"
if (-not (Test-Path -LiteralPath $toolchainPath -PathType Leaf)) {
    throw "缺少工具鏈規範：$toolchainPath"
}
try { $toolchain = Get-Content -Raw -LiteralPath $toolchainPath | ConvertFrom-Json }
catch { throw "tools/toolchain.json 不是有效 JSON：$toolchainPath" }
foreach ($field in "dotnetSdk", "mauiWorkloadSet", "mauiManifest", "androidNdk", "cargoNdk") {
    if ([string]::IsNullOrWhiteSpace([string]$toolchain.$field)) {
        throw "tools/toolchain.json 缺少 $field"
    }
}

$dotnetCandidates = [System.Collections.Generic.List[string]]::new()
$pathDotnet = Get-Command dotnet -ErrorAction SilentlyContinue
if ($pathDotnet -and -not [string]::IsNullOrWhiteSpace([string]$pathDotnet.Source)) {
    $dotnetCandidates.Add([string]$pathDotnet.Source)
}
foreach ($candidate in @(
    (Join-Path $env:ProgramFiles "dotnet\dotnet.exe"),
    (Join-Path $env:LOCALAPPDATA "Microsoft\dotnet\dotnet.exe")
)) {
    if ((Test-Path -LiteralPath $candidate -PathType Leaf) -and -not $dotnetCandidates.Contains($candidate)) {
        $dotnetCandidates.Add($candidate)
    }
}

# 一台發行機可能同時有 VS/系統與 user-local SDK。依規範版本選 host，而非固定偏好某個
# 安裝根目錄，否則較舊的 user-local host 會遮蔽 PATH 上已安裝的正確 SDK。
$dotnet = $null
$dotnetActual = $null
$dotnetSeen = [System.Collections.Generic.List[string]]::new()
foreach ($candidate in $dotnetCandidates) {
    $actual = (& $candidate --version 2>$null | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace([string]$actual)) { continue }
    $actual = ([string]$actual).Trim()
    $dotnetSeen.Add("$candidate => $actual")
    if ($actual -eq [string]$toolchain.dotnetSdk) {
        $dotnet = $candidate
        $dotnetActual = $actual
        break
    }
}
if (-not $dotnet) {
    $found = if ($dotnetSeen.Count -eq 0) { "未找到可用 dotnet host" } else { $dotnetSeen -join "；" }
    throw "發行需要 .NET SDK $($toolchain.dotnetSdk)（tools/toolchain.json 固定，與 CI 相同）；$found。請安裝精確版本後重試。"
}
$dotnetDir = Split-Path $dotnet -Parent
$env:PATH = "$env:USERPROFILE\.cargo\bin;$dotnetDir;$env:PATH"
$env:DOTNET_CLI_TELEMETRY_OPTOUT = "1"
Write-Host ("  ✓ .NET SDK $dotnetActual ($dotnet)") -ForegroundColor Green

# ── MAUI workload 閘：即使 -SkipAndroid，multi-target restore 仍需 maui-windows 與
#    maui-android 兩個 workload；manifest 必須精確等於 mauiManifest（安裝時用的
#    mauiWorkloadSet 是另一欄位，兩者不得混比）。直接解析 dotnet workload list 的
#    row（locale-independent），不做 sdk-manifests 檔案路徑假設。
$workloadList = @(& $dotnet workload list)
if ($LASTEXITCODE -ne 0) { throw "dotnet workload list 失敗（退出碼 $LASTEXITCODE）" }
foreach ($workload in "maui-windows", "maui-android") {
    $manifestToken = $null
    foreach ($line in $workloadList) {
        $m = [regex]::Match($line, ("^\s*" + [regex]::Escape($workload) + "\s+(?<manifest>[^\s/]+)/"))
        if ($m.Success) { $manifestToken = $m.Groups['manifest'].Value; break }
    }
    if ([string]::IsNullOrWhiteSpace($manifestToken)) {
        throw "缺少已安裝 workload：$workload（需 manifest $($toolchain.mauiManifest)；請執行：dotnet workload install $workload --version $($toolchain.mauiWorkloadSet)）"
    }
    if ($manifestToken -ne [string]$toolchain.mauiManifest) {
        throw "workload $workload 的 manifest 不符：需要 $($toolchain.mauiManifest)（tools/toolchain.json 固定）；目前：$manifestToken。請執行：dotnet workload install $workload --version $($toolchain.mauiWorkloadSet)"
    }
}
Write-Host ("  ✓ MAUI workload manifest $($toolchain.mauiManifest)（maui-windows + maui-android）") -ForegroundColor Green

# ── Rust 閘：cargo 建置前確認工具鏈與 repo 設定一致。
#    Rust channel 的規範來源是 root rust-toolchain.toml（rustup 自動採用）；
#    tools/toolchain.json 不重複放 rust 欄位，避免雙源。
$rustToolchainConfig = Get-Content -Raw -LiteralPath (Join-Path $root "rust-toolchain.toml")
$rustChannel = [regex]::Match($rustToolchainConfig, '(?im)^\s*channel\s*=\s*"([^"]+)"').Groups[1].Value
if ([string]::IsNullOrWhiteSpace($rustChannel)) {
    throw "root rust-toolchain.toml 缺少 channel；發行前請先修正。"
}
$rustcActual = Get-ToolVersion -Name "rustc"
if ($rustcActual -notmatch ("^rustc " + [regex]::Escape($rustChannel) + "\s")) {
    throw "發行需要 Rust $rustChannel（root rust-toolchain.toml 固定，rustup 自動採用）；目前：$rustcActual。請以 rustup 安裝後重試。"
}
Write-Host ("  ✓ Rust $rustcActual") -ForegroundColor Green

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
    if (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        $sdkCandidates += (Join-Path $env:LOCALAPPDATA "Android\sdk")
        $sdkCandidates += (Join-Path $env:LOCALAPPDATA "Android\android-sdk")
    }
    if (-not [string]::IsNullOrWhiteSpace(${env:ProgramFiles(x86)})) { $sdkCandidates += (Join-Path ${env:ProgramFiles(x86)} "Android\android-sdk") }
    $env:ANDROID_HOME = Resolve-ExistingDirectory -Label "Android SDK" -Candidates $sdkCandidates
    $env:ANDROID_SDK_ROOT = $env:ANDROID_HOME
}

$core = Join-Path $root "core"
$buildCore = Join-Path $tools "build-core.ps1"
New-Item -ItemType Directory -Force -Path $dist | Out-Null
Add-Type -AssemblyName System.IO.Compression.FileSystem

# marker 放在暫存區，不會混入發行資產；每次執行使用新的 GUID。
$markerRoot = Join-Path ([IO.Path]::GetTempPath()) ("AutoTronclass-release-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $markerRoot | Out-Null
$winMarkerPath = Join-Path $markerRoot "windows.json"
$androidMarkerPath = Join-Path $markerRoot "android.json"

Step "cargo fmt --check"
Invoke-Native -FilePath "cargo" -Arguments @("fmt", "--manifest-path", "$core/Cargo.toml", "--all", "--", "--check") -FailureMessage "Rust cargo fmt --check 失敗"
Step "cargo test"
Invoke-Native -FilePath "cargo" -Arguments @("test", "--manifest-path", "$core/Cargo.toml", "--locked", "--all-targets", "--all-features") -FailureMessage "Rust cargo test 失敗"
Step "cargo clippy"
Invoke-Native -FilePath "cargo" -Arguments @("clippy", "--manifest-path", "$core/Cargo.toml", "--locked", "--all-targets", "--all-features", "--", "-D", "warnings") -FailureMessage "Rust cargo clippy 失敗"

# 三支跨平台檢查一律執行；只有 DPAPI/NativeCore lifecycle 的 DeviceKey 依 Windows head。
# 每支都直接連結 production source，防止 Rust↔C# wire、設定／Android FGS 純邏輯漂移。
foreach ($check in $csharpChecks) {
    if (-not (Test-Path -LiteralPath $check.Path -PathType Leaf)) {
        throw "缺少 $($check.Name) 可執行檢查：$($check.Path)"
    }
    Step "$($check.Name) 可執行檢查"
    # Checks declare RollForward Major via tools/checks/Directory.Build.props so both CI and
    # release share the same project-declared runtime policy (no env duplication).
    Invoke-Native -FilePath $dotnet -Arguments @("run", "--project", $check.Path, "-c", "Release") -FailureMessage "$($check.Name) 可執行檢查失敗"
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
    if ([string]$androidMarker.ndkVersion -ne [string]$toolchain.androidNdk) {
        throw "Android marker NDK 版本不符：需要 $($toolchain.androidNdk)（tools/toolchain.json 固定）；marker 記錄：$($androidMarker.ndkVersion)"
    }
}

# ── Windows portable + smoke ──
if (-not $SkipWindows) {
    Step "publish Windows portable (self-contained)"
    Clear-HeadBuildOutput -TargetFramework $winTfm
    Invoke-Native -FilePath $dotnet -Arguments @(
        "publish", (Join-Path $root "ui/Ui.csproj"), "-f", $winTfm, "-c", "Release", "-r", "win-x64", "--self-contained",
        "-p:PackageMode=portable", "-p:WindowsAppSDKSelfContained=true",
        "-p:Version=$($version.WindowsVersion)", "-p:FileVersion=$($version.WindowsVersion)",
        "-p:InformationalVersion=$($version.DisplayVersion)", "-p:IncludeSourceRevisionInInformationalVersion=false"
    ) -FailureMessage "Windows publish 失敗"
    $pub = Join-Path $root "ui\bin\Release\$winTfm\win-x64\publish"
    if (-not (Test-Path -LiteralPath $pub -PathType Container)) { throw "Windows publish 目錄不存在：$pub" }
    Assert-PublishedNativeHash -Marker $winMarker -PublishedPath (Join-Path $pub "tronclass_core.dll")

    # 產物內版本必須與 Tag 計算值一致；不符即 fail，release 不得依賴手改 Ui.csproj。
    $uiExe = Join-Path $pub "Ui.exe"
    $exeVersionInfo = [Diagnostics.FileVersionInfo]::GetVersionInfo($uiExe)
    if ($exeVersionInfo.FileVersion -ne $version.WindowsVersion) {
        throw "Ui.exe FileVersion 不符：實際 $($exeVersionInfo.FileVersion)；預期 $($version.WindowsVersion)"
    }
    if ($exeVersionInfo.ProductVersion -ne $version.DisplayVersion) {
        throw "Ui.exe ProductVersion 不符：實際 $($exeVersionInfo.ProductVersion)；預期 $($version.DisplayVersion)"
    }
    Write-Host ("  ✓ Ui.exe FileVersion=$($exeVersionInfo.FileVersion) ProductVersion=$($exeVersionInfo.ProductVersion)") -ForegroundColor Green

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
    Write-Host ("  ✓ dist\$winName.zip ({0:N0} MB, {1:N0} bytes)" -f ((Get-Item -LiteralPath $zip).Length / 1MB), (Get-Item -LiteralPath $zip).Length) -ForegroundColor Green

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
            "/Qp", "/DMyAppVersion=$Tag", "/DMyVersionInfoVersion=$($version.WindowsVersion)",
            "/DPubDir=$pubAbs", "/DOutDir=$dist",
            (Join-Path $tools "installer\AutoTronclass.iss")
        ) -FailureMessage "Inno 安裝檔建置失敗"
        $setup = Join-Path $dist "$setupName.exe"
        if (-not (Test-Path -LiteralPath $setup -PathType Leaf)) { throw "沒產出 setup.exe" }
        Write-Host ("  ✓ dist\$setupName.exe ({0:N0} MB, {1:N0} bytes)" -f ((Get-Item -LiteralPath $setup).Length / 1MB), (Get-Item -LiteralPath $setup).Length) -ForegroundColor Green
    }
}

# ── Android：簽章、apksigner verify、固定 fingerprint ──
if (-not $SkipAndroid) {
    Step "publish signed Android APK"
    Clear-HeadBuildOutput -TargetFramework "net11.0-android"
    $signingConfigPath = Join-Path $tools "android-signing.json"
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
            throw "keystore.properties／環境變數指定的 APK fingerprint 與 tools/android-signing.json 不符"
        }
    }
    $expectedFingerprintNormalized = $pinnedFingerprint
    $ksPath = Join-Path $root ([string]$kp.storeFile)
    if (-not (Test-Path -LiteralPath $ksPath -PathType Leaf)) { throw "找不到簽章 keystore：$ksPath" }
    $ksAbs = (Resolve-Path -LiteralPath $ksPath).Path
    $androidPublish = Join-Path $root "ui\bin\Release\net11.0-android\publish"
    Invoke-Native -FilePath $dotnet -Arguments @(
        "publish", (Join-Path $root "ui/Ui.csproj"), "-f", "net11.0-android", "-c", "Release",
        "-p:AndroidKeyStore=true", "-p:AndroidSigningKeyStore=$ksAbs",
        "-p:AndroidSigningStorePass=$($kp.storePassword)", "-p:AndroidSigningKeyAlias=$($kp.keyAlias)",
        "-p:AndroidSigningKeyPass=$($kp.keyPassword)",
        "-p:ApplicationVersion=$($version.VersionCode)", "-p:ApplicationDisplayVersion=$($version.DisplayVersion)"
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
    Assert-ApkNativeHashes -Marker $androidMarker -ApkPath $apkDest

    # APK 內 versionName/versionCode 必須與 Tag 計算值一致；不符即 fail。
    $aapt = Find-Aapt
    if (-not $aapt) { throw "找不到 aapt（Android SDK build-tools 未安裝）" }
    $badging = (& $aapt dump badging $apkDest 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0) { throw "aapt dump badging 失敗（退出碼 $LASTEXITCODE）" }
    $vcMatch = [regex]::Match($badging, "versionCode='(\d+)'")
    $vnMatch = [regex]::Match($badging, "versionName='([^']*)'")
    if (-not $vcMatch.Success -or -not $vnMatch.Success) {
        throw "APK badging 缺少 versionCode/versionName"
    }
    if ($vcMatch.Groups[1].Value -ne [string]$version.VersionCode) {
        throw "APK versionCode 不符：實際 $($vcMatch.Groups[1].Value)；預期 $($version.VersionCode)"
    }
    if ($vnMatch.Groups[1].Value -ne $version.DisplayVersion) {
        throw "APK versionName 不符：實際 $($vnMatch.Groups[1].Value)；預期 $($version.DisplayVersion)"
    }
    Write-Host ("  ✓ APK versionCode=$($vcMatch.Groups[1].Value) versionName=$($vnMatch.Groups[1].Value)") -ForegroundColor Green
    Write-Host ("  ✓ dist\$apkName ({0:N0} MB, {1:N0} bytes)，簽章 fingerprint 已核對" -f ($apk.Length / 1MB), $apk.Length) -ForegroundColor Green
}

# 長時間雙平台建置期間若 HEAD、tracked source 或要求的 tag 被移動，先前記下的 SHA 已不再
# 能代表實際產物；在寫 metadata／發布計畫前再次 fail closed。
$finalHeadSha = [string](& git rev-parse HEAD 2>$null)
if ($LASTEXITCODE -ne 0 -or $finalHeadSha.Trim() -ne $headSha) {
    throw "建置期間 HEAD 已變更；起始 $headSha，現在 $($finalHeadSha.Trim())。"
}
$finalGitStatus = @(& git status --porcelain 2>$null)
if ($LASTEXITCODE -ne 0) { throw "建置完成後無法執行 git status。" }
if ($finalGitStatus.Count -gt 0) {
    $redacted = @($finalGitStatus | ForEach-Object { ($_ -replace "^\s*\S+\s+", "").Trim() } | Where-Object { $_ -ne "" } | Select-Object -First 20)
    if ($redacted.Count -gt 0) {
        Write-Host ("  ! 工作樹變更（節選，最多 20 筆，已去敏僅列路徑）：") -ForegroundColor Yellow
        foreach ($dirtyPath in $redacted) { Write-Host ("    - $dirtyPath") -ForegroundColor Yellow }
        try { & git diff --name-only 2>$null | Select-Object -First 20 | ForEach-Object { Write-Host ("    diff: $_") -ForegroundColor Yellow } } catch {}
    }
    throw "建置期間工作樹出現 $($finalGitStatus.Count) 筆變更；拒絕產生來源不明的 release metadata。"
}
Assert-RawBytesExact -Phase "final"
if ($RequireTaggedHead) {
    $finalTagCommit = [string](& git rev-parse --verify --quiet "refs/tags/$Tag^{commit}" 2>$null)
    if ($LASTEXITCODE -ne 0 -or $finalTagCommit.Trim() -ne $headSha) {
        throw "建置期間 git tag $Tag 已消失或移動；拒絕產生 release metadata。"
    }
}

Step "資產備妥於 dist\（尚未發布）"
foreach ($asset in $expectedAssets) {
    if (-not (Test-Path -LiteralPath $asset -PathType Leaf)) { throw "缺少預期發行資產：$asset" }
    $item = Get-Item -LiteralPath $asset
    if ($item.Length -le 0) { throw "發行資產為空：$asset" }
    Write-Host ("  {0}  {1:N0} MB ({2:N0} bytes)" -f $item.Name, ($item.Length / 1MB), $item.Length)
}
$noteLines = @(
    "# Auto-Tronclass $Tag",
    "",
    "此檔由 release.ps1 產生；腳本只建置與驗證，不會發布 GitHub Release。",
    "",
    "## 版本（由 Tag 依共享公式計算）",
    "",
    "- DisplayVersion: $($version.DisplayVersion)",
    "- versionCode: $($version.VersionCode)",
    "- Windows 數值版本: $($version.WindowsVersion)",
    "- Commit: $headSha",
    "",
    "## 已驗證資產",
    ""
)
$noteLines += ($releaseAssets | ForEach-Object { "- ``$([IO.Path]::GetFileName($_))``" })
Set-Content -LiteralPath $notesPath -Encoding UTF8 -Value ($noteLines -join "`n")
if ((Get-Item -LiteralPath $notesPath).Length -le 0) { throw "release notes 為空：$notesPath" }

# machine-readable 建置中繼資料；與所有平台產物一起列入 SHA256SUMS，並作為 Release 資產。
$toolchains = [ordered]@{
    rustc  = Get-ToolVersion -Name "rustc"
    cargo  = Get-ToolVersion -Name "cargo"
    dotnet = Get-ToolVersion -Name $dotnet
}
if (-not $SkipAndroid) { $toolchains["ndk"] = [string]$androidMarker.ndkVersion }
# 產物位元組數：此處所有平台產物皆已是最終形態(上方的存在性＋非空斷言剛跑完)，
# 無需第二次建置即可記錄；僅新增 assets 欄位(檔名＋bytes)，既有欄位與 schema=1 不動，
# 無任何解析此檔的程式碼(僅 CI/README 列檔名)，故不構成 schema break。無尺寸硬閘門。
$assetSizes = @(
    foreach ($file in @($expectedAssets + $metadataPath)) {
        [ordered]@{ name = [IO.Path]::GetFileName($file); bytes = (Get-Item -LiteralPath $file).Length }
    }
    [ordered]@{ name = [IO.Path]::GetFileName($sumsPath); bytes = $null }
)
$metadata = [ordered]@{
    schema         = 1
    tag            = $version.Tag
    displayVersion = $version.DisplayVersion
    versionCode    = $version.VersionCode
    windowsVersion = $version.WindowsVersion
    stage          = $version.Stage
    ordinal        = $version.Ordinal
    commit         = $headSha
    builtAtUtc     = [DateTime]::UtcNow.ToString("o")
    toolchains     = $toolchains
    assets         = @($assetSizes)
}
$metadata | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $metadataPath -Encoding UTF8

$sumLines = @()
foreach ($file in @($expectedAssets + $metadataPath)) {
    $hash = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
    $sumLines += "{0}  {1}" -f $hash, [IO.Path]::GetFileName($file)
}
$sumLines = @($sumLines | Sort-Object)
Set-Content -LiteralPath $sumsPath -Encoding ASCII -Value ($sumLines -join "`n")
# SHA256SUMS 定稿後回填自己的位元組數：metadata 內容隨之變更，故僅重算它自己的
# hash 行並重寫 sums；其餘產物 hash 一律不動，SHA256SUMS 仍是準確的 manifest。
$metadataSumsBytes = (Get-Item -LiteralPath $sumsPath).Length
$metadataJson = Get-Content -Raw -LiteralPath $metadataPath | ConvertFrom-Json
foreach ($entry in @($metadataJson.assets)) {
    if ($entry.name -eq [IO.Path]::GetFileName($sumsPath)) { $entry.bytes = $metadataSumsBytes }
}
$metadataJson | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $metadataPath -Encoding UTF8
$metadataName = [IO.Path]::GetFileName($metadataPath)
$metadataRehash = (Get-FileHash -LiteralPath $metadataPath -Algorithm SHA256).Hash.ToLowerInvariant()
$sumLines = @($sumLines | Where-Object { -not $_.EndsWith("  $metadataName") })
$sumLines += "{0}  {1}" -f $metadataRehash, $metadataName
$sumLines = @($sumLines | Sort-Object)
Set-Content -LiteralPath $sumsPath -Encoding ASCII -Value ($sumLines -join "`n")
foreach ($asset in $releaseAssets) {
    if (-not (Test-Path -LiteralPath $asset -PathType Leaf)) { throw "缺少預期 GitHub Release 資產：$asset" }
    if ((Get-Item -LiteralPath $asset).Length -le 0) { throw "GitHub Release 資產為空：$asset" }
}
Write-Host ("  ✓ build-metadata.json + SHA256SUMS.txt（{0} 個檔）" -f $sumLines.Count) -ForegroundColor Green

$assets = ($releaseAssets | ForEach-Object { "dist/$([IO.Path]::GetFileName($_))" }) -join " "
Write-Host "`n下一步（使用者決定後）：" -ForegroundColor Yellow
Write-Host "  gh release create $Tag --repo hot-YUser/auto-Tronclass-APP --prerelease --target $headSha ``"
Write-Host "    --title `"$Tag`" --notes-file dist/RELEASE_NOTES-$Tag.md ``"
Write-Host "    $assets"

# 成功時清理本次暫存 marker；失敗時保留供稽核，不會碰工作區。
if (Test-Path -LiteralPath $markerRoot) {
    Remove-Item -LiteralPath $markerRoot -Recurse -Force -ErrorAction SilentlyContinue
}
