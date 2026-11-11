use super::*;
use crate::ww::WwHost;

#[test]
fn host_windows_focus_fullscreen() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let mut surfaces = Vec::new();
    for title in ["first", "second"] {
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        window.set_title(title);
        window.commit();
        f.roundtrip(id);

        let window = f.client(id).window(&surface);
        window.attach_new_buffer();
        window.ack_last_and_commit();
        f.double_roundtrip(id);
        surfaces.push(surface);
    }

    let windows = f.niri_state().windows();
    let titles: Vec<_> = windows.iter().map(|w| w.title.clone().unwrap()).collect();
    assert_eq!(titles, ["first", "second"]);
    // A new window takes the focus.
    let first = windows[0].id.0;
    let second = windows[1].id.0;
    assert!(!windows[0].focused && windows[1].focused);
    assert!(windows.iter().all(|w| !w.fullscreen && w.output.is_some()));

    assert!(f.niri_state().focus(first));
    // Keyboard focus follows on niri's next refresh.
    f.double_roundtrip(id);
    assert!(f.niri_state().window(first).unwrap().focused);

    assert!(f.niri_state().set_fullscreen(second, true));
    f.double_roundtrip(id);
    f.client(id).window(&surfaces[1]).ack_last_and_commit();
    f.double_roundtrip(id);
    assert!(f.niri_state().window(second).unwrap().fullscreen);

    assert!(!f.niri_state().focus(9999));
    assert!(!f.niri_state().close_window(9999));
    assert!(f.niri_state().window(9999).is_none());
}

#[test]
fn host_show_uses_transition() {
    use std::time::Duration;

    use exact_units::Millis;
    use ww_niri_api::{Curve, Transition};

    use super::animations::set_time;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // The window on the second workspace, the first one in view.
    f.niri().layout.move_to_workspace_down(false);
    f.niri_complete_animations();
    let mon = f.niri().layout.active_monitor_ref().unwrap();
    assert_eq!(mon.active_workspace_idx(), 0);
    let wid = f.niri_state().windows()[0].id.0;

    set_time(f.niri(), Duration::ZERO);
    let transition = Transition {
        duration: Millis::new(1000),
        curve: Curve::Linear,
    };
    assert!(f.niri_state().show_window(wid, Some(transition), None));

    let mut render_idx_at = |ms| {
        set_time(f.niri(), Duration::from_millis(ms));
        f.niri().advance_animations();
        let mon = f.niri().layout.active_monitor_ref().unwrap();
        (mon.active_workspace_idx(), mon.workspace_render_idx())
    };
    let (active, idx) = render_idx_at(500);
    assert_eq!(active, 1);
    assert!((idx - 0.5).abs() < 1e-6, "halfway at 500 ms, got {idx}");
    // Done at 1000 ms; niri then drops the empty workspace above, so the indices move up.
    let (active, idx) = render_idx_at(1000);
    assert_eq!(idx, active as f64);
    assert!(!f.niri().layout.are_animations_ongoing(None));
}
