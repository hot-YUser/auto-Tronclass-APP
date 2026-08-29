#!/usr/bin/env pwsh
#requires -Version 7.4
# Explicit safe remediation/check for hidden CRLF on a normal Git-clean tree.
# - Without -Fix: checks raw bytes vs HEAD blob (via GitRawHelper) and fails if any tracked text file has CRLF-hidden mismatch.
# - With -Fix: first proves git status --porcelain is clean, then per-file atomically replaces mismatched files from HEAD blobs.
# Release itself never rewrites source; this tool requires explicit invocation.

param([switch]$Fix)

$ErrorActionPreference = "Stop"
$tools = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $tools
Set-Location $root

Import-Module (Join-Path $tools "GitRawHelper.psm1") -Force

function Get-RawMismatch {
    $failures = [System.Collections.Generic.List[string]]::new()
    $entries = Get-GitHeadEntries -RepoRoot $root
    foreach ($e in $entries) {
        $rel = $e.Path
        $esc = Escape-GitPath $rel
        $abs = Join-Path $root $rel
        if (-not (Test-Path -LiteralPath $abs -PathType Leaf)) {
            $failures.Add("$esc (missing)")
            continue
        }
        try { $raw = Get-FileGitBlobHash -LiteralPath $abs }
        catch { throw "hash failed for $esc : $($_.Exception.Message)" }
        if ($raw -ne $e.Sha) { $failures.Add($esc) }
    }
    return $failures, $entries
}

if (-not $Fix) {
    $failures, $null = Get-RawMismatch
    if ($failures.Count -gt 0) {
        $show = @($failures | Select-Object -First 20)
        foreach ($p in $show) { Write-Host "  ! $p" -ForegroundColor Yellow }
        if ($failures.Count -gt 20) { Write-Host "  ... and $($failures.Count - 20) more" -ForegroundColor Yellow }
        throw "raw-byte check failed ($($failures.Count) file(s)); run pwsh ./tools/check-crlf.ps1 -Fix after confirming git status clean."
    }
    Write-Host "  raw-byte check: OK (all tracked files match HEAD raw bytes)" -ForegroundColor Green
    exit 0
}

# -Fix: require ordinary git status clean first
# Use ArgumentList-based git to avoid shell.
$porcelainBytes = Invoke-GitRawBytes -ArgumentList @("status","--porcelain") -WorkingDirectory $root
$porcelainText = [System.Text.Encoding]::UTF8.GetString($porcelainBytes)
$porcelain = @($porcelainText -split "`n" | Where-Object { $_.Trim() -ne "" })
if ($porcelain.Count -gt 0) {
    $redacted = @($porcelain | ForEach-Object { ($_ -replace "^\s*\S+\s+", "").Trim() } | Where-Object { $_ -ne "" } | Select-Object -First 20)
    foreach ($p in $redacted) { Write-Host "  ! $(Escape-GitPath $p)" -ForegroundColor Yellow }
    throw "refusing to re-checkout: working tree not clean via git status --porcelain ($($porcelain.Count) file(s)). Commit or stash first."
}

Write-Host "  git status clean — re-checking out tracked files to normalize line endings..." -ForegroundColor Cyan

$failures, $entries = Get-RawMismatch
if ($failures.Count -eq 0) {
    Write-Host "  no raw mismatches to fix" -ForegroundColor Green
    exit 0
}
# $failures contains escaped paths; need original paths for those mismatches.
# Re-derive mismatched entries (those where hash != sha).
$toFixEntries = [System.Collections.Generic.List[object]]::new()
$failSet = [System.Collections.Generic.HashSet[string]]::new()
foreach ($f in $failures) { $null = $failSet.Add($f) }
foreach ($e in $entries) {
    $esc = Escape-GitPath $e.Path
    if ($failSet.Contains($esc)) { $toFixEntries.Add($e) }
}

Write-Host "  fixing $($toFixEntries.Count) file(s)..."
$stagingFiles = [System.Collections.Generic.List[string]]::new()
try {
    foreach ($e in $toFixEntries) {
        $rel = $e.Path
        $abs = Join-Path $root $rel
        $dir = Split-Path -Parent $abs
        if (-not (Test-Path -LiteralPath $dir -PathType Container)) {
            New-Item -ItemType Directory -Force -Path $dir | Out-Null
        }
        # Obtain exact HEAD blob bytes by SHA via binary cat-file.
        $blobBytes = Get-GitBlobBytes -Sha $e.Sha -RepoRoot $root
        # Stage to unique same-directory sibling with CreateNew, Flush(true), no deletion of original yet.
        $attempts = 0
        $stagingPath = $null
        $fs = $null
        while ($attempts -lt 8) {
            $attempts++
            $suffix = [guid]::NewGuid().ToString("N").Substring(0, 8)
            $candidate = Join-Path $dir ".crlf-fix-$suffix.tmp"
            try {
                $fs = [System.IO.File]::Open($candidate, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
                $stagingPath = $candidate
                break
            } catch [System.IO.IOException] {
                if ($_.Exception.Message -match "already exists") { continue }
                throw
            }
        }
        if (-not $stagingPath) { throw "could not allocate staging for $(Escape-GitPath $rel)" }
        $null = $stagingFiles.Add($stagingPath)
        try {
            $fs.Write($blobBytes, 0, $blobBytes.Length)
            $fs.Flush($true)
            $fs.Close(); $fs = $null
            # Atomic replace with bounded Windows sharing retries.
            $retries = 0
            $maxRetries = 6
            $replaced = $false
            while (-not $replaced -and $retries -le $maxRetries) {
                try {
                    if (Test-Path -LiteralPath $abs -PathType Leaf) {
                        try { [System.IO.File]::Replace($stagingPath, $abs, $null) }
                        catch {
                            # .NET Replace with null backupPath throws on some runtimes; fallback to Move.
                            [System.IO.File]::Move($stagingPath, $abs, $true)
                        }
                    } else {
                        [System.IO.File]::Move($stagingPath, $abs)
                    }
                    $replaced = $true
                    # Remove from staging tracking since it is now the target.
                    $null = $stagingFiles.Remove($stagingPath)
                } catch [System.IO.IOException] {
                    if ($retries -eq $maxRetries) { throw }
                    $retries++
                    Start-Sleep -Milliseconds (50 * $retries)
                }
            }
            if (-not $replaced) { throw "atomic replace failed for $(Escape-GitPath $rel)" }
            # Revalidate this file immediately.
            $verifyHash = Get-FileGitBlobHash -LiteralPath $abs
            if ($verifyHash -ne $e.Sha) {
                throw "revalidate failed for $(Escape-GitPath $rel): expected $($e.Sha) got $verifyHash"
            }
        } finally {
            try { if ($fs) { $fs.Close() } } catch {}
        }
    }
} catch {
    # Preserve original (we never deleted it first) and cleanup staging siblings narrowly.
    throw
} finally {
    # Cleanup any leftover staging siblings (only those we created in this run).
    foreach ($s in @($stagingFiles)) {
        for ($r = 0; $r -lt 3; $r++) {
            try {
                if (Test-Path -LiteralPath $s -PathType Leaf) { Remove-Item -LiteralPath $s -Force -ErrorAction Stop; break }
                else { break }
            } catch [System.IO.IOException] { Start-Sleep -Milliseconds 50 }
            catch { break }
        }
        if (Test-Path -LiteralPath $s -PathType Leaf) {
            Write-Host "  leftover staging (ignored, not compiled): $(Escape-GitPath $s)" -ForegroundColor Yellow
        }
    }
}

# Revalidate all at end.
$remaining, $null = Get-RawMismatch
if ($remaining.Count -gt 0) {
    $show = @($remaining | Select-Object -First 20)
    foreach ($p in $show) { Write-Host "  ! still mismatched: $p" -ForegroundColor Yellow }
    throw "re-checkout did not fix all mismatches ($($remaining.Count) remain)"
}
Write-Host "  re-checkout complete — raw bytes now match HEAD" -ForegroundColor Green
