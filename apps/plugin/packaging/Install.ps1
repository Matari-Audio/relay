#Requires -RunAsAdministrator
$ErrorActionPreference = 'Stop'
$dependencies = "$env:ProgramFiles\Matari Audio\RELAY\0.1.2"
New-Item -ItemType Directory -Force $dependencies | Out-Null
Copy-Item "$PSScriptRoot\CLAP\RELAY\*.dll" $dependencies -Force
foreach ($format in @('CLAP', 'VST3')) {
    $destination = "$env:CommonProgramFiles\$format"
    New-Item -ItemType Directory -Force $destination | Out-Null
    Copy-Item "$PSScriptRoot\$format\*" $destination -Recurse -Force
}
$path = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if ($dependencies -notin ($path -split ';')) {
    [Environment]::SetEnvironmentVariable('Path', "$path;$dependencies", 'Machine')
}
Write-Host 'RELAY installed. Restart Windows, then rescan plugins in your DAW.'
