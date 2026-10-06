# =============================================================================
# SoraCard test driver - TOOLCHAIN INSTALL (idempotent)
#   Rust (rustup) + LLVM 17.0.6 + VS Build Tools (VCTools) + Windows SDK 22621
#   + WDK 22621 + cargo-wdk + cargo-make
# Everything here is removable with teardown.ps1.
# Run elevated: powershell -NoProfile -ExecutionPolicy Bypass -File install_toolchain.ps1
# =============================================================================
$ErrorActionPreference = 'Continue'
$root = Join-Path $env:TEMP 'soracard'
New-Item -ItemType Directory -Force -Path $root | Out-Null
$log  = Join-Path $root 'install_toolchain.txt'
Set-Content -LiteralPath $log -Value "=== install_toolchain $(Get-Date -Format s) ==="
function W($m){ Add-Content -LiteralPath $log -Value $m; Write-Output $m }
function Step($name, [scriptblock]$b){
  W "----- $name -----"
  try { & $b 2>&1 | ForEach-Object { W $_ } } catch { W "FAIL:$($_.Exception.Message)" }
}
$WG = @('--accept-source-agreements','--accept-package-agreements','--disable-interactivity','--source','winget')
$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'

# --- 1. Rust ---------------------------------------------------------------
Step 'rust' {
  if(-not (Test-Path $cargo)){
    $init = Join-Path $env:TEMP 'rustup-init.exe'
    Invoke-RestMethod -Uri 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe' -OutFile $init
    & $init -y --default-toolchain stable --profile minimal --no-modify-path
  }
  & $cargo --version
}

# --- 2. LLVM 17.0.6 (libclang for bindgen) ---------------------------------
Step 'llvm' { winget install --id LLVM.LLVM --version 17.0.6 --exact @WG }

# --- 3. VS Build Tools 2022 (cl.exe / link.exe) ----------------------------
Step 'vs-buildtools' {
  winget install --id Microsoft.VisualStudio.2022.BuildTools --exact @WG `
    --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
}

# --- 4. Windows SDK 10.0.22621 ---------------------------------------------
Step 'win-sdk' { winget install --id Microsoft.WindowsSDK.10.0.22621 --exact @WG }

# --- 5. WDK 10.0.22621 -----------------------------------------------------
Step 'wdk' { winget install --id Microsoft.WindowsWDK.10.0.22621 --exact @WG }

# --- 6. cargo tools --------------------------------------------------------
Step 'cargo-wdk'  { & $cargo install cargo-wdk --locked }
Step 'cargo-make' { & $cargo install cargo-make --no-default-features --features tls-native --locked }

# --- verification ----------------------------------------------------------
W "----- verify -----"
W "rustc : $( (& $cargo --version) 2>&1 )"
$clang = @('C:\Program Files\LLVM\bin\clang.exe','clang.exe') | Where-Object { Test-Path $_ -ErrorAction SilentlyContinue } | Select-Object -First 1
W "clang : $((& clang --version 2>&1 | Select-Object -First 1))"
W "SDK include exists : $(Test-Path 'C:\Program Files (x86)\Windows Kits\10\Include')"
W "cargo-wdk : $(Test-Path (Join-Path $env:USERPROFILE '.cargo\bin\cargo-wdk.exe'))"
W "=== install_toolchain done ==="
