---
description: Kill the running app, do a release build, and launch it
---

Follow these steps in order:

1. Kill any running instance:
   ```powershell
   Stop-Process -Name "claude-code-usage-monitor" -Force -ErrorAction SilentlyContinue
   ```

2. Build in release mode:
   ```powershell
   cargo build --release
   ```

3. If the build succeeded, launch the release binary:
   ```powershell
   Start-Process "target\release\claude-code-usage-monitor.exe"
   ```
