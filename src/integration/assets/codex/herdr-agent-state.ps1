# installed by herdr
# managed by herdr; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# HERDR_INTEGRATION_ID=codex
# HERDR_INTEGRATION_VERSION=9

param([string]$Action = "")

if ($Action -eq "prompt") {
    # Forward raw hook JSON bytes without console decoding or newline changes.
    if ($env:HERDR_ENV -ne "1") { exit 0 }
    if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }
    $buffer = $null
    $process = $null
    try {
        $buffer = New-Object System.IO.MemoryStream
        [Console]::OpenStandardInput().CopyTo($buffer)
        $buffer.Position = 0
        $herdr = if ([string]::IsNullOrWhiteSpace($env:HERDR_BIN_PATH)) { "herdr" } else { $env:HERDR_BIN_PATH }
        $startInfo = New-Object System.Diagnostics.ProcessStartInfo
        $startInfo.FileName = $herdr
        $startInfo.Arguments = "pane report-prompt-submitted `"$($env:HERDR_PANE_ID)`" --source herdr:codex --agent codex"
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardInput = $true
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true
        # One deadline includes forwarding: a child that never reads stdin must
        # not hold this hook open before the exit wait starts.
        $deadline = [System.Diagnostics.Stopwatch]::StartNew()
        $process = [System.Diagnostics.Process]::Start($startInfo)
        $forwarding = $buffer.CopyToAsync($process.StandardInput.BaseStream)
        $remaining = [Math]::Max(0, 5000 - [int]$deadline.ElapsedMilliseconds)
        if ($forwarding.Wait($remaining)) {
            $process.StandardInput.Close()
            $remaining = [Math]::Max(0, 5000 - [int]$deadline.ElapsedMilliseconds)
            [void]$process.WaitForExit($remaining)
        }
    } catch {
    } finally {
        if ($null -ne $process) {
            try { if (-not $process.HasExited) { $process.Kill() } } catch {}
            try { $process.StandardInput.Close() } catch {}
            $process.Dispose()
        }
        if ($null -ne $buffer) { $buffer.Dispose() }
    }
    exit 0
}
if ($Action -notin @("session", "working", "idle")) { exit 0 }
if ($env:HERDR_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:HERDR_PANE_ID)) { exit 0 }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    exit 0
}

$expectedEvents = @{ session = @("SessionStart"); working = @("UserPromptSubmit"); idle = @("Stop", "Interrupt") }[$Action]
if ($payload.hook_event_name -and $payload.hook_event_name -notin $expectedEvents) { exit 0 }

$sessionId = $payload.session_id
if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }
if ($Action -eq "session" -and [string]::IsNullOrWhiteSpace($payload.transcript_path)) { exit 0 }
if (-not [string]::IsNullOrWhiteSpace($env:CODEX_THREAD_ID) -and $env:CODEX_THREAD_ID -ne $sessionId) { exit 0 }

$seq = [DateTimeOffset]::UtcNow.Ticks
$herdr = if ([string]::IsNullOrWhiteSpace($env:HERDR_BIN_PATH)) { "herdr" } else { $env:HERDR_BIN_PATH }
try {
    $args = @(
        "pane",
        $(if ($Action -eq "session") { "report-agent-session" } else { "report-agent" }),
        $env:HERDR_PANE_ID,
        "--source",
        "herdr:codex",
        "--agent",
        "codex",
        "--seq",
        "$seq",
        "--agent-session-id",
        "$sessionId"
    )
    if ($Action -ne "session") { $args += @("--state", $Action) }
    if ($payload.hook_event_name -eq "SessionStart" -and $payload.source -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.source)) {
        $args += @("--session-start-source", "$($payload.source)")
    }
    & $herdr @args 2>$null | Out-Null
} catch {
}
