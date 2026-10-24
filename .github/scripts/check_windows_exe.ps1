# Fails when a built program needs the Visual C++ runtime DLL, or runs under the wrong Windows subsystem.
param(
    [string[]]$Console = @(),
    [string[]]$Windowed = @()
)
$ErrorActionPreference = 'Stop'

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$dumpbin = & $vswhere -latest -products * -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe' | Select-Object -First 1
if (-not $dumpbin) { throw 'dumpbin.exe was not found' }

$failed = $false
$checks = @($Console | ForEach-Object { @{ Exe = $_; Subsystem = 'Windows CUI' } }) +
    @($Windowed | ForEach-Object { @{ Exe = $_; Subsystem = 'Windows GUI' } })
if ($checks.Count -eq 0) { throw 'no program to check' }
foreach ($check in $checks) {
    $exe = $check.Exe
    if (-not (Test-Path $exe)) { throw "$exe was not built" }
    $dependents = & $dumpbin /nologo /dependents $exe | Out-String
    if ($dependents -match '(?i)\b(vcruntime\d+(_\d+)?|msvcp\d+)\.dll') {
        Write-Host "::error::$exe needs $($Matches[0]), which Windows does not always have"
        $failed = $true
    }
    $headers = & $dumpbin /nologo /headers $exe | Out-String
    if ($headers -notmatch [regex]::Escape("subsystem ($($check.Subsystem))")) {
        Write-Host "::error::$exe is not built for the $($check.Subsystem) subsystem"
        $failed = $true
    }
    Write-Host "checked $exe"
}
if ($failed) { exit 1 }
