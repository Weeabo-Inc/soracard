# Dump reader/disk state and the SoraCard StorPort diagnostics.
$id  = 'USB\VID_0BDA&PID_0129\20100201396000000'
$par = 'HKLM\SYSTEM\CurrentControlSet\Services\SoraCard\Parameters'
Write-Output "--- reader ---"
Get-PnpDevice -InstanceId $id | Format-List Status, Problem, Class, FriendlyName | Out-String -Width 200 | Write-Output
Write-Output "--- disks/volumes ---"
Get-Disk | Format-Table Number, FriendlyName, BusType, Size, PartitionStyle, OperationalStatus, IsReadOnly -Auto | Out-String -Width 200 | Write-Output
Get-Volume | Where-Object DriveLetter | Format-Table DriveLetter, FileSystemLabel, FileSystem, Size, HealthStatus -Auto | Out-String -Width 200 | Write-Output
Write-Output "--- diagnostics ---"
reg query $par
Write-Output "--- recent errors (10 min) ---"
Get-WinEvent -FilterHashtable @{LogName='System'; Level=1,2,3; StartTime=(Get-Date).AddMinutes(-10)} -MaxEvents 15 -ErrorAction SilentlyContinue |
  ForEach-Object { "{0} {1} id={2} {3}" -f $_.TimeCreated, $_.ProviderName, $_.Id, (($_.Message -replace '\s+',' ') -replace '^(.{160}).*','$1') }
