use anyhow::Context;
use tracing::info;
use wm_platform::WindowId;

use crate::{
  commands::{
    container::set_focused_descendant,
    window::unmanage_window,
    workspace::{deactivate_workspace, reapply_assigned_columns},
  },
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn handle_window_destroyed(
  native_window_id: WindowId,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let found_window = state
    .windows()
    .into_iter()
    .find(|window| window.native().id() == native_window_id);

  // Unmanage the window if it's currently managed.
  if let Some(window) = found_window {
    let workspace = window.workspace().context("No workspace.")?;

    // Remove the window from the workspace's focus-order buffer
    // before unmanaging so the LIFO list stays consistent.
    workspace.remove_from_window_order(window.id());

    info!("Window closed: {window}");
    unmanage_window(window, state)?;

    // Destroy parent workspace if window was killed while its workspace
    // was not displayed (e.g. via task manager).
    if !workspace.config().keep_alive
      && !workspace.has_children()
      && !workspace.is_displayed()
    {
      deactivate_workspace(workspace, state)?;
    } else {
      // Re-tidy the workspace's columns (if any) now a window's gone.
      reapply_assigned_columns(&workspace, state, config)?;

      // Focus the last window in the LIFO order buffer.
      let focus_target = workspace
        .window_order()
        .last()
        .and_then(|id| state.container_by_id(*id))
        .or_else(|| state.focused_container());
      if let Some(target) = focus_target {
        set_focused_descendant(&target, None);
        state.pending_sync.queue_focus_change();
      }
    }
  }

  Ok(())
}
