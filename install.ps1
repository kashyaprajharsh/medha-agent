# MEDHA installer — Windows (PowerShell).
#
#   irm https://raw.githubusercontent.com/kashyaprajharsh/medha-agent/main/install.ps1 | iex
#
# Downloads the release build for this machine and puts medha.exe on your PATH.
# Override with $env:MEDHA_INSTALL_DIR, or pin a build with $env:MEDHA_VERSION.

$ErrorActionPreference = 'Stop'

$Repo    = if ($env:MEDHA_REPO)    { $env:MEDHA_REPO }    else { 'kashyaprajharsh/medha-agent' }
$Version = if ($env:MEDHA_VERSION) { $env:MEDHA_VERSION } else { 'latest' }
$Dest    = if ($env:MEDHA_INSTALL_DIR) { $env:MEDHA_INSTALL_DIR } else { "$env:LOCALAPPDATA\Programs\medha" }

if ([Environment]::Is64BitOperatingSystem -eq $false) {
    throw "medha requires a 64-bit version of Windows."
}
$Target = 'x86_64-pc-windows-msvc'

if ($Version -eq 'latest') {
    Write-Host "Resolving the latest release..."
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ 'User-Agent' = 'medha-installer' }
        $Version = $release.tag_name
    } catch {
        throw "Could not resolve the latest release of $Repo. Set `$env:MEDHA_VERSION to a tag, or check that the repository has a published release."
    }
}

$Asset = "medha-$Target.zip"
$Url   = "https://github.com/$Repo/releases/download/$Version/$Asset"
$Tmp   = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $Tmp -Force | Out-Null

try {
    Write-Host "Downloading medha $Version for $Target..."
    $archive = Join-Path $Tmp $Asset
    try {
        Invoke-WebRequest -Uri $Url -OutFile $archive -UseBasicParsing
    } catch {
        throw "Download failed: $Url`nThis platform may not have a published build for $Version."
    }

    # Verify the checksum when the release publishes one.
    try {
        $sumFile = "$archive.sha256"
        Invoke-WebRequest -Uri "$Url.sha256" -OutFile $sumFile -UseBasicParsing
        $expected = ((Get-Content $sumFile -Raw) -replace '[^A-Fa-f0-9]', '').ToLower()
        $actual   = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
        if ($expected -and ($expected -ne $actual)) {
            throw "Checksum mismatch - refusing to install."
        }
        Write-Host "Checksum verified."
    } catch [System.Net.WebException] {
        # No checksum published for this release; continue.
    }

    Expand-Archive -Path $archive -DestinationPath $Tmp -Force
    $binary = Get-ChildItem -Path $Tmp -Filter 'medha.exe' -Recurse | Select-Object -First 1
    if (-not $binary) { throw "The archive did not contain medha.exe." }

    New-Item -ItemType Directory -Path $Dest -Force | Out-Null
    Copy-Item $binary.FullName -Destination (Join-Path $Dest 'medha.exe') -Force

    # Put it on PATH for future sessions, and this one.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -notlike "*$Dest*") {
        [Environment]::SetEnvironmentVariable('Path', "$userPath;$Dest", 'User')
        Write-Host "Added $Dest to your PATH."
    }
    $env:Path = "$env:Path;$Dest"

    Write-Host ""
    Write-Host "medha $Version installed to $Dest\medha.exe"
    Write-Host "Run 'medha' to get started. (Open a new terminal if the command is not found.)"
}
finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
