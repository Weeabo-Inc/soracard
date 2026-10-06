# Hotplug logger: once a second, record a line whenever the reader state
# changes - the driver's CardPresent/SdInit, the SoraCard disk, and whether
# the card's volume is mounted. Log: C:\Users\Public\hpwatch.log
# Run as SYSTEM at boot (see docs/TESTING.md) or interactively.
param([string]$Drive = 'E', [int]$IntervalMs = 1000)
$log = 'C:\Users\Public\hpwatch.log'
$par = 'HKLM:\SYSTEM\CurrentControlSet\Services\SoraCard\Parameters'
"start $(Get-Date -f 'yyyy-MM-dd HH:mm:ss')" | Out-File $log -Encoding ascii
$prev = ''
while ($true) {
  $p = Get-ItemProperty $par -ErrorAction SilentlyContinue
  $d = Get-CimInstance -Namespace root\Microsoft\Windows\Storage MSFT_Disk -Filter "FriendlyName='SoraCard SD Card Reader'" -ErrorAction SilentlyContinue
  $sz = if ($d) { '{0}GB' -f [math]::Round($d.Size / 1GB, 1) } else { 'nodisk' }
  $vol = if (Get-Volume -DriveLetter $Drive -ErrorAction SilentlyContinue | Where-Object Size -gt 0) { "${Drive}:mounted" } else { "${Drive}:-" }
  $s = "cardPresent=$($p.CardPresent) sdInit=$('{0:X}' -f $p.SdInit) disk=$sz $vol inserts=$($p.Inserts) removals=$($p.Removals)"
  if ($s -ne $prev) { "$(Get-Date -f 'HH:mm:ss') $s" | Out-File $log -Append -Encoding ascii; $prev = $s }
  Start-Sleep -Milliseconds $IntervalMs
}
