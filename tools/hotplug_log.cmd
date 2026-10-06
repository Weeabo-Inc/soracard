@rem Wrapper so a scheduled task can start the logger without quoting trouble.
@powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0hotplug_log.ps1"
