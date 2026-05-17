---
description: Kill the running app, do a debug build, and launch it
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

3. If the build succeeded, launch the debug binary:
   ```powershell
   Start-Process "target\debug\claude-code-usage-monitor.exe"
   ```
