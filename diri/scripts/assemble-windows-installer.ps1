# Compile an installer from an already staged payload. Signing is a separate
# step so hosted signing can run before embedding the executable files.
[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$PayloadDirectory,
  [Parameter(Mandatory)][string]$OutputDirectory,
  [Parameter(Mandatory)][ValidateSet('x64','arm64')][string]$Architecture,
  [Parameter(Mandatory)][string]$Version
)
$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw 'Invalid release version' }
$payload = [IO.Path]::GetFullPath($PayloadDirectory)
$output = [IO.Path]::GetFullPath($OutputDirectory)
foreach ($name in @('diri','dirijord-rs','diri-holder','diri-ssh-askpass','dirijor','dirijor-mcp')) {
  if (!(Test-Path -LiteralPath (Join-Path $payload "$name.exe") -PathType Leaf)) { throw "Missing $name.exe" }
}
if (@(Get-ChildItem (Join-Path $payload 'manifests') -Filter '*.json').Count -lt 20) { throw 'Incomplete Agent catalog' }
if (!(Test-Path -LiteralPath (Join-Path $payload 'remote-helpers/manifest.json'))) { throw 'Missing Helper catalog' }
$compiler = (Get-Command ISCC.exe -ErrorAction SilentlyContinue).Source
if (!$compiler) { $compiler = "${env:ProgramFiles(x86)}/Inno Setup 6/ISCC.exe" }
if (!(Test-Path -LiteralPath $compiler)) { throw 'Inno Setup 6 is required on the packaging builder' }
New-Item -ItemType Directory -Force $output | Out-Null
& $compiler "/DVersion=$Version" "/DArchitecture=$Architecture" "/DPayload=$payload" "/O$output" (Join-Path $PSScriptRoot '../assets/windows/installer.iss')
if ($LASTEXITCODE -ne 0) { throw 'Installer compilation failed' }
