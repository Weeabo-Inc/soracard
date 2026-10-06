param([string]$Set = '')
$root = Join-Path $env:TEMP 'soracard'
New-Item -ItemType Directory -Force -Path $root | Out-Null
$log  = Join-Path $root 'set_params.txt'
Set-Content -LiteralPath $log -Value "=== set_params $(Get-Date -Format s) set=[$Set] ==="
function W($m){ Add-Content -LiteralPath $log -Value $m; Write-Output $m }
$k = 'HKLM:\SYSTEM\CurrentControlSet\Services\RTSUER\UVSTOR'
$bak = Join-Path $root 'UVSTOR_before.reg'
if(-not (Test-Path $bak)){ reg export 'HKLM\SYSTEM\CurrentControlSet\Services\RTSUER\UVSTOR' $bak /y | Out-Null; W "backed up UVSTOR -> $bak" }
W "before:"; W ((Get-ItemProperty $k | Format-List | Out-String))
if($Set -ne ''){
  foreach($pair in $Set.Split(',')){
    $kv = $pair.Split('='); $n = $kv[0].Trim(); $v = [int]$kv[1].Trim()
    Set-ItemProperty -Path $k -Name $n -Value $v -Type DWord
    W "set $n = $v"
  }
}
W "after:"; W ((Get-ItemProperty $k | Format-List | Out-String))
$id = 'USB\VID_0BDA&PID_0129\20100201396000000'
W "--- restart-device ---"
W ((pnputil /restart-device $id 2>&1) -join "`n")
Start-Sleep -Seconds 7
$dev = Get-PnpDevice -InstanceId $id -ErrorAction SilentlyContinue
W ("reader present={0} problem={1}" -f $dev.Present,$dev.Problem)
Get-Disk -ErrorAction SilentlyContinue | Where-Object { $_.BusType -eq 'USB' } | ForEach-Object { W ("USB disk {0}: {1}" -f $_.Number,$_.FriendlyName) }
Get-Volume -ErrorAction SilentlyContinue | Where-Object { $_.DriveLetter } | ForEach-Object { W ("Vol {0}: {1}" -f $_.DriveLetter,$_.FileSystemLabel) }
W "=== end ==="
