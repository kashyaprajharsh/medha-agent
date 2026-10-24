# MEDHA installer — Windows (PowerShell).
#
#   irm https://raw.githubusercontent.com/kashyaprajharsh/medha-agent/main/install.ps1 | iex
#
# Installs the release build. Set MEDHA_INSTALL_DIR to override the destination.
# To pin both installer and binary:
#   $version = 'v0.1.0'; $env:MEDHA_VERSION = $version
#   irm "https://raw.githubusercontent.com/kashyaprajharsh/medha-agent/$version/install.ps1" | iex

$ErrorActionPreference = 'Stop'

function Get-ChecksumDigest {
    param([Parameter(Mandatory = $true)][string] $Path)

    $records = @(Get-Content -LiteralPath $Path | Where-Object {
        -not [string]::IsNullOrWhiteSpace($_)
    })
    if ($records.Count -ne 1) {
        throw "Checksum file must contain exactly one non-empty record."
    }

    # Accept bare, GNU, and BSD records with exactly one digest.
    $digests = @($records[0].Trim() -split '\s+' | Where-Object {
        $_ -cmatch '^[A-Fa-f0-9]{64}$'
    })
    if ($digests.Count -ne 1) {
        throw "Checksum file must contain exactly one 64-hex SHA-256 digest."
    }
    return $digests[0].ToLowerInvariant()
}

function Get-NormalizedPathSegment {
    param([AllowNull()][string] $Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $null
    }
    $trimmed = $Path.Trim().Trim('"')
    return ([System.IO.Path]::GetFullPath($trimmed)).TrimEnd('\', '/')
}

function Test-PathContains {
    param(
        [AllowNull()][string] $PathValue,
        [Parameter(Mandatory = $true)][string] $Expected
    )

    $wanted = Get-NormalizedPathSegment $Expected
    foreach ($segment in @($PathValue -split ';')) {
        if ([string]::IsNullOrWhiteSpace($segment)) {
            continue
        }
        try {
            $candidate = Get-NormalizedPathSegment $segment
        } catch {
            continue
        }
        if ([System.StringComparer]::OrdinalIgnoreCase.Equals($candidate, $wanted)) {
            return $true
        }
    }
    return $false
}

function Expand-ValidatedMedhaArchive {
    param(
        [Parameter(Mandatory = $true)][string] $Archive,
        [Parameter(Mandatory = $true)][string] $Output
    )

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [System.IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        $entries = @($zip.Entries)
        if ($entries.Count -ne 1) {
            throw "Archive layout is invalid; expected exactly one root file named medha.exe."
        }
        $entry = $entries[0]
        if (($entry.FullName -cne 'medha.exe') -or ($entry.Name -cne 'medha.exe')) {
            throw "Archive layout is invalid; expected exactly one root file named medha.exe."
        }
        if ($entry.Length -le 0) {
            throw "Archive medha.exe entry is empty."
        }

        # Reject non-regular Unix entries; zero is normal for Windows ZIPs.
        $attributes = [System.BitConverter]::ToUInt32(
            [System.BitConverter]::GetBytes([int32] $entry.ExternalAttributes),
            0
        )
        $unixType = (($attributes -shr 16) -band 0xF000)
        $dosDirectory = (($attributes -band 0x10) -ne 0)
        if ($dosDirectory -or (($unixType -ne 0) -and ($unixType -ne 0x8000))) {
            throw "Archive medha.exe entry is not a regular file."
        }

        $inputStream = $entry.Open()
        try {
            $outputStream = [System.IO.File]::Create($Output)
            try {
                $inputStream.CopyTo($outputStream)
                $outputStream.Flush()
            } finally {
                $outputStream.Dispose()
            }
        } finally {
            $inputStream.Dispose()
        }
    } finally {
        $zip.Dispose()
    }
}

function Lock-MedhaInstall {
    param([string] $Path, [int] $Seconds)
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ($true) {
        try {
            return [System.IO.File]::Open($Path, 'OpenOrCreate', 'ReadWrite', 'None')
        } catch [System.IO.IOException] {
            if ((Get-Date) -ge $deadline) { throw "Another Medha install is still running ($Path)." }
            Start-Sleep -Milliseconds 250
        }
    }
}

# The junction's own folder, or $null when there is none; a real folder there is never ours to replace.
function Get-MedhaJunctionTarget {
    param([string] $Path)
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    if (-not $item) { return $null }
    if (-not ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
        throw "Refusing to replace $Path because it is not a link this installer made."
    }
    return [System.IO.Path]::GetFullPath(@($item.Target)[0])
}

# Directory.Delete without recursion removes only the junction, never the version it points to.
function Switch-MedhaJunction {
    param([string] $Link, [string] $Target)
    $next = "$Link.next"
    if (Get-Item -LiteralPath $next -Force -ErrorAction SilentlyContinue) { [System.IO.Directory]::Delete($next) }
    New-Item -ItemType Junction -Path $next -Target $Target | Out-Null
    if ($null -ne (Get-MedhaJunctionTarget $Link)) { [System.IO.Directory]::Delete($Link) }
    Rename-Item -LiteralPath $next -NewName (Split-Path -Leaf $Link)
}

# Each version keeps its own folder and `bin` points at one, so an update never touches a running medha.exe.
function Install-MedhaVersion {
    param([string] $Binary, [string] $Root, [string] $Version, [int] $LockSeconds = 120)
    if ($Version -notmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$') { throw "Not a release tag: $Version" }
    $versions = Join-Path $Root 'versions'
    $bin = Join-Path $Root 'bin'
    New-Item -ItemType Directory -Path $versions -Force | Out-Null
    $lock = Lock-MedhaInstall (Join-Path $Root 'install.lock') $LockSeconds
    try {
        # Holding the lock, anything hidden here was left by an install that crashed or was still running.
        Get-ChildItem -LiteralPath $versions -Force -Filter '.*' -ErrorAction SilentlyContinue |
            Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
        $staging = Join-Path $versions ".staging.$([guid]::NewGuid().ToString('N'))"
        New-Item -ItemType Directory -Path $staging | Out-Null
        Copy-Item -LiteralPath $Binary -Destination (Join-Path $staging 'medha.exe')
        $release = [System.IO.Path]::GetFullPath((Join-Path $versions $Version))
        if (Test-Path -LiteralPath $release) {
            try { Remove-Item -LiteralPath $release -Recurse -Force } catch { $release = "$release.$([guid]::NewGuid().ToString('N'))" }
        }
        Move-Item -LiteralPath $staging -Destination $release
        # An install from before version folders: moving a running exe aside is allowed, writing over it is not.
        $loose = Join-Path $Root 'medha.exe'
        if (Test-Path -LiteralPath $loose -PathType Leaf) {
            Move-Item -LiteralPath $loose -Destination (Join-Path $versions ".loose.$([guid]::NewGuid().ToString('N')).exe")
        }
        $previous = Get-MedhaJunctionTarget $bin
        Switch-MedhaJunction $bin $release
        Get-ChildItem -LiteralPath $versions -Directory |
            Where-Object { $_.FullName -ne $release -and $_.FullName -ne $previous } |
            Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
    } finally {
        $lock.Dispose()
    }
    return $bin
}

# PATH loses the folder older installers used and gains `bin`, once.
function Update-MedhaPath {
    param([AllowNull()][string] $PathValue, [string] $Root, [string] $Bin)
    if ((Test-PathContains -PathValue $PathValue -Expected $Bin) -and -not (Test-PathContains -PathValue $PathValue -Expected $Root)) {
        return $PathValue
    }
    $kept = @(@($PathValue -split ';') | Where-Object {
        -not [string]::IsNullOrWhiteSpace($_) -and
        -not (Test-PathContains -PathValue $_ -Expected $Root) -and
        -not (Test-PathContains -PathValue $_ -Expected $Bin)
    })
    return (@($kept) + $Bin) -join ';'
}

if ($env:MEDHA_INSTALLER_TEST_MODE -eq '1') {
    # Dot-sourcing exposes helpers to tests without running the installer.
    return
}

# Use color only on an interactive console unless NO_COLOR is set.
$UseStyle  = (-not [Console]::IsOutputRedirected) -and [string]::IsNullOrEmpty($env:NO_COLOR)
$GlyphStep = if ($UseStyle) { [char]0x2192 } else { '>' }
$GlyphOk   = if ($UseStyle) { [char]0x2713 } else { '-' }
$MidDot    = [char]0x00B7

function Write-Step {
    param([string] $Message)
    if ($UseStyle) {
        Write-Host '  ' -NoNewline
        Write-Host $GlyphStep -ForegroundColor Cyan -NoNewline
        Write-Host " $Message"
    } else { Write-Host "  $GlyphStep $Message" }
}
function Write-Ok {
    param([string] $Message)
    if ($UseStyle) {
        Write-Host '  ' -NoNewline
        Write-Host $GlyphOk -ForegroundColor Green -NoNewline
        Write-Host " $Message"
    } else { Write-Host "  $GlyphOk $Message" }
}
function Write-Note {
    param([string] $Message)
    if ($UseStyle) { Write-Host "  $Message" -ForegroundColor DarkGray }
    else { Write-Host "  $Message" }
}

Write-Host ''
if ($UseStyle) {
    Write-Host '  ' -NoNewline
    Write-Host 'medha' -ForegroundColor Cyan -NoNewline
    Write-Host " $MidDot verification-first agent harness" -ForegroundColor DarkGray
} else {
    Write-Host "  medha $MidDot verification-first agent harness"
}
Write-Host '  ----------------------------------------' -ForegroundColor DarkGray
Write-Host ''

$Repo    = if ($env:MEDHA_REPO)    { $env:MEDHA_REPO }    else { 'kashyaprajharsh/medha-agent' }
$Version = if ($env:MEDHA_VERSION) { $env:MEDHA_VERSION } else { 'latest' }
$Dest    = if ($env:MEDHA_INSTALL_DIR) { $env:MEDHA_INSTALL_DIR } else { "$env:LOCALAPPDATA\Programs\medha" }

if ([Environment]::Is64BitOperatingSystem -eq $false) {
    throw "medha requires a 64-bit version of Windows."
}
$Target = 'x86_64-pc-windows-msvc'

# Bound response waits; PowerShell 6+ also retries transient failures.
$WebArgs = @{ UseBasicParsing = $true; TimeoutSec = 30 }
if ($PSVersionTable.PSVersion.Major -ge 6) {
    $WebArgs['MaximumRetryCount'] = 3
    $WebArgs['RetryIntervalSec'] = 2
}

if ($Version -eq 'latest') {
    Write-Step 'resolving latest release'
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ 'User-Agent' = 'medha-installer' } @WebArgs
        $Version = $release.tag_name
    } catch {
        throw "Could not resolve the latest release of $Repo. Set `$env:MEDHA_VERSION to a tag, or check that the repository has a published release."
    }
    Write-Ok "resolved $Version"
}

$Asset = "medha-$Target.zip"
$Url   = "https://github.com/$Repo/releases/download/$Version/$Asset"
$Tmp   = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $Tmp -Force | Out-Null

try {
    Write-Step "downloading medha $Version  ($Target)"
    $archive = Join-Path $Tmp $Asset
    try {
        Invoke-WebRequest -Uri $Url -OutFile $archive @WebArgs
    } catch {
        throw "Download failed: $Url`nThis platform may not have a published build for $Version."
    }

    # Every release publishes a checksum beside each archive; a missing or unreadable one fails closed.
    $sumFile = "$archive.sha256"
    try {
        Invoke-WebRequest -Uri "$Url.sha256" -OutFile $sumFile @WebArgs
    } catch {
        throw "Checksum download failed: $Url.sha256 - refusing an unverifiable install. $($_.Exception.Message)"
    }
    $expected = Get-ChecksumDigest $sumFile
    $actual   = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($expected -cne $actual) {
        throw "Checksum mismatch - refusing to install."
    }
    Write-Ok 'checksum verified'

    $binary = Join-Path $Tmp 'medha.exe'
    Expand-ValidatedMedhaArchive -Archive $archive -Output $binary

    New-Item -ItemType Directory -Path $Dest -Force | Out-Null
    $bin = Install-MedhaVersion -Binary $binary -Root $Dest -Version $Version

    # Update both persistent and current-session PATH.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $newUserPath = Update-MedhaPath -PathValue $userPath -Root $Dest -Bin $bin
    if ($newUserPath -cne $userPath) {
        [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
        Write-Note "added $bin to your PATH"
    }
    $env:Path = Update-MedhaPath -PathValue $env:Path -Root $Dest -Bin $bin

    Write-Ok "installed  $bin\medha.exe"
    Write-Host ''
    Write-Host "  medha $Version is ready.  run " -NoNewline
    Write-Host 'medha' -ForegroundColor Cyan -NoNewline
    Write-Host ' to begin.'
    Write-Note 'open a new terminal if medha is not found yet.'
    Write-Host ''
}
finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
