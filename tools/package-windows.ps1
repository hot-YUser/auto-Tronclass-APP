#!/usr/bin/env pwsh
# Windows 發行包入口：正式 portable 一律走 release.ps1 的完整驗證鏈。
# MSIX 僅供明確標記的本機開發測試，不能冒充正式發行物。

param(
    [ValidateSet("portable", "msix", "both")] [string]$Mode = "portable",
    [switch]$DevelopmentOnly,
    # release.ps1 需要嚴格 SemVer（v?M.m.p[-alpha|beta|rc.N]）；開發用預設值，不需 git tag 已存在。
    [string]$Tag = "v0.0.0-alpha.0"
)

$ErrorActionPreference = "Stop"
# 腳本集中於 tools/；$root 維持 = repo root。
$tools = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $tools
Set-Location $root

if ($Mode -ne "portable" -and -not $DevelopmentOnly) {
    throw "MSIX 是自簽開發產物；請明確指定 -DevelopmentOnly，正式 portable 請使用預設模式。"
}

function Invoke-ScriptChecked {
    param([Parameter(Mandatory)] [string]$Path, [Parameter(Mandatory)] [object[]]$Arguments)
    & $Path @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Path 失敗（退出碼 $LASTEXITCODE）" }
}

function Resolve-Dotnet {
    $candidate = Join-Path $env:LOCALAPPDATA "Microsoft\dotnet\dotnet.exe"
    if (Test-Path -LiteralPath $candidate -PathType Leaf) { return $candidate }
    $command = Get-Command dotnet -ErrorAction SilentlyContinue
    if (-not $command) { throw "找不到 dotnet；請安裝 .NET SDK。" }
    return $command.Source
}

$dotnet = Resolve-Dotnet
$proj = Join-Path $root "ui\Ui.csproj"
$tfm = "net11.0-windows10.0.19041.0"

function Publish-Portable {
    # release.ps1 會執行 cargo、原生 marker、self-contained publish、真實資料隔離 smoke、資產檢查。
    Invoke-ScriptChecked -Path (Join-Path $tools "release.ps1") -Arguments @(
        "-Tag", $Tag, "-SkipAndroid", "-SkipInstaller"
    )
    Write-Host "portable -> dist/AutoTronclass-$Tag-windows-x64-portable.zip" -ForegroundColor Green
}

function Publish-MsixDevelopment {
    Write-Warning "MSIX 僅供開發測試，使用自簽憑證；不代表可發布的正式安裝檔。"
    Invoke-ScriptChecked -Path (Join-Path $tools "build-core.ps1") -Arguments @("-Head", "windows")

    foreach ($path in @(
        (Join-Path $root "ui\bin\Release\$tfm"),
        (Join-Path $root "ui\obj\Release\$tfm"),
        (Join-Path $root "ui\AppPackages")
    )) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Recurse -Force }
    }

    $certSubject = "CN=TronClass Dev"
    $cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $certSubject } | Select-Object -First 1
    if (-not $cert) {
        $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $certSubject `
            -CertStoreLocation Cert:\CurrentUser\My -KeyUsage DigitalSignature `
            -FriendlyName "TronClass Dev (sideload)" -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3")
    }
    & $dotnet publish $proj -f $tfm -c Release -p:PackageMode=msix -p:CertThumbprint=$($cert.Thumbprint)
    if ($LASTEXITCODE -ne 0) { throw "MSIX 開發建置失敗（退出碼 $LASTEXITCODE）" }

    $out = Join-Path $root "ui\AppPackages"
    if (-not (Test-Path -LiteralPath $out -PathType Container) -or
        -not (Get-ChildItem -LiteralPath $out -File -Recurse | Where-Object Length -gt 0)) {
        throw "MSIX 建置未產生有效套件：$out"
    }
    Export-Certificate -Cert $cert -FilePath (Join-Path $out "tronclass-dev.cer") | Out-Null
    Write-Host "msix -> ui/AppPackages/（開發自簽；安裝前須信任 tronclass-dev.cer）" -ForegroundColor Yellow
}

switch ($Mode) {
    "portable" { Publish-Portable }
    "msix" { Publish-MsixDevelopment }
    "both" { Publish-Portable; Publish-MsixDevelopment }
}
