# Build and launch an unmistakable development copy of diri on Windows.
# Counterpart of scripts/dev.sh: every run gets a fresh copy of the binaries
# (a running diri.exe locks its image, so builds never overwrite a live one),
# and every commit gets its own app-support root, Engine, socket and state.
[CmdletBinding()]
param(
  [switch]$Release,
  [ValidateSet('', 'general', 'appearance', 'terminal', 'resources', 'remote', 'diagnostics', 'usage')]
  [string]$Settings = '',
  [Parameter(ValueFromRemainingArguments)][string[]]$CargoArgs = @()
)
$ErrorActionPreference = 'Stop'
$workspace = Split-Path -Parent $PSScriptRoot
$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $workspace 'target' }
$profileName = if ($Release) { 'release' } else { 'debug' }
foreach ($argument in $CargoArgs) {
  if ($argument -match '^--(target|target-dir|profile|release)(=|$)') {
    throw "$argument changes where the app binary is written; use -Release or CARGO_TARGET_DIR"
  }
}

$branch = & git -C $workspace symbolic-ref --quiet --short HEAD
if (!$branch) { $branch = 'detached' }
$shortSha = (& git -C $workspace rev-parse --short=8 HEAD).Trim()
& git -C $workspace diff --quiet --ignore-submodules --
$dirtyTree = $LASTEXITCODE -ne 0
& git -C $workspace diff --cached --quiet --ignore-submodules --
$dirty = if ($dirtyTree -or $LASTEXITCODE -ne 0) { '+dirty' } else { '' }
$buildLabel = "$branch@$shortSha$dirty"
$bundleId = "com.dirijor.diri.dev.$shortSha"

# AF_UNIX paths are limited to 108 bytes, including the trailing NUL.
$appSupport = Join-Path $targetDir "diri-dev-$shortSha-support"
if ([Text.Encoding]::UTF8.GetByteCount((Join-Path $appSupport 'daemon.sock')) -ge 108) {
  $appSupport = Join-Path ([IO.Path]::GetTempPath()) "diri-dev-$shortSha-support"
  Write-Output "==> App support exceeds the Unix socket limit; using $appSupport"
}

# Cargo reports progress on stderr. Windows PowerShell turns redirected native
# stderr into error records, so success is judged by the exit code alone.
function Invoke-Cargo([string[]]$Arguments) {
  $ErrorActionPreference = 'Continue'
  & cargo @Arguments
  if ($LASTEXITCODE -ne 0) { throw "cargo $($Arguments[0]) failed" }
}

Push-Location $workspace
try {
  Write-Output "==> Building diri dev $shortSha ($profileName)"
  $profileArgs = @()
  if ($Release) { $profileArgs += '--release' }
  Invoke-Cargo (@('build', '--package', 'diri-app', '--bin', 'diri', '--package', 'diri-engine', '--bin', 'dirijord-rs', '--bin', 'diri-holder', '--bin', 'diri-ssh-askpass') + $profileArgs + $CargoArgs)
  Invoke-Cargo (@('build', '--package', 'dirijor-mcp') + $profileArgs + $CargoArgs)
} finally { Pop-Location }

$built = Join-Path $targetDir $profileName
$copy = Join-Path $targetDir "diri-dev-$shortSha-$([guid]::NewGuid().ToString('N').Substring(0, 8))"
New-Item -ItemType Directory -Force $copy, $appSupport | Out-Null
foreach ($binary in @('diri', 'dirijord-rs', 'diri-holder', 'diri-ssh-askpass', 'dirijor', 'dirijor-mcp')) {
  $source = Join-Path $built "$binary.exe"
  if (!(Test-Path $source)) { throw "cargo did not produce $source" }
  Copy-Item $source $copy
}
Copy-Item (Join-Path $workspace 'crates/diri-engine/manifests') (Join-Path $copy 'manifests') -Recurse
Set-Content -NoNewline -Encoding ascii -Path (Join-Path $copy 'diri-dev-bundle-id') -Value $bundleId

$launch = @{
  DIRI_DEV = '1'
  DIRI_DEV_BUILD = $buildLabel
  DIRIJOR_APP_SUPPORT = $appSupport
  DIRIJORD_PATH = (Join-Path $copy 'dirijord-rs.exe')
  DIRI_SETTINGS_PREVIEW = $(if ($Settings) { $Settings } else { $null })
  DIRIJOR_SOCKET = $null
  DIRIJOR_SESSION_ID = $null
  DIRIJOR_CLI = $null
  NO_COLOR = $null
  FORCE_COLOR = $null
}
# Start-Process inherits this process's environment. Restore it afterwards so
# running the script inside an existing PowerShell session leaves no trace.
$saved = @{}
foreach ($name in $launch.Keys) { $saved[$name] = [Environment]::GetEnvironmentVariable($name) }
try {
  foreach ($name in $launch.Keys) { [Environment]::SetEnvironmentVariable($name, $launch[$name]) }
  Write-Output "==> Launching diri dev $shortSha ($buildLabel)"
  Write-Output "    app:     $copy\diri.exe"
  Write-Output "    support: $appSupport"
  Start-Process -FilePath (Join-Path $copy 'diri.exe') -WorkingDirectory $copy | Out-Null
} finally {
  foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name]) }
}
