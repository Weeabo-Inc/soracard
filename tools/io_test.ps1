# Read/write verification on the card's volume and raw disk.
#   io_test.ps1 -Read          256 MB raw reads at two offsets (twice at 0, hashes must match)
#   io_test.ps1 -Write         write a 32 MB random file + a text file (write-through)
#   io_test.ps1 -Verify        after a reader restart: hash the file read back from the card, chkdsk (read-only)
# -Write needs a writable card (AllowWrites not 0, lock switch off). Only
# creates soracard_wtest.* files.
param([switch]$Read, [switch]$Write, [switch]$Verify, [string]$Drive = 'E', [int]$Disk = -1)
$f = "${Drive}:\soracard_wtest.bin"; $t = "${Drive}:\soracard_wtest.txt"; $h = 'C:\Users\Public\soracard_wtest.sha'
if ($Disk -lt 0) { $Disk = (Get-Disk | Where-Object FriendlyName -eq 'SoraCard SD Card Reader').Number }

function ReadRaw($offMB, $mb = 256) {
  $fs = [IO.File]::Open("\\.\PhysicalDrive$Disk", 'Open', 'Read', 'ReadWrite')
  $fs.Position = [int64]$offMB * 1MB
  $buf = New-Object byte[] (1MB); $sha = [Security.Cryptography.SHA256]::Create()
  $sw = [Diagnostics.Stopwatch]::StartNew()
  for ($i = 0; $i -lt $mb; $i++) {
    $n = $fs.Read($buf, 0, $buf.Length)
    if ($n -ne $buf.Length) { "short read at $i MB: $n"; break }
    [void]$sha.TransformBlock($buf, 0, $n, $null, 0)
  }
  [void]$sha.TransformFinalBlock($buf, 0, 0); $sw.Stop(); $fs.Close()
  '{0} MB @ {1} MB: {2:N1} MB/s sha={3}' -f $mb, $offMB, ($mb / $sw.Elapsed.TotalSeconds), ([BitConverter]::ToString($sha.Hash) -replace '-', '').Substring(0, 16)
}

if ($Read) { ReadRaw 0; ReadRaw 0; ReadRaw 100000 }
if ($Write) {
  $buf = New-Object byte[] (32MB); (New-Object Random 1234).NextBytes($buf)
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $fs = [IO.FileStream]::new($f, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None, 1048576, [IO.FileOptions]::WriteThrough)
  $fs.Write($buf, 0, $buf.Length); $fs.Flush($true); $fs.Close(); $sw.Stop()
  'wrote 32 MB in {0:N1}s = {1:N1} MB/s' -f $sw.Elapsed.TotalSeconds, (32 / $sw.Elapsed.TotalSeconds)
  "SoraCard write test $(Get-Date -f s)" | Set-Content $t
  ([BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($buf)) -replace '-', '') | Set-Content $h
  'now restart the reader (or re-insert the card) and run -Verify'
}
if ($Verify) {
  $exp = Get-Content $h; $got = (Get-FileHash $f -Algorithm SHA256).Hash
  "expected  $exp"; "from card $got"; if ($exp -eq $got) { 'MATCH' } else { 'MISMATCH' }
  Get-Content $t
  chkdsk "${Drive}:" | Select-Object -Last 12
}
