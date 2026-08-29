#!/usr/bin/env pwsh
#requires -Version 7.4
# Maintainer-only binding regeneration — never invoked during normal cargo build/CI/release.
# Uses committed repo-local tools/bindings-generator (Cargo.lock pinned, --locked --offline, no network).
# Transactional state machine: original bytes retained; backup deleted only after post-verify succeeds.

param(
    [switch]$CheckOnly
)

$ErrorActionPreference = "Stop"
$tools = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $tools
Set-Location $root

$tracked = Join-Path $root "ui/Interop/NativeMethods.g.cs"
$trackedDir = Split-Path -Parent $tracked
$generatorManifest = Join-Path $tools "bindings-generator/Cargo.toml"
$generatorInput = Join-Path $root "core/src/lib.rs"
# CI version-match gate: generator and core must pin same csbindgen.
$genLock = Join-Path $tools "bindings-generator/Cargo.lock"
$coreLock = Join-Path $root "core/Cargo.lock"
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if (Test-Path -LiteralPath $cargoBin -PathType Container) { $env:PATH = "$cargoBin;$env:PATH" }
$cargo = Join-Path $cargoBin "cargo.exe"
if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) { $cargo = "cargo" }

$genTmpDir = Join-Path ([IO.Path]::GetTempPath()) ("tron-bindings-gen-" + [guid]::NewGuid().ToString("N"))
$genOutput = Join-Path $genTmpDir "NativeMethods.g.cs"
$stagingTmp = $null
$backupPath = $null
$injectFail = $env:TRONCLASS_UPDATE_BINDINGS_INJECT_FAIL

function Invoke-CargoMetadataLockedOffline {
    param([Parameter(Mandatory)][string]$ManifestPath)
    if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) { throw "missing manifest: $ManifestPath" }
    $psi = [Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $cargo
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    foreach ($a in @("metadata","--manifest-path",$ManifestPath,"--locked","--offline","--format-version","1")) { $null = $psi.ArgumentList.Add($a) }
    $proc = [Diagnostics.Process]::new()
    $proc.StartInfo = $psi
    $out = ""
    $err = ""
    try {
        if (-not $proc.Start()) { throw "failed to start cargo metadata for $ManifestPath" }
        $out = $proc.StandardOutput.ReadToEnd()
        $err = $proc.StandardError.ReadToEnd()
        $proc.WaitForExit()
        if ($proc.ExitCode -ne 0) {
            $msg = "cargo metadata failed for $ManifestPath (exit $($proc.ExitCode))"
            if (-not [string]::IsNullOrWhiteSpace($err)) { $msg += ": $($err.Trim())" }
            throw $msg
        }
    } finally { $proc.Dispose() }
    if ([string]::IsNullOrWhiteSpace($out)) { throw "cargo metadata empty output for $ManifestPath" }
    try { $json = $out | ConvertFrom-Json }
    catch { throw "cargo metadata malformed JSON for $ManifestPath : $($_.Exception.Message)" }
    return $json
}

function Get-CsbindgenVersionFromMetadata {
    param([Parameter(Mandatory)][object]$Metadata)
    $pkgs = @($Metadata.packages | Where-Object { $_.name -eq "csbindgen" })
    if ($pkgs.Count -eq 0) { throw "csbindgen not found in cargo metadata (missing lockfile or dependency)" }
    $vers = @($pkgs | ForEach-Object { $_.version } | Sort-Object -Unique)
    if ($vers.Count -ne 1) { throw "csbindgen has $($vers.Count) distinct versions in metadata: $($vers -join ', ') (expected exactly 1)" }
    $v = $vers[0]
    if ([string]::IsNullOrWhiteSpace($v)) { throw "csbindgen version empty in metadata" }
    return $v
}

function Assert-CsbindgenVersionGate {
    if (-not (Test-Path -LiteralPath $generatorManifest -PathType Leaf)) { throw "missing generator manifest: $generatorManifest" }
    if (-not (Test-Path -LiteralPath $genLock -PathType Leaf)) { throw "missing generator lock: $genLock" }
    if (-not (Test-Path -LiteralPath $coreLock -PathType Leaf)) { throw "missing core lock: $coreLock" }
    $coreManifest = Join-Path $root "core/Cargo.toml"
    if (-not (Test-Path -LiteralPath $coreManifest -PathType Leaf)) { throw "missing core manifest: $coreManifest" }
    $genMeta = Invoke-CargoMetadataLockedOffline -ManifestPath $generatorManifest
    $coreMeta = Invoke-CargoMetadataLockedOffline -ManifestPath $coreManifest
    $genVer = Get-CsbindgenVersionFromMetadata -Metadata $genMeta
    $coreVer = Get-CsbindgenVersionFromMetadata -Metadata $coreMeta
    if ($genVer -ne $coreVer) { throw "csbindgen version mismatch: generator $genVer vs core $coreVer (fix tools/bindings-generator/Cargo.toml to =${coreVer})" }
    Write-Host "  csbindgen $genVer OK (generator == core, locked offline)" -ForegroundColor Green
    return $genVer
}


function Cleanup-GenTmp {
    try { if ($genTmpDir -and (Test-Path -LiteralPath $genTmpDir -PathType Container)) { Remove-Item -LiteralPath $genTmpDir -Recurse -Force -ErrorAction SilentlyContinue } } catch {}
}

function Cleanup-StagingWithRetry {
    param([string]$Path)
    if (-not $Path) { return $false }
    for ($r = 0; $r -lt 4; $r++) {
        try {
            if (Test-Path -LiteralPath $Path -PathType Leaf) {
                Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
            }
            return $true
        } catch [System.IO.IOException] {
            Start-Sleep -Milliseconds (50 * ($r + 1))
        } catch { break }
    }
    if ($Path -and (Test-Path -LiteralPath $Path -PathType Leaf)) {
        Write-Host "  leftover staging (ignored, not compiled): $Path (AV lock?)" -ForegroundColor Yellow
        return $false
    }
    return $true
}

try {
    # Fail-closed version gate: cargo metadata --locked --offline --format-version 1, exactly one version each, equality required.
    # Must run before generation or any tracked-file staging.
    $null = Assert-CsbindgenVersionGate

    New-Item -ItemType Directory -Force -Path $genTmpDir | Out-Null

    Write-Host "  generating bindings via locked generator..." -ForegroundColor Cyan
    # --locked --offline ensures no network, no floating.
    & $cargo run --manifest-path $generatorManifest --locked --offline --quiet -- $generatorInput $genOutput 2>&1
    if ($LASTEXITCODE -ne 0) { throw "csbindgen generation failed (exit $LASTEXITCODE)" }
    if (-not (Test-Path -LiteralPath $genOutput -PathType Leaf)) { throw "generator did not produce $genOutput" }

    $newBytes = [System.IO.File]::ReadAllBytes($genOutput)
    # Gen temp is system temp; cleanup with bounded retries (AV lock can leave residue).
    Cleanup-GenTmp
    if ($newBytes.Length -eq 0) { throw "generated bindings are empty" }
    $text = [System.Text.Encoding]::UTF8.GetString($newBytes)
    if ($text -notmatch "NativeMethods" -or $text -notmatch "core_init") {
        throw "generated bindings failed validation (missing expected symbols)"
    }

    if ($CheckOnly) {
        if (-not (Test-Path -LiteralPath $tracked -PathType Leaf)) { throw "tracked bindings missing at $tracked" }
        $oldBytes = [System.IO.File]::ReadAllBytes($tracked)
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

    # Retain original bytes/hash for rollback.
    $hadOriginal = Test-Path -LiteralPath $tracked -PathType Leaf
    $originalBytes = $null
    $originalHash = $null
    if ($hadOriginal) {
        $originalBytes = [System.IO.File]::ReadAllBytes($tracked)
        $sha = [System.Security.Cryptography.SHA256]::Create()
        try { $originalHash = [Convert]::ToHexString($sha.ComputeHash($originalBytes)).ToLowerInvariant() }
        finally { $sha.Dispose() }
    }

    if ($injectFail -eq "before-replace") { throw "injected failure before replace (testing rollback)" }

    # Allocate unique same-dir staging with CreateNew (exclusive), write, flush.
    $attempts = 0
    $maxAttempts = 8
    $stagingFile = $null
    while ($attempts -lt $maxAttempts) {
        $attempts++
        $suffix = [guid]::NewGuid().ToString("N").Substring(0, 8)
        $candidate = Join-Path $trackedDir ".NativeMethods.g.cs.tmp-$suffix"
        try {
            $fs = [System.IO.File]::Open($candidate, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            $stagingTmp = $candidate
            $stagingFile = $fs
            break
        } catch [System.IO.IOException] {
            if ($_.Exception.Message -match "already exists") { continue }
            throw
        }
    }
    if (-not $stagingFile) { throw "could not allocate staging temp after $maxAttempts attempts" }

    $replaceSucceeded = $false
    $postVerifySucceeded = $false
    try {
        $stagingFile.Write($newBytes, 0, $newBytes.Length)
        $stagingFile.Flush($true)
        $stagingFile.Close()
        $stagingFile = $null

        if ($injectFail -eq "after-staging") {
            throw "injected failure after staging temp written (testing rollback)"
        }

        # Atomic replace: prefer Replace (preserves backup), fallback Move.
        # Keep backup path for rollback.
        if ($hadOriginal) {
            $backupPath = "$tracked.bak-$([guid]::NewGuid().ToString('N').Substring(0,8))"
            $replaced = $false
            try {
                [System.IO.File]::Replace($stagingTmp, $tracked, $backupPath)
                $stagingTmp = $null
                $replaced = $true
            } catch {
                # Fallback: Move with overwrite
                for ($r = 0; $r -lt 6; $r++) {
                    try {
                        # For rollback we need a backup: copy original to backup first.
                        # Actually create backup by copying original bytes we retained.
                        if (-not (Test-Path -LiteralPath $backupPath -PathType Leaf)) {
                            [System.IO.File]::WriteAllBytes($backupPath, $originalBytes)
                        }
                        [System.IO.File]::Move($stagingTmp, $tracked, $true)
                        $stagingTmp = $null
                        $replaced = $true
                        break
                    } catch [System.IO.IOException] {
                        if ($r -eq 5) { throw "atomic replace failed: $($_.Exception.Message)" }
                        Start-Sleep -Milliseconds (50 * ($r + 1))
                    }
                }
            }
            if (-not $replaced) { throw "atomic replace failed" }
        } else {
            [System.IO.File]::Move($stagingTmp, $tracked)
            $stagingTmp = $null
        }
        $replaceSucceeded = $true

        if ($injectFail -eq "after-replace") { throw "injected failure after replace (testing rollback)" }

        Write-Host "  updated $tracked ($($newBytes.Length) bytes)" -ForegroundColor Green
        # Post-replace verification
        $verify = [System.IO.File]::ReadAllBytes($tracked)
        if ($verify.Length -ne $newBytes.Length) { throw "verify failed: length mismatch" }
        for ($i = 0; $i -lt $verify.Length; $i++) { if ($verify[$i] -ne $newBytes[$i]) { throw "verify failed at $i" } }
        if ($injectFail -eq "postverify-mismatch") {
            throw "verify failed at 0 (injected)"
        }
        $postVerifySucceeded = $true
    } catch {
        $err = $_
        # On any replace/postverify error, attempt bounded rollback to original.
        if ($replaceSucceeded -and $hadOriginal -and $originalBytes) {
            Write-Host "  post-replace failure — attempting rollback..." -ForegroundColor Yellow
            $rolledBack = $false
            for ($r = 0; $r -lt 6; $r++) {
                try {
                    if ($backupPath -and (Test-Path -LiteralPath $backupPath -PathType Leaf)) {
                        [System.IO.File]::Copy($backupPath, $tracked, $true)
                    } else {
                        [System.IO.File]::WriteAllBytes($tracked, $originalBytes)
                    }
                    $rolledBack = $true
                    break
                } catch [System.IO.IOException] {
                    Start-Sleep -Milliseconds (50 * ($r + 1))
                }
            }
            if ($rolledBack) {
                try {
                    $rb = [System.IO.File]::ReadAllBytes($tracked)
                    $ok = $rb.Length -eq $originalBytes.Length
                    if ($ok) { for ($i = 0; $i -lt $rb.Length; $i++) { if ($rb[$i] -ne $originalBytes[$i]) { $ok = $false; break } } }
                    if (-not $ok) { throw "rollback verify mismatch" }
                    Write-Host "  rollback verified" -ForegroundColor Green
                    # Rollback succeeded — try to clean backup, but if cleanup fails leave it with a message.
                    if ($backupPath -and (Test-Path -LiteralPath $backupPath -PathType Leaf)) {
                        $null = Cleanup-StagingWithRetry -Path $backupPath
                        if (Test-Path -LiteralPath $backupPath -PathType Leaf) {
                            Write-Host "  rollback succeeded but backup cleanup deferred: $backupPath" -ForegroundColor Yellow
                        } else { $backupPath = $null }
                    }
                } catch {
                    Write-Host "  rollback verify failed: $($_.Exception.Message)" -ForegroundColor Yellow
                    if ($backupPath) { Write-Host "  recovery: backup preserved at $backupPath" -ForegroundColor Yellow }
                    throw $err
                }
            } else {
                if ($backupPath) { Write-Host "  rollback failed — backup preserved at $backupPath" -ForegroundColor Yellow }
                throw $err
            }
        }
        throw $err
    } finally {
        try { if ($stagingFile) { $stagingFile.Close() } } catch {}
    }

    # Only delete backup after post-verify succeeded.
    if ($postVerifySucceeded -and $backupPath -and (Test-Path -LiteralPath $backupPath -PathType Leaf)) {
        $deleted = Cleanup-StagingWithRetry -Path $backupPath
        if ($deleted) { $backupPath = $null }
        else {
            Write-Host "  backup leftover (ignored, not compiled): $backupPath" -ForegroundColor Yellow
            $backupPath = $null
        }
    }
} finally {
    # Staging cleanup with bounded sharing retries; AV lock may leave residue — report, don't fail.
    if ($stagingTmp -and (Test-Path -LiteralPath $stagingTmp -PathType Leaf)) {
        $null = Cleanup-StagingWithRetry -Path $stagingTmp
    }
    # Gen temp cleanup (system temp) with bounded retries.
    for ($r = 0; $r -lt 4; $r++) {
        try {
            if (Test-Path -LiteralPath $genTmpDir -PathType Container) { Remove-Item -LiteralPath $genTmpDir -Recurse -Force -ErrorAction Stop }
            break
        } catch [System.IO.IOException] { Start-Sleep -Milliseconds 50 }
        catch { break }
    }
    if (Test-Path -LiteralPath $genTmpDir -PathType Container) {
        Write-Host "  gen temp leftover (system temp, not source tree): $genTmpDir" -ForegroundColor Yellow
    }
    # If rollback failed and backup remains, never silently delete it.
    if ($backupPath -and (Test-Path -LiteralPath $backupPath -PathType Leaf)) {
        Write-Host "  recovery: backup at $backupPath — restore manually if needed" -ForegroundColor Yellow
    }
}
