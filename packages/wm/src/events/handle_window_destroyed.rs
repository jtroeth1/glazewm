use anyhow::Context;
use tracing::{debug, info};
use wm_platform::WindowId;

use crate::{
  commands::{
    container::set_focused_descendant,
    window::unmanage_window,
    workspace::{
      column_neighbor_of, deactivate_workspace, effective_columns,
      reapply_assigned_columns, workspace_center_window_id,
    },
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

    // Note the current center before unmanaging so the reapply keeps it
    // in place — closing a side window shouldn't move the center window.
    let center = workspace_center_window_id(&workspace);

    // When columns are active, find the adjacent window in the same
    // column so focus goes to a spatial neighbor rather than the most
    // recently focused window (which is often the center).
    let neighbor_id =
      if effective_columns(&workspace, config)?.is_some() {
        let nid = column_neighbor_of(&workspace, window.id());
        debug!(
          "destroyed: columns active, neighbor_id={:?} for window={}",
          nid,
          window.id()
        );
        nid
      } else {
        debug!("destroyed: no columns active");
        None
      };

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
      reapply_assigned_columns(&workspace, center, state, config)?;

      // Focus the column neighbor (spatial adjacency) if available,
      // otherwise keep whatever unmanage_window chose.
      let focus_target = neighbor_id
        .and_then(|id| state.container_by_id(id))
        .or_else(|| state.focused_container());
      debug!(
        "destroyed: after reapply, focus_target={:?}, focused_container={:?}",
        focus_target.as_ref().map(CommonGetters::id),
        state.focused_container().map(|c| c.id())
      );
      if let Some(target) = focus_target {
        set_focused_descendant(&target, None);
        state.pending_sync.queue_focus_change();
        debug!(
          "destroyed: set focus to {:?}, focused_container now={:?}",
          target.id(),
          state.focused_container().map(|c| c.id())
        );
      }
    }
  }

  Ok(())
}
