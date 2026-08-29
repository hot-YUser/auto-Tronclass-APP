#!/usr/bin/env pwsh
#requires -Version 7.4
# Maintainer-only binding regeneration — never invoked during normal cargo build/CI/release.
# Generates csbindgen output outside tracked path, validates, then atomically replaces
# ui/Interop/NativeMethods.g.cs with unique sibling temp, exclusive create, flush, atomic rename.

param(
    [switch]$CheckOnly
)

$ErrorActionPreference = "Stop"
$tools = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $tools
Set-Location $root

$tracked = Join-Path $root "ui/Interop/NativeMethods.g.cs"
$trackedDir = Split-Path -Parent $tracked
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if (Test-Path -LiteralPath $cargoBin -PathType Container) { $env:PATH = "$cargoBin;$env:PATH" }
$cargo = Join-Path $cargoBin "cargo.exe"
if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) { $cargo = "cargo" }

$genTmpDir = Join-Path ([IO.Path]::GetTempPath()) ("tron-bindings-gen-" + [guid]::NewGuid().ToString("N"))
$genOutput = Join-Path $genTmpDir "NativeMethods.g.cs"
$stagingTmp = $null
$backupPath = $null
$injectFail = $env:TRONCLASS_UPDATE_BINDINGS_INJECT_FAIL

function Cleanup {
    try { if ($genTmpDir -and (Test-Path -LiteralPath $genTmpDir -PathType Container)) { Remove-Item -LiteralPath $genTmpDir -Recurse -Force -ErrorAction SilentlyContinue } } catch {}
    try { if ($stagingTmp -and (Test-Path -LiteralPath $stagingTmp -PathType Leaf)) { Remove-Item -LiteralPath $stagingTmp -Force -ErrorAction SilentlyContinue } } catch {}
    try { if ($backupPath -and (Test-Path -LiteralPath $backupPath -PathType Leaf)) { Remove-Item -LiteralPath $backupPath -Force -ErrorAction SilentlyContinue } } catch {}
}

try {
    New-Item -ItemType Directory -Force -Path $genTmpDir | Out-Null
    $genProjDir = Join-Path $genTmpDir "gen"
    New-Item -ItemType Directory -Force -Path $genProjDir | Out-Null
    @"
[package]
name = "tron-bindings-gen"
version = "0.1.0"
edition = "2021"
[dependencies]
csbindgen = "1"
"@ | Set-Content -LiteralPath (Join-Path $genProjDir "Cargo.toml") -Encoding UTF8

    New-Item -ItemType Directory -Force -Path (Join-Path $genProjDir "src") | Out-Null
    $coreLib = Join-Path $root "core/src/lib.rs"
    $coreLibEsc = $coreLib.Replace('\', '/')
    @"
fn main() {
    let out = std::env::args().nth(1).expect("output path required");
    csbindgen::Builder::default()
        .input_extern_file("$coreLibEsc")
        .csharp_dll_name("tronclass_core")
        .csharp_namespace("TronClass.Interop")
        .csharp_class_name("NativeMethods")
        .generate_csharp_file(&out)
        .expect("csbindgen generate failed");
}
"@ | Set-Content -LiteralPath (Join-Path $genProjDir "src/main.rs") -Encoding UTF8

    Write-Host "  generating bindings via csbindgen..." -ForegroundColor Cyan
    & $cargo run --manifest-path (Join-Path $genProjDir "Cargo.toml") --quiet -- $genOutput 2>&1
    if ($LASTEXITCODE -ne 0) { throw "csbindgen generation failed (exit $LASTEXITCODE)" }
    if (-not (Test-Path -LiteralPath $genOutput -PathType Leaf)) { throw "generator did not produce $genOutput" }

    $newBytes = [IO.File]::ReadAllBytes($genOutput)
    if ($newBytes.Length -eq 0) { throw "generated bindings are empty" }
    $text = [Text.Encoding]::UTF8.GetString($newBytes)
    if ($text -notmatch "NativeMethods" -or $text -notmatch "core_init") {
        throw "generated bindings failed validation (missing expected symbols)"
    }

    if ($CheckOnly) {
        if (-not (Test-Path -LiteralPath $tracked -PathType Leaf)) { throw "tracked bindings missing at $tracked" }
        $oldBytes = [IO.File]::ReadAllBytes($tracked)
        $same = $newBytes.Length -eq $oldBytes.Length
        if ($same) {
            for ($i = 0; $i -lt $newBytes.Length; $i++) { if ($newBytes[$i] -ne $oldBytes[$i]) { $same = $false; break } }
        }
        if ($same) { Write-Host "  bindings up to date" -ForegroundColor Green; exit 0 }
        else { throw "bindings mismatch — run pwsh ./tools/update-bindings.ps1 to update" }
    }

    if (-not (Test-Path -LiteralPath $trackedDir -PathType Container)) {
        New-Item -ItemType Directory -Force -Path $trackedDir | Out-Null
    }

    # Allocate unique sibling temp with CreateNew (exclusive)
    $attempts = 0
    $maxAttempts = 8
    $stagingFile = $null
    while ($attempts -lt $maxAttempts) {
        $attempts++
        $suffix = [guid]::NewGuid().ToString("N").Substring(0, 8)
        $candidate = Join-Path $trackedDir ".NativeMethods.g.cs.tmp-$suffix"
        try {
            $fs = [IO.File]::Open($candidate, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            $stagingTmp = $candidate
            $stagingFile = $fs
            break
        } catch [IO.IOException] {
            if ($_.Exception.Message -match "already exists") { continue }
            throw
        }
    }
    if (-not $stagingFile) { throw "could not allocate staging temp after $maxAttempts attempts" }

    try {
        $stagingFile.Write($newBytes, 0, $newBytes.Length)
        $stagingFile.Flush($true)
        $stagingFile.Close()
        $stagingFile = $null

        if ($injectFail -eq "after-staging") {
            throw "injected failure after staging temp written (testing cleanup)"
        }

        if (Test-Path -LiteralPath $tracked -PathType Leaf) {
            $backupPath = "$tracked.bak-$([guid]::NewGuid().ToString('N').Substring(0,8))"
            try {
                [IO.File]::Replace($stagingTmp, $tracked, $backupPath)
                $stagingTmp = $null
            } catch {
                try {
                    [IO.File]::Move($stagingTmp, $tracked, $true)
                    $stagingTmp = $null
                } catch {
                    throw "atomic replace failed: $($_.Exception.Message)"
                }
            }
        } else {
            [IO.File]::Move($stagingTmp, $tracked)
            $stagingTmp = $null
        }

        Write-Host "  updated $tracked ($($newBytes.Length) bytes)" -ForegroundColor Green
        $verify = [IO.File]::ReadAllBytes($tracked)
        if ($verify.Length -ne $newBytes.Length) { throw "verify failed: length mismatch" }
        for ($i = 0; $i -lt $verify.Length; $i++) { if ($verify[$i] -ne $newBytes[$i]) { throw "verify failed at $i" } }
    } finally {
        try { if ($stagingFile) { $stagingFile.Close() } } catch {}
    }
} finally {
    Cleanup
    if ($backupPath -and (Test-Path -LiteralPath $backupPath -PathType Leaf)) {
        try { Remove-Item -LiteralPath $backupPath -Force -ErrorAction SilentlyContinue } catch {}
    }
}
