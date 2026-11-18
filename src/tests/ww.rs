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

#[test]
fn host_window_points_and_pointer() {
    use smithay::utils::Point;
    use ww_niri_api::InputErrorOwned;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    for _ in 0..2 {
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        window.commit();
        f.roundtrip(id);
        let window = f.client(id).window(&surface);
        window.attach_new_buffer();
        window.set_size(300, 200);
        window.ack_last_and_commit();
        f.double_roundtrip(id);
    }
    let ids: Vec<u64> = f.niri_state().windows().iter().map(|w| w.id.0).collect();

    // A window point is where niri finds that window, at that point of its surface.
    let mut seen = Vec::new();
    for &window in &ids {
        let state = f.niri_state();
        let (x, y) = state.window_point(window, 5., 7.).unwrap();
        assert!(state.on_screen(x, y), "window {window} at ({x}, {y})");
        seen.push(x);

        state
            .niri
            .seat
            .get_pointer()
            .unwrap()
            .set_location(Point::from((x, y)));
        let position = state.pointer_position();
        let under = position.window.expect("a window under the pointer");
        assert_eq!(under.id.0, window);
        assert_eq!((under.x.get(), under.y.get()), (5, 7));
    }
    // Side by side, not on top of each other.
    assert_ne!(seen[0], seen[1]);

    let state = f.niri_state();
    assert_eq!(
        state.window_point(9999, 0., 0.),
        Err(InputErrorOwned::NoSuchWindow)
    );
    assert!(!state.on_screen(5000., 0.));
    // The configured keymap (us) has the keys.
    let a = state
        .key_for(smithay::input::keyboard::Keysym::A.raw())
        .unwrap();
    assert!(a.shift);
}

#[test]
fn host_relative_moves_and_pointer_notices() {
    use smithay::input::keyboard::Keysym;
    use smithay::utils::Point;

    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let state = f.niri_state();
    let set = |state: &mut crate::niri::State, x: f64, y: f64| {
        let pointer = state.niri.seat.get_pointer().unwrap();
        pointer.set_location(Point::from((x, y)));
    };

    set(state, 100., 100.);
    assert_eq!(state.pointer_moved_by(5., -3.), Ok((105., 97.)));
    // Past the output: stops at its edge.
    assert_eq!(state.pointer_moved_by(5000., -500.), Ok((1919., 0.)));

    // A motion niri has not read yet: moves go on from where it goes, until niri moved the
    // pointer.
    let at = Point::from((100., 100.));
    state.niri.ww.pointer_sent = Some(crate::ww::input::Sent::new((500., 300.), at));
    state.ww_refresh_pointer();
    assert_eq!(state.pointer_moved_by(1., 1.), Ok((501., 301.)));
    set(state, 500.2, 300.2);
    state.ww_refresh_pointer();
    assert!(state.niri.ww.pointer_sent.is_none());
    // Back where it was: from there, not from the old motion.
    set(state, 100., 100.);
    assert_eq!(state.pointer_moved_by(1., 1.), Ok((101., 101.)));

    // Observers hear of a move once, at the next refresh after it.
    let moved = state.services().hub.subscribe_pointer();
    state.ww_refresh_pointer();
    moved.try_recv().ok();
    state.ww_refresh_pointer();
    assert!(moved.try_recv().is_err(), "nothing moved");
    set(state, 300., 200.);
    state.ww_refresh_pointer();
    assert!(moved.try_recv().is_ok(), "the pointer moved");

    // The configured keymap (us) has the media and remote keys, on their evdev codes plus 8.
    for (sym, code) in [
        (Keysym::XF86_AudioPlay, 172),
        (Keysym::XF86_AudioMute, 121),
        (Keysym::XF86_AudioRaiseVolume, 123),
        (Keysym::XF86_Back, 166),
        (Keysym::XF86_HomePage, 180),
        (Keysym::XF86_Search, 225),
    ] {
        let key = state.key_for(sym.raw());
        assert_eq!(
            key.map(|k| (k.code, k.shift)),
            Some((code, false)),
            "{sym:?}"
        );
    }
}
