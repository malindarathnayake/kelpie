# managed by kelpie; reinstalling the integration replaces this file.
# KELPIE_INTEGRATION_ID=cursor
# KELPIE_INTEGRATION_VERSION=2

param([string]$Action = "")

# Private child action: the prompt parent can terminate decoding and parsing.
# Raw input is confined to stdin; the only possible output is a validated ID.
if ($Action -ceq "__prompt-metadata") {
    $ErrorActionPreference = "Stop"
    $metadataBuffer = $null
    try {
        # Resolve the system module directly to avoid autoload discovery latency
        # consuming the shared deadline in an isolated child environment.
        Import-Module ($PSHOME + '\Modules\Microsoft.PowerShell.Utility\Microsoft.PowerShell.Utility.psd1')
        $metadataBuffer = New-Object System.IO.MemoryStream
        [Console]::OpenStandardInput().CopyTo($metadataBuffer)
        $utf8 = New-Object System.Text.UTF8Encoding($false, $true)
        $metadata = $utf8.GetString($metadataBuffer.ToArray())
        if ($metadata.Length -gt 0 -and $metadata[0] -eq [char]0xFEFF) {
            $metadata = $metadata.Substring(1)
        }
        # ConvertFrom-Json can unwrap arrays, so check the JSON root too.
        if ($metadata -cmatch '\A[ \t\r\n]*\{') {
            $payload = ConvertFrom-Json -InputObject $metadata -ErrorAction Stop
            if ($payload -is [System.Management.Automation.PSCustomObject]) {
                $eventProperty = $payload.PSObject.Properties['hook_event_name']
                $idProperty = $payload.PSObject.Properties['conversation_id']
                if ($null -ne $eventProperty -and $eventProperty.Name -ceq 'hook_event_name' -and
                    $eventProperty.Value -is [string] -and $eventProperty.Value -ceq 'beforeSubmitPrompt' -and
                    $null -ne $idProperty -and $idProperty.Name -ceq 'conversation_id' -and
                    $idProperty.Value -is [string] -and
                    $idProperty.Value -cmatch '\A[A-Za-z0-9_][A-Za-z0-9_-]{0,255}\z') {
                    [Console]::Out.Write($idProperty.Value)
                }
            }
        }
    } catch {
    } finally {
        if ($null -ne $metadataBuffer) { try { $metadataBuffer.Dispose() } catch {} }
    }
    exit 0
}

if ($Action -eq "prompt") {
    # This path owns its single allow response; reporting can never deny a prompt.
    $ErrorActionPreference = "Stop"
    $buffer = $null
    try {
        if ($env:KELPIE_ENV -eq "1" -and -not [string]::IsNullOrWhiteSpace($env:KELPIE_PANE_ID)) {
            $buffer = New-Object System.IO.MemoryStream
            [Console]::OpenStandardInput().CopyTo($buffer)
            $kelpie = if ([string]::IsNullOrWhiteSpace($env:KELPIE_BIN_PATH)) { "kelpie" } else { $env:KELPIE_BIN_PATH }
            $deadline = [System.Diagnostics.Stopwatch]::StartNew()

            function Quote-NativeArgument([string]$Value) {
                $escaped = [regex]::Replace($Value, '(\\*)"', '${1}${1}\"')
                $escaped = [regex]::Replace($escaped, '(\\+)\z', '${1}${1}')
                return '"' + $escaped + '"'
            }

            function Invoke-BoundedReport([string]$Arguments, [System.IO.MemoryStream]$InputBuffer) {
                $process = $null
                try {
                    if ($deadline.ElapsedMilliseconds -ge 5000) { return }
                    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
                    $startInfo.FileName = $kelpie
                    $startInfo.Arguments = $Arguments
                    $startInfo.UseShellExecute = $false
                    $startInfo.CreateNoWindow = $true
                    $startInfo.RedirectStandardInput = $true
                    $startInfo.RedirectStandardOutput = $true
                    $startInfo.RedirectStandardError = $true
                    # Both reports and metadata share one five-second deadline,
                    # including a reporter that never reads its stdin.
                    $process = [System.Diagnostics.Process]::Start($startInfo)
                    if ($null -ne $InputBuffer) {
                        $InputBuffer.Position = 0
                        $forwarding = $InputBuffer.CopyToAsync($process.StandardInput.BaseStream)
                        $remaining = [Math]::Max(0, 5000 - [int]$deadline.ElapsedMilliseconds)
                        if (-not $forwarding.Wait($remaining)) { return }
                    }
                    $process.StandardInput.Close()
                    $remaining = [Math]::Max(0, 5000 - [int]$deadline.ElapsedMilliseconds)
                    [void]$process.WaitForExit($remaining)
                } catch {
                } finally {
                    if ($null -ne $process) {
                        try { if (-not $process.HasExited) { $process.Kill() } } catch {}
                        try { $process.StandardInput.Close() } catch {}
                        try { $process.Dispose() } catch {}
                    }
                }
            }

            $paneArgument = Quote-NativeArgument $env:KELPIE_PANE_ID
            Invoke-BoundedReport "pane report-prompt-submitted $paneArgument --source kelpie:cursor --agent cursor" $buffer

            # Decode in a disposable child, so synchronous UTF-8/JSON work can
            # be interrupted. Reserve the final 100ms for terminating/reaping it.
            function Get-BoundedPromptSessionId([System.IO.MemoryStream]$InputBuffer) {
                $process = $null
                try {
                    if ($deadline.ElapsedMilliseconds -ge 4900) { return }
                    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
                    $startInfo.FileName = [System.Diagnostics.Process]::GetCurrentProcess().MainModule.FileName
                    $scriptArgument = Quote-NativeArgument $PSCommandPath
                    $startInfo.Arguments = "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $scriptArgument __prompt-metadata"
                    $startInfo.UseShellExecute = $false
                    $startInfo.CreateNoWindow = $true
                    $startInfo.RedirectStandardInput = $true
                    $startInfo.RedirectStandardOutput = $true
                    $startInfo.RedirectStandardError = $true
                    $process = [System.Diagnostics.Process]::Start($startInfo)
                    $output = $process.StandardOutput.ReadToEndAsync()
                    # Drain but never expose child diagnostics or raw metadata.
                    $discardedErrors = $process.StandardError.ReadToEndAsync()
                    $InputBuffer.Position = 0
                    $forwarding = $InputBuffer.CopyToAsync($process.StandardInput.BaseStream)
                    $remaining = [Math]::Max(0, 4900 - [int]$deadline.ElapsedMilliseconds)
                    if (-not $forwarding.Wait($remaining)) { return }
                    $process.StandardInput.Close()
                    $remaining = [Math]::Max(0, 4900 - [int]$deadline.ElapsedMilliseconds)
                    if (-not $process.WaitForExit($remaining)) { return }
                    $remaining = [Math]::Max(0, 4900 - [int]$deadline.ElapsedMilliseconds)
                    if (-not $output.Wait($remaining)) { return }
                    if ($deadline.ElapsedMilliseconds -ge 5000) { return }
                    $candidate = $output.Result
                    if ($process.ExitCode -eq 0 -and
                        $candidate -cmatch '\A[A-Za-z0-9_][A-Za-z0-9_-]{0,255}\z') {
                        return $candidate
                    }
                } catch {
                } finally {
                    if ($null -ne $process) {
                        try { if (-not $process.HasExited) { $process.Kill() } } catch {}
                        $remaining = [Math]::Max(0, 5000 - [int]$deadline.ElapsedMilliseconds)
                        try { [void]$process.WaitForExit($remaining) } catch {}
                        try { $process.StandardInput.Close() } catch {}
                        try { $process.StandardOutput.Close() } catch {}
                        try { $process.StandardError.Close() } catch {}
                        try { $process.Dispose() } catch {}
                    }
                }
            }

            $sessionId = Get-BoundedPromptSessionId $buffer
            if ($null -ne $sessionId) {
                $seq = [DateTime]::UtcNow.Ticks
                Invoke-BoundedReport "pane report-agent-session $paneArgument --source kelpie:cursor --agent cursor --seq $seq --agent-session-id $sessionId --session-start-source prompt" $null
            }
        }
    } catch {
    } finally {
        if ($null -ne $buffer) { try { $buffer.Dispose() } catch {} }
        Write-Output '{"continue":true}'
    }
    exit 0
}

function Exit-Hook {
    Write-Output "{}"
    exit 0
}

if ($Action -ne "session") { Exit-Hook }
if ($env:KELPIE_ENV -ne "1") { Exit-Hook }
if ([string]::IsNullOrWhiteSpace($env:KELPIE_PANE_ID)) { Exit-Hook }

$inputText = [Console]::In.ReadToEnd()
$jsonStart = $inputText.IndexOf("{")
if ($jsonStart -gt 0) {
    $inputText = $inputText.Substring($jsonStart)
}
try {
    $payload = if ([string]::IsNullOrWhiteSpace($inputText)) { $null } else { $inputText | ConvertFrom-Json }
} catch {
    Exit-Hook
}

if ($null -eq $payload) { Exit-Hook }
$event = if ($payload.hook_event_name -is [string]) { $payload.hook_event_name } else { $payload.hookEventName }
if (-not [string]::IsNullOrWhiteSpace($event) -and $event -ne "sessionStart") { Exit-Hook }

$sessionId = $null
foreach ($name in @("session_id", "sessionId", "conversation_id", "conversationId")) {
    $value = $payload.$name
    if ($value -is [string] -and -not [string]::IsNullOrWhiteSpace($value)) {
        $sessionId = $value
        break
    }
}
if ([string]::IsNullOrWhiteSpace($sessionId)) { Exit-Hook }

$seq = [DateTime]::UtcNow.Ticks
$kelpie = if ([string]::IsNullOrWhiteSpace($env:KELPIE_BIN_PATH)) { "kelpie" } else { $env:KELPIE_BIN_PATH }
try {
    & $kelpie pane report-agent-session $env:KELPIE_PANE_ID --source kelpie:cursor --agent cursor --seq $seq --agent-session-id $sessionId 2>$null | Out-Null
} catch {
}

Exit-Hook
