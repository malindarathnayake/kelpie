# Installs Kelpie for the current user. No administrator rights needed.
#   powershell -ExecutionPolicy Bypass -File .\install.ps1
#   powershell -ExecutionPolicy Bypass -File .\install.ps1 -Integrations claude,codex,cursor
param(
    [string[]]$Integrations = @()
)
$ErrorActionPreference = 'Stop'
# `powershell -File` passes "claude,codex,cursor" as one string; split it.
$Integrations = @($Integrations | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })

$source = Join-Path $PSScriptRoot 'kelpie.exe'
if (-not (Test-Path $source)) { throw "kelpie.exe not found next to this script." }

$dest = Join-Path $env:LOCALAPPDATA 'Programs\kelpie'
$exe = Join-Path $dest 'kelpie.exe'

if (Get-Process -Name kelpie -ErrorAction SilentlyContinue) {
    throw "Kelpie is running. Run 'kelpie server stop', wait until no kelpie.exe is left, then run this again."
}

New-Item -ItemType Directory -Force $dest | Out-Null
# Downloaded files carry a "from the internet" mark that makes Windows warn on every start.
Get-ChildItem $PSScriptRoot -Recurse -File | Unblock-File
Copy-Item $source $exe -Force
# The bundled ConPTY (Microsoft-signed conpty.dll and OpenConsole.exe) and its notices.
foreach ($folder in 'conpty', 'THIRD-PARTY-NOTICES') {
    $from = Join-Path $PSScriptRoot $folder
    if (Test-Path $from) {
        $to = Join-Path $dest $folder
        if (Test-Path $to) { Remove-Item $to -Recurse -Force }
        Copy-Item $from $to -Recurse -Force
    }
}
Write-Host "Installed $exe"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $dest) {
    [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ';' + $dest), 'User')
    Write-Host "Added $dest to your user PATH (open a new terminal to use it)."
}

foreach ($agent in $Integrations) {
    & $exe integration install $agent
}

& $exe --version
Write-Host ""
Write-Host "Next: open a NEW terminal and run:  kelpie"
Write-Host "Agent hooks (optional, per agent):  kelpie integration install claude"
