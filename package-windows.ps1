#!/usr/bin/env pwsh
# Produces the Windows distributions of the skeleton from one project:
#   ./package-windows.ps1                → portable (unpackaged exe folder), default
#   ./package-windows.ps1 -Mode msix     → signed .msix installer (self-signed dev cert)
#   ./package-windows.ps1 -Mode both
param(
    [ValidateSet("portable", "msix", "both")] [string]$Mode = "portable"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$proj = Join-Path $root "ui\Ui.csproj"
$tfm = "net11.0-windows10.0.19041.0"
$certSubject = "CN=TronClass Dev"   # MUST match the Package.appxmanifest Identity Publisher

# Prefer the user-dir .NET 11 SDK if present (this machine's global dotnet is 10), else PATH.
$dotnet = Join-Path $env:LOCALAPPDATA "Microsoft\dotnet\dotnet.exe"
if (-not (Test-Path $dotnet)) { $dotnet = "dotnet" }

# Build the native core (Windows dll) into core/target/release first.
& (Join-Path $root "build-core.ps1")

function Publish-Portable {
    # WindowsAppSDKSelfContained bundles the Windows App Runtime so the unpackaged exe launches on a
    # clean machine (else REGDB_E_CLASSNOTREG when the runtime isn't installed machine-wide).
    & $dotnet publish $proj -f $tfm -c Release -p:PackageMode=portable -p:WindowsAppSDKSelfContained=true
    Write-Host "portable -> ui/bin/Release/$tfm/win-x64/publish/"
}

function Publish-Msix {
    # Ensure a signing cert whose Subject matches the manifest Publisher exists.
    $cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $certSubject } | Select-Object -First 1
    if (-not $cert) {
        $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $certSubject `
            -CertStoreLocation Cert:\CurrentUser\My -KeyUsage DigitalSignature `
            -FriendlyName "TronClass Dev (sideload)" -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3")
    }
    & $dotnet publish $proj -f $tfm -c Release -p:PackageMode=msix -p:CertThumbprint=$($cert.Thumbprint)

    # Export the public cert next to the package so a user can trust it before installing.
    $out = Join-Path $root "ui\AppPackages"
    New-Item -ItemType Directory -Force -Path $out | Out-Null
    Export-Certificate -Cert $cert -FilePath (Join-Path $out "tronclass-dev.cer") | Out-Null
    Write-Host "msix -> ui/AppPackages/  (trust ui/AppPackages/tronclass-dev.cer before installing)"
}

switch ($Mode) {
    "portable" { Publish-Portable }
    "msix" { Publish-Msix }
    "both" { Publish-Portable; Publish-Msix }
}
