//! Hidden windows (`windows.hide` of the control API): a window taken out of every workspace,
//! kept alive and mapped, but never drawn, focused or reached by workspace switches or the
//! overview. `windows.show` puts it back on the active workspace of the active output.
//!
//! A hidden window keeps its tile, as an interactively moved one does: lookups by surface or id,
//! commits and the window's close still find it, and IPC lists it with no workspace and no output.

use super::monitor::MonitorAddWindowTarget;
use super::workspace::{Workspace, WorkspaceAddWindowTarget};
use super::{
    ActivateWindow, InteractiveMoveState, Layout, LayoutElement, MonitorSet, RemovedTile,
    Transaction,
};

impl<W: LayoutElement> Layout<W> {
    /// Takes the window out of its workspace and keeps it hidden; false when the layout has no
    /// such window in a workspace (already hidden, interactively moved, or unknown).
    pub fn ww_hide_window(&mut self, window: &W::Id) -> bool {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return false;
            }
        }
        if self.ww_is_hidden(window) {
            return false;
        }
        let in_workspace = self.workspaces().any(|(_, _, ws)| ws.has_window(window));
        if !in_workspace {
            return false;
        }
        let Some(mut removed) = self.remove_window(window, Transaction::new()) else {
            return false;
        };
        removed.tile.stop_move_animations();
        self.hidden.push(removed);
        true
    }

    /// Puts a hidden window back on the active workspace of the active output, focused; false
    /// when it is not hidden.
    pub fn ww_unhide_window(&mut self, window: &W::Id) -> bool {
        let Some(removed) = self.ww_take_hidden(window) else {
            return false;
        };
        let RemovedTile {
            tile,
            width,
            is_full_width,
            is_floating,
        } = removed;
        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                monitors[*active_monitor_idx].add_tile(
                    tile,
                    MonitorAddWindowTarget::Auto,
                    ActivateWindow::Yes,
                    true,
                    width,
                    is_full_width,
                    is_floating,
                    None,
                );
            }
            MonitorSet::NoOutputs { workspaces } => {
                if workspaces.is_empty() {
                    workspaces.push(Workspace::new_no_outputs(
                        self.clock.clone(),
                        self.options.clone(),
                    ));
                }
                workspaces[0].add_tile(
                    tile,
                    WorkspaceAddWindowTarget::Auto,
                    ActivateWindow::Yes,
                    width,
                    is_full_width,
                    is_floating,
                    None,
                );
            }
        }
        true
    }

    pub fn ww_is_hidden(&self, window: &W::Id) -> bool {
        self.hidden.iter().any(|h| h.tile.window().id() == window)
    }

    /// Removes the window from the hidden ones and hands its tile back.
    pub(super) fn ww_take_hidden(&mut self, window: &W::Id) -> Option<RemovedTile<W>> {
        let idx = self
            .hidden
            .iter()
            .position(|h| h.tile.window().id() == window)?;
        Some(self.hidden.remove(idx))
    }

    pub(super) fn ww_hidden_tiles(&self) -> impl Iterator<Item = &super::tile::Tile<W>> + '_ {
        self.hidden.iter().map(|h| &h.tile)
    }

    pub(super) fn ww_hidden_tiles_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut super::tile::Tile<W>> + '_ {
        self.hidden.iter_mut().map(|h| &mut h.tile)
    }
}
