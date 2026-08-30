#!/usr/bin/env pwsh
#requires -Version 7.4
# Maintainer-only binding regeneration — never invoked during normal cargo build/CI/release.
# Uses committed repo-local tools/bindings-generator (Cargo.lock pinned, --locked --offline, no network).
# Transactional state machine: original bytes retained; backup deleted only after post-verify succeeds.

param(
    [switch]$CheckOnly,
    [switch]$VerifyToolchainOnly,
    [switch]$VerifyLockOnly,
    [switch]$PrepareToolchainCache
)

$ErrorActionPreference = "Stop"
# Fail-closed switch combinations: exactly zero or one of -CheckOnly/-VerifyToolchainOnly/-VerifyLockOnly/-PrepareToolchainCache may be set.
$__switchCount = 0
if ($CheckOnly) { $__switchCount++ }
if ($VerifyToolchainOnly) { $__switchCount++ }
if ($VerifyLockOnly) { $__switchCount++ }
if ($PrepareToolchainCache) { $__switchCount++ }
if ($__switchCount -gt 1) { throw "cannot combine -CheckOnly/-VerifyToolchainOnly/-VerifyLockOnly/-PrepareToolchainCache (fail-closed: exactly zero or one may be set)" }
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

# Canonical crates.io registry source as observed via `cargo metadata --locked --offline --format-version 1`
# for the pinned csbindgen version. Must match exactly; vendor/source-replacement that retains the
# canonical package source (e.g. cargo vendor with [source."crates-io"] replace-with) keeps this value
# and therefore remains offline-compatible. Any git/path/alternate registry or null/empty source fails.
$ExpectedCsbindgenSource = "registry+https://github.com/rust-lang/crates.io-index"
$ExpectedCsbindgenChecksum = "950f59b281d7e20f050b4efd56d7c36c0deb853bf9ea1f20b985a75ae5b03b34"

$genTmpDir = Join-Path ([IO.Path]::GetTempPath()) ("tron-bindings-gen-" + [guid]::NewGuid().ToString("N"))
$genOutput = Join-Path $genTmpDir "NativeMethods.g.cs"
$stagingTmp = $null
$backupPath = $null
$injectFail = $env:TRONCLASS_UPDATE_BINDINGS_INJECT_FAIL

function Invoke-CargoProcessWithTimeout {
    param(
        [Parameter(Mandatory)][string[]]$CargoArgs,
        [Parameter(Mandatory)][string]$Label,
        [int]$TimeoutMs = 600000
    )
    if (Test-Path Env:CARGO_NET_OFFLINE) {
        $v = $env:CARGO_NET_OFFLINE
        if ([string]::IsNullOrWhiteSpace($v)) {
            Remove-Item Env:CARGO_NET_OFFLINE -ErrorAction SilentlyContinue
        } elseif ($v -ne "true" -and $v -ne "false") {
            Remove-Item Env:CARGO_NET_OFFLINE -ErrorAction SilentlyContinue
        }
    }
    $psi = [Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $cargo
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    foreach ($a in $CargoArgs) { $null = $psi.ArgumentList.Add($a) }
    $proc = [Diagnostics.Process]::new()
    $proc.StartInfo = $psi
    $outTask = $null
    $errTask = $null
    $out = ""
    $err = ""
    try {
        if (-not $proc.Start()) { throw "failed to start cargo $Label" }
        $outTask = $proc.StandardOutput.ReadToEndAsync()
        $errTask = $proc.StandardError.ReadToEndAsync()
        $exited = $proc.WaitForExit($TimeoutMs)
        if (-not $exited) {
            try { $proc.Kill($true) } catch { try { $proc.Kill() } catch {} }
            # Bounded drain after kill — do not block indefinitely on .Result.
            try { [Threading.Tasks.Task]::WhenAll($outTask, $errTask).Wait(10000) } catch {}
            $partial = ""
            try {
                if ($null -ne $errTask -and $errTask.IsCompletedSuccessfully) { $partial = $errTask.Result }
                elseif ($null -ne $errTask -and $errTask.IsCompleted) {
                    try { $partial = $errTask.Result } catch { $partial = "" }
                }
            } catch { $partial = "" }
            if (-not [string]::IsNullOrWhiteSpace($partial)) {
                $partial = $partial.Trim()
                if ($partial.Length -gt 2048) { $partial = $partial.Substring($partial.Length - 2048) }
                $partial = ": $partial"
            }
            throw "cargo $Label timeout after $($TimeoutMs/1000)s$partial"
        }
        # Process exited — drain pipes with bounded wait, avoid indefinite .Result block.
        $drained = $false
        try { $drained = [Threading.Tasks.Task]::WhenAll($outTask, $errTask).Wait(10000) } catch { $drained = $false }
        if (-not $drained) {
            # One more bounded attempt before falling back to whatever completed.
            try { [Threading.Tasks.Task]::WhenAll($outTask, $errTask).Wait(5000) } catch {}
        }
        # Only read Result if task completed; otherwise use empty to avoid blocking.
        if ($null -ne $outTask -and $outTask.IsCompleted) {
            try { $out = $outTask.Result } catch { $out = "" }
        }
        if ($null -ne $errTask -and $errTask.IsCompleted) {
            try { $err = $errTask.Result } catch { $err = "" }
        }
        if ($proc.ExitCode -ne 0) {
            $detail = ""
            if (-not [string]::IsNullOrWhiteSpace($err)) { $detail = $err.Trim() }
            elseif (-not [string]::IsNullOrWhiteSpace($out)) { $detail = $out.Trim() }
            if ($detail.Length -gt 2048) { $detail = $detail.Substring($detail.Length - 2048) }
            $msg = "cargo $Label failed (exit $($proc.ExitCode))"
            if (-not [string]::IsNullOrWhiteSpace($detail)) { $msg += ": $detail" }
            throw $msg
        }
    } finally { try { $proc.Dispose() } catch {} }
    return @{ Out = $out; Err = $err }
}

function Invoke-CargoMetadataLockedOffline {
    param([Parameter(Mandatory)][string]$ManifestPath)
    if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) { throw "missing manifest: $ManifestPath" }
    $hadOffline = Test-Path Env:CARGO_NET_OFFLINE
    $prevOffline = if ($hadOffline) { $env:CARGO_NET_OFFLINE } else { $null }
    $env:CARGO_NET_OFFLINE = "true"
    try {
        $label = "metadata --locked --offline $ManifestPath"
        $res = Invoke-CargoProcessWithTimeout -CargoArgs @("metadata","--manifest-path",$ManifestPath,"--locked","--offline","--format-version","1") -Label $label -TimeoutMs 90000
        $out = $res.Out
        if ([string]::IsNullOrWhiteSpace($out)) { throw "cargo metadata empty output for $ManifestPath" }
        try { $json = $out | ConvertFrom-Json }
        catch { throw "cargo metadata malformed JSON for $ManifestPath : $($_.Exception.Message)" }
        return $json
    } finally {
        if ($hadOffline) {
            if ([string]::IsNullOrWhiteSpace($prevOffline) -or ($prevOffline -ne "true" -and $prevOffline -ne "false")) {
                Remove-Item Env:CARGO_NET_OFFLINE -ErrorAction SilentlyContinue
            } else {
                $env:CARGO_NET_OFFLINE = $prevOffline
            }
        } else { Remove-Item Env:CARGO_NET_OFFLINE -ErrorAction SilentlyContinue }
    }
}

function Get-ExpectedCsbindgenPin {
    # Derive single fail-closed expected version from the two committed Cargo.toml pins.
    # Requires exactly one `csbindgen = "=x.y.z"` (or `{ version = "=x.y.z" }`) per manifest and equality.
    $coreManifest = Join-Path $root "core/Cargo.toml"
    $re = 'csbindgen\s*=\s*(?:\"=([^\"]+)\"|\{[^\}]*version\s*=\s*\"=([^\"]+)\"[^\}]*\})'
    $genRaw = Get-Content -Raw -LiteralPath $generatorManifest
    $coreRaw = Get-Content -Raw -LiteralPath $coreManifest
    $genMatches = [regex]::Matches($genRaw, $re)
    $coreMatches = [regex]::Matches($coreRaw, $re)
    if ($genMatches.Count -ne 1) { throw "expected exactly one csbindgen = pin in $generatorManifest (found $($genMatches.Count))" }
    if ($coreMatches.Count -ne 1) { throw "expected exactly one csbindgen = pin in $coreManifest (found $($coreMatches.Count))" }
    $genVer = if (-not [string]::IsNullOrWhiteSpace($genMatches[0].Groups[1].Value)) { $genMatches[0].Groups[1].Value } else { $genMatches[0].Groups[2].Value }
    $coreVer = if (-not [string]::IsNullOrWhiteSpace($coreMatches[0].Groups[1].Value)) { $coreMatches[0].Groups[1].Value } else { $coreMatches[0].Groups[2].Value }
    if ([string]::IsNullOrWhiteSpace($genVer) -or [string]::IsNullOrWhiteSpace($coreVer)) { throw "csbindgen pin version empty in manifests" }
    if ($genVer -ne $coreVer) { throw "csbindgen pin mismatch: generator $genVer vs core $coreVer (fix Cargo.toml to same =version)" }
    return $genVer
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

function Get-CsbindgenPackage {
    param(
        [Parameter(Mandatory)][object]$Metadata,
        [Parameter(Mandatory)][string]$ManifestPath
    )
    if ($null -eq $Metadata -or $null -eq $Metadata.packages) { throw "cargo metadata missing packages for $ManifestPath" }
    $pkgs = @($Metadata.packages | Where-Object { $_.name -eq "csbindgen" })
    if ($pkgs.Count -eq 0) { throw "csbindgen not found in cargo metadata for $ManifestPath (missing lockfile or dependency)" }
    if ($pkgs.Count -ne 1) { throw "csbindgen has $($pkgs.Count) packages in metadata for $ManifestPath (expected exactly 1; distinct versions/sources must be exactly one)" }
    $pkg = $pkgs[0]
    if ([string]::IsNullOrWhiteSpace([string]$pkg.name)) { throw "csbindgen name empty in metadata for $ManifestPath" }
    if ([string]::IsNullOrWhiteSpace([string]$pkg.version)) { throw "csbindgen version empty in metadata for $ManifestPath" }
    if ($null -eq $pkg.source -or [string]::IsNullOrWhiteSpace([string]$pkg.source)) { throw "csbindgen source missing/empty in metadata for $ManifestPath (expected canonical registry source $ExpectedCsbindgenSource)" }
    if ($null -eq $pkg.id -or [string]::IsNullOrWhiteSpace([string]$pkg.id)) { throw "csbindgen id missing/empty in metadata for $ManifestPath" }
    return $pkg
}

function Assert-CsbindgenVersionGate {
    if (-not (Test-Path -LiteralPath $generatorManifest -PathType Leaf)) { throw "missing generator manifest: $generatorManifest" }
    if (-not (Test-Path -LiteralPath $genLock -PathType Leaf)) { throw "missing generator lock: $genLock" }
    if (-not (Test-Path -LiteralPath $coreLock -PathType Leaf)) { throw "missing core lock: $coreLock" }
    $coreManifest = Join-Path $root "core/Cargo.toml"
    if (-not (Test-Path -LiteralPath $coreManifest -PathType Leaf)) { throw "missing core manifest: $coreManifest" }
    $expectedVer = Get-ExpectedCsbindgenPin
    $expectedId = "$ExpectedCsbindgenSource#csbindgen@$expectedVer"
    $genMeta = Invoke-CargoMetadataLockedOffline -ManifestPath $generatorManifest
    $coreMeta = Invoke-CargoMetadataLockedOffline -ManifestPath $coreManifest
    if ($null -eq $genMeta.packages -or $null -eq $coreMeta.packages) { throw "cargo metadata missing packages array" }
    $genPkg = Get-CsbindgenPackage -Metadata $genMeta -ManifestPath $generatorManifest
    $corePkg = Get-CsbindgenPackage -Metadata $coreMeta -ManifestPath $coreManifest
    if ($genPkg.version -ne $expectedVer) { throw "csbindgen version mismatch generator $($genPkg.version) != expected $expectedVer (fix Cargo.toml pin to =$expectedVer)" }
    if ($corePkg.version -ne $expectedVer) { throw "csbindgen version mismatch core $($corePkg.version) != expected $expectedVer (fix Cargo.toml pin to =$expectedVer)" }
    if ($genPkg.source -ne $ExpectedCsbindgenSource) { throw "csbindgen source not canonical for generator: $($genPkg.source) != $ExpectedCsbindgenSource (git/path/alternate registry not allowed)" }
    if ($corePkg.source -ne $ExpectedCsbindgenSource) { throw "csbindgen source not canonical for core: $($corePkg.source) != $ExpectedCsbindgenSource (git/path/alternate registry not allowed)" }
    if ($genPkg.id -ne $expectedId) { throw "csbindgen id not canonical for generator: $($genPkg.id) != $expectedId" }
    if ($corePkg.id -ne $expectedId) { throw "csbindgen id not canonical for core: $($corePkg.id) != $expectedId" }
    if ($genPkg.name -ne $corePkg.name -or $genPkg.version -ne $corePkg.version -or $genPkg.source -ne $corePkg.source -or $genPkg.id -ne $corePkg.id) {
        throw "csbindgen triple mismatch: generator $($genPkg.name) $($genPkg.version) $($genPkg.source) $($genPkg.id) vs core $($corePkg.name) $($corePkg.version) $($corePkg.source) $($corePkg.id) (must be identical)"
    }
    Write-Host "  csbindgen $expectedVer OK (generator == core, locked offline, source $ExpectedCsbindgenSource)" -ForegroundColor Green
    return $expectedVer
}

function Get-LockPackagesStrict {
    param([Parameter(Mandatory)][string]$LockPath)
    if (-not (Test-Path -LiteralPath $LockPath -PathType Leaf)) { throw "missing lock file: $LockPath" }
    $bytes = [System.IO.File]::ReadAllBytes($LockPath)
    if ($bytes.Length -eq 0) { throw "$LockPath`: empty file" }
    if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) { throw "$LockPath`: BOM not allowed" }
    $utf8Strict = [System.Text.UTF8Encoding]::new($false, $true)
    try { $text = $utf8Strict.GetString($bytes) }
    catch { throw "$LockPath`: invalid UTF-8: $($_.Exception.Message)" }
    $lines = $text -split "\r?\n"
    $versionFound = $false
    $versionValue = $null
    $packages = [System.Collections.Generic.List[hashtable]]::new()
    $seenBlockKeys = [System.Collections.Generic.HashSet[string]]::new()
    $current = $null
    $currentKeys = $null
    $currentStartLine = 0
    $inDepsArray = $false
    $depsArrayLine = 0
    for ($idx = 0; $idx -lt $lines.Count; $idx++) {
        $lineNum = $idx + 1
        $rawLine = $lines[$idx]
        $trimmed = $rawLine.Trim()
        if ($inDepsArray) {
            if ($trimmed -eq "" -or $trimmed.StartsWith("#")) { continue }
            if ($trimmed -eq "]") { $inDepsArray = $false; continue }
            if ($trimmed -match '^"[^"]*"\s*,?\s*(?:#.*)?$') { continue }
            throw "$LockPath`:$lineNum`: malformed dependencies entry: $rawLine (opened at $depsArrayLine)"
        }
        if ($trimmed -eq "" -or $trimmed.StartsWith("#")) { continue }
        if ($trimmed -match '^\s*version\s*=\s*(.+?)\s*$') {
            $isHeader = ($null -eq $current) -and (-not $versionFound)
            if ($isHeader) {
                $valPart = $Matches[1].Trim()
                if ($valPart -match '^"(\d+)"$') { $valPart = $Matches[1] }
                elseif ($valPart -match '^(\d+)$') { $valPart = $Matches[1] }
                else { throw "$LockPath`:$lineNum`: malformed version value: $valPart" }
                $versionFound = $true
                $versionValue = $valPart
                if ($versionValue -ne "4") { throw "$LockPath`:$lineNum`: unsupported lock version $versionValue (expected 4)" }
                continue
            }
        }
        if ($trimmed -eq "[[package]]") {
            if ($null -ne $current) {
                if (-not $current.ContainsKey("name")) { throw "$LockPath`:$currentStartLine`: package block missing name" }
                if (-not $current.ContainsKey("version")) { throw "$LockPath`:$currentStartLine`: package block missing version for $($current["name"])" }
                $srcKey = if ($current.ContainsKey("source")) { $current["source"] } else { "" }
                $key = "$($current["name"])|$($current["version"])|$srcKey"
                if (-not $seenBlockKeys.Add($key)) { throw "$LockPath`:$currentStartLine`: duplicate package block $key" }
                $packages.Add($current)
            }
            $current = @{}
            $currentKeys = [System.Collections.Generic.HashSet[string]]::new()
            $currentStartLine = $lineNum
            continue
        }
        if ($trimmed -eq "[package]") {
            throw "$LockPath`:$lineNum`: malformed package header [package] (expected [[package]])"
        }
        if ($null -eq $current) {
            throw "$LockPath`:$lineNum`: unexpected content outside [[package]]: $trimmed"
        }
        if ($trimmed -match '^dependencies\s*=\s*\[') {
            if (-not $currentKeys.Add("dependencies")) { throw "$LockPath`:$lineNum`: duplicate key dependencies in package at $currentStartLine" }
            $current["dependencies"] = "[array]"
            $after = $trimmed.Substring($trimmed.IndexOf("["))
            if ($after -match '^\[.*\]\s*(?:#.*)?$') {
                continue
            } else {
                $remainder = $after.Substring(1).Trim()
                if ($remainder -ne "" -and $remainder -notmatch '^(?:#.*)?$') {
                    throw "$LockPath`:$lineNum`: malformed dependencies array start: $rawLine"
                }
                $inDepsArray = $true
                $depsArrayLine = $lineNum
                continue
            }
        }
        # Quoted value may contain # — must not strip as comment. Try quoted first, then digits.
        $qMatch = [regex]::Match($trimmed, '^([A-Za-z0-9_-]+)\s*=\s*("[^"]*")\s*(?:#.*)?$')
        if ($qMatch.Success) {
            $k = $qMatch.Groups[1].Value
            $vRaw = $qMatch.Groups[2].Value
            if (-not $currentKeys.Add($k)) { throw "$LockPath`:$lineNum`: duplicate key $k in package at $currentStartLine" }
            $inner = $vRaw.Substring(1, $vRaw.Length - 2)
            $current[$k] = $inner
        } else {
            $nMatch = [regex]::Match($trimmed, '^([A-Za-z0-9_-]+)\s*=\s*(\d+)\s*(?:#.*)?$')
            if ($nMatch.Success) {
                $k = $nMatch.Groups[1].Value
                $vRaw = $nMatch.Groups[2].Value
                if (-not $currentKeys.Add($k)) { throw "$LockPath`:$lineNum`: duplicate key $k in package at $currentStartLine" }
                $current[$k] = $vRaw
            } else {
                throw "$LockPath`:$lineNum`: malformed line: $rawLine"
            }
        }
    }
    if ($inDepsArray) { throw "$LockPath`:$depsArrayLine`: unclosed dependencies array" }
    if ($null -ne $current) {
        if (-not $current.ContainsKey("name")) { throw "$LockPath`:$currentStartLine`: package block missing name" }
        if (-not $current.ContainsKey("version")) { throw "$LockPath`:$currentStartLine`: package block missing version" }
        $srcKey = if ($current.ContainsKey("source")) { $current["source"] } else { "" }
        $key = "$($current["name"])|$($current["version"])|$srcKey"
        if (-not $seenBlockKeys.Add($key)) { throw "$LockPath`:$currentStartLine`: duplicate package block $key" }
        $packages.Add($current)
    }
    if (-not $versionFound) { throw "$LockPath`: missing version = 4 header" }
    return $packages
}

function Assert-LockPreflight {
    $pin = Get-ExpectedCsbindgenPin
    $cargoConfigPaths = @(
        (Join-Path $root ".cargo/config.toml")
        (Join-Path $root ".cargo/config")
    )
    foreach ($cc in $cargoConfigPaths) {
        if (Test-Path -LiteralPath $cc -PathType Leaf) {
            $cBytes = [System.IO.File]::ReadAllBytes($cc)
            $cText = [System.Text.Encoding]::UTF8.GetString($cBytes)
            if ($cText -match '(?m)^\s*\[source' -or $cText -match 'replace-with' -or $cText -match 'directory\s*=') {
                $isVendorAllow = $false
                if ($cText -match 'replace-with\s*=\s*[''"]vendored-sources[''"]') { $isVendorAllow = $true }
                if (-not $isVendorAllow) {
                    throw "cargo config at $cc contains [source]/replace-with/directory (fail-closed; vendor not allow-listed)"
                }
                # Even when vendored-sources is present, reject any extra [source.*] beyond the two allowed ones,
                # and any registry/git/path source injection.
                $sourceHeaders = [regex]::Matches($cText, '(?m)^\s*\[source\.([^\]]+)\]')
                foreach ($m in $sourceHeaders) {
                    $srcName = $m.Groups[1].Value.Trim()
                    # Allow only "crates-io" and "vendored-sources" (with or without quotes)
                    $normalized = $srcName -replace '^[''"]|[''"]$',''
                    if ($normalized -ne "crates-io" -and $normalized -ne "vendored-sources") {
                        throw "cargo config at $cc contains non-allow-listed [source.$srcName] (fail-closed)"
                    }
                }
                if ($cText -match '(?m)^\s*registry\s*=') {
                    throw "cargo config at $cc contains registry = (fail-closed; only vendored directory allowed)"
                }
                # Directory must be exactly "vendor" under [source.vendored-sources]; any other path is fail-closed.
                $dirMatches = [regex]::Matches($cText, '(?m)^\s*directory\s*=\s*[''"]?([^''"\s#]+)[''"]?\s*(?:#.*)?$')
                foreach ($dm in $dirMatches) {
                    $dirVal = $dm.Groups[1].Value.Trim()
                    if ($dirVal -ne "vendor") {
                        throw "cargo config at $cc contains non-allow-listed directory = `"$dirVal`" (only `"vendor`" allowed)"
                    }
                }
            }
        }
    }
    $genPkgs = Get-LockPackagesStrict -LockPath $genLock
    $corePkgs = Get-LockPackagesStrict -LockPath $coreLock
    foreach ($entry in @(@{ pkgs=$genPkgs; path=$genLock }, @{ pkgs=$corePkgs; path=$coreLock })) {
        $pkgs = $entry.pkgs
        $lockPath = $entry.path
        $cs = @($pkgs | Where-Object { $_["name"] -eq "csbindgen" })
        if ($cs.Count -ne 1) { throw "$lockPath`: expected exactly one csbindgen package block, found $($cs.Count)" }
        $c = $cs[0]
        if ($c["version"] -ne $pin) { throw "$lockPath`: csbindgen version $($c["version"]) != pin $pin" }
        $src = $c["source"]
        if ([string]::IsNullOrWhiteSpace($src)) { throw "$lockPath`: csbindgen source missing/empty (expected $ExpectedCsbindgenSource)" }
        if ($src -ne $ExpectedCsbindgenSource) { throw "$lockPath`: csbindgen source not canonical: $src != $ExpectedCsbindgenSource (git/path/alternate registry not allowed)" }
        $chk = $c["checksum"]
        if ([string]::IsNullOrWhiteSpace($chk)) { throw "$lockPath`: csbindgen checksum missing/empty" }
        if ($chk -cnotmatch '^[0-9a-f]{64}$') { throw "$lockPath`: csbindgen checksum malformed: $chk (expected 64 lower hex)" }
        if ($chk -ne $ExpectedCsbindgenChecksum) { throw "$lockPath`: csbindgen checksum $chk != expected $ExpectedCsbindgenChecksum (must match pinned canonical crate)" }
        foreach ($p in $pkgs) {
            $n = $p["name"]
            $s = if ($p.ContainsKey("source")) { $p["source"] } else { $null }
            $ch = if ($p.ContainsKey("checksum")) { $p["checksum"] } else { $null }
            if ([string]::IsNullOrWhiteSpace($n)) { throw "$lockPath`: package name empty" }
            if (-not $p.ContainsKey("version") -or [string]::IsNullOrWhiteSpace($p["version"])) { throw "$lockPath`: package $n version empty" }
            if ($null -ne $s) {
                if ([string]::IsNullOrWhiteSpace($s)) { throw "$lockPath`: package $n source empty" }
                if ($s -ne $ExpectedCsbindgenSource) { throw "$lockPath`: package $n source not canonical: $s (only $ExpectedCsbindgenSource allowed; git/path/alternate rejected)" }
                if ([string]::IsNullOrWhiteSpace($ch)) { throw "$lockPath`: package $n checksum missing for registry source" }
                if ($ch -cnotmatch '^[0-9a-f]{64}$') { throw "$lockPath`: package $n checksum malformed: $ch" }
            } else {
                if ($n -ne "tronclass-core" -and $n -ne "tron-bindings-gen") {
                    throw "$lockPath`: package $n source missing (expected canonical registry source)"
                }
                if ($null -ne $ch -and -not [string]::IsNullOrWhiteSpace($ch)) {
                    throw "$lockPath`: package $n unexpected checksum for path package"
                }
            }
        }
    }
    $genCs = @($genPkgs | Where-Object { $_["name"] -eq "csbindgen" })[0]
    $coreCs = @($corePkgs | Where-Object { $_["name"] -eq "csbindgen" })[0]
    if ($genCs["version"] -ne $coreCs["version"] -or $genCs["source"] -ne $coreCs["source"] -or $genCs["checksum"] -ne $coreCs["checksum"]) {
        throw "csbindgen mismatch between locks: generator $($genCs["version"]) $($genCs["source"]) $($genCs["checksum"]) vs core $($coreCs["version"]) $($coreCs["source"]) $($coreCs["checksum"]) (must be identical)"
    }
    Write-Host "  lock preflight OK (csbindgen $pin canonical, checksum $($genCs["checksum"].Substring(0,8))..., locks identical)" -ForegroundColor Green
}

function Invoke-CargoFetchLocked {
    param([Parameter(Mandatory)][string]$ManifestPath)
    if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) { throw "missing manifest: $ManifestPath" }
    $hadOffline = Test-Path Env:CARGO_NET_OFFLINE
    $prevOffline = if ($hadOffline) { $env:CARGO_NET_OFFLINE } else { $null }
    Remove-Item Env:CARGO_NET_OFFLINE -ErrorAction SilentlyContinue
    try {
        $label = "fetch --locked $ManifestPath"
        $null = Invoke-CargoProcessWithTimeout -CargoArgs @("fetch","--locked","--manifest-path",$ManifestPath) -Label $label -TimeoutMs 600000
        Write-Host "  cargo fetch --locked OK for $ManifestPath (downloads locked checksummed sources, no compile/build script)" -ForegroundColor Green
    } finally {
        if ($hadOffline) {
            if ([string]::IsNullOrWhiteSpace($prevOffline) -or ($prevOffline -ne "true" -and $prevOffline -ne "false")) {
                Remove-Item Env:CARGO_NET_OFFLINE -ErrorAction SilentlyContinue
            } else {
                $env:CARGO_NET_OFFLINE = $prevOffline
            }
        } else { Remove-Item Env:CARGO_NET_OFFLINE -ErrorAction SilentlyContinue }
    }
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
    # VerifyLockOnly: static preflight only — no cargo, no network, no CARGO_HOME, no writes, no temp.
    if ($VerifyLockOnly) {
        Assert-LockPreflight
        Write-Host "  verify lock only: OK (offline, no cargo, side-effect-free)" -ForegroundColor Green
        exit 0
    }
    # PrepareToolchainCache: static preflight first (fail-closed before any network), then cargo fetch --locked for both manifests (no build script execution), then stays online caller scope; no tracked file writes.
    if ($PrepareToolchainCache) {
        Assert-LockPreflight
        Write-Host "  priming cargo registry cache (cargo fetch --locked, canonical only; does not compile or execute build scripts)..." -ForegroundColor Cyan
        Invoke-CargoFetchLocked -ManifestPath $generatorManifest
        Invoke-CargoFetchLocked -ManifestPath (Join-Path $root "core/Cargo.toml")
        Write-Host "  prepare toolchain cache: OK (CARGO_NET_OFFLINE not set here; caller must gate next step offline)" -ForegroundColor Green
        exit 0
    }
    # VerifyToolchainOnly: static preflight + cargo metadata offline triple gate, side-effect-free.
    if ($VerifyToolchainOnly) {
        Assert-LockPreflight
        $null = Assert-CsbindgenVersionGate
        Write-Host "  verify toolchain only: OK" -ForegroundColor Green
        exit 0
    }

    # Normal generation/update: static preflight + offline metadata gate before any staging.
    Assert-LockPreflight
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
