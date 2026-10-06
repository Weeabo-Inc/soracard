# Install the freshly built package over the running reader (live, no
# reboot) with pnputil, then show the result. Needed whenever the INF
# changes; for code-only changes tools\redeploy.ps1 is faster.
# Settings in Parameters (AllowWrites, Arm, BusType, HighSpeed, InitSeq,
# Trace) are kept; trace values are cleared first so the dump only shows
# this run. -Release installs the optimized build.
param([int]$TimeoutSec = 80, [switch]$Release)
$ErrorActionPreference = 'Continue'
$prof = if ($Release) { 'release' } else { 'debug' }
$inf = Join-Path $PSScriptRoot "..\storport\target\x86_64-pc-windows-msvc\$prof\soracard_package\soracard.inf"
$id  = 'USB\VID_0BDA&PID_0129\20100201396000000'
$par = 'HKLM:\SYSTEM\CurrentControlSet\Services\SoraCard\Parameters'
$keep = 'AllowWrites', 'Arm', 'BusType', 'HighSpeed', 'InitSeq', 'Trace'

if (-not (Test-Path $par)) { New-Item $par -Force | Out-Null }
(Get-Item $par).Property | Where-Object { $_ -notin $keep } | ForEach-Object { Remove-ItemProperty $par -Name $_ }

$t = Get-Date
$job = Start-Job { param($f) pnputil /add-driver $f /install } -ArgumentList $inf
if (Wait-Job $job -Timeout $TimeoutSec) {
    Receive-Job $job | Select-String 'publicado|instalado|Published|installed|error|Error'
    'install took {0:N1}s' -f ((Get-Date) - $t).TotalSeconds
} else {
    "INSTALL STILL RUNNING after ${TimeoutSec}s - do not start another PnP operation"
}
Start-Sleep -Seconds 5
& (Join-Path $PSScriptRoot 'sp_state.ps1')
