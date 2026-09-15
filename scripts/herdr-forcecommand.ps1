# ForceCommand wrapper for a herdr-mirror `kind = "windows"` host.
#
# Optional but recommended: it lets a dedicated SSH key drive herdr and the
# herdr-mirror relay and nothing else. A plain SSH account works without it.
#
# Install:
#   1. Copy this file to the Windows host, e.g. C:\ProgramData\ssh\herdr-forcecommand.ps1
#   2. Set $HerdrExe / $RelayExe below to the SAME paths you put in hosts.toml
#      as `remote_bin` / `relay_bin`.
#   3. Add ONE line to the account's authorized_keys (all on one line):
#        restrict,from="<client-ip>",command="powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"C:\ProgramData\ssh\herdr-forcecommand.ps1\"" ssh-ed25519 AAAA... mirror
#
# Notes:
#   - `restrict` disables pty, agent/X11/TCP forwarding and user-rc. herdr-mirror
#     needs none of them for a Windows host: the API travels over the relay's
#     stdio, not a forwarded socket.
#   - On Windows the sshd account file is ~/.ssh/authorized_keys, except for
#     members of Administrators, which use
#     C:\ProgramData\ssh\administrators_authorized_keys. That file's ACL must be
#     exactly SYSTEM + Administrators or sshd ignores it silently. Grant by SID,
#     never by name -- the group is renamed on localized Windows:
#        icacls <file> /inheritance:r /grant "*S-1-5-18:F" /grant "*S-1-5-32-544:F"
#   - herdr's self-updater puts herdr.exe behind an NTFS junction that a network
#     logon cannot traverse. Point $HerdrExe at a plain copy and refresh it after
#     each herdr update.

$HerdrExe = 'C:\tools\herdr.exe'
$RelayExe = 'C:\tools\herdr-mirror.exe'

$orig = $env:SSH_ORIGINAL_COMMAND
if ([string]::IsNullOrEmpty($orig)) {
    Write-Error "herdr-forcecommand: no command supplied"
    exit 1
}

# Exactly two command shapes are allowed:
#   exec <HerdrExe> <args...>
#   exec <RelayExe> relay [--session-b64=<url-safe base64, no padding>]
$targets = @(
    [pscustomobject]@{ Prefix = "exec $HerdrExe "; Executable = $HerdrExe; Relay = $false },
    [pscustomobject]@{ Prefix = "exec $RelayExe "; Executable = $RelayExe; Relay = $true }
)
$target = $targets | Where-Object { $orig.StartsWith($_.Prefix) } | Select-Object -First 1
if ($null -eq $target) {
    Write-Error "herdr-forcecommand: rejected (only the configured herdr.exe or herdr-mirror.exe relay commands are allowed)"
    exit 1
}
$rest = $orig.Substring($target.Prefix.Length)

if ($target.Relay -and $rest -notmatch '^relay(?: --session-b64=[A-Za-z0-9_-]{2,171})?$') {
    Write-Error "herdr-forcecommand: rejected (invalid herdr-mirror relay command)"
    exit 1
}

if (-not (Test-Path -LiteralPath $target.Executable)) {
    Write-Error "herdr-forcecommand: binary missing at $($target.Executable)"
    exit 1
}

# herdr-mirror quotes values that may contain spaces POSIX-style with single
# quotes (config::shell_quote); tokenize the same way rather than handing the
# string to a remote shell. Deliberately minimal: single-quote toggling only,
# no backslash escapes, which is the whole grammar herdr-mirror emits.
function Split-PosixArgs([string]$s) {
    $result = @()
    $cur = New-Object System.Text.StringBuilder
    $inSingle = $false
    $have = $false
    foreach ($ch in $s.ToCharArray()) {
        if ($inSingle) {
            if ($ch -eq "'") { $inSingle = $false } else { [void]$cur.Append($ch) }
            $have = $true
        } elseif ($ch -eq "'") {
            $inSingle = $true
            $have = $true
        } elseif ($ch -eq ' ') {
            if ($have) { $result += $cur.ToString(); [void]$cur.Clear(); $have = $false }
        } else {
            [void]$cur.Append($ch)
            $have = $true
        }
    }
    if ($have) { $result += $cur.ToString() }
    return $result
}

function ConvertTo-WindowsArg([string]$a) {
    # Minimal CommandLineToArgvW-safe quoting: always quote, double any
    # backslashes that immediately precede the closing quote, escape embedded
    # quotes. Covers herdr's argument shapes (paths, session/pane ids, numbers).
    $escaped = $a -replace '(\\*)"', '$1$1\"'
    $escaped = $escaped -replace '(\\+)$', '$1$1'
    return '"' + $escaped + '"'
}

$parsedArgs = @(Split-PosixArgs $rest)

try {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $target.Executable
    # `.Arguments`, not `.ArgumentList`: sshd runs the system `powershell.exe`
    # (Windows PowerShell 5.1 / .NET Framework), where ArgumentList does not
    # exist -- even when pwsh is installed and is the interactive default.
    $psi.Arguments = ($parsedArgs | ForEach-Object { ConvertTo-WindowsArg $_ }) -join ' '
    $psi.UseShellExecute = $false
    $proc = [System.Diagnostics.Process]::Start($psi)
    $proc.WaitForExit()
    exit $proc.ExitCode
} catch {
    Write-Error "herdr-forcecommand: launch failed: $($_.Exception.Message)"
    exit 1
}
