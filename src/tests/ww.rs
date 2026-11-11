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
