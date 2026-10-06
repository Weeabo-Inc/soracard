# Build the StorPort miniport (storport/) and, with -Package, the signed
# driver package; -Release for the optimized build (package under
# target\x86_64-pc-windows-msvc\release\ instead of debug\); -Clippy lints
# the driver crate (warnings are errors). Streams cargo output as plain text (run it over SSH).
param([switch]$Package, [switch]$Release, [switch]$Clippy)
$ErrorActionPreference = 'Continue'

# MSVC environment from whichever Visual Studio / Build Tools has the C++ tools.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = if (Test-Path $vswhere) {
  & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
}
$vcvars = if ($vs) { Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat' } else { '' }
if ($vcvars -and (Test-Path $vcvars)) {
  foreach ($line in (cmd /c "call `"$vcvars`" >nul 2>&1 && set")) {
    if ($line -match '^(.*?)=(.*)$') { Set-Item -Path ("Env:" + $matches[1]) -Value $matches[2] -ErrorAction SilentlyContinue }
  }
}
# libclang for bindgen: keep a LIBCLANG_PATH set by the caller (CI), else the
# default LLVM install.
if (-not $env:LIBCLANG_PATH) { $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin' }
$env:Path = "$env:LIBCLANG_PATH;" + $env:Path

$wdk = Get-ChildItem 'C:\packages' -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -like 'Microsoft.Windows.WDK.x64.*' } | Select-Object -First 1
$sdk = Get-ChildItem 'C:\packages' -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '^Microsoft\.Windows\.SDK\.CPP\.\d' } | Select-Object -First 1
$ver = '10.0.26100.0'
if ($wdk) { $env:WDKContentRoot = "$($wdk.FullName)\c\"; $env:WDKBinRoot = "$($wdk.FullName)\c\bin\$ver"; $env:WDKToolRoot = "$($wdk.FullName)\c\tools\$ver" }
$env:Version_Number = $ver; $env:NugetPackagesRoot = 'C:\packages'
if ($sdk) { $env:WindowsSdkBinPath = "$($sdk.FullName)\c\bin" }
$env:CARGO_TERM_COLOR = 'never'

Set-Location (Join-Path $PSScriptRoot '..\storport')
# cargo from PATH (CI), else rustup's default location (it is not on an SSH
# session's PATH on the test machine).
$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) { $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
$prof = if ($Release) { 'release' } else { 'dev' }
if ($Package) { $cmd = "wdk build --profile $prof --target-arch amd64" } else { $cmd = "build --profile $prof" }
if ($Clippy) { $cmd = "clippy --profile $prof -- -D warnings" }
# cmd /c keeps stderr as plain text instead of PowerShell ErrorRecords.
cmd /c "`"$cargo`" $cmd 2>&1"
$code = $LASTEXITCODE
Write-Output "exit=$code"
exit $code
