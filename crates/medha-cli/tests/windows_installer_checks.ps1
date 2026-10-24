param(
    [Parameter(Mandatory = $true)][string] $Installer,
    [Parameter(Mandatory = $true)][string] $TempRoot
)

$ErrorActionPreference = 'Stop'
$env:MEDHA_INSTALLER_TEST_MODE = '1'
. $Installer
Remove-Item Env:MEDHA_INSTALLER_TEST_MODE

function Assert-True {
    param([bool] $Condition, [string] $Message)
    if (-not $Condition) {
        throw $Message
    }
}

function Assert-Throws {
    param([scriptblock] $Action, [string] $Message)
    $threw = $false
    try {
        & $Action
    } catch {
        $threw = $true
    }
    if (-not $threw) {
        throw $Message
    }
}

function Write-Checksum {
    param([string] $Name, [string] $Value)
    $path = Join-Path $TempRoot $Name
    [System.IO.File]::WriteAllText($path, $Value)
    return $path
}

$lower = 'a' * 64
$upper = 'B' * 64
$bare = Write-Checksum 'bare.sha256' $lower
$gnu = Write-Checksum 'gnu.sha256' "$upper  medha.zip`r`n"
$bsd = Write-Checksum 'bsd.sha256' "SHA256 (medha.zip) = $lower`n"
Assert-True ((Get-ChecksumDigest $bare) -ceq $lower) 'bare digest was not parsed'
Assert-True ((Get-ChecksumDigest $gnu) -ceq $upper.ToLowerInvariant()) 'GNU uppercase/CRLF digest was not parsed'
Assert-True ((Get-ChecksumDigest $bsd) -ceq $lower) 'BSD digest was not parsed'

$malformed = Write-Checksum 'malformed.sha256' 'not-a-digest'
$multiple = Write-Checksum 'multiple.sha256' "$lower file-one`n$lower file-two`n"
$ambiguous = Write-Checksum 'ambiguous.sha256' "$lower $upper"
Assert-Throws { Get-ChecksumDigest $malformed } 'malformed digest was accepted'
Assert-Throws { Get-ChecksumDigest $multiple } 'multiple checksum records were accepted'
Assert-Throws { Get-ChecksumDigest $ambiguous } 'multiple digest tokens were accepted'

Assert-True (-not (Test-PathContains 'C:\Tools\medha-old;C:\Else' 'C:\Tools\medha')) 'PATH substring was mistaken for an exact segment'
Assert-True (Test-PathContains 'C:\Else;C:\Tools\medha\' 'c:\tools\MEDHA') 'normalized exact PATH segment was missed'

# ZipArchive itself lives in System.IO.Compression; only the ZipFile helpers
# live in .FileSystem. PowerShell 7 resolves both from the shared framework,
# but Windows PowerShell 5.1 loads .NET Framework assemblies by name, so
# without the first line ZipArchive is an unresolvable type there.
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
function New-TestZip {
    param([string] $Name, [object[]] $Entries)
    $path = Join-Path $TempRoot $Name
    $stream = [System.IO.File]::Open($path, [System.IO.FileMode]::CreateNew)
    $zip = [System.IO.Compression.ZipArchive]::new(
        $stream,
        [System.IO.Compression.ZipArchiveMode]::Create,
        $false
    )
    try {
        foreach ($spec in $Entries) {
            $entry = $zip.CreateEntry([string] $spec.Name)
            if ($null -ne $spec.Attributes) {
                $entry.ExternalAttributes = [int32] $spec.Attributes
            }
            $writer = New-Object System.IO.StreamWriter($entry.Open())
            try {
                $writer.Write([string] $spec.Body)
            } finally {
                $writer.Dispose()
            }
        }
    } finally {
        $zip.Dispose()
        $stream.Dispose()
    }
    return $path
}

$regular = [pscustomobject]@{ Name = 'medha.exe'; Body = 'binary'; Attributes = 0 }
$validZip = New-TestZip 'valid.zip' @($regular)
$validOutput = Join-Path $TempRoot 'valid-medha.exe'
Expand-ValidatedMedhaArchive $validZip $validOutput
Assert-True (([System.IO.File]::ReadAllText($validOutput)) -ceq 'binary') 'valid root binary did not extract'

$duplicateZip = New-TestZip 'duplicate.zip' @($regular, $regular)
$traversalZip = New-TestZip 'traversal.zip' @(
    [pscustomobject]@{ Name = '../medha.exe'; Body = 'binary'; Attributes = 0 }
)
$absoluteZip = New-TestZip 'absolute.zip' @(
    [pscustomobject]@{ Name = '/medha.exe'; Body = 'binary'; Attributes = 0 }
)
$symlinkUnsigned = [System.Convert]::ToUInt32('A1FF0000', 16)
$symlinkBits = [System.BitConverter]::ToInt32(
    [System.BitConverter]::GetBytes($symlinkUnsigned),
    0
)
$symlinkZip = New-TestZip 'symlink.zip' @(
    [pscustomobject]@{ Name = 'medha.exe'; Body = '/tmp/attacker'; Attributes = $symlinkBits }
)
Assert-Throws { Expand-ValidatedMedhaArchive $duplicateZip (Join-Path $TempRoot 'duplicate.exe') } 'duplicate binary archive was accepted'
Assert-Throws { Expand-ValidatedMedhaArchive $traversalZip (Join-Path $TempRoot 'traversal.exe') } 'traversal archive was accepted'
Assert-Throws { Expand-ValidatedMedhaArchive $absoluteZip (Join-Path $TempRoot 'absolute.exe') } 'absolute-path archive was accepted'
Assert-Throws { Expand-ValidatedMedhaArchive $symlinkZip (Join-Path $TempRoot 'symlink.exe') } 'symlink archive was accepted'

function Write-Build {
    param([string] $Name, [string] $Body)
    $path = Join-Path $TempRoot $Name
    [System.IO.File]::WriteAllText($path, $Body)
    return $path
}

function Get-VersionNames {
    param([string] $Root)
    return (@(Get-ChildItem -LiteralPath (Join-Path $Root 'versions') -Directory | ForEach-Object Name | Sort-Object) -join ',')
}

function Start-StandIn {
    param([string] $Path)
    $process = Start-Process -FilePath $Path -ArgumentList '-n', '60', '127.0.0.1' -WindowStyle Hidden -PassThru
    Start-Sleep -Milliseconds 500
    Assert-True (-not $process.HasExited) 'the stand-in backend did not start'
    return $process
}

$ping = Join-Path $TempRoot 'ping-build.exe'
Copy-Item -LiteralPath (Join-Path $env:SystemRoot 'System32\PING.EXE') -Destination $ping

# An update must land while an older medha.exe is still running as the backend.
$root = Join-Path $TempRoot 'install-root'
$bin = Install-MedhaVersion -Binary $ping -Root $root -Version 'v-one'
$command = Join-Path $bin 'medha.exe'
Assert-True ([bool]((Get-Item -LiteralPath $bin -Force).Attributes -band [System.IO.FileAttributes]::ReparsePoint)) 'bin is not a junction'
$running = Start-StandIn $command
try {
    Assert-Throws { Copy-Item -LiteralPath (Write-Build 'probe.exe' 'probe') -Destination (Join-Path $root 'versions\v-one\medha.exe') -Force } 'a running binary was overwritten in place; the check proves nothing'
    Install-MedhaVersion -Binary (Write-Build 'two.exe' 'build two') -Root $root -Version 'v-two' | Out-Null
    Assert-True (([System.IO.File]::ReadAllText($command)) -ceq 'build two') 'bin does not point at the new version'
    Assert-True (-not $running.HasExited) 'installing stopped the running backend'
    Assert-True ((Get-VersionNames $root) -ceq 'v-one,v-two') 'the running version was not kept'
    Install-MedhaVersion -Binary (Write-Build 'two-again.exe' 'build two again') -Root $root -Version 'v-two' | Out-Null
    Assert-True (([System.IO.File]::ReadAllText($command)) -ceq 'build two again') 'reinstalling a version did not take effect'
} finally {
    Stop-Process -Id $running.Id -Force -ErrorAction SilentlyContinue
    $running.WaitForExit()
}
Install-MedhaVersion -Binary (Write-Build 'three.exe' 'build three') -Root $root -Version 'v-three' | Out-Null
Assert-True ((Get-VersionNames $root) -ceq 'v-three,v-two') 'only the current and previous version stay'

# A medha.exe from an older installer, still running, moves aside instead of blocking the update.
$oldRoot = Join-Path $TempRoot 'old-layout'
New-Item -ItemType Directory -Path $oldRoot | Out-Null
$loose = Join-Path $oldRoot 'medha.exe'
Copy-Item -LiteralPath $ping -Destination $loose
$oldBackend = Start-StandIn $loose
try {
    $oldBin = Install-MedhaVersion -Binary (Write-Build 'new.exe' 'new layout') -Root $oldRoot -Version 'v-new'
    Assert-True (([System.IO.File]::ReadAllText((Join-Path $oldBin 'medha.exe'))) -ceq 'new layout') 'the old layout was not upgraded'
    Assert-True (-not (Test-Path -LiteralPath $loose)) 'the old medha.exe stayed where the old PATH finds it'
    Assert-True (-not $oldBackend.HasExited) 'upgrading the layout stopped the running backend'
} finally {
    Stop-Process -Id $oldBackend.Id -Force -ErrorAction SilentlyContinue
    $oldBackend.WaitForExit()
}

# A second installer waits for the first, and gives up with a message instead of hanging.
$held = [System.IO.File]::Open((Join-Path $root 'install.lock'), 'OpenOrCreate', 'ReadWrite', 'None')
try {
    Assert-Throws { Install-MedhaVersion -Binary (Write-Build 'locked.exe' 'locked') -Root $root -Version 'v-locked' -LockSeconds 1 } 'a second install ran while the first held the lock'
} finally {
    $held.Dispose()
}
Assert-True (-not (Test-Path -LiteralPath (Join-Path $root 'versions\v-locked'))) 'a locked-out install still wrote a version'

Assert-Throws { Install-MedhaVersion -Binary $ping -Root $root -Version '..\escape' } 'a version that is not a tag was used as a folder name'

# PATH swaps the folder older installers used for bin, and is left alone once it is right.
$pathRoot = 'C:\Users\me\AppData\Local\Programs\medha'
$pathBin = "$pathRoot\bin"
Assert-True ((Update-MedhaPath -PathValue "C:\Windows;$pathRoot" -Root $pathRoot -Bin $pathBin) -ceq "C:\Windows;$pathBin") 'the old folder was not replaced by bin'
Assert-True ((Update-MedhaPath -PathValue "C:\Windows;$pathBin;C:\Tools" -Root $pathRoot -Bin $pathBin) -ceq "C:\Windows;$pathBin;C:\Tools") 'a PATH that was already right was rewritten'
Assert-True ((Update-MedhaPath -PathValue $null -Root $pathRoot -Bin $pathBin) -ceq $pathBin) 'an empty PATH did not gain bin'

Write-Host 'PowerShell installer checks passed.'
