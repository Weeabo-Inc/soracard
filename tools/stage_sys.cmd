@echo off
rem Swap the soracard.sys that the 'soracard' service loads (its ImagePath in
rem the DriverStore) for the fresh build. The old file is renamed, not deleted,
rem because it may still be mapped. Must run as SYSTEM: only SYSTEM may create
rem files in the DriverStore. Takes effect the next time the driver loads
rem (device restart or reboot). Log: C:\Users\Public\soracard_stage.log
setlocal
set LOG=C:\Users\Public\soracard_stage.log
rem Optional argument: build profile directory, debug (default) or release.
set PROFILE=%1
if "%PROFILE%"=="" set PROFILE=debug
set NEW=%~dp0..\storport\target\x86_64-pc-windows-msvc\%PROFILE%\soracard_package\soracard.sys
for /f "tokens=3" %%i in ('reg query HKLM\SYSTEM\CurrentControlSet\Services\soracard /v ImagePath ^| find "ImagePath"') do set IMG=%%i
rem ImagePath is "\SystemRoot\System32\..."; "\SystemRoot" is 11 characters.
set IMG=%SystemRoot%%IMG:~11%
echo %DATE% %TIME% image=%IMG% from=%NEW% > "%LOG%"
ren "%IMG%" soracard.old-%RANDOM% >> "%LOG%" 2>&1 || goto :eof
copy /Y "%NEW%" "%IMG%" >> "%LOG%" 2>&1
echo done >> "%LOG%"
