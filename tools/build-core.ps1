#!/usr/bin/env pwsh
# 建置 Rust 原生核心，並確認輸出確實由本次建置產生。
#   ./tools/build-core.ps1                 -> Windows: core/target/release/tronclass_core.dll
#   ./tools/build-core.ps1 -Head android   -> Android: core/jniLibs/{arm64-v8a,x86_64}/libtronclass_core.so

param(
    [ValidateSet("windows", "android")]
    [string]$Head = "windows",
    # release.ps1 會傳入暫存 marker；單獨執行時則放在 core 下，方便稽核。
    [string]$BuildMarkerPath
)

$ErrorActionPreference = "Stop"
# 腳本集中於 tools/；$root 維持 = repo root，讓 marker/產物路徑不變。
$tools = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $tools
$core = Join-Path $root "core"
# 工具鏈單一真理：NDK／cargo-ndk 版本都來自 tools/toolchain.json，不在此硬編。
$toolchainPath = Join-Path $tools "toolchain.json"
if (-not (Test-Path -LiteralPath $toolchainPath -PathType Leaf)) {
    throw "缺少工具鏈規範：$toolchainPath"
}
try { $toolchain = Get-Content -Raw -LiteralPath $toolchainPath | ConvertFrom-Json }
catch { throw "tools/toolchain.json 不是有效 JSON：$toolchainPath" }
if ([string]::IsNullOrWhiteSpace([string]$toolchain.androidNdk) -or [string]::IsNullOrWhiteSpace([string]$toolchain.cargoNdk)) {
    throw "tools/toolchain.json 缺少 androidNdk/cargoNdk"
}
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if (Test-Path -LiteralPath $cargoBin -PathType Container) { $env:PATH = "$cargoBin;$env:PATH" }
$cargo = Join-Path $cargoBin "cargo.exe"
if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) { $cargo = "cargo" }

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

function Get-ArtifactRecord {
    param(
        [Parameter(Mandatory)] [string]$Path,
        [Parameter(Mandatory)] [DateTime]$BuildStartedUtc
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "原生核心建置成功但缺少輸出：$Path"
    }
    $item = Get-Item -LiteralPath $Path
    # 建置前已刪除精確輸出。cargo 的 top-level artifact 是 deps 產物的一組 hardlink，零變更重發時
    # 不會重寫 mtime；因此「輸出不得舊於 core 目前原始檔」比「不得舊於本次 build 開始」更正確，
    # 同時仍能抓出 source 已改、輸出卻未更新的真實 stale 風險。
    $sources = @(Get-ChildItem -LiteralPath (Join-Path $core "src") -Recurse -File -ErrorAction SilentlyContinue)
    $sources += Get-Item -LiteralPath (Join-Path $core "Cargo.toml") -ErrorAction SilentlyContinue
    $buildRs = Join-Path $core "build.rs"
    if (Test-Path -LiteralPath $buildRs -PathType Leaf) { $sources += Get-Item -LiteralPath $buildRs }
    $newestSource = ($sources | Measure-Object -Property LastWriteTimeUtc -Maximum).Maximum
    if ($null -ne $newestSource -and $item.LastWriteTimeUtc -lt $newestSource) {
        throw "原生核心輸出早於最新原始檔：$Path"
    }
    $hash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    return [ordered]@{
        path       = [IO.Path]::GetRelativePath($root, $Path)
        sha256     = $hash
        mtimeUtc   = $item.LastWriteTimeUtc.ToString("o")
        length     = $item.Length
    }
}

function Write-BuildMarker {
    param(
        [Parameter(Mandatory)] [string]$HeadName,
        [Parameter(Mandatory)] [DateTime]$BuildStartedUtc,
        [Parameter(Mandatory)] [object[]]$Artifacts,
        # Android head 專用：完整 NDK 版本（如 27.2.12479018）；release 端會重驗精確版本。
        [string]$NdkVersion
    )

    $marker = if ($BuildMarkerPath) {
        $BuildMarkerPath
    }
    else {
        Join-Path $core ".native-build-$HeadName.json"
    }
    $markerParent = Split-Path -Parent $marker
    New-Item -ItemType Directory -Force -Path $markerParent | Out-Null
    $payload = [ordered]@{
        schema       = 1
        buildId      = [guid]::NewGuid().ToString("N")
        head         = $HeadName
        startedUtc   = $BuildStartedUtc.ToString("o")
        completedUtc = [DateTime]::UtcNow.ToString("o")
        artifacts    = @($Artifacts)
    }
    if (-not [string]::IsNullOrWhiteSpace($NdkVersion)) { $payload["ndkVersion"] = $NdkVersion }
    $payload | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $marker -Encoding UTF8
    Write-Host ("  ✓ native build marker {0} ({1})" -f $marker, $payload.buildId) -ForegroundColor Green
    return $marker
}

function Resolve-AndroidNdk {
    # 明確設定的 ANDROID_NDK_HOME 最高優先；未設定時收集所有常見 SDK 安裝位置，
    # 全域找第一個精確 pin 版本的 ndk 目錄；都無精確版時退而取任一已安裝 NDK，
    # 讓後續的精確版本檢查給出明確錯誤；完全沒有 NDK 才 not found。
    if ([string]::IsNullOrWhiteSpace($env:ANDROID_NDK_HOME)) {
        $sdkCandidates = @($env:ANDROID_HOME, $env:ANDROID_SDK_ROOT)
        foreach ($base in @($env:LOCALAPPDATA, ${env:ProgramFiles(x86)})) {
            if (-not [string]::IsNullOrWhiteSpace($base)) {
                $sdkCandidates += (Join-Path $base "Android\sdk")
                $sdkCandidates += (Join-Path $base "Android\android-sdk")
            }
        }
        $sdks = @($sdkCandidates | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) -and (Test-Path -LiteralPath ([string]$_) -PathType Container) })
        $ndk = $null
        foreach ($sdk in $sdks) {
            $exactNdk = Join-Path (Join-Path ([string]$sdk) "ndk") $toolchain.androidNdk
            if (Test-Path -LiteralPath $exactNdk -PathType Container) {
                $ndk = Get-Item -LiteralPath $exactNdk
                break
            }
        }
        if (-not $ndk) {
            foreach ($sdk in $sdks) {
                $ndk = Get-ChildItem -LiteralPath (Join-Path ([string]$sdk) "ndk") -Directory -ErrorAction SilentlyContinue |
                    Sort-Object Name -Descending | Select-Object -First 1
                if ($ndk) { break }
            }
        }
        if ($ndk) { $env:ANDROID_NDK_HOME = $ndk.FullName }
    }
    if ([string]::IsNullOrWhiteSpace($env:ANDROID_NDK_HOME) -or -not (Test-Path -LiteralPath $env:ANDROID_NDK_HOME -PathType Container)) {
        throw "找不到 Android NDK；請設定 ANDROID_NDK_HOME 或安裝 SDK 內的 NDK。"
    }
    # 精確版本由 tools/toolchain.json 固定（與 CI 相同）；完整版本寫入 marker 供 release 重驗。
    $propsPath = Join-Path $env:ANDROID_NDK_HOME "source.properties"
    if (-not (Test-Path -LiteralPath $propsPath -PathType Leaf)) {
        throw "NDK 缺少 source.properties：$propsPath（$env:ANDROID_NDK_HOME 不是標準 NDK 安裝）"
    }
    $revLine = Get-Content -LiteralPath $propsPath | Where-Object { $_ -match '^\s*Pkg\.Revision\s*=' } | Select-Object -First 1
    if (-not $revLine -or $revLine -notmatch '=\s*(\d+(?:\.\d+)*)') {
        throw "NDK source.properties 缺少 Pkg.Revision：$propsPath"
    }
    $ndkVersion = $Matches[1]
    if ($ndkVersion -ne $toolchain.androidNdk) {
        throw "需要 NDK $($toolchain.androidNdk)（tools/toolchain.json 固定）；目前安裝：$ndkVersion（$env:ANDROID_NDK_HOME）。請安裝精確版本或調整 ANDROID_NDK_HOME。"
    }
    return $ndkVersion
}

$buildStartedUtc = [DateTime]::UtcNow
$artifacts = @()
$ndkVersion = ""

if ($Head -eq "windows") {
    $output = Join-Path $core "target\release\tronclass_core.dll"
    # 只清理這一個 native output，不刪除 target 或其他建置快取。
    if (Test-Path -LiteralPath $output -PathType Leaf) {
        Remove-Item -LiteralPath $output -Force
    }
    Invoke-Native -FilePath $cargo -Arguments @("build", "--manifest-path", "$core/Cargo.toml", "--release", "--locked") -FailureMessage "Windows 原生核心 cargo build 失敗"
    $artifacts += Get-ArtifactRecord -Path $output -BuildStartedUtc $buildStartedUtc
}
else {
    Push-Location $core
    try {
        $ndkVersion = Resolve-AndroidNdk
        $outputs = @(
            (Join-Path $core "jniLibs\arm64-v8a\libtronclass_core.so"),
            (Join-Path $core "jniLibs\x86_64\libtronclass_core.so")
        )
        # cargo-ndk 只會產生這兩個 ABI；精確刪除，避免舊 .so 被沿用。
        foreach ($output in $outputs) {
            if (Test-Path -LiteralPath $output -PathType Leaf) {
                Remove-Item -LiteralPath $output -Force
            }
        }
        $cargoNdkVersion = & $cargo ndk --version
        if ($LASTEXITCODE -ne 0 -or $cargoNdkVersion -notmatch ('cargo-ndk ' + [regex]::Escape($toolchain.cargoNdk))) {
            throw "cargo-ndk 必須是 $($toolchain.cargoNdk)（tools/toolchain.json 固定）；目前：$cargoNdkVersion"
        }
        Invoke-Native -FilePath $cargo -Arguments @("ndk", "-t", "arm64-v8a", "-t", "x86_64", "-o", "jniLibs", "build", "--release", "--locked") -FailureMessage "Android 原生核心 cargo ndk 失敗"
        foreach ($output in $outputs) {
            $artifacts += Get-ArtifactRecord -Path $output -BuildStartedUtc $buildStartedUtc
        }
    }
    finally {
        Pop-Location
    }
}

$markerPath = Write-BuildMarker -HeadName $Head -BuildStartedUtc $buildStartedUtc -Artifacts $artifacts -NdkVersion $ndkVersion
Write-Host ("{0}: 原生核心輸出已確認（{1}）" -f $Head, (($artifacts | ForEach-Object { $_.path }) -join ", "))
