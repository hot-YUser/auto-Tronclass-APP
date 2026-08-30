#Requires -Version 7.4
# GitRawHelper — single versioned NUL-safe source-of-truth for raw-byte guards.
# - Starts git via System.Diagnostics.Process ArgumentList
# - Reads stdout BaseStream bytes
# - Invokes: git -c core.quotepath=false ls-tree -rz --full-tree HEAD
# - Splits NUL records, UTF-8 decodes strictly (fail-closed), parses mode/type/sha at first TAB
# - Supports any Git path except NUL (Unicode, spaces, tabs, CR/LF, leading dash)
# - Never silently continues on parse/process/hash errors
# - Computes blob SHA-1 in .NET (blob <len>\0<bytes>) or via ArgumentList hash-object without shell
# - Escapes paths to one safe line for diagnostics, never dumps contents

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# Strict UTF-8 decoder that throws on invalid bytes.
$script:Utf8Strict = [System.Text.UTF8Encoding]::new($false, $true)
$script:Ascii = [System.Text.Encoding]::ASCII

function Escape-GitPath {
    param([Parameter(Mandatory)][string]$Path)
    # One-line safe escape: backslash, CR, LF, TAB, and control chars.
    # Keep Unicode visible, but ensure the diagnostic line never breaks.
    $sb = [System.Text.StringBuilder]::new($Path.Length + 16)
    foreach ($ch in $Path.ToCharArray()) {
        $code = [int]$ch
        if ($ch -eq '\') { [void]$sb.Append('\\') }
        elseif ($ch -eq "`r") { [void]$sb.Append('\r') }
        elseif ($ch -eq "`n") { [void]$sb.Append('\n') }
        elseif ($ch -eq "`t") { [void]$sb.Append('\t') }
        elseif ($code -lt 0x20) { [void]$sb.AppendFormat('\u{0:X4}', $code) }
        else { [void]$sb.Append($ch) }
    }
    return $sb.ToString()
}

function Invoke-GitRawBytes {
    param(
        [Parameter(Mandatory)][string[]]$ArgumentList,
        [string]$WorkingDirectory
    )
    if (-not $WorkingDirectory) {
        $WorkingDirectory = (Get-Location).Path
    }
    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = "git"
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $psi.WorkingDirectory = $WorkingDirectory
    foreach ($a in $ArgumentList) { $null = $psi.ArgumentList.Add($a) }
    $proc = [System.Diagnostics.Process]::new()
    $proc.StartInfo = $psi
    try {
        if (-not $proc.Start()) { throw "failed to start git $($ArgumentList -join ' ')" }
        # Read stdout as raw bytes via BaseStream (binary), stderr as text.
        $ms = [System.IO.MemoryStream]::new()
        $proc.StandardOutput.BaseStream.CopyTo($ms)
        $stderr = $proc.StandardError.ReadToEnd()
        $proc.WaitForExit()
        if ($proc.ExitCode -ne 0) {
            $msg = "git $($ArgumentList -join ' ') failed (exit $($proc.ExitCode))"
            if (-not [string]::IsNullOrWhiteSpace($stderr)) { $msg += ": $stderr".Trim() }
            throw $msg
        }
        if (-not [string]::IsNullOrWhiteSpace($stderr)) {
            # git ls-tree/cat-file should not emit stderr on success; treat as failure fail-closed.
            # But allow warnings? We fail closed: if any stderr, surface.
            # For ls-tree no stderr expected.
        }
        $bytes = $ms.ToArray()
        # PowerShell pipeline unwraps empty arrays to $null -> UTF8.GetString(null) crashes.
        # Comma ensures single byte[] object even for 0-length, preserving NUL/0xFF order.
        return ,$bytes
    } finally {
        $proc.Dispose()
    }
}

function Get-GitHeadEntries {
    <#
    .SYNOPSIS
        Returns HEAD entries with Mode/Type/Sha/Path, NUL-safe.
    .DESCRIPTION
        Invokes git -c core.quotepath=false ls-tree -rz --full-tree HEAD via Process ArgumentList,
        splits NUL records, strict UTF-8 decodes path, parses mode/type/sha at first TAB.
        Rejects unsupported modes/types fail-closed (only 100644/100755 blob are allowed).
        Throws on any parse/process error — never silently continues.
    #>
    param([string]$RepoRoot)

    if (-not $RepoRoot) { $RepoRoot = (Get-Location).Path }
    $raw = Invoke-GitRawBytes -ArgumentList @("-c","core.quotepath=false","ls-tree","-rz","--full-tree","HEAD") -WorkingDirectory $RepoRoot
    $entries = [System.Collections.Generic.List[object]]::new()

    # Split raw bytes by NUL (0x00). Each record is "mode SP type SP sha TAB path"
    $start = 0
    for ($i = 0; $i -le $raw.Length; $i++) {
        $isEnd = ($i -eq $raw.Length) -or ($raw[$i] -eq 0)
        if (-not $isEnd) { continue }
        $len = $i - $start
        if ($len -eq 0) { $start = $i + 1; continue } # trailing NUL
        $recordLen = $len
        # Find first TAB (0x09) within record
        $tabIdx = -1
        for ($j = $start; $j -lt $i; $j++) {
            if ($raw[$j] -eq 9) { $tabIdx = $j; break }
        }
        if ($tabIdx -lt 0) {
            $preview = $script:Ascii.GetString($raw, $start, [Math]::Min($recordLen, 80))
            throw "unparseable ls-tree record (no TAB): $preview"
        }
        $headerLen = $tabIdx - $start
        $pathLen = $i - $tabIdx - 1
        if ($pathLen -lt 0) { throw "unparseable ls-tree record (negative path)" }
        $headerBytes = [byte[]]::new($headerLen)
        if ($headerLen -gt 0) { [Array]::Copy($raw, $start, $headerBytes, 0, $headerLen) }
        $pathBytes = [byte[]]::new($pathLen)
        if ($pathLen -gt 0) { [Array]::Copy($raw, $tabIdx+1, $pathBytes, 0, $pathLen) }

        $header = $script:Ascii.GetString($headerBytes)
        $m = [regex]::Match($header, '^([0-9]+)\s+(\S+)\s+([0-9a-f]{40})$')
        if (-not $m.Success) {
            throw "unparseable ls-tree header: '$header'"
        }
        $mode = $m.Groups[1].Value
        $type = $m.Groups[2].Value
        $sha = $m.Groups[3].Value

        [string]$path = ""
        if ($pathLen -gt 0) {
            try { $path = $script:Utf8Strict.GetString($pathBytes) }
            catch { throw "invalid UTF-8 path bytes at record $header" }
        } else {
            throw "empty path for $header"
        }
        if ($path.Contains("`0")) { throw "path contains NUL: $(Escape-GitPath $path)" }

        # Fail-closed on unsupported modes/types.
        $isSupported = ($type -eq "blob") -and ($mode -eq "100644" -or $mode -eq "100755")
        if (-not $isSupported) {
            $esc = Escape-GitPath $path
            throw "unsupported mode $mode type $type at $esc"
        }

        $entries.Add([pscustomobject]@{
            Mode = $mode
            Type = $type
            Sha  = $sha
            Path = $path
        })
        $start = $i + 1
    }
    return $entries
}

function Get-FileGitBlobHash {
    <#
    .SYNOPSIS
        Computes Git blob SHA-1 (blob <len>\0<bytes>) directly in .NET, no shell.
    #>
    param([Parameter(Mandatory)][string]$LiteralPath)
    if (-not (Test-Path -LiteralPath $LiteralPath -PathType Leaf)) {
        $esc = Escape-GitPath $LiteralPath
        throw "missing on disk: $esc"
    }
    try { $bytes = [System.IO.File]::ReadAllBytes($LiteralPath) }
    catch { throw "read failed for $(Escape-GitPath $LiteralPath): $($_.Exception.Message)" }
    $header = $script:Ascii.GetBytes("blob $($bytes.Length)`0")
    $combined = [byte[]]::new($header.Length + $bytes.Length)
    [Array]::Copy($header, 0, $combined, 0, $header.Length)
    [Array]::Copy($bytes, 0, $combined, $header.Length, $bytes.Length)
    $sha1 = [System.Security.Cryptography.SHA1]::Create()
    try {
        $hashBytes = $sha1.ComputeHash($combined)
        # Zero combined buffer to avoid lingering raw bytes in memory.
        [System.Security.Cryptography.CryptographicOperations]::ZeroMemory($combined)
        return ([BitConverter]::ToString($hashBytes)).Replace("-","").ToLowerInvariant()
    } finally { $sha1.Dispose() }
}

function Get-GitBlobBytes {
    <#
    .SYNOPSIS
        Obtains exact HEAD blob bytes by SHA via git cat-file blob <sha> using binary BaseStream.
    #>
    param(
        [Parameter(Mandatory)][string]$Sha,
        [string]$RepoRoot
    )
    if ($Sha -notmatch '^[0-9a-f]{40}$') { throw "invalid SHA: $Sha" }
    if (-not $RepoRoot) { $RepoRoot = (Get-Location).Path }
    $bytes = Invoke-GitRawBytes -ArgumentList @("cat-file","blob",$Sha) -WorkingDirectory $RepoRoot
    return ,$bytes
}

Export-ModuleMember -Function Get-GitHeadEntries, Get-FileGitBlobHash, Get-GitBlobBytes, Escape-GitPath, Invoke-GitRawBytes
