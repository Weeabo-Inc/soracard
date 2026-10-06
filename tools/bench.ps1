# Throughput benchmark with the driver's per-phase timing (Perf* trace values).
#   bench.ps1            read 256 MB raw (4 MB requests) + write 128 MB file (write-through)
#   bench.ps1 -ReadOnly  skip the write (creates and deletes E:\soracard_bench.bin otherwise)
param([switch]$ReadOnly, [string]$Drive = 'E', [int]$OffsetMB = 50000)
$p = 'HKLM:\SYSTEM\CurrentControlSet\Services\SoraCard\Parameters'
$keys = 'PerfXfers', 'PerfBlocks', 'PerfCmdMs', 'PerfSetupMs', 'PerfDataMs', 'PerfStatusMs', 'PerfStopMs'
function Delta($before) {
  Start-Sleep 1
  $a = Get-ItemProperty $p
  # Perf* are flushed every 256 commands, so deltas cover most, not all, of the run.
  ($keys | % { '{0}+{1}' -f $_.Substring(4), ($a.$_ - $before.$_) }) -join ' '
}
$d = (Get-Disk | ? FriendlyName -eq 'SoraCard SD Card Reader').Number
'speed mode (SdSpeed): ' + (Get-ItemProperty $p).SdSpeed

$b = Get-ItemProperty $p
$fs = [IO.File]::Open("\\.\PhysicalDrive$d", 'Open', 'Read', 'ReadWrite')
$fs.Position = [int64]$OffsetMB * 1MB
$buf = New-Object byte[] (4MB)
$sw = [Diagnostics.Stopwatch]::StartNew()
for ($i = 0; $i -lt 64; $i++) { [void]$fs.Read($buf, 0, $buf.Length) }
$sw.Stop(); $fs.Close()
'read  256 MB: {0:N1} MB/s' -f (256 / $sw.Elapsed.TotalSeconds)
'  ' + (Delta $b)

if (-not $ReadOnly) {
  $b = Get-ItemProperty $p
  (New-Object Random 7).NextBytes($buf)
  $f = "${Drive}:\soracard_bench.bin"
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $ws = [IO.FileStream]::new($f, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None, 4194304, [IO.FileOptions]::WriteThrough)
  for ($i = 0; $i -lt 32; $i++) { $ws.Write($buf, 0, $buf.Length) }
  $ws.Flush($true); $ws.Close(); $sw.Stop()
  'write 128 MB: {0:N1} MB/s (write-through)' -f (128 / $sw.Elapsed.TotalSeconds)
  '  ' + (Delta $b)
  Remove-Item $f
}
