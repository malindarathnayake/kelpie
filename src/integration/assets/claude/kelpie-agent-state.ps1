# installed by kelpie
# managed by kelpie; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# KELPIE_INTEGRATION_ID=claude
# KELPIE_INTEGRATION_VERSION=13

param([string]$Action = "")

if ($Action -eq "prompt") {
    # UserPromptSubmit: kelpie reads the hook JSON and reports only a hash of the prompt.
    # The raw stdin bytes are forwarded unchanged, so no console code page touches them.
    if ($env:KELPIE_ENV -ne "1") { exit 0 }
    if ([string]::IsNullOrWhiteSpace($env:KELPIE_PANE_ID)) { exit 0 }
    if (Test-Path Env:CURSOR_VERSION) { exit 0 }
    try {
        $buffer = New-Object System.IO.MemoryStream
        [Console]::OpenStandardInput().CopyTo($buffer)
        $bytes = $buffer.ToArray()
        $kelpie = if ([string]::IsNullOrWhiteSpace($env:KELPIE_BIN_PATH)) { "kelpie" } else { $env:KELPIE_BIN_PATH }
        $startInfo = New-Object System.Diagnostics.ProcessStartInfo
        $startInfo.FileName = $kelpie
        $startInfo.Arguments = "pane report-prompt-submitted `"$($env:KELPIE_PANE_ID)`" --source kelpie:claude --agent claude"
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardInput = $true
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true
        $process = [System.Diagnostics.Process]::Start($startInfo)
        $process.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
        $process.StandardInput.Close()
        # Output is a few bytes at most; never block on it. kelpie bounds its own server
        # exchange to 2 s; a report still running after 5 s (Claude's hook limit is 10 s)
        # is killed.
        if (-not $process.WaitForExit(5000)) { $process.Kill() }
    } catch {
    }
    exit 0
}
if ($Action -ne "session") { exit 0 }
if ($env:KELPIE_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:KELPIE_PANE_ID)) { exit 0 }

# Read hook input as raw bytes so UTF-8 payloads decode correctly even when the
# console input code page is not UTF-8 (for example CP949 on Korean Windows).
$stdinReader = New-Object System.IO.StreamReader([Console]::OpenStandardInput(), [System.Text.Encoding]::UTF8)
$inputText = $stdinReader.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    exit 0
}

$propertyNames = @($payload.PSObject.Properties.Name)
if ((Test-Path Env:CURSOR_VERSION) -or $propertyNames -ccontains "cursor_version") { exit 0 }
if (-not ($propertyNames -ccontains "hook_event_name") -or $payload.hook_event_name -isnot [string] -or $payload.hook_event_name -cne "SessionStart") { exit 0 }
if (-not [string]::IsNullOrWhiteSpace($payload.agent_id)) { exit 0 }

$sessionId = $payload.session_id
if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$kelpie = if ([string]::IsNullOrWhiteSpace($env:KELPIE_BIN_PATH)) { "kelpie" } else { $env:KELPIE_BIN_PATH }
try {
    $args = @(
        "pane",
        "report-agent-session",
        $env:KELPIE_PANE_ID,
        "--source",
        "kelpie:claude",
        "--agent",
        "claude",
        "--seq",
        "$seq",
        "--agent-session-id",
        "$sessionId"
    )
    if ($payload.transcript_path -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.transcript_path)) {
        $args += @("--agent-session-path", "$($payload.transcript_path)")
    }
    if ($payload.hook_event_name -eq "SessionStart" -and $payload.source -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.source)) {
        $args += @("--session-start-source", "$($payload.source)")
    }
    & $kelpie @args 2>$null | Out-Null
} catch {
}
