#!/usr/bin/env pwsh
# Drives `scripts/install.ps1` through the four paths a Windows installation
# takes: a clean install from a local release, a refusal when a partial upgrade
# is already staged beside the target, a refusal when the manifest's digest does
# not match the archive, and a complete removal.
#
# The Windows CI job runs this against a release it just packaged. Nothing here
# is Windows-only, so the same script verifies the installer wherever PowerShell
# is available, which is what makes the assertions reviewable off a runner.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ReleaseDirectory,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$InstallDirectory,
    [Parameter(Mandatory = $true)][string]$CompletionDirectory
)

$ErrorActionPreference = "Stop"

$installer = Join-Path (Split-Path -Parent $PSScriptRoot) "install.ps1"
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
    throw "the installer under test is missing: $installer"
}

$archive = "mistral-vibe-rs-$Version-windows-x86_64.zip"
$env:VIBE_VERSION = $Version
$env:VIBE_INSTALL_DIR = $InstallDirectory
$env:VIBE_COMPLETION_DIR = $CompletionDirectory
$env:VIBE_RELEASE_BASE_URL = "file://$ReleaseDirectory"
$vibe = Join-Path $InstallDirectory "vibe.exe"
$vibeAcp = Join-Path $InstallDirectory "vibe-acp.exe"
$completions = @("vibe.bash", "_vibe", "vibe.fish", "vibe.ps1")

function Assert-That {
    param([bool]$Condition, [string]$Because)

    if (-not $Condition) {
        throw "install.ps1 verification failed: $Because"
    }
}

# Runs the installer expecting a refusal, and reports the message it refused
# with. A run that completes is itself the failure this catches.
function Assert-Refusal {
    param([string]$Pattern, [string]$Because, [string[]]$InstallerArguments = @())

    $message = $null
    try {
        & $installer @InstallerArguments | Out-Null
    } catch {
        $message = $_.Exception.Message
    }
    Assert-That ($null -ne $message) $Because
    Assert-That ($message -match $Pattern) "$Because; it reported instead: $message"
    Write-Output "  refused with: $message"
}

function Get-StagingResidue {
    Get-ChildItem -LiteralPath $InstallDirectory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name.EndsWith(".new") -or $_.Name.EndsWith(".previous") } |
        ForEach-Object { $_.Name }
}

Write-Output "== a clean install from a local release =="
& $installer
Assert-That (Test-Path -LiteralPath $vibe -PathType Leaf) "the install left no vibe.exe in $InstallDirectory"
Assert-That (Test-Path -LiteralPath $vibeAcp -PathType Leaf) "the install left no vibe-acp.exe in $InstallDirectory"
foreach ($completion in $completions) {
    $installed = Join-Path $CompletionDirectory $completion
    Assert-That (Test-Path -LiteralPath $installed -PathType Leaf) "the install left no $completion in $CompletionDirectory"
}
Assert-That (@(Get-StagingResidue).Count -eq 0) "the install left staging files behind: $(Get-StagingResidue)"
$reported = (& $vibe --version) -join " "
Assert-That ($reported -match [regex]::Escape($Version)) "vibe --version printed '$reported' rather than $Version"
& $vibeAcp --help | Out-Null
Assert-That ($LASTEXITCODE -eq 0) "vibe-acp --help exited $LASTEXITCODE"
Write-Output "  installed $reported"

Write-Output "== an interrupted upgrade is refused rather than compounded =="
$stray = "$vibe.previous"
Set-Content -LiteralPath $stray -Value "an interrupted upgrade left this behind"
Assert-Refusal -Pattern "Partial upgrade detected" `
    -Because "the installer overwrote an installation an earlier run had left half-swapped"
Remove-Item -LiteralPath $stray -Force

Write-Output "== a mismatched digest is refused before anything moves =="
$tampered = Join-Path ([System.IO.Path]::GetTempPath()) ("vibe-tampered-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tampered | Out-Null
try {
    Copy-Item -LiteralPath (Join-Path $ReleaseDirectory $archive) -Destination (Join-Path $tampered $archive)
    $manifest = Get-Content -LiteralPath (Join-Path $ReleaseDirectory "SHA256SUMS")
    ($manifest -replace "^[0-9a-fA-F]{64}", ("0" * 64)) |
        Set-Content -LiteralPath (Join-Path $tampered "SHA256SUMS")
    $installed = @{}
    foreach ($binary in @($vibe, $vibeAcp)) {
        $installed[$binary] = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash
    }
    $env:VIBE_RELEASE_BASE_URL = "file://$tampered"
    Assert-Refusal -Pattern "checksum mismatch" `
        -Because "the installer staged an archive whose digest the manifest does not name"
    foreach ($binary in @($vibe, $vibeAcp)) {
        $current = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash
        Assert-That ($current -eq $installed[$binary]) "$binary changed although the archive failed verification"
    }
    Assert-That (@(Get-StagingResidue).Count -eq 0) "the refused install staged files into ${InstallDirectory}: $(Get-StagingResidue)"
} finally {
    Remove-Item -LiteralPath $tampered -Recurse -Force -ErrorAction SilentlyContinue
    $env:VIBE_RELEASE_BASE_URL = "file://$ReleaseDirectory"
}

Write-Output "== -Uninstall removes everything the install placed =="
& $installer -Uninstall
Assert-That (-not (Test-Path -LiteralPath $vibe)) "vibe.exe survived -Uninstall"
Assert-That (-not (Test-Path -LiteralPath $vibeAcp)) "vibe-acp.exe survived -Uninstall"
foreach ($completion in $completions) {
    $installed = Join-Path $CompletionDirectory $completion
    Assert-That (-not (Test-Path -LiteralPath $installed)) "$completion survived -Uninstall"
}

Write-Output "install.ps1 verified: install, interrupted-upgrade refusal, digest refusal, removal"
