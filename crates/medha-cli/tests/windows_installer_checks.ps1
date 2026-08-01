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

$direct404 = [pscustomobject]@{ StatusCode = 404; Response = $null; InnerException = $null }
$response503 = [pscustomobject]@{
    StatusCode = $null
    Response = [pscustomobject]@{ StatusCode = 503 }
    InnerException = $null
}
$nested404 = [pscustomobject]@{
    StatusCode = $null
    Response = $null
    InnerException = $direct404
}
Assert-True ((Find-HttpStatusCode $direct404) -eq 404) 'PowerShell 7 status shape was not recognized'
Assert-True ((Find-HttpStatusCode $nested404) -eq 404) 'nested HTTP status was not recognized'
Assert-True ((Find-HttpStatusCode $response503) -eq 503) 'Windows PowerShell response status shape was not recognized'
Assert-True ((Find-HttpStatusCode $response503) -ne 404) 'transient response was mistaken for not-found'

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

Write-Host 'PowerShell installer checks passed.'
