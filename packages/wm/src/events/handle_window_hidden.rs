use tracing::info;
use wm_common::{DisplayState, HideMethod};
use wm_platform::NativeWindow;

use crate::{
  commands::{
    container::set_focused_descendant,
    window::unmanage_window,
    workspace::reapply_assigned_columns,
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

      // Remove from the LIFO order buffer before unmanaging.
      if let Some(ws) = workspace.as_ref() {
        ws.remove_from_window_order(window.id());
      }

      unmanage_window(window, state)?;

      // Re-tidy the workspace's columns (if any) now a window's gone.
      if let Some(workspace) = workspace {
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
  }

  Ok(())
}
