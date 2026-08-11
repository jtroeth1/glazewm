# GlazeWM - Claude Context

## Project

GlazeWM is a tiling window manager for macOS and Windows, written in Rust (nightly toolchain). This is a personal fork (`jtroeth1/glazewm`, branch `main`) with custom column layout features.

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

# Deploy: exit GlazeWM first (the running image is locked), then promote.
cp "/mnt/c/Program Files/glzr.io/GlazeWM/glazewm-jt.exe" \
   "/mnt/c/Program Files/glzr.io/GlazeWM/glazewm-jt-bak.exe"
cp target/x86_64-pc-windows-gnu/release/glazewm.exe \
   "/mnt/c/Program Files/glzr.io/GlazeWM/glazewm-jt.exe"

# Relaunch elevated.
powershell.exe -NoProfile -Command "schtasks /run /tn 'StartGlazeZsolt'"
```

- WSL can write to `C:\Program Files\glzr.io\GlazeWM\` directly — no elevation needed for the copy. But the running `.exe` is locked by Windows, so **GlazeWM must be exited before promoting**.
- The scheduled task that actually runs GlazeWM is `StartGlazeZsolt`, and its action is `glazewm-jt.exe` **directly** — it does *not* invoke `start-glazewm.ps1`. So the staging/auto-update path below is currently **inert**; staging `glazewm-new.exe` alone will not update anything.
- **Staging path (only works if the task is repointed at the script)**: `cp …/release/glazewm.exe /mnt/c/Users/jtroeth/.glzr/glazewm/glazewm-new.exe`. The binary must be named `glazewm-new.exe`. On launch `start-glazewm.ps1` backs up `glazewm-jt.exe` → `glazewm-jt-bak.exe`, promotes the staged build, removes the staging file, then launches. To enable it, change the task action to `powershell.exe -NoProfile -File "%USERPROFILE%\.glzr\glazewm\start-glazewm.ps1"`.
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
3. `WindowManager::new()` → `WmState::populate()` — `CloakJournal::recover()` first (see below), then enumerate monitors, then `visible_windows()` in reverse z-order, calling `manage_window()` for each, then `journal_managed_windows()`.
4. Register event listeners (window, display, mouse, keybinding).
5. Run startup commands (e.g. `shell-exec zebar`).
6. Enter `tokio::select!` event loop.

### Window Management Pipeline
- `visible_windows()` → `EnumWindows` + `is_visible()` filter (checks `IsWindowVisible` AND `DWMWA_CLOAKED`).
- `manage_window()` → `check_is_manageable()` filters: not visible → skip, `WS_CHILD`/`WS_EX_NOACTIVATE`/`WS_EX_TOOLWINDOW` → skip, owner without caption → skip. A `check_is_manageable` *error* is logged at `warn!` with the handle instead of being swallowed by `unwrap_or(None)`.
- Window rules (config `window_rules:`) run after management — can `ignore`, `set floating`, etc.
- `handle_window_shown` event catches windows that appear after startup.
- Diagnostic logging in `check_is_manageable` reports why each window is skipped (process, title, style flags).
- `discover_windows()` (5s tick) resolves the nearest monitor's displayed workspace as `target_parent` and logs each newly discovered window at `info!`.

### Cloak Recovery (`cloak_journal.rs`)
With `hide_method: 'cloak'`, windows on non-displayed workspaces are hidden via `IApplicationView::set_cloak`. A cloaked window is invisible to `visible_windows()` *and* to `discover_windows()`, and `show()`/`SW_SHOWNA` cannot reveal it — only uncloaking can. Previously nothing uncloaked on exit, so a crash orphaned those windows permanently: unmanaged, untileable, and (with title bars hidden) unclosable.

Fix has two layers:
1. **Uncloak on every exit path.** `WindowManager::cleanup` uncloaks all `state.windows()`; `WmState::drop` uncloaks before `show()` as an unwind-safe backstop; `wm-watcher` uncloaks its recorded handles. All idempotent.
2. **Journal for hard kills.** `~/.glzr/glazewm/cloaked-windows.txt` (config dir), one `<handle>\t<process_name>` line per managed window, rewritten in `populate` and on the 5s `cleanup_invalid_windows` tick. `recover()` runs before enumeration and deletes the file after.

Recovery is driven **only by journalled handles**, never by the cloak flag — `DWMWA_CLOAKED` returns `DWM_CLOAKED_SHELL` (2) for suspended UWP apps and windows on other virtual desktops too, so a blanket uncloak-all would drag those onto the screen. Each entry must additionally pass `is_valid()`, `is_cloaked() == true`, and a `process_name()` match against the journal before being uncloaked, which also guards against handle reuse.

### Container Tree
Root → Monitor(s) → Workspace(s) → SplitContainer(s)/TilingWindow(s)/NonTilingWindow(s). Focus tracked via `child_focus_order` deques. `set_focused_descendant()` propagates focus up the tree.

## Custom Feature: Column Layouts

### Overview
Declarative column layouts. Two invariants carry the whole feature:

1. **Window order is derived, never stored.** It is the layout read back
   row-major (`ColumnGrid::windows`), which is exactly the order
   `distribute_columns` deals windows into columns — so read and
   distribute are inverses and reapplying a layout is a no-op. There is no
   `window_order` buffer to drift out of sync with the tree.
2. **The center (`C`) column's occupant is explicit.** `Workspace::master_window: Option<Uuid>` names it. Never inferred from focus, z-order, or column widths.

Both replaced guess-based mechanisms that were the source of the
random master-window flipping: a `window_order`/`grid_affinity` pair that
drifted from the tree, and a `center_index()` that took the widest column
(a tie for `C,*` at `center: 0.5`, resolving to the *stack*).

### Key Files
- `packages/wm/src/commands/workspace/columns/mod.rs` — Commands: `apply_columns`, `apply_grid`, `reapply_assigned_columns`, `reapply_columns_for_new_window`, `reapply_columns_after_move`, `toggle_columns_mode`, `apply_rotate`, `apply_center`, `move_window_in_columns`, `focus_in_columns`. Internals: `ordered_windows`, `resolve_master`, `center_column`. 30 tests.
- `packages/wm/src/commands/workspace/columns/spec.rs` — Pure spec parsing (`parse_columns_spec`, `distribute_columns`, `row_major`, `column_widths`). No tree dependency. 10 tests.
- `packages/wm/src/commands/workspace/columns/grid.rs` — `ColumnGrid` bridge: reads container tree into flat grid, renders grid back to tree. Focus preservation across tree rebuilds.
- `packages/wm/src/models/workspace.rs` — `master_window` and `columns_mode` fields with accessors.

### Column Spec Syntax
Comma-separated tokens, left-to-right: `C` = center column (exactly one), `*` = flexible stack, number = fixed stack count. Examples: `C,*`, `*,C,*`, `2,1,C,3`.

Side windows are dealt **row-major, left to right**: one per non-center column, then a second row, etc. A fixed column drops out once full. This makes assignment prefix-stable (window *n* always lands in the same column) and invertible. There is deliberately **no bias knob** — dealing from anywhere but the leftmost column is not recoverable from the grid, so it would break idempotence.

### Layout Modes (`ColumnsMode` enum)
- `MasterStackLeft` (default): master window in `C` column, spec as configured.
- `MasterStackRight`: spec reversed (`C,*` → `*,C`). A symmetric spec (`*,C,*`) reverses to itself — its master is already between two stacks, so there is nothing to flip.
- `Grid`: Round-robin into equal columns (which *is* row-major, so also idempotent). Requires ≥4 windows; "armed" with fewer (mode stays Grid, layout falls back to master-stack-left, auto-applies when 4th window arrives).

Toggle cycle via `Alt+G`: Left → Grid → Right → Left.

### Master Window Lifecycle
- Set explicitly by `apply_center`, `apply_rotate`, and `move_window_in_columns` (any move into or out of the `C` column).
- Cleared when it leaves the workspace: `handle_window_destroyed.rs`, `handle_window_hidden.rs`, `move_window_in_direction.rs`, `move_window_to_workspace.rs`.
- `resolve_master` repairs a missing/stale designation by promoting the first window in on-screen order. This is the *only* implicit change.
- New windows: `reapply_columns_for_new_window(workspace, id, ...)` in `manage_window.rs` forces the new window to the end of the order, so it takes the next free slot and nothing else moves. Same mechanism for the cross-workspace target in `reapply_columns_after_move`.
- `focus_workspace.rs` reapplies unconditionally — safe because reapply is idempotent.

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
- GlazeWM cloak journal: `C:\Users\jtroeth\.glzr\glazewm\cloaked-windows.txt` (deleted on successful startup recovery)
- Zebar config: `C:\Users\jtroeth\.glzr\zebar\custom-bar\`
- Zebar settings: `C:\Users\jtroeth\.glzr\zebar\settings.json`

## Version Control

- GlazeWM fork: `github.com:jtroeth1/glazewm.git`, branch `main`.
- Config backup: `github.com:jtroeth1/jt.git` (main), under `config/glazewm/` and `config/zebar/`.

## Known Issues & Gotchas

- **Elevation required**: GlazeWM must run elevated to reposition windows. Non-elevated → "Access is denied" on every `SetWindowPos`. Task Scheduler runs it elevated in production.
- **Window styles**: Some apps (WSLg RAIL windows, Alacritty helper windows) have `WS_EX_TOOLWINDOW`/`WS_EX_NOACTIVATE` and are correctly skipped by `check_is_manageable`. Diagnostic logging shows skip reasons.
- **`ColumnGrid::render` focus corruption**: `move_container_within_tree` and `wrap_in_split_container` silently shift the focus chain during tree rebuilds. `grid.rs` saves/restores focused window ID across Phase 3 to fix this.
- **Config reload**: `default_columns` are re-resolved on every reapply (`effective_columns`), so moving a workspace to a different-aspect-ratio monitor picks up that monitor's rule.
- **Cloaking is not hiding**: `show()`/`SW_SHOWNA` cannot reveal a window cloaked via `IApplicationView::set_cloak` — only `set_cloaked(false)` can. Any new shutdown path must uncloak, or it orphans windows (see Cloak Recovery above).
- **`DWMWA_CLOAKED == 2` is ambiguous**: suspended UWP apps and windows on other virtual desktops report `DWM_CLOAKED_SHELL` just like GlazeWM-hidden windows. Never uncloak based on the flag alone; match against the journal.
