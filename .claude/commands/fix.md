---
description: Kill the running app, debug build, launch with --diagnose, then show the log
---

Follow these steps in order:

1. Kill any running instance:
   ```powershell
   Stop-Process -Name "claude-code-usage-monitor" -Force -ErrorAction SilentlyContinue
   ```

2. Build in debug mode:
   ```powershell
   cargo build
   ```

3. If the build succeeded, launch with the diagnose flag:
   ```powershell
   Start-Process "target\debug\claude-code-usage-monitor.exe" -ArgumentList "--diagnose"
   ```

4. Wait 2 seconds, then read and display the log:
   ```powershell
   Start-Sleep -Seconds 2
   Get-Content "$env:TEMP\claude-code-usage-monitor.log"
   ```
