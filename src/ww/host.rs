//! What the API handlers need from the compositor. Handlers run as tasks on the loop without
//! access to its state; they hand a closure to the loop ([Run]), which calls it with `&mut` access
//! when idle, as niri's IPC does. Tests use a fake host instead of niri's `State`.

use std::rc::Rc;

use async_channel::Receiver;
use calloop::LoopHandle;
use ww_niri_api::{OutputOwned, Transition, WindowOwned};

pub trait WwHost: 'static {
    fn windows(&mut self) -> Vec<WindowOwned>;
    fn window(&mut self, id: u64) -> Option<WindowOwned>;
    fn outputs(&mut self) -> Vec<OutputOwned>;

    /// Each returns false when there is no such window.
    fn focus(&mut self, id: u64) -> bool;
    fn show_window(
        &mut self,
        id: u64,
        transition: Option<Transition>,
        fullscreen: Option<bool>,
    ) -> bool;
    fn close_window(&mut self, id: u64) -> bool;
    fn set_fullscreen(&mut self, id: u64, on: bool) -> bool;

    /// What every connection shares.
    fn services(&mut self) -> Rc<super::Services>;

    /// niri's IPC event stream, without the replay of the current state; `None` when niri runs
    /// without IPC.
    fn subscribe_events(&mut self) -> Option<Receiver<niri_ipc::Event>>;
}

/// Runs a closure on the loop with the host.
pub type Run = Rc<dyn Fn(Box<dyn FnOnce(&mut dyn WwHost)>)>;

pub fn runner<D: WwHost>(handle: LoopHandle<'static, D>) -> Run {
    Rc::new(move |f| {
        handle.insert_idle(move |host: &mut D| f(host));
    })
}

/// Calls `f` on the loop and waits for its answer; `None` if the loop went away first.
pub async fn call<R: 'static>(
    run: &Run,
    f: impl FnOnce(&mut dyn WwHost) -> R + 'static,
) -> Option<R> {
    let (tx, rx) = async_channel::bounded(1);
    run(Box::new(move |host| {
        let _ = tx.try_send(f(host));
    }));
    rx.recv().await.ok()
}

/// niri's animation for an API transition.
pub fn animation(transition: Transition) -> niri_config::Animation {
    use niri_config::animations::{Curve as NiriCurve, EasingParams, Kind};
    use ww_niri_api::Curve;

    let curve = match transition.curve {
        Curve::Linear => NiriCurve::Linear,
        Curve::EaseOutQuad => NiriCurve::EaseOutQuad,
        // A curve from a newer client: the closest one this side knows.
        Curve::EaseOutCubic | Curve::Unknown(_) => NiriCurve::EaseOutCubic,
        Curve::EaseOutExpo => NiriCurve::EaseOutExpo,
    };
    niri_config::Animation {
        off: false,
        kind: Kind::Easing(EasingParams {
            duration_ms: u32::from(transition.duration.get()),
            curve,
        }),
    }
}

mod niri_state {
    use smithay::desktop::Window;
    use smithay::output::Output;
    use ww_niri_api::{OutputOwned, Transition, WindowId, WindowOwned, WorkspaceId};

    use super::WwHost;
    use crate::layout::LayoutElement as _;
    use crate::niri::State;
    use crate::utils::with_toplevel_role;
    use crate::window::mapped::MappedId;
    use crate::window::Mapped;

    fn make_window(
        mapped: &Mapped,
        output: Option<&Output>,
        workspace: Option<crate::layout::workspace::WorkspaceId>,
    ) -> WindowOwned {
        let (title, app_id) = with_toplevel_role(mapped.toplevel(), |role| {
            (role.title.clone(), role.app_id.clone())
        });
        WindowOwned {
            id: WindowId(mapped.id().get()),
            app_id,
            title,
            pid: mapped.credentials().map(|c| c.pid as u32),
            workspace: workspace.map(|id| WorkspaceId(id.get())),
            output: output.map(|o| o.name()),
            focused: mapped.is_focused(),
            fullscreen: mapped.sizing_mode().is_fullscreen(),
            browser: None,
        }
    }

    impl State {
        fn ww_find(&self, id: u64) -> Option<Window> {
            self.niri.find_window_by_id(MappedId::specific(id))
        }
    }

    impl WwHost for State {
        fn windows(&mut self) -> Vec<WindowOwned> {
            let mut windows = Vec::new();
            self.niri
                .layout
                .with_windows(|mapped, output, workspace, _| {
                    windows.push(make_window(mapped, output, workspace));
                });
            windows
        }

        fn window(&mut self, id: u64) -> Option<WindowOwned> {
            let mut found = None;
            self.niri
                .layout
                .with_windows(|mapped, output, workspace, _| {
                    if mapped.id().get() == id {
                        found = Some(make_window(mapped, output, workspace));
                    }
                });
            found
        }

        fn outputs(&mut self) -> Vec<OutputOwned> {
            let focused = self.niri.layout.active_output().map(|o| o.name());
            let outputs = self.backend.ipc_outputs();
            let outputs = outputs.lock().unwrap();
            outputs
                .values()
                .map(|o| {
                    let mode = o.current_mode.and_then(|i| o.modes.get(i));
                    let scale = o.logical.as_ref().map_or(1., |l| l.scale);
                    OutputOwned {
                        name: o.name.clone(),
                        make: o.make.clone(),
                        model: o.model.clone(),
                        width: mode.map_or(0, |m| m.width).into(),
                        height: mode.map_or(0, |m| m.height).into(),
                        refresh: mode.map_or(0, |m| m.refresh_rate).into(),
                        scale: ((scale * 100.).round() as u16).into(),
                        focused: focused.as_deref() == Some(o.name.as_str()),
                    }
                })
                .collect()
        }

        fn focus(&mut self, id: u64) -> bool {
            let Some(window) = self.ww_find(id) else {
                return false;
            };
            self.focus_window(&window);
            true
        }

        fn show_window(
            &mut self,
            id: u64,
            transition: Option<Transition>,
            fullscreen: Option<bool>,
        ) -> bool {
            let Some(window) = self.ww_find(id) else {
                return false;
            };
            self.focus_window_with_anim_config(&window, transition.map(super::animation));
            if let Some(on) = fullscreen {
                self.niri.layout.set_fullscreen(&window, on);
                self.niri.queue_redraw_all();
            }
            true
        }

        fn close_window(&mut self, id: u64) -> bool {
            match self.niri.find_mapped_output_by_id(MappedId::specific(id)) {
                Some((mapped, _)) => {
                    mapped.toplevel().send_close();
                    true
                }
                None => false,
            }
        }

        fn set_fullscreen(&mut self, id: u64, on: bool) -> bool {
            let Some(window) = self.ww_find(id) else {
                return false;
            };
            self.niri.layout.set_fullscreen(&window, on);
            self.niri.queue_redraw_all();
            true
        }

        fn services(&mut self) -> std::rc::Rc<crate::ww::Services> {
            self.niri.ww.services()
        }

        fn subscribe_events(&mut self) -> Option<async_channel::Receiver<niri_ipc::Event>> {
            Some(self.niri.ipc_server.as_ref()?.subscribe_internal())
        }
    }
}
