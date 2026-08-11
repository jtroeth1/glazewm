use anyhow::Context;
use tracing::info;
use wm_platform::WindowId;

use crate::{
  commands::{
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

    // Give up the master designation before unmanaging, so the layout
    // promotes the first remaining window in on-screen order instead of
    // holding a dangling id.
    if workspace.master_window() == Some(window.id()) {
      workspace.set_master_window(None);
    }

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
      // `unmanage_window` has already moved focus to the most recently
      // focused survivor and the columns render preserves it.
      reapply_assigned_columns(&workspace, state, config)?;
    }
  }

  Ok(())
}
