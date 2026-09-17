//! The `columns` feature: declarative, centered-focus column layouts.
//!
//! Layout is split across three concerns:
//! - [`spec`]: pure parsing of a column `spec` and distribution of windows
//!   into columns (no tree dependency, unit-tested).
//! - [`grid`]: the [`ColumnGrid`] bridge that lifts the container tree
//!   into a flat grid and renders it back.
//! - this module: the commands and config resolution that drive the two.

mod grid;
mod spec;

use anyhow::Context;
use uuid::Uuid;
use wm_common::{ColumnLayout, ColumnsMode};
use wm_platform::Direction;

use self::{
  grid::ColumnGrid,
  spec::{column_widths, distribute_columns, parse_columns_spec},
};
use crate::{
  commands::{
    container::focus_container_by_id,
    window::move_to_workspace_in_direction,
  },
  models::{Container, TilingWindow, WindowContainer, Workspace},
  traits::{CommonGetters, PositionGetters, TilingSizeGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Arranges the workspace into a centered-focus layout described by a
/// comma-separated column `spec`, laid out left-to-right. Each token is
/// one column: a number is that many windows stacked, `*` claims an even
/// share of the leftover windows, and `C` is the wide center (exactly
/// one). E.g. `*,C,*` is a center flanked by two even stacks, `C,*` drops
/// the left band for a narrow monitor, and `2,1,C,3` is fully explicit.
///
/// The `C` column holds the workspace's master window at `center` width
/// (a fraction of the workspace, clamped to `0.1..=0.9`); the remaining
/// columns split the rest of the width evenly. Columns that end up empty
/// are dropped and the widths renormalise, so a wide spec still degrades
/// cleanly on a small monitor.
///
/// Nothing here is guessed. The `C` column takes the workspace's
/// designated master (see [`resolve_master`]) and the other windows keep
/// the canonical order read back out of the container tree, so applying
/// a layout twice is a no-op and no window changes column unless a
/// command deliberately moves it.
pub fn apply_columns(
  workspace: &Workspace,
  spec: &str,
  center: f32,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  apply_spec(workspace, spec, center, None, state, config)
}

/// Applies a column `spec`, optionally forcing `last` to the end of the
/// workspace's window order.
///
/// `last` is the window that has just been added to the workspace.
/// Moving it to the end of the order means it takes the next free slot
/// and every window already on screen keeps its column and its row.
fn apply_spec(
  workspace: &Workspace,
  spec: &str,
  center: f32,
  last: Option<Uuid>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let kinds = parse_columns_spec(spec)?;
  let windows = ordered_windows(workspace, last);

  if windows.len() < 2 {
    return Ok(());
  }

  let Some(master) = resolve_master(workspace, &windows) else {
    return Ok(());
  };

  let rest = windows
    .into_iter()
    .filter(|window| window.id() != master.id())
    .collect::<Vec<_>>();

  let columns = distribute_columns(&kinds, master, rest);
  let widths = column_widths(&kinds, center.clamp(0.1, 0.9));

  ColumnGrid { columns, widths }.render(workspace, state, config)
}

/// Arranges the workspace into an equal-width grid, dealing windows
/// round-robin across the columns in canonical order.
///
/// Because the order is the container tree's reading order and a new
/// window is forced to the end of it, dealing round-robin drops the new
/// window at the bottom of the next column and leaves every other window
/// exactly where it was. The previous "grid affinity" fixup instead
/// swapped the newest window into the focused window's column, which
/// reshuffled the grid differently depending on what happened to be
/// focused.
///
/// Requires at least 4 tiling windows; the caller falls back to
/// master-stack when fewer are present.
fn apply_grid(
  workspace: &Workspace,
  num_columns: usize,
  last: Option<Uuid>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let windows = ordered_windows(workspace, last);

  if windows.len() < 4 || num_columns == 0 {
    return Ok(());
  }

  let mut columns: Vec<Vec<TilingWindow>> =
    (0..num_columns).map(|_| Vec::new()).collect();

  for (index, window) in windows.into_iter().enumerate() {
    columns[index % num_columns].push(window);
  }

  #[allow(clippy::cast_precision_loss)]
  let width = 1.0 / num_columns as f32;

  ColumnGrid {
    columns,
    widths: vec![width; num_columns],
  }
  .render(workspace, state, config)
}

/// Assigns a column layout to the workspace and applies it immediately.
/// The assignment is stored on the workspace and reapplied whenever the
/// workspace is focused (see `focus_workspace`), until it is unassigned or
/// the config is reloaded. Runtime assignments are ephemeral; edit
/// `config.yaml` to persist.
pub fn assign_columns(
  workspace: &Workspace,
  spec: &str,
  center: f32,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let mut workspace_config = workspace.config();
  workspace_config.columns = Some(ColumnLayout {
    spec: spec.to_string(),
    center,
  });
  workspace.set_config(workspace_config);

  apply_columns(workspace, spec, center, state, config)
}

/// Clears any column layout assigned to the workspace, so switching to it
/// no longer reapplies a layout. The current arrangement is left
/// untouched.
pub fn unassign_columns(workspace: &Workspace) {
  let mut workspace_config = workspace.config();
  workspace_config.columns = None;
  workspace.set_config(workspace_config);
}

/// Resolves the columns a workspace should currently use: its own assigned
/// `columns` if set, otherwise the first `general.default_columns` rule
/// whose aspect-ratio band contains the workspace's current monitor's
/// aspect ratio (`width / height`).
///
/// Returns `None` when the workspace has no assignment and matches no
/// default rule (or matches an explicit `default`/`none` rule), in which
/// case the workspace keeps the default tiling. Re-resolved on every
/// reapply, so a workspace moved to a differently-shaped monitor picks up
/// that monitor's default.
pub fn effective_columns(
  workspace: &Workspace,
  config: &UserConfig,
) -> anyhow::Result<Option<ColumnLayout>> {
  if let Some(columns) = workspace.config().columns {
    return Ok(Some(columns));
  }

  let Some(default_columns) = &config.value.general.default_columns else {
    return Ok(None);
  };

  let monitor_rect =
    workspace.monitor().context("No monitor.")?.to_rect()?;
  #[allow(clippy::cast_precision_loss)]
  let aspect_ratio =
    monitor_rect.width() as f32 / monitor_rect.height() as f32;

  Ok(default_columns.columns_for(aspect_ratio))
}

/// Reapplies the workspace's effective columns. In master-stack mode the
/// master window occupies the `C` column; in grid mode windows are dealt
/// round-robin. A no-op when nothing resolves or the workspace has fewer
/// than two tiling windows.
///
/// The `C` column is sized from the columns' stored `center`, which a
/// manual resize updates at runtime (see [`store_center_width`]). Reading
/// from the stored template keeps the center width stable across
/// reapplies.
pub fn reapply_assigned_columns(
  workspace: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  reapply_columns(workspace, None, state, config)
}

/// Reapplies the workspace's effective columns after `appended` has just
/// been added to it.
///
/// The new window is placed last in the workspace's window order, so it
/// takes the next free slot and no window already on screen moves.
pub fn reapply_columns_for_new_window(
  workspace: &Workspace,
  appended: Uuid,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  reapply_columns(workspace, Some(appended), state, config)
}

/// Reapplies the workspace's effective columns for its current mode.
fn reapply_columns(
  workspace: &Workspace,
  last: Option<Uuid>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let Some(columns) = effective_columns(workspace, config)? else {
    return Ok(());
  };

  let mode = workspace.columns_mode();

  // Grid needs >= 4 windows. While armed with fewer, the layout falls
  // back to master-stack; the mode stays `Grid` so the next window
  // addition applies the grid once the threshold is met.
  if mode == ColumnsMode::Grid
    && ColumnGrid::read(workspace).window_count() >= 4
  {
    return apply_grid(workspace, 2, last, state, config);
  }

  // Master-stack-right is the mirror image, expressed by reversing the
  // spec: `C,*` becomes `*,C`. A spec that is already symmetric (`*,C,*`)
  // reverses to itself, and rightly so — its master sits between two
  // stacks, so there is no left or right to flip.
  let spec = if mode == ColumnsMode::MasterStackRight {
    reverse_spec(&columns.spec)
  } else {
    columns.spec.clone()
  };

  apply_spec(workspace, &spec, columns.center, last, state, config)
}

/// Reapplies assigned columns after the tiling window `moved` travels
/// from `source` to `target`.
///
/// `moved` takes the last slot in `target`'s layout, so it lands in the
/// next free position and no window already on `target` changes column.
/// A no-op for either workspace that has no effective columns.
pub fn reapply_columns_after_move(
  source: &Workspace,
  target: &Workspace,
  moved: Uuid,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  reapply_assigned_columns(source, state, config)?;
  reapply_columns_for_new_window(target, moved, state, config)?;
  Ok(())
}

/// The window that occupies the `C` column in master-stack mode, if the
/// workspace has one.
///
/// Repairs a designation that names a window which has left the
/// workspace, so the returned id is always a live tiling window.
pub fn workspace_center_window_id(workspace: &Workspace) -> Option<Uuid> {
  let windows = ordered_windows(workspace, None);
  resolve_master(workspace, &windows).map(|master| master.id())
}

/// Index of the column holding the workspace's master window — the
/// layout's center column.
///
/// Deliberately *not* the widest column: the widths tie for the common
/// `C,*` layout at a `center` of 0.5, and a tie resolved to the last
/// maximum picked the stack column, so `center`, `rotate` and moves into
/// the center all operated on the wrong column.
fn center_column(
  grid: &ColumnGrid,
  workspace: &Workspace,
) -> Option<usize> {
  let master = workspace_center_window_id(workspace)?;
  grid.find(master).map(|(column, _)| column)
}

/// Current width fraction of the workspace's center column when it has
/// at least two tiling windows.
///
/// Returns `None` for a workspace with fewer than two tiling windows,
/// where no meaningful center width exists.
fn workspace_center_width(workspace: &Workspace) -> Option<f32> {
  let grid = ColumnGrid::read(workspace);

  if grid.window_count() < 2 {
    return None;
  }

  let column = center_column(&grid, workspace)?;
  grid.widths.get(column).copied()
}

/// Records the workspace's current center-column width into its assigned
/// columns, so a manual resize of the center becomes the template width
/// used by later reapplies (window add/remove, switch-in). Call this after
/// a user resize; the runtime value lives on the workspace config and is
/// reset to the spec by config reload or restart.
///
/// A no-op when the workspace has no assigned columns or no laid-out
/// center column (fewer than two tiling windows).
pub fn store_center_width(workspace: &Workspace) {
  let mut config = workspace.config();
  let Some(columns) = config.columns.as_mut() else {
    return;
  };

  let Some(width) = workspace_center_width(workspace) else {
    return;
  };

  columns.center = width;
  workspace.set_config(config);
}

/// Reverses a comma-separated column spec so the center column moves to
/// the opposite side (e.g. `C,*` → `*,C`, `*,C,*` stays symmetric).
fn reverse_spec(spec: &str) -> String {
  spec.split(',').rev().collect::<Vec<_>>().join(",")
}

/// Id of the focused tiling window on the workspace, if any.
fn focused_window_id(workspace: &Workspace) -> Option<Uuid> {
  workspace.descendant_focus_order().find_map(
    |container| match container {
      Container::TilingWindow(window) => Some(window.id()),
      _ => None,
    },
  )
}

/// The workspace's tiling windows in canonical order, optionally forcing
/// `last` to the end of it.
///
/// Canonical order is the container tree's reading order (see
/// [`ColumnGrid::windows`]). It is derived on every read, never stored.
/// The previous design kept a parallel `window_order` buffer on the
/// workspace, which drifted out of sync in both directions: a window that
/// entered tiling by another route was missing from the buffer and so
/// dropped out of the layout entirely, and every manual rearrangement
/// (`center`, `rotate`, a directional move) was silently reverted the
/// next time the buffer was replayed — which is what made a new window
/// yank the master back to some older window.
fn ordered_windows(
  workspace: &Workspace,
  last: Option<Uuid>,
) -> Vec<TilingWindow> {
  let mut windows = ColumnGrid::read(workspace).windows();

  if let Some(last) = last {
    if let Some(index) =
      windows.iter().position(|window| window.id() == last)
    {
      let window = windows.remove(index);
      windows.push(window);
    }
  }

  windows
}

/// The workspace's master window — the occupant of the `C` column.
///
/// Resolution is deterministic and never consults focus, z-order or
/// recency:
/// 1. the explicitly designated master, when it is still one of `windows`;
/// 2. otherwise the first window in canonical order, which is then
///    recorded as the designated master.
///
/// Case 2 fires only when the designation is missing or names a window
/// that has left the workspace, so closing the master promotes the
/// top-left window and nothing else moves.
fn resolve_master(
  workspace: &Workspace,
  windows: &[TilingWindow],
) -> Option<TilingWindow> {
  let designated = workspace
    .master_window()
    .and_then(|id| windows.iter().find(|window| window.id() == id));

  if let Some(master) = designated {
    return Some(master.clone());
  }

  let master = windows.first()?.clone();
  workspace.set_master_window(Some(master.id()));

  Some(master)
}

/// Toggles the workspace's column layout mode through the cycle
/// `MasterStackLeft → Grid → MasterStackRight → MasterStackLeft`,
/// then reapplies. Grid mode requires ≥ 4 windows and auto-falls
/// back to master-stack-left when that threshold isn't met.
pub fn toggle_columns_mode(
  workspace: &Workspace,
  forced: Option<ColumnsMode>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let new_mode =
    forced.unwrap_or_else(|| match workspace.columns_mode() {
      ColumnsMode::MasterStackLeft => ColumnsMode::Grid,
      ColumnsMode::Grid => ColumnsMode::MasterStackRight,
      ColumnsMode::MasterStackRight => ColumnsMode::MasterStackLeft,
    });

  workspace.set_columns_mode(new_mode);
  reapply_assigned_columns(workspace, state, config)
}

/// Rotates the windows of the focused workspace by one slot, keeping the
/// existing column layout (column count, per-column window counts, and
/// column widths) fixed — only the window occupying each slot changes.
///
/// Windows travel a clockwise loop around the center column: up the left
/// columns, across through the center, down the right columns, then
/// wrapping from the bottom-right slot back to the bottom-left. So for a
/// `*,C,*` layout with 2/1/2 windows the cycle is bottom-left → top-left →
/// center → top-right → bottom-right → back to bottom-left. Clockwise is
/// the default; `ccw` reverses it. The focused slot stays focused, so its
/// occupant changes under a steady highlight and repeated rotates cycle
/// every window through it.
///
/// The window rotated into the center column becomes the workspace's
/// master, so the new arrangement survives the next reapply instead of
/// being undone by it.
pub fn apply_rotate(
  workspace: &Workspace,
  ccw: bool,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let grid = ColumnGrid::read(workspace);

  if grid.window_count() < 2 {
    return Ok(());
  }

  let Some(center) = center_column(&grid, workspace) else {
    return Ok(());
  };

  // Windows loop clockwise around the center column: the columns left of
  // center are traversed bottom-to-top (so `ring_slots` lists them
  // reversed), then the center and right columns top-to-bottom.
  let mut ring_slots: Vec<(usize, usize)> = Vec::new();
  for (col, column) in grid.columns.iter().enumerate().take(center) {
    ring_slots.extend((0..column.len()).map(|row| (col, row)));
  }
  let left_len = ring_slots.len();
  ring_slots[..left_len].reverse();
  for (col, column) in grid.columns.iter().enumerate().skip(center) {
    ring_slots.extend((0..column.len()).map(|row| (col, row)));
  }

  // The window currently in each ring slot, in ring order.
  let ring: Vec<TilingWindow> = ring_slots
    .iter()
    .map(|&(c, r)| grid.columns[c][r].clone())
    .collect();

  // Remember which ring position holds focus so the same physical slot
  // stays focused after the windows rotate beneath it.
  let focused_pos = focused_window_id(workspace)
    .and_then(|id| ring.iter().position(|window| window.id() == id));

  // Clockwise: every window advances one slot, so slot `i` takes slot
  // `i-1`'s former occupant (last wraps to first). Counter-clockwise is
  // the reverse.
  let mut rotated = ring;
  if ccw {
    rotated.rotate_left(1);
  } else {
    rotated.rotate_right(1);
  }

  let focus_target = focused_pos.map(|pos| rotated[pos].id());

  // Place each rotated window back into its ring slot, preserving the
  // exact column shape.
  let mut columns: Vec<Vec<Option<TilingWindow>>> = grid
    .columns
    .iter()
    .map(|col| vec![None; col.len()])
    .collect();
  for (&(c, r), window) in ring_slots.iter().zip(rotated) {
    columns[c][r] = Some(window);
  }
  let columns = columns
    .into_iter()
    .map(|col| col.into_iter().flatten().collect())
    .collect::<Vec<Vec<TilingWindow>>>();

  // The center slot has a new occupant, so the master moves with it.
  let new_master = columns
    .get(center)
    .and_then(|column| column.first())
    .map(CommonGetters::id);

  ColumnGrid {
    columns,
    widths: grid.widths,
  }
  .render(workspace, state, config)?;

  if new_master.is_some() {
    workspace.set_master_window(new_master);
  }

  if let Some(id) = focus_target {
    focus_container_by_id(&id, state)?;
  }

  Ok(())
}

/// Swaps a window into the center slot — the single-window `C` column.
///
/// When the focused window is *not* the center, it swaps into the center
/// and the old center takes its place; focus follows into the center. When
/// the focused window *is* the center, it swaps back with the window most
/// recently phased out of the center (`state.last_centered_out`), so
/// pressing `center` repeatedly toggles between two windows. The window
/// leaving the center is remembered as the next toggle target, and
/// whichever window lands in the center becomes the workspace's master
/// and gains focus. A no-op if there is no toggle target, it no longer
/// exists, or there is nothing to swap.
pub fn apply_center(
  workspace: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let mut grid = ColumnGrid::read(workspace);

  if grid.window_count() < 2 {
    return Ok(());
  }

  let Some(center) = center_column(&grid, workspace) else {
    return Ok(());
  };

  let Some(center_id) =
    grid.columns[center].first().map(CommonGetters::id)
  else {
    return Ok(());
  };

  let Some(focused_id) = focused_window_id(workspace) else {
    return Ok(());
  };

  // Pick the slot to swap with the center. Normally that's the focused
  // window; if the focused window already is the center, fall back to the
  // last window moved out of the center so `center` toggles between two.
  let partner = if focused_id == center_id {
    state.last_centered_out.and_then(|id| grid.find(id))
  } else {
    grid.find(focused_id)
  };

  let Some((pc, pr)) = partner else {
    return Ok(());
  };

  // Nothing to do if the partner already occupies the center.
  if (pc, pr) == (center, 0) {
    return Ok(());
  }

  let leaving = grid.columns[center][0].clone();
  let entering = grid.columns[pc][pr].clone();
  grid.columns[center][0] = entering.clone();
  grid.columns[pc][pr] = leaving.clone();

  grid.render(workspace, state, config)?;

  // The window that landed in the center is the new master, so the swap
  // outlives the next reapply. Remember the window pushed out as the next
  // toggle target, and move focus with the window that came in.
  workspace.set_master_window(Some(entering.id()));
  state.last_centered_out = Some(leaving.id());
  focus_container_by_id(&entering.id(), state)?;

  Ok(())
}

/// Moves the focused window one slot within its workspace's assigned
/// columns grid, returning whether the move was handled here.
///
/// The workspace is treated as columns left-to-right, each a top-to-bottom
/// stack of windows. Interior moves swap the focused window with a
/// neighbouring slot: `Up`/`Down` with the window above/below it in its
/// column, `Left`/`Right` with the window level with it in the adjacent
/// column ([`straight_across_row`]). Swapping keeps every column's
/// window count fixed, so the center column — a single-window slot —
/// always stays exactly one window: moving a side window into the center
/// displaces the old center out to the vacated side slot, and moving the
/// center window out promotes the side window it swaps with into the
/// center. Whichever window ends up in the center column becomes the
/// workspace's master, so the arrangement survives the next reapply.
/// The result is rendered directly through
/// `ColumnGrid::render`, so the declarative columns stay intact and the
/// spec is not re-derived from geometry, and focus follows the moved
/// window.
///
/// At a column's edge the window leaves the workspace for the monitor
/// stacked in that direction: `Up`/`Down` past the top/bottom of a column
/// and `Left`/`Right` past the outermost column both move the window to
/// the adjacent monitor's displayed workspace (a no-op when there is no
/// monitor there), re-tidying the columns left behind. This is handled
/// here rather than by the default mover, which would either reparent the
/// window into a new perpendicular split or re-column a window that has
/// stacked neighbours inside this workspace — both of which break the
/// declarative columns.
///
/// Returns `false` — leaving the caller to fall back to the default
/// directional mover — only when the workspace has no effective columns
/// (see [`effective_columns`]: its own assignment, else a monitor-shape
/// `general.default_columns` rule) or the subject is not a tiling window
/// in the grid.
pub fn move_window_in_columns(
  window: &WindowContainer,
  direction: &Direction,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  let WindowContainer::TilingWindow(tiling) = window else {
    return Ok(false);
  };

  let Some(workspace) = tiling.workspace() else {
    return Ok(false);
  };

  if effective_columns(&workspace, config)?.is_none() {
    return Ok(false);
  }

  let mut grid = ColumnGrid::read(&workspace);
  let Some((col, row)) = grid.find(tiling.id()) else {
    return Ok(false);
  };

  let center = center_column(&grid, &workspace);

  match direction {
    Direction::Up | Direction::Down => {
      let target_row = match direction {
        Direction::Up if row > 0 => row - 1,
        Direction::Down if row + 1 < grid.columns[col].len() => row + 1,
        // Top/bottom of the column: leave for the workspace of the
        // monitor stacked in this direction (a no-op when there is none),
        // re-tidying the columns we leave behind. As with the left/right
        // edge, this is handled here rather than by the default mover,
        // which would reparent the window into a new perpendicular split
        // and break the declarative columns.
        _ => {
          move_to_workspace_in_direction(
            window, direction, state, config,
          )?;
          return Ok(true);
        }
      };
      grid.columns[col].swap(row, target_row);
    }
    Direction::Left | Direction::Right => {
      let target_col = match direction {
        Direction::Left if col > 0 => col - 1,
        Direction::Right if col + 1 < grid.columns.len() => col + 1,
        // Outermost column: leave for the adjacent monitor's workspace in
        // this direction (a no-op when there is none), which re-tidies the
        // columns we left behind. Handled here rather than by the default
        // mover, which would re-column the window inside this workspace
        // when it has stacked neighbours in its column.
        _ => {
          move_to_workspace_in_direction(
            window, direction, state, config,
          )?;
          return Ok(true);
        }
      };
      // Swap with the window level with this one so both columns keep
      // their window counts — the center stays a single window.
      let Some(target_row) = straight_across_row(
        &grid.columns[col],
        row,
        &grid.columns[target_col],
      ) else {
        return Ok(false);
      };

      let moved = grid.columns[col][row].clone();
      grid.columns[col][row] =
        grid.columns[target_col][target_row].clone();
      grid.columns[target_col][target_row] = moved;
    }
  }

  // The swap may have changed the center column's occupant, so record
  // the master explicitly rather than letting the next reapply guess.
  let new_master = center
    .and_then(|center| grid.columns.get(center))
    .and_then(|column| column.first())
    .map(CommonGetters::id);

  grid.render(&workspace, state, config)?;

  if new_master.is_some() {
    workspace.set_master_window(new_master);
  }

  focus_container_by_id(&tiling.id(), state)?;

  Ok(true)
}

/// Focuses the spatially adjacent window within a workspace's column
/// grid, returning whether the focus was handled here.
///
/// `Up`/`Down` moves to the window above/below in the same column.
/// `Left`/`Right` moves straight across in `Grid` mode, and to the
/// adjacent column's most recently focused window in the master-stack
/// modes (see [`neighbour_in_column`]). At a column edge, returns
/// `false` so the caller can fall through to cross-monitor focus.
pub fn focus_in_columns(
  window: &WindowContainer,
  direction: &Direction,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  let WindowContainer::TilingWindow(tiling) = window else {
    return Ok(false);
  };

  let Some(workspace) = tiling.workspace() else {
    return Ok(false);
  };

  if effective_columns(&workspace, config)?.is_none() {
    return Ok(false);
  }

  let grid = ColumnGrid::read(&workspace);
  let Some((col, row)) = grid.find(tiling.id()) else {
    return Ok(false);
  };

  let target_id = match direction {
    Direction::Up if row > 0 => Some(grid.columns[col][row - 1].id()),
    Direction::Down if row + 1 < grid.columns[col].len() => {
      Some(grid.columns[col][row + 1].id())
    }
    Direction::Left if col > 0 => {
      neighbour_in_column(&grid, &workspace, (col, row), col - 1)
    }
    Direction::Right if col + 1 < grid.columns.len() => {
      neighbour_in_column(&grid, &workspace, (col, row), col + 1)
    }
    // Edge of grid — let caller handle cross-monitor focus.
    _ => return Ok(false),
  };

  let Some(target_id) = target_id else {
    return Ok(false);
  };

  focus_container_by_id(&target_id, state)?;
  state.pending_sync.queue_focus_change().queue_cursor_jump();

  Ok(true)
}

/// Returns the window to focus in `target_col` when navigating left or
/// right out of row `source_row` of `source_col`.
///
/// In `Grid` mode every column is a stack, so the neighbour is the one
/// straight across — [`straight_across`]. Remembering the column's last
/// focused window there means most sideways moves land on a different
/// row than the one you left, which reads as a diagonal jump whose
/// destination depends on where you last were in that column.
///
/// The master-stack modes keep the focus memory: the only ambiguous
/// move is out of the single-window `C` column into a stack, and
/// returning to the window you left is more useful there than landing
/// on whichever row happens to sit level with the full-height center.
///
/// `None` only when the target column is empty, which `ColumnGrid::read`
/// never produces.
fn neighbour_in_column(
  grid: &ColumnGrid,
  workspace: &Workspace,
  (source_col, source_row): (usize, usize),
  target_col: usize,
) -> Option<Uuid> {
  let target = &grid.columns[target_col];

  // Single-window column — no choice to make.
  if target.len() == 1 {
    return Some(target[0].id());
  }

  if workspace.columns_mode() != ColumnsMode::Grid {
    // Multi-window columns are wrapped in a `SplitContainer` whose
    // `child_focus_order` tracks the most recently focused child. Use it
    // when it points at a window still in the column.
    let remembered = target
      .first()
      .and_then(CommonGetters::parent)
      .and_then(|parent| parent.child_focus_order().next())
      .filter(|focused| {
        target.iter().any(|window| window.id() == focused.id())
      });

    if let Some(remembered) = remembered {
      return Some(remembered.id());
    }
  }

  straight_across(&grid.columns[source_col], source_row, target)
}

/// Returns the window in `target` level with row `source_row` of
/// `source` — see [`straight_across_row`].
fn straight_across(
  source: &[TilingWindow],
  source_row: usize,
  target: &[TilingWindow],
) -> Option<Uuid> {
  straight_across_row(source, source_row, target)
    .and_then(|row| target.get(row))
    .map(CommonGetters::id)
}

/// Returns the row in `target` level with row `source_row` of `source`:
/// the one whose vertical span overlaps the source window's the most,
/// ties going to the topmost. `None` for an empty `target`.
///
/// Matching by row index instead would drift whenever the two columns
/// hold different numbers of windows, which grid mode produces for any
/// odd window count: row 1 of a two-row column covers the bottom half,
/// which is row 2 of a three-row column, not row 1.
fn straight_across_row(
  source: &[TilingWindow],
  source_row: usize,
  target: &[TilingWindow],
) -> Option<usize> {
  let source_span = row_spans(source)
    .get(source_row)
    .copied()
    .unwrap_or((0.0, 1.0));

  let mut best: Option<(usize, f32)> = None;
  for (index, span) in row_spans(target).into_iter().enumerate() {
    let overlap =
      (source_span.1.min(span.1) - source_span.0.max(span.0)).max(0.0);

    if best.is_none_or(|(_, best_overlap)| overlap > best_overlap) {
      best = Some((index, overlap));
    }
  }

  best.map(|(index, _)| index)
}

/// Vertical span of every window in a column, as start/end fractions of
/// the column's height.
///
/// Taken from the windows' tiling sizes rather than assuming even rows,
/// so a column whose rows have been resized still matches by geometry.
fn row_spans(column: &[TilingWindow]) -> Vec<(f32, f32)> {
  // A single-window column is not wrapped in a vertical split, so its
  // `tiling_size` is the column's width fraction, not a row height. The
  // window spans the full height.
  if column.len() < 2 {
    return column.iter().map(|_| (0.0, 1.0)).collect();
  }

  let total = column
    .iter()
    .map(TilingSizeGetters::tiling_size)
    .sum::<f32>()
    .max(f32::EPSILON);

  let mut start = 0.0;
  column
    .iter()
    .map(|window| {
      let end = start + window.tiling_size() / total;
      let span = (start, end);
      start = end;
      span
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use uuid::Uuid;
  use wm_common::{ColumnsMode, ParsedConfig};
  use wm_platform::{Direction, Rect};

  use super::{
    apply_center, apply_columns, apply_grid, apply_rotate, assign_columns,
    effective_columns, focus_in_columns, focused_window_id,
    grid::ColumnGrid, move_window_in_columns, reapply_assigned_columns,
    reapply_columns_for_new_window, store_center_width,
    toggle_columns_mode, unassign_columns, workspace_center_window_id,
  };
  use crate::{
    commands::{
      container::{attach_container, focus_container_by_id},
      workspace::move_workspace_in_direction,
    },
    models::{
      Monitor, TilingContainer, TilingWindow, WindowContainer, Workspace,
    },
    test_utils::{mock_user_config, mock_wm_state},
    traits::{CommonGetters, TilingSizeGetters},
    user_config::UserConfig,
    wm_state::WmState,
  };

  fn setup(
    window_count: usize,
  ) -> (WmState, Workspace, Vec<TilingWindow>) {
    let state = mock_wm_state();
    let (workspace, windows) =
      workspace_with_windows(&state, None, window_count);
    (state, workspace, windows)
  }

  fn add_monitor(
    state: &WmState,
    bounds: Rect,
    count: usize,
  ) -> (Workspace, Vec<TilingWindow>) {
    workspace_with_windows(state, Some(bounds), count)
  }

  /// Attaches a monitor holding one workspace of `count` tiling windows,
  /// each its own top-level column.
  fn workspace_with_windows(
    state: &WmState,
    bounds: Option<Rect>,
    count: usize,
  ) -> (Workspace, Vec<TilingWindow>) {
    let windows = (0..count)
      .map(|_| TilingWindow::mock().call())
      .collect::<Vec<_>>();
    let workspace = Workspace::mock()
      .tiling_containers(
        windows
          .iter()
          .cloned()
          .map(TilingContainer::TilingWindow)
          .collect(),
      )
      .call();
    let monitor = Monitor::mock().workspaces(vec![workspace.clone()]);
    let monitor = match bounds {
      Some(bounds) => monitor.bounds(bounds).call(),
      None => monitor.call(),
    };
    attach_container(
      &monitor.into(),
      &state.root_container.clone().into(),
      None,
    )
    .unwrap();
    (workspace, windows)
  }

  /// Appends a new tiling window to the workspace, as `manage_window`
  /// does before it reapplies the columns.
  fn add_window(workspace: &Workspace) -> TilingWindow {
    let window = TilingWindow::mock().call();
    attach_container(
      &window.clone().into(),
      &workspace.clone().into(),
      None,
    )
    .unwrap();
    window
  }

  fn config_with_default_columns(spec: &str) -> UserConfig {
    let yaml = format!(
      "general:\n  default_columns:\n    - min_aspect_ratio: 1.5\n      \
       spec: '{spec}'\n"
    );
    UserConfig::mock(serde_yaml::from_str::<ParsedConfig>(&yaml).unwrap())
  }

  fn id_grid(workspace: &Workspace) -> Vec<Vec<Uuid>> {
    ColumnGrid::read(workspace)
      .columns
      .iter()
      .map(|col| col.iter().map(CommonGetters::id).collect())
      .collect()
  }

  #[test]
  fn applies_star_center_star_layout() {
    let (mut state, workspace, windows) = setup(5);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(
      &workspace,
      "*,C,*",
      0.6,
      &mut state,
      &mock_user_config(),
    )
    .unwrap();

    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[1], ids[3]], vec![ids[0]], vec![ids[2], ids[4]]]
    );

    // The center column is the master's, and it is the wide one.
    let grid = ColumnGrid::read(&workspace);
    assert_eq!(grid.find(ids[0]), Some((1, 0)));
    assert!((grid.widths[1] - 0.6).abs() < 1e-3);
  }

  #[test]
  fn applies_fixed_column_layout() {
    let (mut state, workspace, windows) = setup(6);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(
      &workspace,
      "2,C,*",
      0.6,
      &mut state,
      &mock_user_config(),
    )
    .unwrap();

    // The fixed column takes two windows, dealt on the first two rows.
    assert_eq!(
      id_grid(&workspace),
      vec![
        vec![ids[1], ids[3]],
        vec![ids[0]],
        vec![ids[2], ids[4], ids[5]]
      ]
    );
  }

  #[test]
  fn center_column_is_the_master_not_the_widest() {
    // The regression this guards: `C,*` at a 0.5 center width makes both
    // columns exactly 0.5 wide, and the center used to be taken as the
    // widest column. The tie resolved to the *stack*, so `center` swapped
    // the focused window into the right-hand column and the master
    // visibly jumped to the right the moment a third window appeared.
    let (mut state, workspace, windows) = setup(3);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = mock_user_config();
    apply_columns(&workspace, "C,*", 0.5, &mut state, &config).unwrap();

    let grid = ColumnGrid::read(&workspace);
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0]], vec![ids[1], ids[2]]]
    );
    assert!((grid.widths[0] - grid.widths[1]).abs() < 1e-3);

    focus_container_by_id(&ids[2], &mut state).unwrap();
    apply_center(&workspace, &mut state, &config).unwrap();

    // The focused window moves into the left column and takes the master
    // designation; the old master drops into the slot it vacated.
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[2]], vec![ids[1], ids[0]]]
    );
    assert_eq!(workspace_center_window_id(&workspace), Some(ids[2]));
  }

  #[test]
  fn master_survives_a_new_window() {
    // The regression this guards: a manually chosen master used to be
    // reverted by the next window opening, because the layout replayed a
    // stored window order whose first entry was some older window.
    let (mut state, workspace, windows) = setup(3);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = config_with_default_columns("C,*");
    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();

    focus_container_by_id(&ids[2], &mut state).unwrap();
    apply_center(&workspace, &mut state, &config).unwrap();
    assert_eq!(workspace_center_window_id(&workspace), Some(ids[2]));

    let new_window = add_window(&workspace);
    reapply_columns_for_new_window(
      &workspace,
      new_window.id(),
      &mut state,
      &config,
    )
    .unwrap();

    // The master is untouched and the new window lands at the end of the
    // stack, below every window already there.
    assert_eq!(workspace_center_window_id(&workspace), Some(ids[2]));
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[2]], vec![ids[1], ids[0], new_window.id()]]
    );
  }

  #[test]
  fn new_window_does_not_move_existing_windows() {
    let (mut state, workspace, windows) = setup(4);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = config_with_default_columns("*,C,*");
    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[1], ids[3]], vec![ids[0]], vec![ids[2]]]
    );

    let new_window = add_window(&workspace);
    reapply_columns_for_new_window(
      &workspace,
      new_window.id(),
      &mut state,
      &config,
    )
    .unwrap();

    // Every original window keeps its exact slot; only the new window is
    // added, at the first free one.
    assert_eq!(
      id_grid(&workspace),
      vec![
        vec![ids[1], ids[3]],
        vec![ids[0]],
        vec![ids[2], new_window.id()]
      ]
    );
  }

  #[test]
  fn reapply_is_idempotent() {
    // Reapplying must be a no-op. Window order is read back out of the
    // tree, so a layout that permuted its own output would drift a little
    // further on every workspace switch, window close, and config reload.
    let (mut state, workspace, _) = setup(6);
    let config = config_with_default_columns("*,C,*");
    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();

    let expected = id_grid(&workspace);
    for _ in 0..3 {
      reapply_assigned_columns(&workspace, &mut state, &config).unwrap();
      assert_eq!(id_grid(&workspace), expected);
    }
  }

  #[test]
  fn master_promotes_to_first_window_when_it_leaves() {
    let (mut state, workspace, windows) = setup(4);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = config_with_default_columns("*,C,*");
    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();
    assert_eq!(workspace_center_window_id(&workspace), Some(ids[0]));

    // The close handlers clear the designation before unmanaging.
    workspace.set_master_window(None);
    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();

    // The first window in on-screen order takes over — no focus, z-order
    // or recency involved.
    assert_eq!(workspace_center_window_id(&workspace), Some(ids[1]));
  }

  #[test]
  fn rotates_windows_clockwise_keeping_shape() {
    let (mut state, workspace, windows) = setup(5);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = mock_user_config();
    apply_columns(&workspace, "*,C,*", 0.6, &mut state, &config).unwrap();
    apply_rotate(&workspace, false, &mut state, &config).unwrap();

    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[3], ids[4]], vec![ids[1]], vec![ids[0], ids[2]]]
    );

    // The window rotated into the center column becomes the master.
    assert_eq!(workspace_center_window_id(&workspace), Some(ids[1]));
  }

  #[test]
  fn rotates_windows_counter_clockwise() {
    let (mut state, workspace, windows) = setup(5);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = mock_user_config();
    apply_columns(&workspace, "*,C,*", 0.6, &mut state, &config).unwrap();
    apply_rotate(&workspace, true, &mut state, &config).unwrap();

    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0], ids[1]], vec![ids[2]], vec![ids[4], ids[3]]]
    );
    assert_eq!(workspace_center_window_id(&workspace), Some(ids[2]));
  }

  #[test]
  fn rotating_back_restores_the_arrangement() {
    let (mut state, workspace, _) = setup(5);
    let config = mock_user_config();
    apply_columns(&workspace, "*,C,*", 0.6, &mut state, &config).unwrap();

    let expected = id_grid(&workspace);
    apply_rotate(&workspace, false, &mut state, &config).unwrap();
    apply_rotate(&workspace, true, &mut state, &config).unwrap();

    assert_eq!(id_grid(&workspace), expected);
  }

  #[test]
  fn center_swaps_focused_then_toggles_back() {
    let (mut state, workspace, windows) = setup(5);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = mock_user_config();
    apply_columns(&workspace, "*,C,*", 0.6, &mut state, &config).unwrap();

    focus_container_by_id(&ids[1], &mut state).unwrap();
    apply_center(&workspace, &mut state, &config).unwrap();
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0], ids[3]], vec![ids[1]], vec![ids[2], ids[4]]]
    );
    assert_eq!(focused_window_id(&workspace), Some(ids[1]));

    apply_center(&workspace, &mut state, &config).unwrap();
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[1], ids[3]], vec![ids[0]], vec![ids[2], ids[4]]]
    );
    assert_eq!(focused_window_id(&workspace), Some(ids[0]));
  }

  #[test]
  fn moves_window_within_column() {
    let (mut state, workspace, windows) = setup(5);
    let config = config_with_default_columns("*,C,*");
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(&workspace, "*,C,*", 0.6, &mut state, &config).unwrap();

    // Window 3 sits below window 1 in the left column.
    let window = WindowContainer::TilingWindow(windows[3].clone());
    assert!(move_window_in_columns(
      &window,
      &Direction::Up,
      &mut state,
      &config
    )
    .unwrap());

    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[3], ids[1]], vec![ids[0]], vec![ids[2], ids[4]]]
    );
    assert_eq!(workspace_center_window_id(&workspace), Some(ids[0]));
  }

  #[test]
  fn moves_window_across_to_the_level_row() {
    let (mut state, workspace, windows) = setup(5);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = config_with_default_columns("C,*");

    workspace.set_columns_mode(ColumnsMode::Grid);
    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0], ids[2], ids[4]], vec![ids[1], ids[3]]]
    );

    // ids[3] fills the bottom half of the two-row column, so it swaps
    // with the bottom third of the three-row column, not the middle.
    let window = WindowContainer::TilingWindow(windows[3].clone());
    assert!(move_window_in_columns(
      &window,
      &Direction::Left,
      &mut state,
      &config
    )
    .unwrap());

    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0], ids[2], ids[3]], vec![ids[1], ids[4]]]
    );
  }

  #[test]
  fn moves_into_center_keeps_it_single_window() {
    let (mut state, workspace, windows) = setup(5);
    let config = config_with_default_columns("*,C,*");
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(&workspace, "*,C,*", 0.6, &mut state, &config).unwrap();

    let window = WindowContainer::TilingWindow(windows[1].clone());
    assert!(move_window_in_columns(
      &window,
      &Direction::Right,
      &mut state,
      &config
    )
    .unwrap());

    // The old center is displaced into the slot the mover vacated, and
    // the mover becomes the master.
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0], ids[3]], vec![ids[1]], vec![ids[2], ids[4]]]
    );
    assert_eq!(workspace_center_window_id(&workspace), Some(ids[1]));
  }

  #[test]
  fn moves_out_to_horizontally_adjacent_monitor() {
    let mut state = mock_wm_state();
    let config = config_with_default_columns("*,C,*");
    let (ws_left, wins_left) =
      add_monitor(&state, Rect::from_xy(0, 0, 1680, 1050), 5);
    let (ws_right, _) =
      add_monitor(&state, Rect::from_xy(1680, 0, 1680, 1050), 2);
    let ids = wins_left.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(&ws_left, "*,C,*", 0.6, &mut state, &config).unwrap();

    // Window 4 is at the bottom of the rightmost column.
    let window = WindowContainer::TilingWindow(wins_left[4].clone());
    assert!(move_window_in_columns(
      &window,
      &Direction::Right,
      &mut state,
      &config
    )
    .unwrap());

    assert_eq!(
      wins_left[4].workspace().map(|w| w.id()),
      Some(ws_right.id())
    );
    assert_eq!(workspace_center_window_id(&ws_left), Some(ids[0]));
  }

  #[test]
  fn moves_out_to_vertically_adjacent_monitor() {
    let mut state = mock_wm_state();
    let config = config_with_default_columns("*,C,*");
    let (ws_top, wins_top) =
      add_monitor(&state, Rect::from_xy(0, 0, 1680, 1050), 5);
    let (ws_bottom, _) =
      add_monitor(&state, Rect::from_xy(0, 1050, 1680, 1050), 2);
    let ids = wins_top.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(&ws_top, "*,C,*", 0.6, &mut state, &config).unwrap();

    // Window 4 is at the bottom of its column.
    let window = WindowContainer::TilingWindow(wins_top[4].clone());
    assert!(move_window_in_columns(
      &window,
      &Direction::Down,
      &mut state,
      &config
    )
    .unwrap());

    assert_eq!(
      wins_top[4].workspace().map(|w| w.id()),
      Some(ws_bottom.id())
    );
    assert_eq!(workspace_center_window_id(&ws_top), Some(ids[0]));
  }

  #[test]
  fn moving_the_master_out_promotes_a_survivor() {
    let mut state = mock_wm_state();
    let config = config_with_default_columns("*,C");
    let (ws_left, wins_left) =
      add_monitor(&state, Rect::from_xy(0, 0, 1680, 1050), 3);
    let (ws_right, _) =
      add_monitor(&state, Rect::from_xy(1680, 0, 1680, 1050), 2);
    let ids = wins_left.iter().map(CommonGetters::id).collect::<Vec<_>>();
    reapply_assigned_columns(&ws_left, &mut state, &config).unwrap();
    assert_eq!(workspace_center_window_id(&ws_left), Some(ids[0]));

    // `*,C` puts the master in the rightmost column, so `right` takes it
    // off this workspace entirely.
    let window = WindowContainer::TilingWindow(wins_left[0].clone());
    assert!(move_window_in_columns(
      &window,
      &Direction::Right,
      &mut state,
      &config
    )
    .unwrap());

    // Nothing is left dangling: the workspace it left promotes its first
    // remaining window, and the workspace it joined keeps its own master.
    assert_eq!(workspace_center_window_id(&ws_left), Some(ids[1]));
    assert_ne!(workspace_center_window_id(&ws_right), Some(ids[0]));
  }

  #[test]
  fn workspace_recolumns_when_moved_to_monitor_with_different_aspect() {
    let mut state = mock_wm_state();
    let yaml = "
general:
  default_columns:
    - min_aspect_ratio: 2.1
      spec: '*,C,*'
    - min_aspect_ratio: 1.5
      spec: 'C,*'
";
    let config = UserConfig::mock(
      serde_yaml::from_str::<ParsedConfig>(yaml).unwrap(),
    );
    let (workspace, _) =
      add_monitor(&state, Rect::from_xy(0, 0, 3440, 1440), 3);
    let ultrawide = workspace.monitor().unwrap();
    let filler = Workspace::mock().name("filler".to_string()).call();
    attach_container(&filler.into(), &ultrawide.clone().into(), None)
      .unwrap();
    add_monitor(&state, Rect::from_xy(3440, 0, 1920, 1080), 0);

    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();
    assert_eq!(ColumnGrid::read(&workspace).columns.len(), 3);

    move_workspace_in_direction(
      &workspace,
      &Direction::Right,
      &mut state,
      &config,
    )
    .unwrap();

    assert_eq!(
      effective_columns(&workspace, &config)
        .unwrap()
        .map(|c| c.spec),
      Some("C,*".to_string())
    );
    assert_eq!(ColumnGrid::read(&workspace).columns.len(), 2);
  }

  #[test]
  fn move_without_columns_is_not_handled() {
    let (mut state, _, windows) = setup(3);
    let window = WindowContainer::TilingWindow(windows[0].clone());
    assert!(!move_window_in_columns(
      &window,
      &Direction::Left,
      &mut state,
      &mock_user_config()
    )
    .unwrap());
  }

  #[test]
  fn effective_columns_uses_default_when_unassigned() {
    let (_, workspace, _) = setup(2);
    assert_eq!(
      effective_columns(&workspace, &config_with_default_columns("C,*"))
        .unwrap()
        .map(|c| c.spec),
      Some("C,*".to_string())
    );
  }

  #[test]
  fn effective_columns_prefers_assignment() {
    let (mut state, workspace, _) = setup(2);
    let config = config_with_default_columns("*,C,*");
    assign_columns(&workspace, "1,C", 0.6, &mut state, &config).unwrap();
    assert_eq!(
      effective_columns(&workspace, &config)
        .unwrap()
        .map(|c| c.spec),
      Some("1,C".to_string())
    );
  }

  #[test]
  fn unassign_clears_assignment() {
    let (mut state, workspace, _) = setup(2);
    assign_columns(
      &workspace,
      "*,C,*",
      0.6,
      &mut state,
      &mock_user_config(),
    )
    .unwrap();
    assert!(workspace.config().columns.is_some());
    unassign_columns(&workspace);
    assert!(workspace.config().columns.is_none());
  }

  #[test]
  fn reports_center_window_id() {
    let (_, workspace, windows) = setup(5);
    assert_eq!(
      workspace_center_window_id(&workspace),
      Some(windows[0].id())
    );
  }

  #[test]
  fn store_center_width_records_resize() {
    let (mut state, workspace, windows) = setup(5);
    let config = mock_user_config();
    assign_columns(&workspace, "*,C,*", 0.6, &mut state, &config).unwrap();

    let center_id = workspace_center_window_id(&workspace).unwrap();
    let center = windows.iter().find(|w| w.id() == center_id).unwrap();
    center.set_tiling_size(0.7);
    store_center_width(&workspace);

    assert!(
      (workspace.config().columns.unwrap().center - 0.7).abs() < 1e-3
    );
  }

  #[test]
  fn grid_distributes_round_robin() {
    let (mut state, workspace, windows) = setup(4);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_grid(&workspace, 2, None, &mut state, &mock_user_config())
      .unwrap();

    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0], ids[2]], vec![ids[1], ids[3]]]
    );
    assert!((ColumnGrid::read(&workspace).widths[0] - 0.5).abs() < 1e-3);
  }

  #[test]
  fn grid_appends_a_new_window_without_reshuffling() {
    let (mut state, workspace, windows) = setup(4);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = mock_user_config();
    apply_grid(&workspace, 2, None, &mut state, &config).unwrap();

    let new_window = add_window(&workspace);
    apply_grid(&workspace, 2, Some(new_window.id()), &mut state, &config)
      .unwrap();

    // The previous grid-affinity fixup swapped the newest window into the
    // focused window's column, so the grid reshuffled differently
    // depending on what happened to be focused. It now simply appends.
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0], ids[2], new_window.id()], vec![ids[1], ids[3]]]
    );
  }

  #[test]
  fn grid_requires_four_windows() {
    let (mut state, workspace, _) = setup(3);
    apply_grid(&workspace, 2, None, &mut state, &mock_user_config())
      .unwrap();
    assert_eq!(ColumnGrid::read(&workspace).columns.len(), 3);
  }

  #[test]
  fn toggle_columns_mode_cycles() {
    let (mut state, workspace, _) = setup(5);
    let config = config_with_default_columns("C,*");
    assert_eq!(workspace.columns_mode(), ColumnsMode::MasterStackLeft);
    toggle_columns_mode(&workspace, None, &mut state, &config).unwrap();
    assert_eq!(workspace.columns_mode(), ColumnsMode::Grid);
    toggle_columns_mode(&workspace, None, &mut state, &config).unwrap();
    assert_eq!(workspace.columns_mode(), ColumnsMode::MasterStackRight);
    toggle_columns_mode(&workspace, None, &mut state, &config).unwrap();
    assert_eq!(workspace.columns_mode(), ColumnsMode::MasterStackLeft);
  }

  #[test]
  fn master_stack_right_mirrors_the_spec() {
    let (mut state, workspace, windows) = setup(3);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = config_with_default_columns("C,*");

    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0]], vec![ids[1], ids[2]]]
    );

    workspace.set_columns_mode(ColumnsMode::MasterStackRight);
    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();

    // `C,*` reverses to `*,C`: the same windows, mirrored, with the same
    // master.
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[1], ids[2]], vec![ids[0]]]
    );
    assert_eq!(workspace_center_window_id(&workspace), Some(ids[0]));
  }

  #[test]
  fn grid_armed_with_fewer_than_four_windows() {
    let (mut state, workspace, _) = setup(3);
    let config = config_with_default_columns("C,*");
    workspace.set_columns_mode(ColumnsMode::Grid);
    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();

    // Mode stays armed; layout falls back to master-stack.
    assert_eq!(workspace.columns_mode(), ColumnsMode::Grid);
    assert_eq!(ColumnGrid::read(&workspace).columns.len(), 2);
  }

  #[test]
  fn focus_remembers_last_focused_in_column() {
    let (mut state, workspace, windows) = setup(5);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = config_with_default_columns("C,*");

    // Layout: center [ids[0]] | stack [ids[1], ids[2], ids[3], ids[4]].
    // Focus starts on center.
    apply_columns(&workspace, "C,*", 0.5, &mut state, &config).unwrap();
    focus_container_by_id(&ids[0], &mut state).unwrap();

    // Navigate right — no focus history yet, should land on row 0.
    let w0 = WindowContainer::TilingWindow(windows[0].clone());
    assert!(
      focus_in_columns(&w0, &Direction::Right, &mut state, &config)
        .unwrap()
    );
    assert_eq!(focused_window_id(&workspace), Some(ids[1]));

    // Navigate down to ids[3] (row 2 in the stack).
    let w1 = WindowContainer::TilingWindow(windows[1].clone());
    assert!(focus_in_columns(&w1, &Direction::Down, &mut state, &config)
      .unwrap());
    assert_eq!(focused_window_id(&workspace), Some(ids[2]));
    let w2 = WindowContainer::TilingWindow(windows[2].clone());
    assert!(focus_in_columns(&w2, &Direction::Down, &mut state, &config)
      .unwrap());
    assert_eq!(focused_window_id(&workspace), Some(ids[3]));

    // Navigate left back to center.
    let w3 = WindowContainer::TilingWindow(windows[3].clone());
    assert!(focus_in_columns(&w3, &Direction::Left, &mut state, &config)
      .unwrap());
    assert_eq!(focused_window_id(&workspace), Some(ids[0]));

    // Navigate right again — should return to ids[3] (the last-focused
    // window in the stack), NOT ids[0]/row 0.
    assert!(
      focus_in_columns(&w0, &Direction::Right, &mut state, &config)
        .unwrap()
    );
    assert_eq!(focused_window_id(&workspace), Some(ids[3]));
  }

  #[test]
  fn grid_focus_goes_straight_across() {
    let (mut state, workspace, windows) = setup(5);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = config_with_default_columns("C,*");

    workspace.set_columns_mode(ColumnsMode::Grid);
    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();

    // Round-robin into two columns of unequal height: the left column's
    // three rows do not line up with the right column's two.
    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0], ids[2], ids[4]], vec![ids[1], ids[3]]]
    );

    // Leave the left column's focus history pointing at its top row.
    focus_container_by_id(&ids[0], &mut state).unwrap();

    // Left out of the right column's bottom row lands on the window
    // level with it — the left column's bottom row, not the remembered
    // ids[0].
    let w3 = WindowContainer::TilingWindow(windows[3].clone());
    assert!(focus_in_columns(&w3, &Direction::Left, &mut state, &config)
      .unwrap());
    assert_eq!(focused_window_id(&workspace), Some(ids[4]));

    // And back the other way, which is only symmetric because neither
    // direction consults focus history.
    let w4 = WindowContainer::TilingWindow(windows[4].clone());
    assert!(
      focus_in_columns(&w4, &Direction::Right, &mut state, &config)
        .unwrap()
    );
    assert_eq!(focused_window_id(&workspace), Some(ids[3]));
  }
}
