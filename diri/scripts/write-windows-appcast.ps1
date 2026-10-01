# Run only after signing the installer: these hashes cover the shipped bytes.
[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$Installer,
  [Parameter(Mandatory)][string]$Application,
  [Parameter(Mandatory)][ValidateSet('x64','arm64')][string]$Architecture,
  [Parameter(Mandatory)][string]$Version
)
$ErrorActionPreference = 'Stop'
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw 'Invalid release version' }
function Get-SignerIdentity([string]$Path) {
  $signature = Get-AuthenticodeSignature -LiteralPath $Path
  if ($signature.Status -ne 'Valid' -or !$signature.SignerCertificate) { throw 'Untrusted Authenticode signature' }
  $certificate = $signature.SignerCertificate
  $ekus = @($certificate.EnhancedKeyUsageList | ForEach-Object { $_.ObjectId.Value })
  if ($ekus -contains '1.3.6.1.4.1.311.97.1.0') {
    $profiles = @($ekus | Where-Object { $_ -match '^1\.3\.6\.1\.4\.1\.311\.97\.(?!1\.)(?:\d+\.){3,}\d+$' })
    if ($profiles.Count -ne 1 -or [string]::IsNullOrWhiteSpace($certificate.Subject)) { throw 'Ambiguous Artifact Signing profile' }
    return "artifact-signing:$($profiles[0]):$($certificate.Subject)"
  }
  return "certificate:$($certificate.Thumbprint.ToUpperInvariant())"
}
if ((Get-SignerIdentity $Installer) -cne (Get-SignerIdentity $Application)) { throw 'Installer and app signer identities differ' }
foreach ($path in @($Installer, $Application)) {
  $info = (Get-Item -LiteralPath $path).VersionInfo
  if ($info.ProductName -cne 'Diri' -or $info.ProductVersion -cne $Version) { throw 'Signed product/version does not match release' }
}
$release = @{version=$Version; url="https://github.com/cristicretu/diri/releases/download/v$Version/$(Split-Path -Leaf $Installer)"; size=(Get-Item -LiteralPath $Installer).Length; sha256=(Get-FileHash -LiteralPath $Installer -Algorithm SHA256).Hash.ToLowerInvariant(); minimum_system_version='10.0.22621'}
@{feed_version=1; releases=@($release)} | ConvertTo-Json -Depth 5 | Set-Content -Encoding utf8NoBOM (Join-Path (Split-Path -Parent $Installer) "appcast-windows-$Architecture.json")
