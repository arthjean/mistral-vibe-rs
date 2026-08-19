param(
    [string]$Version = $(if ($env:VIBE_VERSION) { $env:VIBE_VERSION } else { "2.23.1" }),
    [string]$InstallDirectory = $(if ($env:VIBE_INSTALL_DIR) { $env:VIBE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "MistralVibeRS\bin" }),
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$completionDirectory = if ($env:VIBE_COMPLETION_DIR) { $env:VIBE_COMPLETION_DIR } else { Join-Path $env:LOCALAPPDATA "MistralVibeRS\completions" }

if ($Uninstall) {
    @("vibe.exe", "vibe-acp.exe", "vibe.exe.previous", "vibe-acp.exe.previous") |
        ForEach-Object { Remove-Item -LiteralPath (Join-Path $InstallDirectory $_) -Force -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $completionDirectory -Recurse -Force -ErrorAction SilentlyContinue
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
        $source = Join-Path $extracted "bin\$executable"
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
        $source = Join-Path $extracted "completions\$completion"
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
} finally {
    Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
