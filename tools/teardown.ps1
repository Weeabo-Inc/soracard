# =============================================================================
# SoraCard test driver - TOOLCHAIN TEARDOWN
# =============================================================================
# Removes everything the SoraCard driver work installed on this machine:
#   * cargo-wdk / cargo-make
#   * Rust (rustup, toolchain, ~/.cargo + ~/.rustup)
#   * LLVM/Clang 17.0.6            (winget)
#   * Visual Studio Build Tools    (winget)
#   * Windows SDK 10.0.22621       (winget)
#   * Windows Driver Kit 10.0.22621(winget)
#   * NuGet WDK/SDK in C:\packages (what the build actually uses)
#   * test-signing mode (bcdedit) and the WDRLocalTestCert certificate
#   * SoraCard driver packages, service key, scheduled tasks and test logs
#
# Safe to run multiple times. Hands the reader back to the vendor Realtek
# driver (rtsuer.inf, if it is in the DriverStore) before removing ours.
# Run elevated:  powershell -NoProfile -ExecutionPolicy Bypass -File teardown.ps1
# =============================================================================
$ErrorActionPreference = 'Continue'
$root = Join-Path $env:TEMP 'soracard'
New-Item -ItemType Directory -Force -Path $root | Out-Null
$log  = Join-Path $root 'teardown.txt'
Set-Content -LiteralPath $log -Value "=== SoraCard teardown $(Get-Date -Format s) ==="
function W($m){ Add-Content -LiteralPath $log -Value $m; Write-Output $m }
function Try-It([scriptblock]$b, [string]$what){
  try { & $b; W "OK: $what" } catch { W "FAIL($what): $($_.Exception.Message)" }
}
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'

# 1. cargo tools
if(Test-Path $cargo){
  & $cargo uninstall cargo-wdk   2>&1 | ForEach-Object { W $_ }
  & $cargo uninstall cargo-make  2>&1 | ForEach-Object { W $_ }
}

# 2. Rust toolchain
$rustup = Join-Path $env:USERPROFILE '.rustup\bin\rustup.exe'
if(Test-Path $rustup){ & $rustup self uninstall -y  2>&1 | ForEach-Object { W $_ } }

# 3. winget packages
winget uninstall --id LLVM.LLVM --exact --accept-source-agreements --disable-interactivity 2>&1 | ForEach-Object { W $_ }
winget uninstall --id Microsoft.VisualStudio.2022.BuildTools --exact --accept-source-agreements --disable-interactivity 2>&1 | ForEach-Object { W $_ }
winget uninstall --id Microsoft.WindowsWDK.10.0.22621 --exact --accept-source-agreements --disable-interactivity 2>&1 | ForEach-Object { W $_ }
winget uninstall --id Microsoft.WindowsSDK.10.0.22621 --exact --accept-source-agreements --disable-interactivity 2>&1 | ForEach-Object { W $_ }

# 4. give the reader back to the vendor driver, then remove our packages.
#    Locale-independent (pnputil output is localized; Get-WindowsDriver is not).
Try-It {
  $drivers = Get-WindowsDriver -Online
  $vendor = $drivers | Where-Object { $_.OriginalFileName -like '*\rtsuer.inf' } | Select-Object -First 1
  if ($vendor) {
    W "reinstalling vendor driver $($vendor.Driver) ($($vendor.OriginalFileName))"
    & pnputil /add-driver $vendor.OriginalFileName /install /force 2>&1 | ForEach-Object { W $_ }
  } else {
    W 'vendor rtsuer.inf not in the DriverStore: install it from the cab first (docs/VENDOR-DRIVER.md)'
  }
  foreach ($d in ($drivers | Where-Object ProviderName -eq 'SoraCard')) {
    W "removing $($d.Driver)"
    & pnputil /delete-driver $d.Driver /uninstall /force 2>&1 | ForEach-Object { W $_ }
  }
} 'restore vendor driver / remove SoraCard packages'

# 5. driver parameters, scheduled tasks, test logs
Try-It { Remove-Item -Recurse -Force 'HKLM:\SYSTEM\CurrentControlSet\Services\SoraCard' -ErrorAction SilentlyContinue } 'remove SoraCard service key'
foreach ($t in 'SoraHpWatch', 'SoraCardStage') { schtasks /delete /f /tn $t 2>&1 | Out-Null }
Remove-Item -Force C:\Users\Public\hpwatch.log, C:\Users\Public\soracard_stage.log, C:\Users\Public\soracard_wtest.sha -ErrorAction SilentlyContinue

# 6. test signing off
Try-It { bcdedit /set testsigning off } 'testsigning off'

# 7. remove the test-signing certificate (CN=WDRLocalTestCert) from all stores
Try-It {
  Get-ChildItem Cert:\LocalMachine\My, Cert:\LocalMachine\Root, Cert:\LocalMachine\TrustedPublisher,
                Cert:\LocalMachine\WDRTestCertStore, Cert:\CurrentUser\WDRTestCertStore -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -eq 'CN=WDRLocalTestCert' } |
    Remove-Item -Force -DeleteKey -ErrorAction SilentlyContinue
} 'remove test cert'

# 8. NuGet WDK / SDK used by the build
Try-It { Remove-Item -Recurse -Force C:\packages -ErrorAction SilentlyContinue } 'remove C:\packages (NuGet WDK/SDK)'

W "=== teardown done; reboot for testsigning off to apply (verify: where.exe rustc / clang) ==="
