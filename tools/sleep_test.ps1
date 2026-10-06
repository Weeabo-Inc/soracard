# Put the machine into S3 sleep after a delay (run it as a SYSTEM scheduled
# task over SSH, see docs/TESTING.md). Wake it with the power button; the log
# records when it slept and resumed. Log: C:\Users\Public\sleeptest.log
param([int]$DelaySec = 20)
Start-Sleep -Seconds $DelaySec
Add-Type -AssemblyName System.Windows.Forms
"sleep requested $(Get-Date -f HH:mm:ss)" | Out-File C:\Users\Public\sleeptest.log -Encoding ascii
[void][System.Windows.Forms.Application]::SetSuspendState([System.Windows.Forms.PowerState]::Suspend, $false, $false)
"resumed $(Get-Date -f HH:mm:ss)" | Out-File C:\Users\Public\sleeptest.log -Append -Encoding ascii
