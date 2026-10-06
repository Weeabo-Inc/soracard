# Create the test-signing certificate wdk-build signs the driver with
# (CN=WDRLocalTestCert in the MACHINE store WDRTestCertStore) and trust it.
# Run once, elevated (an SSH session as an administrator is elevated).
# User key stores are not writable from SSH (NTE_PERM), hence machine stores.
$pfx = Join-Path $env:TEMP 'WDRLocalTestCert.pfx'
$pw  = 'SoraCard1'

$cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=WDRLocalTestCert' `
          -CertStoreLocation Cert:\LocalMachine\My -NotAfter (Get-Date).AddYears(10)
Write-Output ("created: " + $cert.Thumbprint + " haskey=" + $cert.HasPrivateKey)

Export-PfxCertificate -Cert $cert -FilePath $pfx -Password (ConvertTo-SecureString -String $pw -AsPlainText -Force) | Out-Null
certutil -f -p $pw -importpfx WDRTestCertStore $pfx 2>&1 | Select-Object -Last 2
Remove-Item $pfx

# Trust it so the test-signed package installs without prompts.
$cer = Join-Path $env:TEMP 'WDRLocalTestCert.cer'
Export-Certificate -Cert $cert -FilePath $cer -Force | Out-Null
Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
Remove-Item $cer
Write-Output 'done (test signing must also be on: bcdedit /set testsigning on, Secure Boot off)'
