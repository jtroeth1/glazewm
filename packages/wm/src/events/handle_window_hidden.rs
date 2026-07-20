use tracing::{debug, info};
use wm_common::{DisplayState, HideMethod};
use wm_platform::NativeWindow;

use crate::{
  commands::{
    container::set_focused_descendant,
    window::unmanage_window,
    workspace::{
      column_neighbor_of, effective_columns, reapply_assigned_columns,
      workspace_center_window_id,
    },
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

      // Note the current center before unmanaging so the reapply keeps
      // it in place — closing a side window shouldn't move the center.
      let center = workspace.as_ref().and_then(workspace_center_window_id);

      // When columns are active, find the adjacent window in the same
      // column so focus goes to a spatial neighbor.
      let neighbor_id = workspace.as_ref().and_then(|ws| {
        let has_cols = effective_columns(ws, config)
          .ok()
          .flatten()
          .is_some();
        let nid = if has_cols {
          column_neighbor_of(ws, window.id())
        } else {
          None
        };
        debug!(
          "hidden: columns={has_cols}, neighbor_id={nid:?} for window={}",
          window.id()
        );
        nid
      });

      unmanage_window(window, state)?;

      // Re-tidy the workspace's columns (if any) now a window's gone.
      if let Some(workspace) = workspace {
        reapply_assigned_columns(&workspace, center, state, config)?;

        let focus_target = neighbor_id
          .and_then(|id| state.container_by_id(id))
          .or_else(|| state.focused_container());
        debug!(
          "hidden: after reapply, focus_target={:?}, focused_container={:?}",
          focus_target.as_ref().map(CommonGetters::id),
          state.focused_container().map(|c| c.id())
        );
        if let Some(target) = focus_target {
          set_focused_descendant(&target, None);
          state.pending_sync.queue_focus_change();
          debug!(
            "hidden: set focus to {:?}, focused_container now={:?}",
            target.id(),
            state.focused_container().map(|c| c.id())
          );
        }
      }
    }
  }

  Ok(())
}
