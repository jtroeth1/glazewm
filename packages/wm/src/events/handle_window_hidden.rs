use tracing::info;
use wm_common::{DisplayState, HideMethod};
use wm_platform::NativeWindow;

use crate::{
  commands::{
    window::unmanage_window, workspace::reapply_assigned_columns,
  },
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn handle_window_hidden(
  native_window: &NativeWindow,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let found_window = state.window_from_native(native_window);

  if let Some(window) = found_window {
    info!("Window hidden: {window}");

    // Update the display state.
    if config.value.general.hide_method != HideMethod::PlaceInCorner
      && window.display_state() == DisplayState::Hiding
    {
      window.set_display_state(DisplayState::Hidden);
      return Ok(());
    }

    // Unmanage the window if it's not in a display state transition. Also,
    // since window events are not 100% guaranteed to be in correct order,
    // we need to ignore events where the window is not actually hidden.
    if (config.value.general.hide_method == HideMethod::PlaceInCorner
      || window.display_state() == DisplayState::Shown)
      && !window.native().is_visible().unwrap_or(false)
    {
      let workspace = window.workspace();

      // Give up the master designation before unmanaging, so the layout
      // promotes the first remaining window in on-screen order instead of
      // holding a dangling id.
      if let Some(ws) = workspace.as_ref() {
        if ws.master_window() == Some(window.id()) {
          ws.set_master_window(None);
        }
      }

      unmanage_window(window, state)?;

      // Re-tidy the workspace's columns (if any) now a window's gone.
      // `unmanage_window` has already moved focus to the most recently
      // focused survivor and the columns render preserves it.
      if let Some(workspace) = workspace {
        reapply_assigned_columns(&workspace, state, config)?;
      }
    }
  }

  Ok(())
}
