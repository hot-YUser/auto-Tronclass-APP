#!/usr/bin/env pwsh
# 建置 Rust 原生核心，並確認輸出確實由本次建置產生。
#   ./build-core.ps1                 -> Windows: core/target/release/tronclass_core.dll
#   ./build-core.ps1 -Head android   -> Android: core/jniLibs/{arm64-v8a,x86_64}/libtronclass_core.so

param(
    [ValidateSet("windows", "android")]
    [string]$Head = "windows",
    # release.ps1 會傳入暫存 marker；單獨執行時則放在 core 下，方便稽核。
    [string]$BuildMarkerPath
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$core = Join-Path $root "core"
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
    # 建置前已刪除精確輸出；mtime 再次確認不是殘留檔案。
    if ($item.LastWriteTimeUtc -lt $BuildStartedUtc) {
        throw "原生核心輸出時間早於本次建置：$Path"
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
        [Parameter(Mandatory)] [object[]]$Artifacts
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
    $payload | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $marker -Encoding UTF8
    Write-Host ("  ✓ native build marker {0} ({1})" -f $marker, $payload.buildId) -ForegroundColor Green
    return $marker
}

$buildStartedUtc = [DateTime]::UtcNow
$artifacts = @()

if ($Head -eq "windows") {
    $output = Join-Path $core "target\release\tronclass_core.dll"
    # 只清理這一個 native output，不刪除 target 或其他建置快取。
    if (Test-Path -LiteralPath $output -PathType Leaf) {
        Remove-Item -LiteralPath $output -Force
    }
    Invoke-Native -FilePath $cargo -Arguments @("build", "--manifest-path", "$core/Cargo.toml", "--release") -FailureMessage "Windows 原生核心 cargo build 失敗"
    $artifacts += Get-ArtifactRecord -Path $output -BuildStartedUtc $buildStartedUtc
}
else {
    Push-Location $core
    try {
        # Point cargo-ndk at an explicitly configured or newest installed NDK.
        if ([string]::IsNullOrWhiteSpace($env:ANDROID_NDK_HOME)) {
            $sdkCandidates = @($env:ANDROID_HOME, $env:ANDROID_SDK_ROOT)
            if (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) { $sdkCandidates += (Join-Path $env:LOCALAPPDATA "Android\sdk") }
            $sdk = $sdkCandidates | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) -and (Test-Path -LiteralPath ([string]$_) -PathType Container) } | Select-Object -First 1
            if ($sdk) {
                $ndkBase = Join-Path ([string]$sdk) "ndk"
                $ndk = Get-ChildItem -LiteralPath $ndkBase -Directory -ErrorAction SilentlyContinue |
                    Sort-Object Name -Descending | Select-Object -First 1
                if ($ndk) { $env:ANDROID_NDK_HOME = $ndk.FullName }
            }
        }
        if ([string]::IsNullOrWhiteSpace($env:ANDROID_NDK_HOME) -or -not (Test-Path -LiteralPath $env:ANDROID_NDK_HOME -PathType Container)) {
            throw "找不到 Android NDK；請設定 ANDROID_NDK_HOME 或安裝 SDK 內的 NDK。"
        }
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
        Invoke-Native -FilePath $cargo -Arguments @("ndk", "-t", "arm64-v8a", "-t", "x86_64", "-o", "jniLibs", "build", "--release") -FailureMessage "Android 原生核心 cargo ndk 失敗"
        foreach ($output in $outputs) {
            $artifacts += Get-ArtifactRecord -Path $output -BuildStartedUtc $buildStartedUtc
        }
    }
    finally {
        Pop-Location
    }
}

$markerPath = Write-BuildMarker -HeadName $Head -BuildStartedUtc $buildStartedUtc -Artifacts $artifacts
Write-Host ("{0}: 原生核心輸出已確認（{1}）" -f $Head, (($artifacts | ForEach-Object { $_.path }) -join ", "))
