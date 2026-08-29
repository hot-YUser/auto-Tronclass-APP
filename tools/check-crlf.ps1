#!/usr/bin/env pwsh
#requires -Version 7.4
# Explicit safe remediation/check for hidden CRLF on a normal Git-clean tree.
# - Without -Fix: checks raw bytes vs HEAD blob (like release guard) and fails if any tracked text file has CRLF-hidden mismatch.
# - With -Fix: first proves git status --porcelain is clean, then re-checkouts tracked files to normalize line endings per .gitattributes.
# Release itself never rewrites source; this tool requires explicit invocation.

param([switch]$Fix)

$ErrorActionPreference = "Stop"
$tools = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $tools
Set-Location $root

function Get-RawMismatch {
    $failures = [System.Collections.Generic.List[string]]::new()
    $names = @(git ls-tree -r --name-only HEAD)
    if ($LASTEXITCODE -ne 0) { throw "git ls-tree failed" }
    foreach ($name in $names) {
        $line = git ls-tree HEAD -- $name 2>$null
        if ($LASTEXITCODE -ne 0) { $failures.Add("$name (missing in HEAD)"); continue }
        $m = [regex]::Match($line, '^([0-9]+)\s+(\S+)\s+([0-9a-f]+)\s+')
        if (-not $m.Success) { continue }
        $mode = $m.Groups[1].Value
        $type = $m.Groups[2].Value
        if ($mode -eq "120000" -or $type -eq "commit") { $failures.Add("$name (symlink/submodule)"); continue }
        if (-not (Test-Path -LiteralPath $name -PathType Leaf)) { $failures.Add("$name (missing)"); continue }
        $raw = (git hash-object --no-filters -- $name 2>$null)
        if ($LASTEXITCODE -ne 0) { $failures.Add("$name (hash failed)"); continue }
        $blob = $m.Groups[3].Value
        if ($raw.Trim() -ne $blob) { $failures.Add($name) }
    }
    return $failures
}

if (-not $Fix) {
    $failures = Get-RawMismatch
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
$porcelain = @(git status --porcelain 2>$null)
if ($LASTEXITCODE -ne 0) { throw "git status failed" }
if ($porcelain.Count -gt 0) {
    $redacted = @($porcelain | ForEach-Object { ($_ -replace "^\s*\S+\s+", "").Trim() } | Where-Object { $_ -ne "" } | Select-Object -First 20)
    foreach ($p in $redacted) { Write-Host "  ! $p" -ForegroundColor Yellow }
    throw "refusing to re-checkout: working tree not clean via git status --porcelain ($($porcelain.Count) file(s)). Commit or stash first."
}

Write-Host "  git status clean — re-checking out tracked files to normalize line endings..." -ForegroundColor Cyan
# Collect files that currently mismatch raw bytes
$failures = Get-RawMismatch
if ($failures.Count -eq 0) {
    Write-Host "  no raw mismatches to fix" -ForegroundColor Green
    exit 0
}
$toFix = @($failures | Where-Object { -not $_.Contains(" (") })
Write-Host "  fixing $($toFix.Count) file(s)..."
foreach ($f in $toFix) {
    try { Remove-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue } catch {}
}
# Checkout will restore with correct eol per .gitattributes
git checkout -- . 2>$null
if ($LASTEXITCODE -ne 0) { throw "git checkout failed" }
# Also handle files that were missing (should be restored)
$remaining = Get-RawMismatch
if ($remaining.Count -gt 0) {
    $show = @($remaining | Select-Object -First 20)
    foreach ($p in $show) { Write-Host "  ! still mismatched: $p" -ForegroundColor Yellow }
    throw "re-checkout did not fix all mismatches ($($remaining.Count) remain)"
}
Write-Host "  re-checkout complete — raw bytes now match HEAD" -ForegroundColor Green
