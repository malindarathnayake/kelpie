# installed by kelpie
# managed by kelpie; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# KELPIE_INTEGRATION_ID=codex
# KELPIE_INTEGRATION_VERSION=10

param([string]$Action = "")

if ($Action -eq "prompt") {
    # Forward raw hook JSON bytes without console decoding or newline changes.
    if ($env:KELPIE_ENV -ne "1") { exit 0 }
    if ([string]::IsNullOrWhiteSpace($env:KELPIE_PANE_ID)) { exit 0 }
    $buffer = $null
    $process = $null
    try {
        $buffer = New-Object System.IO.MemoryStream
        [Console]::OpenStandardInput().CopyTo($buffer)
        $buffer.Position = 0
        $kelpie = if ([string]::IsNullOrWhiteSpace($env:KELPIE_BIN_PATH)) { "kelpie" } else { $env:KELPIE_BIN_PATH }
        $startInfo = New-Object System.Diagnostics.ProcessStartInfo
        $startInfo.FileName = $kelpie
        $startInfo.Arguments = "pane report-prompt-submitted `"$($env:KELPIE_PANE_ID)`" --source kelpie:codex --agent codex"
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
if ($Action -ne "session") { exit 0 }
if ($env:KELPIE_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:KELPIE_PANE_ID)) { exit 0 }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    exit 0
}

if ($payload.hook_event_name -and $payload.hook_event_name -ne "SessionStart") { exit 0 }

$sessionId = $payload.session_id
if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }
if ([string]::IsNullOrWhiteSpace($payload.transcript_path)) { exit 0 }
if (-not [string]::IsNullOrWhiteSpace($env:CODEX_THREAD_ID) -and $env:CODEX_THREAD_ID -ne $sessionId) { exit 0 }

$seq = [DateTimeOffset]::UtcNow.Ticks
$kelpie = if ([string]::IsNullOrWhiteSpace($env:KELPIE_BIN_PATH)) { "kelpie" } else { $env:KELPIE_BIN_PATH }
try {
    $args = @(
        "pane",
        "report-agent-session",
        $env:KELPIE_PANE_ID,
        "--source",
        "kelpie:codex",
        "--agent",
        "codex",
        "--seq",
        "$seq",
        "--agent-session-id",
        "$sessionId"
    )
    if ($payload.hook_event_name -eq "SessionStart" -and $payload.source -is [string] -and -not [string]::IsNullOrWhiteSpace($payload.source)) {
        $args += @("--session-start-source", "$($payload.source)")
    }
    & $kelpie @args 2>$null | Out-Null
} catch {
}
