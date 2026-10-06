$ErrorActionPreference='Continue'
$dir = Join-Path $env:TEMP 'soracard'; New-Item -ItemType Directory -Force -Path $dir | Out-Null
$log = Join-Path $dir 'state_check.txt'
Set-Content -LiteralPath $log -Value "=== state_check $(Get-Date -Format s) ==="
function W($m){ Add-Content -LiteralPath $log -Value $m }

$os = Get-CimInstance Win32_OperatingSystem
W ("OS: {0} build {1}" -f $os.Caption,$os.BuildNumber)
W ("Host: {0} User: {1}" -f $env:COMPUTERNAME,$env:USERNAME)
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
W ("Admin: $isAdmin")

W "--- SecureBoot ---"
try { W ("SecureBoot: " + (Confirm-SecureBootUEFI)) } catch { W ("SecureBoot: ERR " + $_.Exception.Message) }

W "--- bcdedit ---"
W ((bcdedit /enum) -join "`n")

W "--- Reader (VID_0BDA) ---"
$r = Get-PnpDevice | Where-Object { $_.InstanceId -like '*VID_0BDA*' }
if($r){ $r | ForEach-Object { W ("{0} | {1} | {2} | {3}" -f $_.Status,$_.Class,$_.FriendlyName,$_.InstanceId) } } else { W "NONE" }

W "--- Reader driver ---"
Get-CimInstance Win32_PnPSignedDriver | Where-Object { $_.DeviceID -like '*VID_0BDA*' } | ForEach-Object {
  W ("{0}`n  Inf={1} Ver={2} Signed={3} Provider={4}" -f $_.DeviceID,$_.InfName,$_.DriverVersion,$_.IsSigned,$_.DriverProviderName) }

W "--- RtsUer files ---"
Get-ChildItem 'C:\Windows\System32\drivers' -Filter 'Rts*' -ErrorAction SilentlyContinue | ForEach-Object { W ("  {0} {1}B {2}" -f $_.Name,$_.Length,$_.LastWriteTime) }
Get-ChildItem 'C:\Windows\INF\oem*.inf' -ErrorAction SilentlyContinue | ForEach-Object {
  $h = Select-String -LiteralPath $_.FullName -Pattern 'RtsUer|0BDA&0129' -ErrorAction SilentlyContinue
  if($h){ W ("  {0}: {1}" -f $_.Name, (($h | Select-Object -First 4 | ForEach-Object { $_.Line.Trim() }) -join ' ;; ')) } }

W "--- Service RTSUER ---"
W ((reg query HKLM\SYSTEM\CurrentControlSet\Services\RTSUER /s 2>&1) -join "`n")

W "--- Class key ---"
W ((reg query "HKLM\SYSTEM\CurrentControlSet\Control\Class\{36fc9e60-c465-11cf-8056-444553540000}" /s 2>&1) -join "`n")

W "--- PnP Enum ---"
W ((reg query "HKLM\SYSTEM\CurrentControlSet\Enum\USB\VID_0BDA&PID_0129" /s 2>&1) -join "`n")

W "--- Storage ---"
Get-PnpDevice -Class USBSTOR -ErrorAction SilentlyContinue | ForEach-Object { W ("USBSTOR: {0} | {1} | {2}" -f $_.Status,$_.FriendlyName,$_.InstanceId) }
Get-Disk -ErrorAction SilentlyContinue | ForEach-Object { W ("Disk {0}: {1} bus={2} size={3}GB" -f $_.Number,$_.FriendlyName,$_.BusType,[int]($_.Size/1GB)) }
Get-Volume -ErrorAction SilentlyContinue | ForEach-Object { W ("Vol {0}: {1} {2} {3}GB" -f $_.DriveLetter,$_.FileSystemLabel,$_.FileSystemType,[int]($_.Size/1GB)) }
Get-PnpDevice -Class WPD -ErrorAction SilentlyContinue | ForEach-Object { W ("WPD: {0} | {1}" -f $_.Status,$_.FriendlyName) }

W "=== end ==="
Write-Output "WROTE $log"
