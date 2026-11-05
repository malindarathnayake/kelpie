# Signs kelpie.exe for a staged draft release and hands it back to CI.
#
# release.yml stages a draft release with an unsigned kelpie-windows-x64.zip.
# This script downloads it, signs kelpie.exe with the Certum certificate
# through SimplySign Desktop, uploads the result as kelpie-signed.exe and runs
# release-publish.yml, which checks the signature, rebuilds the zip and
# publishes the release.
#
# Before running: start SimplySign Desktop and log in with the code from the
# SimplySign mobile app, so the certificate shows up in the Windows store.
#
#   pwsh -File scripts\sign_windows_release.ps1 -Tag v0.1.0

param(
    [Parameter(Mandatory = $true)]
    [string]$Tag,

    [string]$Repo = "malindarathnayake/kelpie"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# Keep in step with SIGNING_THUMBPRINT in .github/workflows/release-publish.yml.
$Thumbprint = "2447CAE7D60C185DCBBCA51A04FE3F7D2AE01C3B"
$TimestampUrl = "http://time.certum.pl"

function Invoke-NativeChecked {
    param(
        [string]$Command,
        [string[]]$Arguments
    )

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command failed with exit code $LASTEXITCODE"
    }
}

if ($Tag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') {
    throw "tag must look like v0.1.0"
}
$version = $Tag.Substring(1)

$certificate = Get-ChildItem Cert:\CurrentUser\My | Where-Object Thumbprint -eq $Thumbprint
if ($null -eq $certificate -or -not $certificate.HasPrivateKey) {
    throw "signing certificate $Thumbprint not found; start SimplySign Desktop and log in first"
}

$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue |
    Sort-Object FullName | Select-Object -Last 1 -ExpandProperty FullName
if (-not $signtool) {
    throw "signtool.exe not found; install the Windows SDK"
}

$draft = gh release view $Tag --repo $Repo --json isDraft --jq .isDraft
if ($LASTEXITCODE -ne 0) { throw "no release found for $Tag; has release.yml finished?" }
if ($draft -ne "true") { throw "release $Tag is already published" }

$work = Join-Path ([IO.Path]::GetTempPath()) "kelpie-sign-$Tag-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
New-Item -ItemType Directory $work | Out-Null
try {
    Invoke-NativeChecked gh @("release", "download", $Tag, "--repo", $Repo, "--pattern", "kelpie-windows-x64.zip", "--dir", $work)
    Expand-Archive (Join-Path $work "kelpie-windows-x64.zip") -DestinationPath (Join-Path $work "stage")
    $exe = Join-Path $work "kelpie-signed.exe"
    Copy-Item (Join-Path $work "stage\kelpie.exe") $exe

    if ((Get-AuthenticodeSignature $exe).Status -ne "NotSigned") {
        throw "kelpie.exe in the draft is already signed"
    }
    $reported = (& $exe --version | Out-String).Trim()
    if ($reported -notmatch [regex]::Escape($version)) {
        throw "draft kelpie.exe reports '$reported', expected $version"
    }

    Invoke-NativeChecked $signtool @("sign", "/sha1", $Thumbprint, "/fd", "sha256", "/tr", $TimestampUrl, "/td", "sha256", "/d", "Kelpie", $exe)
    Invoke-NativeChecked $signtool @("verify", "/pa", $exe)
    $signature = Get-AuthenticodeSignature $exe
    if ($signature.Status -ne "Valid" -or $signature.SignerCertificate.Thumbprint -ne $Thumbprint -or $null -eq $signature.TimeStamperCertificate) {
        throw "signature check failed: $($signature.Status) $($signature.StatusMessage)"
    }

    Invoke-NativeChecked gh @("release", "upload", $Tag, $exe, "--repo", $Repo, "--clobber")
    Invoke-NativeChecked gh @("workflow", "run", "release-publish.yml", "--repo", $Repo, "--ref", $Tag)
    Write-Host "Signed kelpie.exe uploaded. Follow the publish run with: gh run watch --repo $Repo"
}
finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
