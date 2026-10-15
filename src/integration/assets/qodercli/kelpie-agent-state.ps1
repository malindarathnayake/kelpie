# installed by kelpie
# managed by kelpie; reinstalling or updating the integration overwrites this file.
# add custom hooks beside this file instead of editing it.
# KELPIE_INTEGRATION_ID=qodercli
# KELPIE_INTEGRATION_VERSION=3

param([string]$Action = "")

if ($Action -ne "session") { exit 0 }
if ($env:KELPIE_ENV -ne "1") { exit 0 }
if ([string]::IsNullOrWhiteSpace($env:KELPIE_PANE_ID)) { exit 0 }

$inputText = [Console]::In.ReadToEnd()
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    $payload = $null
}

if ($null -eq $payload -or [string]::IsNullOrWhiteSpace($payload.session_id)) { exit 0 }

$seq = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$kelpie = if ([string]::IsNullOrWhiteSpace($env:KELPIE_BIN_PATH)) { "kelpie" } else { $env:KELPIE_BIN_PATH }
try {
    & $kelpie pane report-agent-session $env:KELPIE_PANE_ID --source kelpie:qodercli --agent qodercli --agent-session-id $payload.session_id --seq $seq 2>$null | Out-Null
} catch {
}
