# Native Windows packaging. Build-only CI may omit signing; release packaging
# requires a trusted certificate already available in the current-user store.
[CmdletBinding()]
param(
  [ValidateSet('x64','arm64')][string]$Architecture = 'x64',
  [Parameter(Mandatory)][string]$HelperCatalog,
  [string]$OutputDirectory = '',
  [string]$CertificateThumbprint = $env:DIRI_WINDOWS_SIGN_THUMBPRINT,
  [string]$TimestampUrl = 'http://timestamp.digicert.com',
  [switch]$Unsigned
)
$ErrorActionPreference = 'Stop'
$workspace = Split-Path -Parent $PSScriptRoot
Push-Location $workspace
try {
  $version = ((Select-String '^version = "([^"]+)"' crates/diri-app/Cargo.toml)[0].Matches.Groups[1].Value)
  if ($version -notmatch '^\d+\.\d+\.\d+$') { throw 'A numeric release version is required' }
  if (!$OutputDirectory) { $OutputDirectory = Join-Path $workspace "dist/windows/$Architecture" }
  $OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
  $target = if ($Architecture -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
  & cargo build --locked --release --target $target -p diri-app -p diri-engine -p dirijor-mcp
  if ($LASTEXITCODE -ne 0) { throw 'Windows build failed' }
  $payload = Join-Path $OutputDirectory "payload-$version-$Architecture-$([guid]::NewGuid().ToString('N'))"
  New-Item -ItemType Directory -Force $payload | Out-Null
  foreach ($binary in @('diri','dirijord-rs','diri-holder','diri-ssh-askpass','dirijor','dirijor-mcp')) {
    Copy-Item "target/$target/release/$binary.exe" $payload
  }
  $manifests = @(Get-ChildItem crates/diri-engine/manifests -Filter '*.json')
  if ($manifests.Count -lt 20) { throw "Incomplete Agent catalog: $($manifests.Count) manifests" }
  Copy-Item crates/diri-engine/manifests (Join-Path $payload 'manifests') -Recurse -Force
  $catalog = Get-Content -Raw $HelperCatalog | ConvertFrom-Json
  if ($catalog.protocolMajor -ne 1 -or $catalog.buildId -notmatch '^[A-Za-z0-9._-]{1,128}$') { throw 'Invalid Helper catalog identity' }
  $required = @('x86_64-unknown-linux-musl','aarch64-unknown-linux-musl','aarch64-apple-darwin')
  if (@($catalog.artifacts).Count -ne $required.Count) { throw 'Expected the three supported Helper targets' }
  $helperRoot = Split-Path -Parent ([IO.Path]::GetFullPath($HelperCatalog))
  foreach ($entry in $catalog.artifacts) {
    if ($entry.target -notin $required -or $entry.path -ne "artifacts/$($entry.target)/diri-remote") { throw 'Invalid Helper artifact path' }
    $required = @($required | Where-Object { $_ -ne $entry.target })
    $artifact = Join-Path $helperRoot $entry.path
    if ((Get-Item $artifact).Length -ne $entry.length -or (Get-FileHash $artifact -Algorithm SHA256).Hash -ne $entry.sha256) { throw 'Helper artifact failed integrity verification' }
    $destination = Join-Path $payload "remote-helpers/$($entry.path)"
    New-Item -ItemType Directory -Force (Split-Path -Parent $destination) | Out-Null
    Copy-Item $artifact $destination
  }
  Copy-Item $HelperCatalog (Join-Path $payload 'remote-helpers/manifest.json')
  Copy-Item WINDOWS.md $payload
  Copy-Item ../LICENSE $payload
  $signTool = Get-ChildItem "${env:ProgramFiles(x86)}/Windows Kits/10/bin/*/x64/signtool.exe" | Sort-Object FullName -Descending | Select-Object -First 1
  if (!$Unsigned) {
    if (!$CertificateThumbprint -or !$signTool) { throw 'Release packaging requires a current-user signing certificate and Windows SDK signtool' }
    foreach ($image in Get-ChildItem $payload -Filter '*.exe') {
      & $signTool.FullName sign /sha1 $CertificateThumbprint /fd SHA256 /tr $TimestampUrl /td SHA256 $image.FullName
      if ($LASTEXITCODE -ne 0) { throw 'Binary signing failed' }
      & $signTool.FullName verify /pa $image.FullName
      if ($LASTEXITCODE -ne 0) { throw 'Binary signature verification failed' }
    }
  }
  & (Join-Path $PSScriptRoot 'assemble-windows-installer.ps1') -PayloadDirectory $payload -OutputDirectory $OutputDirectory -Architecture $Architecture -Version $version
  $installer = Join-Path $OutputDirectory "diri-$version-windows-$Architecture-setup.exe"
  if (!$Unsigned) {
    & $signTool.FullName sign /sha1 $CertificateThumbprint /fd SHA256 /tr $TimestampUrl /td SHA256 $installer
    if ($LASTEXITCODE -ne 0) { throw 'Installer signing failed' }
    & $signTool.FullName verify /pa $installer
    if ($LASTEXITCODE -ne 0) { throw 'Installer signature verification failed' }
    & (Join-Path $PSScriptRoot 'write-windows-appcast.ps1') -Installer $installer -Application (Join-Path $payload 'diri.exe') -Architecture $Architecture -Version $version
  }
  Write-Output "Windows package: $installer"
} finally { Pop-Location }
