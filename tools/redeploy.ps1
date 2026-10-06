# Fast redeploy after tools\build_storport.ps1 -Package [-Release] (pass the
# same -Release here), without a new
# pnputil package: stage the fresh soracard.sys into the bound DriverStore
# folder (as SYSTEM, via a scheduled task), restart the reader, and show the
# result. Run elevated.
#
# Requires a healthy running driver: if its worker thread is wedged, the
# device restart hangs. In that case stage only (-StageOnly) and reboot.
param([switch]$StageOnly, [switch]$Release)
$ErrorActionPreference = 'Continue'
$id  = 'USB\VID_0BDA&PID_0129\20100201396000000'
$par = 'HKLM:\SYSTEM\CurrentControlSet\Services\SoraCard\Parameters'
$cmd = Join-Path $PSScriptRoot 'stage_sys.cmd'

$prof = if ($Release) { 'release' } else { 'debug' }
schtasks /create /f /tn SoraCardStage /ru SYSTEM /sc once /st 23:59 /tr "$cmd $prof" | Out-Null
schtasks /run /tn SoraCardStage | Out-Null
Start-Sleep -Seconds 4
schtasks /delete /f /tn SoraCardStage | Out-Null
Get-Content C:\Users\Public\soracard_stage.log
if ($StageOnly) { 'staged; reboot to load it'; return }

# Arm is left alone: absent means armed. (To test a risky build, set Arm=1
# by hand first: it arms one start only, so a crash cannot loop.)
pnputil /restart-device $id
Start-Sleep -Seconds 10
& (Join-Path $PSScriptRoot 'sp_state.ps1')
