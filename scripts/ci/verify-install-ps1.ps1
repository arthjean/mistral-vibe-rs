#!/usr/bin/env pwsh
# Drives `scripts/install.ps1` through the paths a Windows installation takes:
# a clean install from a local release that a new shell then finds on the user
# PATH, a reinstall that leaves that PATH entry single, a refusal when a partial
# upgrade is already staged beside the target, a refusal when the manifest's
# digest does not match the archive, a user PATH that cannot be written, and a
# complete removal.
#
# The Windows CI job runs this against a release it just packaged. The user
# PATH lives in scratch registry keys under HKEY_CURRENT_USER, which
# `VIBE_USER_ENVIRONMENT_KEY` hands the installer, so a run on a workstation
# never edits the PATH its own shells start with.

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
$shell = (Get-Process -Id $PID).Path
$pathEntry = [System.IO.Path]::GetFullPath($InstallDirectory).TrimEnd("\")
$scratch = "Software\MistralVibeRS-verify-" + [guid]::NewGuid().ToString("N")
$userEnvironment = "$scratch\Environment"
$lockedEnvironment = "$scratch\Locked"
# The PATH a user already has: one entry kept as a `%VAR%` reference, which a
# rewrite must neither expand nor drop.
$existingEntries = @("%USERPROFILE%\vibe-verify-tools", "C:\vibe-verify-existing")
$env:VIBE_USER_ENVIRONMENT_KEY = $userEnvironment

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

function Set-ScratchPath([string]$Key, [string[]]$Entries) {
    $handle = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($Key)
    try {
        $handle.SetValue("Path", ($Entries -join ";"), [Microsoft.Win32.RegistryValueKind]::ExpandString)
    } finally {
        $handle.Close()
    }
}

# The user PATH as the registry holds it, and the kind of value it is stored as.
function Get-ScratchPath([string]$Key) {
    $handle = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($Key)
    try {
        $raw = [string]$handle.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        @{ Entries = @($raw -split ";" | Where-Object { $_ }); Kind = $handle.GetValueKind("Path") }
    } finally {
        $handle.Close()
    }
}

function Get-InstallEntryCount([string]$Key) {
    @((Get-ScratchPath $Key).Entries | Where-Object {
        [Environment]::ExpandEnvironmentVariables($_).TrimEnd("\") -eq $pathEntry
    }).Count
}

# What a shell Explorer starts afterward resolves `vibe` to: its PATH is the
# machine PATH followed by the expanded user PATH, not the PATH of this process.
function Invoke-FreshShell([string]$Key) {
    $user = [Environment]::ExpandEnvironmentVariables(((Get-ScratchPath $Key).Entries -join ";"))
    $machine = [Environment]::GetEnvironmentVariable("Path", "Machine")
    $saved = $env:Path
    $env:Path = "$machine;$user"
    try {
        & $shell -NoProfile -NonInteractive -Command '(Get-Command vibe -CommandType Application).Source; vibe --version'
    } finally {
        $env:Path = $saved
    }
}

try {
    Set-ScratchPath $userEnvironment $existingEntries

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
    $stored = Get-ScratchPath $userEnvironment
    Assert-That ($stored.Entries[0] -eq $pathEntry) "the user PATH does not start with ${pathEntry}: $($stored.Entries -join ';')"
    Assert-That ((($stored.Entries | Select-Object -Skip 1) -join ";") -eq ($existingEntries -join ";")) `
        "the install rewrote the entries already on the user PATH: $($stored.Entries -join ';')"
    Assert-That ($stored.Kind -eq [Microsoft.Win32.RegistryValueKind]::ExpandString) `
        "the user PATH is stored as $($stored.Kind), which no longer expands %VAR% entries"
    $fresh = @(Invoke-FreshShell $userEnvironment)
    Assert-That ($LASTEXITCODE -eq 0) "a new shell could not run vibe --version: $($fresh -join ' ')"
    Assert-That ($fresh[0] -eq $vibe) "a new shell resolved vibe to '$($fresh[0])' rather than $vibe"
    Assert-That ($fresh[1] -match [regex]::Escape($Version)) "a new shell's vibe --version printed '$($fresh[1])'"
    Write-Output "  a new shell resolves $($fresh[0])"

    Write-Output "== a reinstall keeps the user PATH entry single =="
    & $installer | Out-Null
    Assert-That ((Get-InstallEntryCount $userEnvironment) -eq 1) "a reinstall added $pathEntry to the user PATH again"
    # The same directory spelled with another case and a trailing separator.
    Set-ScratchPath $userEnvironment (@($pathEntry.ToUpperInvariant() + "\") + $existingEntries)
    & $installer | Out-Null
    Assert-That ((Get-InstallEntryCount $userEnvironment) -eq 1) `
        "a reinstall added $pathEntry beside the entry already naming it in another spelling"
    Set-ScratchPath $userEnvironment (@($pathEntry) + $existingEntries)

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

    Write-Output "== a user PATH that cannot be written is reported with the command to run =="
    Set-ScratchPath $lockedEnvironment $existingEntries
    $handle = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey(
        $lockedEnvironment,
        [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree,
        [System.Security.AccessControl.RegistryRights]::ReadKey -bor [System.Security.AccessControl.RegistryRights]::ChangePermissions)
    try {
        $acl = $handle.GetAccessControl()
        $acl.AddAccessRule((New-Object System.Security.AccessControl.RegistryAccessRule(
            [System.Security.Principal.WindowsIdentity]::GetCurrent().User,
            [System.Security.AccessControl.RegistryRights]::SetValue,
            [System.Security.AccessControl.AccessControlType]::Deny)))
        $handle.SetAccessControl($acl)
    } finally {
        $handle.Close()
    }
    $env:VIBE_USER_ENVIRONMENT_KEY = $lockedEnvironment
    try {
        # Standard error is left alone: Windows PowerShell 5.1 turns a native
        # command's redirected error stream into a terminating error here.
        $printed = (& $shell -NoProfile -NonInteractive -File $installer) -join "`n"
        $status = $LASTEXITCODE
    } finally {
        $env:VIBE_USER_ENVIRONMENT_KEY = $userEnvironment
    }
    Assert-That ($status -ne 0) "the installer exited 0 although the user PATH could not be written: $printed"
    Assert-That ($printed.Contains("[Environment]::SetEnvironmentVariable('Path', '$pathEntry;'")) `
        "the installer did not print the command that adds $pathEntry by hand: $printed"
    Assert-That ((Get-InstallEntryCount $lockedEnvironment) -eq 0) "the locked user PATH changed"
    Write-Output "  exited $status and printed the manual command"

    Write-Output "== -Uninstall removes everything the install placed =="
    & $installer -Uninstall
    Assert-That (-not (Test-Path -LiteralPath $vibe)) "vibe.exe survived -Uninstall"
    Assert-That (-not (Test-Path -LiteralPath $vibeAcp)) "vibe-acp.exe survived -Uninstall"
    foreach ($completion in $completions) {
        $installed = Join-Path $CompletionDirectory $completion
        Assert-That (-not (Test-Path -LiteralPath $installed)) "$completion survived -Uninstall"
    }
    $stored = Get-ScratchPath $userEnvironment
    Assert-That (($stored.Entries -join ";") -eq ($existingEntries -join ";")) `
        "-Uninstall left the user PATH as $($stored.Entries -join ';')"
} finally {
    # Windows PowerShell 5.1 opens every key of a tree for writing to delete it,
    # which the locked key refuses, so that key goes first on its own.
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKey($lockedEnvironment, $false)
    [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($scratch, $false)
}

Write-Output "install.ps1 verified: user PATH install, reinstall, interrupted-upgrade refusal, digest refusal, unwritable PATH, removal"
