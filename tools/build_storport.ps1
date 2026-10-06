# Build the StorPort miniport (storport/) and, with -Package, the signed
# driver package; -Release for the optimized build (package under
# target\x86_64-pc-windows-msvc\release\ instead of debug\); -Clippy lints
# the driver crate (warnings are errors). Streams cargo output as plain text (run it over SSH).
param([switch]$Package, [switch]$Release, [switch]$Clippy)
$ErrorActionPreference = 'Continue'

$vcvars = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat'
if (Test-Path $vcvars) {
  foreach ($line in (cmd /c "call `"$vcvars`" >nul 2>&1 && set")) {
    if ($line -match '^(.*?)=(.*)$') { Set-Item -Path ("Env:" + $matches[1]) -Value $matches[2] -ErrorAction SilentlyContinue }
  }
}
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
$env:Path = 'C:\Program Files\LLVM\bin;' + $env:Path

$wdk = Get-ChildItem 'C:\packages' -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -like 'Microsoft.Windows.WDK.x64.*' } | Select-Object -First 1
$sdk = Get-ChildItem 'C:\packages' -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '^Microsoft\.Windows\.SDK\.CPP\.\d' } | Select-Object -First 1
$ver = '10.0.26100.0'
if ($wdk) { $env:WDKContentRoot = "$($wdk.FullName)\c\"; $env:WDKBinRoot = "$($wdk.FullName)\c\bin\$ver"; $env:WDKToolRoot = "$($wdk.FullName)\c\tools\$ver" }
$env:Version_Number = $ver; $env:NugetPackagesRoot = 'C:\packages'
if ($sdk) { $env:WindowsSdkBinPath = "$($sdk.FullName)\c\bin" }
$env:CARGO_TERM_COLOR = 'never'

Set-Location (Join-Path $PSScriptRoot '..\storport')
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
$prof = if ($Release) { 'release' } else { 'dev' }
if ($Package) { $cmd = "wdk build --profile $prof --target-arch amd64" } else { $cmd = "build --profile $prof" }
if ($Clippy) { $cmd = "clippy --profile $prof -- -D warnings" }
# cmd /c keeps stderr as plain text instead of PowerShell ErrorRecords.
cmd /c "`"$cargo`" $cmd 2>&1"
Write-Output "exit=$LASTEXITCODE"
