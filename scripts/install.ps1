param(
    [string]$Version = $(if ($env:VIBE_VERSION) { $env:VIBE_VERSION } else { "2.24.0" }),
    [string]$InstallDirectory = $(if ($env:VIBE_INSTALL_DIR) { $env:VIBE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "MistralVibeRS\bin" }),
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$completionDirectory = if ($env:VIBE_COMPLETION_DIR) { $env:VIBE_COMPLETION_DIR } else { Join-Path $env:LOCALAPPDATA "MistralVibeRS\completions" }
# The registry key, under HKEY_CURRENT_USER, that holds the user PATH a new
# shell starts with. `scripts/ci/verify-install-ps1.ps1` points it at a scratch
# key so a verification run never edits the PATH of the machine it runs on.
$userEnvironmentKey = if ($env:VIBE_USER_ENVIRONMENT_KEY) { $env:VIBE_USER_ENVIRONMENT_KEY } else { "Environment" }
$pathEntry = [System.IO.Path]::GetFullPath($InstallDirectory).TrimEnd("\")

# The user PATH entries as stored, `%VAR%` references unexpanded, so a rewrite
# keeps them expandable.
function Get-UserPathEntries {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($userEnvironmentKey)
    if ($null -eq $key) {
        return @()
    }
    try {
        $value = [string]$key.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    } finally {
        $key.Close()
    }
    @($value -split ";" | Where-Object { $_ })
}

function Test-InstallPathEntry([string]$Entry) {
    [Environment]::ExpandEnvironmentVariables($Entry).TrimEnd("\") -eq $pathEntry
}

function Set-UserPathEntries([string[]]$Entries) {
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($userEnvironmentKey)
    try {
        $key.SetValue("Path", ($Entries -join ";"), [Microsoft.Win32.RegistryValueKind]::ExpandString)
    } finally {
        $key.Close()
    }
    # A user variable set through .NET broadcasts WM_SETTINGCHANGE, which is
    # what makes Explorer, and every shell it starts afterward, read PATH again.
    # Removing a variable nothing sets sends that broadcast and changes nothing.
    [Environment]::SetEnvironmentVariable("MISTRAL_VIBE_RS_PATH_REFRESH", $null, "User")
}

if ($Uninstall) {
    @("vibe.exe", "vibe-acp.exe", "vibe.exe.previous", "vibe-acp.exe.previous") |
        ForEach-Object { Remove-Item -LiteralPath (Join-Path $InstallDirectory $_) -Force -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $completionDirectory -Recurse -Force -ErrorAction SilentlyContinue
    $entries = @(Get-UserPathEntries)
    $kept = @($entries | Where-Object { -not (Test-InstallPathEntry $_) })
    if ($kept.Count -ne $entries.Count) {
        Set-UserPathEntries $kept
    }
    Write-Output "Mistral Vibe RS removed from $InstallDirectory"
    exit 0
}

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "Only Windows x86_64 is supported"
}

$baseUrl = if ($env:VIBE_RELEASE_BASE_URL) { $env:VIBE_RELEASE_BASE_URL } else { "https://github.com/arthjean/mistral-vibe-rs/releases/download/v$Version" }
if (-not ($baseUrl.StartsWith("https://") -or $baseUrl.StartsWith("file://"))) {
    throw "Refusing non-HTTPS release source"
}

$archive = "mistral-vibe-rs-$Version-windows-x86_64.zip"
$temporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("vibe-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null
try {
    $archivePath = Join-Path $temporaryDirectory $archive
    $checksumPath = Join-Path $temporaryDirectory "SHA256SUMS"
    if ($baseUrl.StartsWith("file://")) {
        $source = $baseUrl.Substring(7)
        Copy-Item -LiteralPath (Join-Path $source $archive) -Destination $archivePath
        Copy-Item -LiteralPath (Join-Path $source "SHA256SUMS") -Destination $checksumPath
    } else {
        Invoke-WebRequest -Uri "$baseUrl/$archive" -OutFile $archivePath
        Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS" -OutFile $checksumPath
    }
    $checksumLine = Get-Content -LiteralPath $checksumPath | Where-Object { $_ -match "\s\*?$([regex]::Escape($archive))$" } | Select-Object -First 1
    if (-not $checksumLine) {
        throw "Release checksum does not contain $archive"
    }
    $expected = ($checksumLine -split "\s+")[0].ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "Release checksum mismatch; the installed binary is unchanged"
    }
    $extracted = Join-Path $temporaryDirectory "extracted"
    Expand-Archive -LiteralPath $archivePath -DestinationPath $extracted
    New-Item -ItemType Directory -Force -Path $InstallDirectory, $completionDirectory | Out-Null
    $transaction = @()
    foreach ($executable in @("vibe.exe", "vibe-acp.exe")) {
        $source = Join-Path (Join-Path $extracted "bin") $executable
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Release archive is missing $executable"
        }
        $destination = Join-Path $InstallDirectory $executable
        $staged = "$destination.new"
        $backup = "$destination.previous"
        if ((Test-Path -LiteralPath $backup) -or (Test-Path -LiteralPath $staged)) {
            throw "Partial upgrade detected beside $destination; restore it before retrying"
        }
        Copy-Item -LiteralPath $source -Destination $staged
        $transaction += @{
            Destination = $destination
            Staged = $staged
            Backup = $backup
            HadExisting = Test-Path -LiteralPath $destination
        }
    }
    foreach ($completion in @("vibe.bash", "_vibe", "vibe.fish", "vibe.ps1")) {
        $source = Join-Path (Join-Path $extracted "completions") $completion
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "Release archive is missing completion $completion"
        }
        $destination = Join-Path $completionDirectory $completion
        $staged = "$destination.new"
        $backup = "$destination.previous"
        if ((Test-Path -LiteralPath $backup) -or (Test-Path -LiteralPath $staged)) {
            throw "Partial upgrade detected beside $destination; restore it before retrying"
        }
        Copy-Item -LiteralPath $source -Destination $staged
        $transaction += @{
            Destination = $destination
            Staged = $staged
            Backup = $backup
            HadExisting = Test-Path -LiteralPath $destination
        }
    }
    try {
        foreach ($entry in $transaction) {
            if ($entry.HadExisting) {
                Move-Item -LiteralPath $entry.Destination -Destination $entry.Backup
            }
        }
        foreach ($entry in $transaction) {
            Move-Item -LiteralPath $entry.Staged -Destination $entry.Destination
        }
        foreach ($entry in $transaction) {
            Remove-Item -LiteralPath $entry.Backup -Force -ErrorAction SilentlyContinue
        }
    } catch {
        foreach ($entry in $transaction) {
            if (Test-Path -LiteralPath $entry.Backup) {
                Remove-Item -LiteralPath $entry.Destination -Force -ErrorAction SilentlyContinue
                Move-Item -LiteralPath $entry.Backup -Destination $entry.Destination
            } elseif (-not $entry.HadExisting) {
                Remove-Item -LiteralPath $entry.Destination -Force -ErrorAction SilentlyContinue
            }
            Remove-Item -LiteralPath $entry.Staged -Force -ErrorAction SilentlyContinue
        }
        throw "Upgrade activation failed; the previous installation was restored"
    }
    & (Join-Path $InstallDirectory "vibe.exe") --version
    & (Join-Path $InstallDirectory "vibe-acp.exe") --help | Out-Null
    Write-Output "Mistral Vibe RS $Version installed in $InstallDirectory"
    try {
        $entries = @(Get-UserPathEntries)
        if (-not ($entries | Where-Object { Test-InstallPathEntry $_ })) {
            Set-UserPathEntries (@($pathEntry) + $entries)
            Write-Output "Added $pathEntry to the user PATH; open a new terminal to use vibe"
        }
    } catch {
        $quoted = $pathEntry.Replace("'", "''")
        Write-Output "Could not add $pathEntry to the user PATH: $($_.Exception.Message)"
        Write-Output "Add it yourself, then open a new terminal:"
        Write-Output "  [Environment]::SetEnvironmentVariable('Path', '$quoted;' + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
        throw "The user PATH could not be updated; vibe is installed in $pathEntry"
    }
} finally {
    Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
