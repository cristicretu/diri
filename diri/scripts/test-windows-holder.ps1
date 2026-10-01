# Hosted runner Jobs prohibit child breakaway. Run only the detached-Holder
# integration binary through the existing local WMI process provider, outside
# that Job. This is CI scaffolding, never an app launch fallback or service setup.
$ErrorActionPreference = 'Stop'
$artifacts = @(& cargo test --locked -p diri-platform -p diri-pty -p diri-proto -p diri-client -p diri-terminal-state -p diri-notes -p diri-updater -p diri-engine --lib --tests --no-run --message-format=json | ForEach-Object {
  $message = $_ | ConvertFrom-Json
  if ($message.reason -eq 'compiler-artifact' -and $message.target.name -eq 'holder_windows' -and $message.executable) { $message.executable }
})
if ($LASTEXITCODE -ne 0 -or $artifacts.Count -ne 1) { throw 'Holder test executable was not built' }
function Quote([string]$Value) { return "'" + $Value.Replace("'", "''") + "'" }
$directory = Join-Path $env:RUNNER_TEMP ("diri-holder-test-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $directory | Out-Null
$log = Join-Path $directory 'output.txt'
$errors = Join-Path $directory 'errors.txt'
$result = Join-Path $directory 'exit.txt'
$body = @"
`$ErrorActionPreference = 'Stop'
Set-Location $(Quote $PWD.Path)
try {
  # Start-Process -Wait creates its own restrictive tracking Job. Use the
  # plain .NET process API so this wrapper does not recreate the CI constraint.
  `$test = [Diagnostics.Process]::new()
  `$test.StartInfo.FileName = $(Quote $artifacts[0])
  `$test.StartInfo.Arguments = '--test-threads=1'
  `$test.StartInfo.UseShellExecute = `$false
  `$test.StartInfo.CreateNoWindow = `$true
  `$test.StartInfo.RedirectStandardOutput = `$true
  `$test.StartInfo.RedirectStandardError = `$true
  `$null = `$test.Start()
  `$stdout = `$test.StandardOutput.ReadToEndAsync()
  `$stderr = `$test.StandardError.ReadToEndAsync()
  if (!`$test.WaitForExit(240000)) { `$test.Kill(`$true); throw 'Holder test binary timed out' }
  [IO.File]::WriteAllText($(Quote $log), `$stdout.GetAwaiter().GetResult())
  [IO.File]::WriteAllText($(Quote $errors), `$stderr.GetAwaiter().GetResult())
  [IO.File]::WriteAllText($(Quote $result), [string]`$test.ExitCode)
  `$test.Dispose()
} catch {
  `$_ | Out-File -Append -FilePath $(Quote $log)
  [IO.File]::WriteAllText($(Quote $result), '1')
}
"@
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($body))
$powershell = (Get-Process -Id $PID).Path
# Pass only the environment needed by these disposable tests, never the runner's
# credential-bearing environment. WMI does not inherit the caller's environment.
$environment = @('USERPROFILE', 'SystemRoot', 'WINDIR', 'TEMP', 'TMP', 'PATH') | ForEach-Object {
  $value = [Environment]::GetEnvironmentVariable($_)
  if ($null -ne $value) { "$_=$value" }
}
$startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{
  CreateFlags = [uint32]0x01000000 # CREATE_BREAKAWAY_FROM_JOB (WMI's own Job)
  EnvironmentVariables = [string[]]$environment
}
$created = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
  CommandLine = "`"$powershell`" -NoLogo -NoProfile -NonInteractive -EncodedCommand $encoded"
  CurrentDirectory = $PWD.Path
  ProcessStartupInformation = $startup
}
if ($created.ReturnValue -ne 0) { throw "Standalone Holder test launch failed: $($created.ReturnValue)" }
$process = [Diagnostics.Process]::GetProcessById([int]$created.ProcessId)
# Acquire the process handle now; timeout cleanup must not act on a reused PID.
$null = $process.Handle
try {
  if (!$process.WaitForExit(300000)) {
    $process.Kill($true)
    throw 'Standalone Holder tests timed out'
  }
  if (Test-Path -LiteralPath $log) { Get-Content -LiteralPath $log }
  if (Test-Path -LiteralPath $errors) { Get-Content -LiteralPath $errors }
  if (!(Test-Path -LiteralPath $result) -or ([string](Get-Content -Raw -LiteralPath $result)).Trim() -ne '0') {
    throw 'Standalone Holder lifecycle tests failed'
  }
} finally {
  $process.Dispose()
  Remove-Item -LiteralPath $directory -Recurse -Force
}
