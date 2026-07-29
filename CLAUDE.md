# GlazeWM - Claude Context

## Project

GlazeWM is a tiling window manager for macOS and Windows, written in Rust (nightly toolchain). This is a personal fork (`jtroeth1/glazewm`, branch `personal-touches`) with custom column layout features.

Upstream: `glzr-io/glazewm`.

## Crate Structure

- **wm** (bin): Core window management logic — models, commands, events, IPC, platform sync. Entry point: `main.rs` → `start_wm()` → `WindowManager::new()` → event loop.
- **wm-cli** (bin, lib): CLI for IPC with the main application.
- **wm-common** (lib): Shared types (`AppCommand`, `WmEvent`, `ColumnsMode`, DTOs, IPC messages), utilities (`try_warn!` macro), and constants.
- **wm-platform** (lib): Platform-specific API wrappers. Other crates never call Windows/macOS APIs directly. Uses `crate::Error`/`crate::Result` (not `anyhow`).
- **wm-ipc-client** (lib): WebSocket client for IPC.
- **wm-watcher** (Windows-only, bin): Watchdog for cleanup on crash.
- **wm-macros** (lib): Derive macros.

## Build & Deploy

Cross-compile target: `x86_64-pc-windows-gnu` (MinGW). Cannot run tests natively on Linux (Windows API deps). Tests compile for Windows target.

```bash
# Build
cargo build --release --target x86_64-pc-windows-gnu

# Deploy — stage for auto-update on next GlazeWM restart
cp target/x86_64-pc-windows-gnu/release/glazewm.exe /mnt/c/Users/jtroeth/.glzr/glazewm/glazewm-new.exe
```

- The binary must be copied as `glazewm-new.exe` — never rename it.
- **Auto-update pipeline**: Task Scheduler runs `start-glazewm.ps1` (elevated). On launch it checks for `glazewm-new.exe`, backs up the current `glazewm-jt.exe` → `glazewm-jt-bak.exe` in `C:\Program Files\glzr.io\GlazeWM\`, promotes the staged build, removes the staging file, then launches GlazeWM. User just restarts GlazeWM to pick up the new binary.
- GlazeWM must run **elevated** (Task Scheduler) to reposition windows. Non-elevated instances get "Access is denied" on `SetWindowPos`/z-order calls.
- Linker configured in `.cargo/config.toml`: `x86_64-w64-mingw32-gcc`.

## Code Style

- **No `.unwrap()`**. Use `anyhow` in all crates except `wm-platform` (which uses `crate::Error`/`crate::Result`).
- **Logging**: `tracing` macros (`tracing::info!`, `tracing::warn!`, etc.). Logs go to stdout; `errors.log` captures ERROR level only.
- **Formatting**: `rustfmt.toml` — 2-space tabs, 75 char max width, crate-level import granularity.
- **Linting**: `clippy::all` + `clippy::pedantic` at warn level.
- **Comments**: All functions documented. Punctuation at end of all comments. Unsafe blocks get `// SAFETY: ...`. Type names in backticks.
- **Tests**: `#[cfg(test)]` modules. Unit tests for core functionality.

## Architecture: Startup & Window Management

### Startup Flow (`main.rs`)
1. `SingleInstance::new()` — mutex prevents duplicate instances.
2. `UserConfig::new()` — parse config.
3. `WindowManager::new()` → `WmState::populate()` — enumerate monitors, then `visible_windows()` in reverse z-order, calling `manage_window()` for each.
4. Register event listeners (window, display, mouse, keybinding).
5. Run startup commands (e.g. `shell-exec zebar`).
6. Enter `tokio::select!` event loop.

### Window Management Pipeline
- `visible_windows()` → `EnumWindows` + `is_visible()` filter (checks `IsWindowVisible` AND `DWMWA_CLOAKED`).
- `manage_window()` → `check_is_manageable()` filters: not visible → skip, `WS_CHILD`/`WS_EX_NOACTIVATE`/`WS_EX_TOOLWINDOW` → skip, owner without caption → skip.
- Window rules (config `window_rules:`) run after management — can `ignore`, `set floating`, etc.
- `handle_window_shown` event catches windows that appear after startup.
- Diagnostic logging in `check_is_manageable` reports why each window is skipped (process, title, style flags).

### Container Tree
Root → Monitor(s) → Workspace(s) → SplitContainer(s)/TilingWindow(s)/NonTilingWindow(s). Focus tracked via `child_focus_order` deques. `set_focused_descendant()` propagates focus up the tree.

## Custom Feature: Column Layouts

### Overview
Declarative column layouts driven by a `window_order: Vec<Uuid>` buffer on each workspace. The buffer (not tree position) determines window ordering.

### Key Files
- `packages/wm/src/commands/workspace/columns/mod.rs` — Main commands: `apply_columns`, `apply_grid`, `reapply_assigned_columns`, `toggle_columns_mode`, `apply_rotate`, `apply_center`, `move_window_in_columns`, `focus_in_columns`. 22 tests.
- `packages/wm/src/commands/workspace/columns/spec.rs` — Pure spec parsing (`parse_columns_spec`, `distribute_columns`, `column_widths`). No tree dependency.
- `packages/wm/src/commands/workspace/columns/grid.rs` — `ColumnGrid` bridge: reads container tree into flat grid, renders grid back to tree. Focus preservation across tree rebuilds.
- `packages/wm/src/models/workspace.rs` — `window_order` and `columns_mode` fields with accessors.

### Column Spec Syntax
Comma-separated tokens, left-to-right: `C` = center column (exactly one), `*` = even share of leftovers, number = fixed stack count. Examples: `C,*`, `*,C,*`, `2,1,C,3`.

### Layout Modes (`ColumnsMode` enum)
- `MasterStackLeft` (default): Center window (`window_order[0]`) in `C` column, spec as configured.
- `MasterStackRight`: Same but spec reversed (`C,*` → `*,C`).
- `Grid`: Round-robin into equal columns. Requires ≥4 windows; "armed" with fewer (mode stays Grid, layout falls back to master-stack-left, auto-applies when 4th window arrives).

Toggle cycle via `Alt+G`: Left → Grid → Right → Left.

### Window Order Buffer
- `window_order[0]` is always the center window. No special tracking needed.
- New windows: `push_window_order()` in `manage_window.rs`.
- Window close/hide: `remove_from_window_order()` in `handle_window_destroyed.rs`, `handle_window_hidden.rs`.
- Window moves between workspaces: remove from source, push to target in `move_window_in_direction.rs`, `move_window_to_workspace.rs`.

### IPC
- **Command**: `toggle-columns-mode` (dispatched as `InvokeCommand::ToggleColumnsMode`).
- **Query**: `query columns-mode` → `ClientResponseData::ColumnsMode(ColumnsModeData)`.
- **Event**: `ColumnsModeChanged` (subscribable via `sub -e columns_mode_changed`).
- **Serde**: `ColumnsMode` serializes as `master_stack_left`/`master_stack_right`/`grid` (snake_case). `ColumnsModeData` fields are camelCase (`columnsMode`, `workspace`).
- IPC messages are parsed via clap. Top-level subcommand for invoking is `command` (alias `c`), queries use `query`, subscriptions use `sub -e <event_name>`.

### Config (`config.yaml`)
```yaml
general:
  default_columns:
    - min_aspect_ratio: 2.1    # Ultrawide
      spec: '*,C,*'
      center: 0.5
    - min_aspect_ratio: 1.5    # Standard widescreen
      spec: 'C,*'
      center: 0.5
    - spec: default            # Narrower: normal tiling
```

## Zebar Integration

Custom Zebar widget pack at `/mnt/c/Users/jtroeth/.glzr/zebar/custom-bar/`.

### Files
- `with-glazewm.html` — React widget with direct WebSocket to GlazeWM IPC (`ws://localhost:6123`). Subscribes to `columns_mode_changed` and `focus_changed` events. Re-queries columns mode on focus change (workspace/monitor switch). Click uses Zebar provider's `runCommand()`.
- `styles.css` — Black background (`rgba(0 0 0 / 90%)`), `.columns-mode` button class.
- `zpack.json` — 30px height preset for `with-glazewm` widget.
- `settings.json` — Points to `custom-bar` pack.

### Icons
- `◧` = MasterStackLeft
- `◨` = MasterStackRight
- `⊞` = Grid

## Config Locations (Windows)

- GlazeWM config: `C:\Users\jtroeth\.glzr\glazewm\config.yaml`
- GlazeWM launcher: `C:\Users\jtroeth\.glzr\glazewm\start-glazewm.ps1`
- GlazeWM staging: `C:\Users\jtroeth\.glzr\glazewm\glazewm-new.exe` (consumed on next launch)
- GlazeWM binary: `C:\Program Files\glzr.io\GlazeWM\glazewm-jt.exe`
- GlazeWM backup: `C:\Program Files\glzr.io\GlazeWM\glazewm-jt-bak.exe`
- GlazeWM logs: `C:\Users\jtroeth\.glzr\glazewm\errors.log` (ERROR only), stdout for INFO+
- Zebar config: `C:\Users\jtroeth\.glzr\zebar\custom-bar\`
- Zebar settings: `C:\Users\jtroeth\.glzr\zebar\settings.json`

## Version Control

- GlazeWM fork: `github.com:jtroeth1/glazewm.git`, branch `personal-touches`.
- Config backup: `github.com:jtroeth1/jt.git` (main), under `config/glazewm/` and `config/zebar/`.

## Known Issues & Gotchas

- **Elevation required**: GlazeWM must run elevated to reposition windows. Non-elevated → "Access is denied" on every `SetWindowPos`. Task Scheduler runs it elevated in production.
- **Window styles**: Some apps (WSLg RAIL windows, Alacritty helper windows) have `WS_EX_TOOLWINDOW`/`WS_EX_NOACTIVATE` and are correctly skipped by `check_is_manageable`. Diagnostic logging shows skip reasons.
- **`ColumnGrid::render` focus corruption**: `move_container_within_tree` and `wrap_in_split_container` silently shift the focus chain during tree rebuilds. `grid.rs` saves/restores focused window ID across Phase 3 to fix this.
- **Config reload**: `default_columns` are re-resolved on every reapply (`effective_columns`), so moving a workspace to a different-aspect-ratio monitor picks up that monitor's rule.
