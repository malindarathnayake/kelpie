# installed by kelpie
# managed by kelpie; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# KELPIE_INTEGRATION_ID=kimi
# KELPIE_INTEGRATION_VERSION=7

param([string]$Action = "")

if (@("session", "working", "blocked", "idle") -notcontains $Action) { exit 0 }
if ($env:KELPIE_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:KELPIE_PANE_ID)) { exit 0 }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    $payload = $null
}

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$sessionId = if ($null -ne $payload -and -not [string]::IsNullOrWhiteSpace($payload.session_id)) { $payload.session_id } else { $null }
$kelpie = if ([string]::IsNullOrWhiteSpace($env:KELPIE_BIN_PATH)) { "kelpie" } else { $env:KELPIE_BIN_PATH }

try {
    if ($Action -eq "session") {
        if ([string]::IsNullOrWhiteSpace($sessionId)) { exit 0 }
        & $kelpie pane report-agent-session $env:KELPIE_PANE_ID --source kelpie:kimi --agent kimi --agent-session-id $sessionId --session-start-source startup --seq $seq 2>$null | Out-Null
    } else {
        if ([string]::IsNullOrWhiteSpace($sessionId)) {
            & $kelpie pane report-agent $env:KELPIE_PANE_ID --source kelpie:kimi --agent kimi --state $Action --seq $seq 2>$null | Out-Null
        } else {
            & $kelpie pane report-agent $env:KELPIE_PANE_ID --source kelpie:kimi --agent kimi --state $Action --agent-session-id $sessionId --seq $seq 2>$null | Out-Null
        }
    }
} catch {
}
