#Requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Synck\bin')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($env:OS -ne 'Windows_NT') {
    throw 'This installer requires Windows. Use install.sh on Linux or macOS.'
}
$architecture = $env:PROCESSOR_ARCHITEW6432
if (-not $architecture) { $architecture = $env:PROCESSOR_ARCHITECTURE }
if ($architecture -ne 'AMD64') { throw 'This release supports Windows x86_64 only.' }
if ([string]::IsNullOrWhiteSpace($InstallDir)) { throw 'InstallDir must not be empty.' }
$versionPattern = '\Av(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\z'
if ($Version -ne 'latest') {
    if (-not $Version.StartsWith('v')) { $Version = "v$Version" }
    if ($Version -cnotmatch $versionPattern) { throw 'Version must be X.Y.Z, vX.Y.Z, or latest.' }
}

[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
if ($Version -eq 'latest') {
    $release = Invoke-RestMethod -Uri 'https://api.github.com/repos/synergyos/synck/releases/latest'
    $Version = $release.tag_name
    if ($Version -cnotmatch $versionPattern) { throw 'Latest release does not have a supported vX.Y.Z tag.' }
}
$asset = "synck-$($Version.Substring(1))-x86_64-pc-windows-msvc.exe"
$url = "https://github.com/synergyos/synck/releases/download/$Version"
$temporary = Join-Path ([IO.Path]::GetTempPath()) ("synck-" + [guid]::NewGuid().ToString('N'))
$staged = $null
try {
    New-Item -ItemType Directory -Path $temporary | Out-Null
    $manifest = Join-Path $temporary 'SHA256SUMS.txt'
    $binary = Join-Path $temporary $asset
    Invoke-WebRequest -UseBasicParsing -Uri "$url/SHA256SUMS.txt" -OutFile $manifest
    Invoke-WebRequest -UseBasicParsing -Uri "$url/$asset" -OutFile $binary
    $lines = @(Get-Content -LiteralPath $manifest | Where-Object {
        $_ -cmatch ('\s' + [regex]::Escape($asset) + '$')
    })
    $pattern = '^([a-fA-F0-9]{64})[ \t]+' + [regex]::Escape($asset) + '$'
    if ($lines.Count -ne 1 -or $lines[0] -cnotmatch $pattern) {
        throw 'Missing, duplicate or malformed checksum.'
    }
    $expected = $Matches[1]
    if ((Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash -ne $expected) {
        throw 'Checksum verification failed; installation unchanged.'
    }

    if ((Get-Item -LiteralPath $binary).Length -eq 0) { throw 'Downloaded executable is empty.' }

    $InstallDir = [IO.Path]::GetFullPath($InstallDir)
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $staged = Join-Path $InstallDir ('.synck-' + [guid]::NewGuid().ToString('N'))
    Copy-Item -LiteralPath $binary -Destination $staged
    $installed = Join-Path $InstallDir 'synck.exe'
    if ([IO.File]::Exists($installed)) {
        [IO.File]::Replace($staged, $installed, [NullString]::Value)
    } else {
        [IO.File]::Move($staged, $installed)
    }
    $staged = $null
    Write-Host "Installed Synck $Version to $installed"
    Write-Host "Ensure $InstallDir is on PATH, then run: synck --version"
} finally {
    if ($staged -and (Test-Path -LiteralPath $staged)) { Remove-Item -LiteralPath $staged -Force }
    if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Recurse -Force }
}
