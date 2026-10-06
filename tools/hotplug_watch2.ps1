param([int]$Seconds = 200)
$root = Join-Path $env:TEMP 'soracard'
New-Item -ItemType Directory -Force -Path $root | Out-Null
$log  = Join-Path $root 'hotplug_watch2.txt'
Add-Content -LiteralPath $log -Value "=== watch2 start $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss.fff') for ${Seconds}s ==="
$id = 'USB\VID_0BDA&PID_0129\20100201396000000'
$end = (Get-Date).AddSeconds($Seconds)
$prev = $null
while ((Get-Date) -lt $end) {
    $t = (Get-Date).ToString('HH:mm:ss.fff')
    $dev = Get-PnpDevice -InstanceId $id -ErrorAction SilentlyContinue
    $lun = (Get-PnpDevice -Class DiskDrive -PresentOnly -ErrorAction SilentlyContinue |
            Where-Object { $_.InstanceId -like '*VEN_RSUER*' } | Measure-Object).Count
    $key = "reader=$($dev.Present) problem=$($dev.Problem) lun=$lun"
    if ($key -ne $prev) {
        Add-Content -LiteralPath $log -Value "$t  $key"
        $prev = $key
    }
    Start-Sleep -Milliseconds 700
}
Add-Content -LiteralPath $log -Value "=== watch2 end $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss.fff') ==="
Write-Output "done"
