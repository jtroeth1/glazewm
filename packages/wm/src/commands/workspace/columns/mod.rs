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
use wm_common::{ColumnBias, ColumnLayout, ColumnsMode};
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
  traits::{CommonGetters, PositionGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Arranges the focused workspace into a centered-focus layout described
/// by a comma-separated column `spec`, laid out left-to-right. Each token
/// is one column: a number is that many windows stacked, `*` claims an
/// even share of the leftover windows, and `C` is the wide center (the
/// focused window; exactly one). E.g. `*,C,*` is a center flanked by two
/// even stacks, `C,*` drops the left band for a narrow monitor, and
/// `2,1,C,3` is fully explicit.
///
/// The center window fills the `C` column at `center` width (a fraction of
/// the workspace, clamped to `0.1..=0.9`); the remaining columns split the
/// rest of the width evenly. Windows are assigned in on-screen order, left
/// to right. When `*` columns can't split the leftovers evenly, `bias`
/// decides which end claims the odd window(s). Any window still unplaced
/// (fixed counts under-specify the total and there is no `*`) is appended
/// to the last non-center column. Columns that end up empty are dropped
/// and the widths renormalise, so a wide spec still degrades cleanly on a
/// small monitor.
///
/// The `C` column takes `preferred_center` when set and that window is
/// still present, otherwise the focused window, otherwise the middle
/// window by position. Passing the current center as `preferred_center`
/// keeps the layout stable across reapplies that aren't meant to move the
/// center (e.g. after a side window closes).
pub fn apply_columns(
  workspace: &Workspace,
  spec: &str,
  center: f32,
  bias: &ColumnBias,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let kinds = parse_columns_spec(spec)?;

  // Use the buffer as the source of truth for window ordering.
  let windows = resolve_ordered_windows(workspace);

  if windows.len() < 2 {
    return Ok(());
  }

  // In the buffer model, window_order[0] is always the center.
  let center_window = windows[0].clone();
  let rest = windows[1..].to_vec();

  // Remember the outgoing center for `center` toggle support.
  remember_outgoing_center(workspace, center_window.id(), state);

  let columns = distribute_columns(&kinds, center_window, rest, bias);
  let widths = column_widths(&kinds, center.clamp(0.1, 0.9));

  ColumnGrid { columns, widths }.render(workspace, state, config)
}

/// Arranges the workspace into an equal-width grid with windows
/// distributed round-robin from the creation-order buffer.
///
/// When the workspace carries a `grid_affinity` target (set by
/// `manage_window` when a new window opens), the newest window is
/// swapped into the affinity target's column so it lands visually
/// adjacent to the previously focused window.
///
/// Requires at least 4 tiling windows. Falls back to master-stack if
/// fewer are present (the caller should check and switch mode).
fn apply_grid(
  workspace: &Workspace,
  num_columns: usize,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let windows = resolve_ordered_windows(workspace);

  if windows.len() < 4 || num_columns == 0 {
    return Ok(());
  }

  // Round-robin distribution: [0]→col0, [1]→col1, [2]→col0, ...
  let mut columns: Vec<Vec<TilingWindow>> =
    (0..num_columns).map(|_| Vec::new()).collect();
  for (i, window) in windows.into_iter().enumerate() {
    columns[i % num_columns].push(window);
  }

  // When a new window was just added, place it in the same column
  // as the previously focused window (the affinity target). Swap
  // the newest window with the last window in the target column.
  if let Some(affinity_id) = workspace.take_grid_affinity() {
    let newest_id =
      workspace.window_order().last().copied();

    if let Some(nid) = newest_id {
      let aff_col = columns.iter().position(|col| {
        col.iter().any(|w| w.id() == affinity_id)
      });
      let new_pos =
        columns.iter().enumerate().find_map(|(ci, col)| {
          col
            .iter()
            .enumerate()
            .find_map(|(ri, w)| {
              (w.id() == nid).then_some((ci, ri))
            })
        });

      tracing::info!(
        "Grid affinity: target={affinity_id}, newest={nid}, \
         aff_col={aff_col:?}, new_pos={new_pos:?}"
      );

      if let (Some(ac), Some((nc, nr))) = (aff_col, new_pos) {
        if ac != nc {
          let last = columns[ac].len() - 1;
          let newest = columns[nc].remove(nr);
          let displaced = columns[ac].remove(last);
          columns[ac].push(newest);
          columns[nc].insert(nr, displaced);
          tracing::info!(
            "Grid affinity: swapped newest into col {ac}"
          );
        } else {
          tracing::info!(
            "Grid affinity: already in same column, no swap"
          );
        }
      } else {
        tracing::info!(
          "Grid affinity: target or newest not found in grid"
        );
      }
    }
  }

  #[allow(clippy::cast_precision_loss)]
  let width = 1.0 / num_columns as f32;
  let widths = vec![width; num_columns];

  ColumnGrid { columns, widths }.render(workspace, state, config)
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
  bias: &ColumnBias,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let mut workspace_config = workspace.config();
  workspace_config.columns = Some(ColumnLayout {
    spec: spec.to_string(),
    center,
    bias: bias.clone(),
  });
  workspace.set_config(workspace_config);

  apply_columns(workspace, spec, center, bias, state, config)
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

/// Reapplies the workspace's effective columns. In master-stack mode
/// `window_order[0]` occupies the `C` column; in grid mode windows are
/// distributed round-robin. A no-op when nothing resolves or the
/// workspace has fewer than two tiling windows.
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
  let Some(columns) = effective_columns(workspace, config)? else {
    return Ok(());
  };

  match workspace.columns_mode() {
    ColumnsMode::MasterStackLeft => {
      apply_columns(
        workspace,
        &columns.spec,
        columns.center,
        &columns.bias,
        state,
        config,
      )?;
    }
    ColumnsMode::MasterStackRight => {
      apply_columns(
        workspace,
        &reverse_spec(&columns.spec),
        columns.center,
        &columns.bias,
        state,
        config,
      )?;
    }
    ColumnsMode::Grid => {
      // Grid needs ≥ 4 windows; use master-stack-left layout while
      // armed. The mode stays Grid so the next window addition
      // auto-applies the grid once the threshold is met.
      if workspace.window_order().len() < 4 {
        apply_columns(
          workspace,
          &columns.spec,
          columns.center,
          &columns.bias,
          state,
          config,
        )?;
      } else {
        apply_grid(workspace, 2, state, config)?;
      }
    }

  }
  Ok(())
}

/// Reapplies assigned columns after a tiling window moves from `source`
/// to `target`. Both workspaces re-tidy from their own `window_order`
/// buffers. A no-op for either workspace that has no assigned columns.
pub fn reapply_columns_after_move(
  source: &Workspace,
  target: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  reapply_assigned_columns(source, state, config)?;
  reapply_assigned_columns(target, state, config)?;
  Ok(())
}

/// Id of the first window in the workspace's creation-order buffer,
/// which occupies the `C` column in master-stack mode.
pub fn workspace_center_window_id(workspace: &Workspace) -> Option<Uuid> {
  workspace.window_order().first().copied()
}

/// Remembers the window leaving the center as the `center` command's
/// swap-back target.
///
/// Given the `new_center` a layout is about to install, this records the
/// window currently in the center (when different) into
/// `state.last_centered_out`, so pressing `center` on the incoming window
/// toggles back to the one it displaced — e.g. after a freshly opened
/// window auto-centers.
///
/// Must be called while the outgoing center is still in place, before the
/// workspace is rebuilt. A no-op when the center is unchanged or the
/// workspace has no center yet.
fn remember_outgoing_center(
  workspace: &Workspace,
  new_center: Uuid,
  state: &mut WmState,
) {
  if let Some(previous) = workspace_center_window_id(workspace) {
    if previous != new_center {
      state.last_centered_out = Some(previous);
    }
  }
}

/// Current width fraction of the workspace's center column when it has
/// at least two tiling windows. Uses the stored center window ID to
/// locate the right column instead of width-based inference.
///
/// Returns `None` for a workspace with fewer than two tiling windows,
/// where no meaningful center width exists.
fn workspace_center_width(workspace: &Workspace) -> Option<f32> {
  let grid = ColumnGrid::read(workspace);

  if grid.window_count() < 2 {
    return None;
  }

  // Use window_order[0] to find the center column index.
  let center_id = workspace.window_order().first().copied()?;
  let (col, _) = grid.find(center_id)?;
  grid.widths.get(col).copied()
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
  spec
    .split(',')
    .rev()
    .collect::<Vec<_>>()
    .join(",")
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

/// Resolves the workspace's `window_order` buffer into live
/// `TilingWindow` objects, preserving creation order. IDs that no longer
/// exist in the tree (e.g. already unmanaged) are silently skipped.
fn resolve_ordered_windows(workspace: &Workspace) -> Vec<TilingWindow> {
  let order = workspace.window_order();
  let all_windows: Vec<TilingWindow> = workspace
    .descendant_focus_order()
    .filter_map(|c| match c {
      Container::TilingWindow(w) => Some(w),
      _ => None,
    })
    .collect();

  order
    .iter()
    .filter_map(|id| all_windows.iter().find(|w| w.id() == *id).cloned())
    .collect()
}

/// Toggles the workspace's column layout mode through the cycle
/// `MasterStackLeft → MasterStackRight → Grid → MasterStackLeft`, then
/// reapplies. Grid mode requires ≥ 4 windows and auto-falls back to
/// master-stack-left when that threshold isn't met.
pub fn toggle_columns_mode(
  workspace: &Workspace,
  forced: Option<ColumnsMode>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let new_mode = forced.unwrap_or_else(|| {
    match workspace.columns_mode() {
      ColumnsMode::MasterStackLeft => ColumnsMode::MasterStackRight,
      ColumnsMode::MasterStackRight => ColumnsMode::Grid,
      ColumnsMode::Grid => ColumnsMode::MasterStackLeft,
    }
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

  // Windows loop clockwise around the center column: the columns left of
  // center are traversed bottom-to-top (so `ring_slots` lists them
  // reversed), then the center and right columns top-to-bottom.
  let center = grid.center_index();

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

  ColumnGrid {
    columns,
    widths: grid.widths,
  }
  .render(workspace, state, config)?;

  if let Some(id) = focus_target {
    focus_container_by_id(&id, state)?;
  }

  Ok(())
}

/// Swaps a window into the center slot — the top of the widest column.
///
/// When the focused window is *not* the center, it swaps into the center
/// and the old center takes its place; focus follows into the center. When
/// the focused window *is* the center, it swaps back with the window most
/// recently phased out of the center (`state.last_centered_out`), so
/// pressing `center` repeatedly toggles between two windows. That target
/// is maintained wherever the center changes — including a freshly opened
/// window auto-centering (see `remember_outgoing_center`) — so the toggle
/// can return to the previous window even when it wasn't this command that
/// centered the current one. The window leaving the center is remembered
/// as the next toggle target, and whichever window lands in the center
/// gains focus. A no-op if there is no toggle target, it no longer exists,
/// or there is nothing to swap.
pub fn apply_center(
  workspace: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let mut grid = ColumnGrid::read(workspace);

  if grid.window_count() < 2 {
    return Ok(());
  }

  let center = grid.center_index();
  if grid.columns[center].is_empty() {
    return Ok(());
  }

  let Some(focused_id) = focused_window_id(workspace) else {
    return Ok(());
  };

  let center_id = grid.columns[center][0].id();

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

  // Remember the window pushed out of the center as the next toggle
  // target, and move focus with the window that landed in the center.
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
/// column, `Left`/`Right` with the nearest window (by row) in the adjacent
/// column. Swapping keeps every column's window count fixed, so the center
/// column — a single-window slot — always stays exactly one window: moving
/// a side window into the center displaces the old center out to the
/// vacated side slot, and moving the center window out promotes the side
/// window it swaps with into the center. The result is rendered directly
/// through `ColumnGrid::render`, so the declarative columns stay intact
/// and the spec is not re-derived from geometry, and focus follows the
/// moved window.
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
      // Swap with the nearest window in the target column so both columns
      // keep their window counts — the center stays a single window.
      let target_row =
        row.min(grid.columns[target_col].len().saturating_sub(1));
      let moved = grid.columns[col][row].clone();
      grid.columns[col][row] =
        grid.columns[target_col][target_row].clone();
      grid.columns[target_col][target_row] = moved;
    }
  }

  grid.render(&workspace, state, config)?;
  focus_container_by_id(&tiling.id(), state)?;

  Ok(true)
}

/// Focuses the spatially adjacent window within a workspace's column
/// grid, returning whether the focus was handled here.
///
/// `Up`/`Down` moves to the window above/below in the same column.
/// `Left`/`Right` moves to the nearest window (by row) in the adjacent
/// column. At a column edge, returns `false` so the caller can fall
/// through to cross-monitor focus.
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
    Direction::Up if row > 0 => grid.columns[col][row - 1].id(),
    Direction::Down if row + 1 < grid.columns[col].len() => {
      grid.columns[col][row + 1].id()
    }
    Direction::Left if col > 0 => {
      let target_row =
        row.min(grid.columns[col - 1].len().saturating_sub(1));
      grid.columns[col - 1][target_row].id()
    }
    Direction::Right if col + 1 < grid.columns.len() => {
      let target_row =
        row.min(grid.columns[col + 1].len().saturating_sub(1));
      grid.columns[col + 1][target_row].id()
    }
    // Edge of grid — let caller handle cross-monitor focus.
    _ => return Ok(false),
  };

  focus_container_by_id(&target_id, state)?;
  state.pending_sync.queue_focus_change().queue_cursor_jump();

  Ok(true)
}

#[cfg(test)]
mod tests {
  use uuid::Uuid;
  use wm_common::{ColumnBias, ColumnsMode, ParsedConfig};
  use wm_platform::{Direction, Rect};

  use super::{
    apply_center, apply_columns, apply_grid, apply_rotate,
    assign_columns, effective_columns, focused_window_id,
    grid::ColumnGrid, move_window_in_columns, reapply_assigned_columns,
    store_center_width, toggle_columns_mode, unassign_columns,
    workspace_center_window_id,
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

  fn setup(window_count: usize) -> (WmState, Workspace, Vec<TilingWindow>) {
    let state = mock_wm_state();
    let windows = (0..window_count).map(|_| TilingWindow::mock().call()).collect::<Vec<_>>();
    let workspace = Workspace::mock()
      .tiling_containers(windows.iter().cloned().map(TilingContainer::TilingWindow).collect())
      .call();
    for window in &windows { workspace.push_window_order(window.id()); }
    let monitor = Monitor::mock().workspaces(vec![workspace.clone()]).call();
    attach_container(&monitor.into(), &state.root_container.clone().into(), None).unwrap();
    (state, workspace, windows)
  }

  fn add_monitor(state: &WmState, bounds: Rect, count: usize) -> (Workspace, Vec<TilingWindow>) {
    let windows = (0..count).map(|_| TilingWindow::mock().call()).collect::<Vec<_>>();
    let workspace = Workspace::mock()
      .tiling_containers(windows.iter().cloned().map(TilingContainer::TilingWindow).collect())
      .call();
    for window in &windows { workspace.push_window_order(window.id()); }
    let monitor = Monitor::mock().bounds(bounds).workspaces(vec![workspace.clone()]).call();
    attach_container(&monitor.into(), &state.root_container.clone().into(), None).unwrap();
    (workspace, windows)
  }

  fn config_with_default_columns(spec: &str) -> UserConfig {
    let yaml = format!("general:\n  default_columns:\n    - min_aspect_ratio: 1.5\n      spec: '{spec}'\n");
    UserConfig::mock(serde_yaml::from_str::<ParsedConfig>(&yaml).unwrap())
  }

  fn id_grid(workspace: &Workspace) -> Vec<Vec<Uuid>> {
    ColumnGrid::read(workspace).columns.iter().map(|col| col.iter().map(CommonGetters::id).collect()).collect()
  }

  #[test]
  fn applies_star_center_star_layout() {
    let (mut state, workspace, windows) = setup(5);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(&workspace, "*,C,*", 0.6, &ColumnBias::Left, &mut state, &mock_user_config()).unwrap();
    assert_eq!(id_grid(&workspace), vec![vec![ids[1], ids[2]], vec![ids[0]], vec![ids[3], ids[4]]]);
    let grid = ColumnGrid::read(&workspace);
    assert_eq!(grid.center_index(), 1);
    assert!((grid.widths[grid.center_index()] - 0.6).abs() < 1e-3);
  }

  #[test]
  fn applies_fixed_column_layout() {
    let (mut state, workspace, windows) = setup(6);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(&workspace, "2,C,*", 0.6, &ColumnBias::Left, &mut state, &mock_user_config()).unwrap();
    assert_eq!(id_grid(&workspace), vec![vec![ids[1], ids[2]], vec![ids[0]], vec![ids[3], ids[4], ids[5]]]);
  }

  #[test]
  fn rotates_windows_clockwise_keeping_shape() {
    let (mut state, workspace, windows) = setup(5);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = mock_user_config();
    apply_columns(&workspace, "*,C,*", 0.6, &ColumnBias::Left, &mut state, &config).unwrap();
    apply_rotate(&workspace, false, &mut state, &config).unwrap();
    assert_eq!(id_grid(&workspace), vec![vec![ids[2], ids[4]], vec![ids[1]], vec![ids[0], ids[3]]]);
  }

  #[test]
  fn rotates_windows_counter_clockwise() {
    let (mut state, workspace, windows) = setup(5);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = mock_user_config();
    apply_columns(&workspace, "*,C,*", 0.6, &ColumnBias::Left, &mut state, &config).unwrap();
    apply_rotate(&workspace, true, &mut state, &config).unwrap();
    assert_eq!(id_grid(&workspace), vec![vec![ids[0], ids[1]], vec![ids[3]], vec![ids[4], ids[2]]]);
  }

  #[test]
  fn center_swaps_focused_then_toggles_back() {
    let (mut state, workspace, windows) = setup(5);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let config = mock_user_config();
    apply_columns(&workspace, "*,C,*", 0.6, &ColumnBias::Left, &mut state, &config).unwrap();

    focus_container_by_id(&ids[1], &mut state).unwrap();
    apply_center(&workspace, &mut state, &config).unwrap();
    assert_eq!(id_grid(&workspace), vec![vec![ids[0], ids[2]], vec![ids[1]], vec![ids[3], ids[4]]]);
    assert_eq!(focused_window_id(&workspace), Some(ids[1]));

    apply_center(&workspace, &mut state, &config).unwrap();
    assert_eq!(id_grid(&workspace), vec![vec![ids[1], ids[2]], vec![ids[0]], vec![ids[3], ids[4]]]);
    assert_eq!(focused_window_id(&workspace), Some(ids[0]));
  }

  #[test]
  fn moves_window_within_column() {
    let (mut state, workspace, windows) = setup(5);
    let config = config_with_default_columns("*,C,*");
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(&workspace, "*,C,*", 0.6, &ColumnBias::Left, &mut state, &config).unwrap();
    let window = WindowContainer::TilingWindow(windows[2].clone());
    assert!(move_window_in_columns(&window, &Direction::Up, &mut state, &config).unwrap());
    assert_eq!(id_grid(&workspace), vec![vec![ids[2], ids[1]], vec![ids[0]], vec![ids[3], ids[4]]]);
  }

  #[test]
  fn moves_into_center_keeps_it_single_window() {
    let (mut state, workspace, windows) = setup(5);
    let config = config_with_default_columns("*,C,*");
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(&workspace, "*,C,*", 0.6, &ColumnBias::Left, &mut state, &config).unwrap();
    let window = WindowContainer::TilingWindow(windows[1].clone());
    assert!(move_window_in_columns(&window, &Direction::Right, &mut state, &config).unwrap());
    assert_eq!(id_grid(&workspace), vec![vec![ids[0], ids[2]], vec![ids[1]], vec![ids[3], ids[4]]]);
  }

  #[test]
  fn moves_out_to_horizontally_adjacent_monitor() {
    let mut state = mock_wm_state();
    let config = config_with_default_columns("*,C,*");
    let (ws_left, wins_left) = add_monitor(&state, Rect::from_xy(0, 0, 1680, 1050), 5);
    let (ws_right, _) = add_monitor(&state, Rect::from_xy(1680, 0, 1680, 1050), 2);
    let ids = wins_left.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(&ws_left, "*,C,*", 0.6, &ColumnBias::Left, &mut state, &config).unwrap();
    let window = WindowContainer::TilingWindow(wins_left[3].clone());
    assert!(move_window_in_columns(&window, &Direction::Right, &mut state, &config).unwrap());
    assert_eq!(wins_left[3].workspace().map(|w| w.id()), Some(ws_right.id()));
    assert_eq!(workspace_center_window_id(&ws_left), Some(ids[0]));
  }

  #[test]
  fn moves_out_to_vertically_adjacent_monitor() {
    let mut state = mock_wm_state();
    let config = config_with_default_columns("*,C,*");
    let (ws_top, wins_top) = add_monitor(&state, Rect::from_xy(0, 0, 1680, 1050), 5);
    let (ws_bottom, _) = add_monitor(&state, Rect::from_xy(0, 1050, 1680, 1050), 2);
    let ids = wins_top.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_columns(&ws_top, "*,C,*", 0.6, &ColumnBias::Left, &mut state, &config).unwrap();
    let window = WindowContainer::TilingWindow(wins_top[2].clone());
    assert!(move_window_in_columns(&window, &Direction::Down, &mut state, &config).unwrap());
    assert_eq!(wins_top[2].workspace().map(|w| w.id()), Some(ws_bottom.id()));
    assert_eq!(workspace_center_window_id(&ws_top), Some(ids[0]));
  }

  #[test]
  fn workspace_recolumns_when_moved_to_monitor_with_different_aspect() {
    let mut state = mock_wm_state();
    let yaml = "general:\n  default_columns:\n    - min_aspect_ratio: 2.1\n      spec: '*,C,*'\n    - min_aspect_ratio: 1.5\n      spec: 'C,*'\n";
    let config = UserConfig::mock(serde_yaml::from_str::<ParsedConfig>(yaml).unwrap());
    let (workspace, _) = add_monitor(&state, Rect::from_xy(0, 0, 3440, 1440), 3);
    let ultrawide = workspace.monitor().unwrap();
    let filler = Workspace::mock().name("filler".to_string()).call();
    attach_container(&filler.into(), &ultrawide.clone().into(), None).unwrap();
    add_monitor(&state, Rect::from_xy(3440, 0, 1920, 1080), 0);
    reapply_assigned_columns(&workspace, &mut state, &config).unwrap();
    assert_eq!(ColumnGrid::read(&workspace).columns.len(), 3);
    move_workspace_in_direction(&workspace, &Direction::Right, &mut state, &config).unwrap();
    assert_eq!(effective_columns(&workspace, &config).unwrap().map(|c| c.spec), Some("C,*".to_string()));
    assert_eq!(ColumnGrid::read(&workspace).columns.len(), 2);
  }

  #[test]
  fn move_without_columns_is_not_handled() {
    let (mut state, _, windows) = setup(3);
    let window = WindowContainer::TilingWindow(windows[0].clone());
    assert!(!move_window_in_columns(&window, &Direction::Left, &mut state, &mock_user_config()).unwrap());
  }

  #[test]
  fn effective_columns_uses_default_when_unassigned() {
    let (_, workspace, _) = setup(2);
    assert_eq!(effective_columns(&workspace, &config_with_default_columns("C,*")).unwrap().map(|c| c.spec), Some("C,*".to_string()));
  }

  #[test]
  fn effective_columns_prefers_assignment() {
    let (mut state, workspace, _) = setup(2);
    let config = config_with_default_columns("*,C,*");
    assign_columns(&workspace, "1,C", 0.6, &ColumnBias::Left, &mut state, &config).unwrap();
    assert_eq!(effective_columns(&workspace, &config).unwrap().map(|c| c.spec), Some("1,C".to_string()));
  }

  #[test]
  fn unassign_clears_assignment() {
    let (mut state, workspace, _) = setup(2);
    assign_columns(&workspace, "*,C,*", 0.6, &ColumnBias::Left, &mut state, &mock_user_config()).unwrap();
    assert!(workspace.config().columns.is_some());
    unassign_columns(&workspace);
    assert!(workspace.config().columns.is_none());
  }

  #[test]
  fn reports_center_window_id() {
    let (_, workspace, windows) = setup(5);
    assert_eq!(workspace_center_window_id(&workspace), Some(windows[0].id()));
  }

  #[test]
  fn store_center_width_records_resize() {
    let (mut state, workspace, windows) = setup(5);
    let config = mock_user_config();
    assign_columns(&workspace, "*,C,*", 0.6, &ColumnBias::Left, &mut state, &config).unwrap();
    let center_id = workspace_center_window_id(&workspace).unwrap();
    let center = windows.iter().find(|w| w.id() == center_id).unwrap();
    center.set_tiling_size(0.7);
    store_center_width(&workspace);
    assert!((workspace.config().columns.unwrap().center - 0.7).abs() < 1e-3);
  }

  #[test]
  fn window_order_appends_to_end() {
    let (_, workspace, windows) = setup(3);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    assert_eq!(workspace.window_order(), vec![ids[0], ids[1], ids[2]]);
  }

  #[test]
  fn window_order_remove_preserves_order() {
    let (_, workspace, windows) = setup(4);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    workspace.remove_from_window_order(ids[1]);
    assert_eq!(workspace.window_order(), vec![ids[0], ids[2], ids[3]]);
  }

  #[test]
  fn lifo_focus_after_close() {
    let (_, workspace, windows) = setup(4);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    workspace.remove_from_window_order(ids[2]);
    assert_eq!(workspace.window_order().last().copied(), Some(ids[3]));
    workspace.remove_from_window_order(ids[3]);
    assert_eq!(workspace.window_order().last().copied(), Some(ids[1]));
  }

  #[test]
  fn grid_distributes_round_robin() {
    let (mut state, workspace, windows) = setup(4);
    let ids = windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    apply_grid(&workspace, 2, &mut state, &mock_user_config()).unwrap();
    assert_eq!(id_grid(&workspace), vec![vec![ids[0], ids[2]], vec![ids[1], ids[3]]]);
    let grid = ColumnGrid::read(&workspace);
    assert!((grid.widths[0] - 0.5).abs() < 1e-3);
  }

  #[test]
  fn grid_requires_four_windows() {
    let (mut state, workspace, _) = setup(3);
    apply_grid(&workspace, 2, &mut state, &mock_user_config()).unwrap();
    assert_eq!(ColumnGrid::read(&workspace).columns.len(), 3);
  }

  #[test]
  fn toggle_columns_mode_cycles() {
    let (mut state, workspace, _) = setup(5);
    let config = config_with_default_columns("C,*");
    assert_eq!(workspace.columns_mode(), ColumnsMode::MasterStackLeft);
    toggle_columns_mode(&workspace, None, &mut state, &config).unwrap();
    assert_eq!(workspace.columns_mode(), ColumnsMode::MasterStackRight);
    toggle_columns_mode(&workspace, None, &mut state, &config).unwrap();
    assert_eq!(workspace.columns_mode(), ColumnsMode::Grid);
    toggle_columns_mode(&workspace, None, &mut state, &config).unwrap();
    assert_eq!(workspace.columns_mode(), ColumnsMode::MasterStackLeft);
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
  fn grid_affinity_places_newest_in_focused_column() {
    // Start with 3 windows in armed-grid, then add a 4th.
    // Without affinity: round-robin gives [0,2] [1,3].
    // With affinity on window 0: expect [0,3] [1,2] — the
    // newest (3) lands in window 0's column.
    let (mut state, workspace, windows) = setup(4);
    let ids: Vec<Uuid> =
      windows.iter().map(CommonGetters::id).collect();

    // Set affinity to window 0 (simulating manage_window
    // setting it to the previously focused window).
    workspace.set_grid_affinity(Some(ids[0]));
    apply_grid(
      &workspace,
      2,
      &mut state,
      &mock_user_config(),
    )
    .unwrap();

    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0], ids[3]], vec![ids[1], ids[2]]]
    );
  }

  #[test]
  fn grid_no_affinity_is_normal_round_robin() {
    // Without affinity the layout is plain round-robin.
    let (mut state, workspace, windows) = setup(4);
    let ids: Vec<Uuid> =
      windows.iter().map(CommonGetters::id).collect();

    apply_grid(
      &workspace,
      2,
      &mut state,
      &mock_user_config(),
    )
    .unwrap();

    assert_eq!(
      id_grid(&workspace),
      vec![vec![ids[0], ids[2]], vec![ids[1], ids[3]]]
    );
  }

  #[test]
  fn grid_affinity_noop_when_same_column() {
    // When the affinity target is already in the newest
    // window's column, no swap occurs.
    let (mut state, workspace, windows) = setup(5);
    let ids: Vec<Uuid> =
      windows.iter().map(CommonGetters::id).collect();

    // Window 4 (last, idx 4) goes to col 0 via round-robin
    // (4 % 2 == 0). Set affinity to window 0 (also col 0).
    workspace.set_grid_affinity(Some(ids[0]));
    apply_grid(
      &workspace,
      2,
      &mut state,
      &mock_user_config(),
    )
    .unwrap();

    // Same as normal round-robin — no swap needed.
    assert_eq!(
      id_grid(&workspace),
      vec![
        vec![ids[0], ids[2], ids[4]],
        vec![ids[1], ids[3]]
      ]
    );
  }
}
