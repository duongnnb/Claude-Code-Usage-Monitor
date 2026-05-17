# Claude Code Usage Monitor

A lightweight Windows taskbar widget that shows real-time Claude Code and Codex usage. Built in Rust using raw Win32 APIs — no framework, no runtime.

## Build & Run

```powershell
# Debug build and run
cargo build
.\target\debug\claude-code-usage-monitor.exe

# Release build
cargo build --release
.\target\release\claude-code-usage-monitor.exe

# Kill running instance before rebuilding
Stop-Process -Name "claude-code-usage-monitor" -Force -ErrorAction SilentlyContinue
```

## Release

Tag a version to trigger the GitHub Actions release workflow:

```powershell
git tag v1.x.x
git push && git push origin v1.x.x
```

The workflow builds the release binary on `windows-latest` and attaches the `.exe` to a GitHub Release automatically.

## Architecture

| File | Role |
|---|---|
| `main.rs` | Entry point — routes CLI args (updater helper mode vs. normal run) |
| `window.rs` | Core Win32 message loop, all UI drawing, state management |
| `tray_icon.rs` | System tray icon creation, badge rendering, tray message handling |
| `native_interop.rs` | Win32 helpers — taskbar finding, window embedding, time formatting, color utils |
| `poller.rs` | Background thread that reads Claude/Codex credentials and fetches usage from API |
| `models.rs` | Data types: `UsageSection`, `UsageData`, `AppUsageData` |
| `updater.rs` | Auto-update logic — GitHub release check, binary download, self-replace, WinGet support |
| `theme.rs` | Dark/light mode detection |
| `localization/` | Strings for each supported language |
| `diagnose.rs` | Optional file logging via `--diagnose` flag |
| `build.rs` | Embeds icon, version info, and `asInvoker` UAC manifest into the PE |

## Key Concepts

**Taskbar embedding** — the widget is a `WS_POPUP` layered window (`WS_EX_LAYERED`) reparented into `Shell_TrayWnd` via `SetParent`. This means standard tooltip controls don't work; custom `WS_POPUP` windows are used instead.

**Rendering** — all drawing is done with GDI into an off-screen DIB section, then composited onto the taskbar via `UpdateLayeredWindow` with per-pixel alpha.

**State** — a single `Mutex<Option<AppState>>` holds all runtime state. Lock it with `lock_state()`, drop the guard before calling anything that also locks (e.g. `show_hover_popup` → `hide_hover_popup`).

**DPI** — the app is Per-Monitor DPI Aware V2. Use `sc(n)` to scale any pixel value by the current DPI factor.

**Hover popup** — `WM_MOUSEMOVE` + `TrackMouseEvent(TME_LEAVE)` / `WM_MOUSELEAVE` on the widget window triggers a native-styled `WS_POPUP` tooltip above the widget showing reset times. `WM_MOUSELEAVE` is not in windows crate v0.58 — defined locally as `const WM_MOUSELEAVE: u32 = 0x02A3`.

**Timers**
| ID | Purpose |
|---|---|
| `TIMER_POLL` | Periodic usage fetch |
| `TIMER_COUNTDOWN` | Per-second countdown tick |
| `TIMER_RESET_POLL` | Detects when a usage window resets |
| `TIMER_UPDATE_CHECK` | 24-hour auto-update check |

## Settings

Saved to `%APPDATA%\ClaudeCodeUsageMonitor\settings.json`. Includes tray offset, poll interval, language, widget visibility, shown models, and last update check timestamp.

## Gotchas

- Kill the running process before rebuilding or you'll get "Access denied" on the `.exe`
- `WM_MOUSELEAVE` is missing from windows crate v0.58 — it's defined as a local const
- The helper exe for self-update must NOT have words like `update`, `install`, `setup` in its name or Windows UAC shim forces elevation (error 740). The `asInvoker` manifest in `build.rs` is the proper fix.
- `GetSysColorBrush` returns a system-owned brush — do NOT call `DeleteObject` on it
