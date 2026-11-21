//! Hidden windows: out of every workspace and focus, listed as hidden, back on `show`.

use ww_niri_api::{WindowId, WindowOwned};

use super::{connect, fake, run};

const KITTY: WindowId = WindowId(1);
const MPV: WindowId = WindowId(2);

fn mpv(windows: &[WindowOwned]) -> &WindowOwned {
    windows.iter().find(|w| w.id == MPV).unwrap()
}

#[test]
fn a_hidden_window_leaves_the_screen_and_show_brings_it_back() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        let w = client.windows();
        w.show(MPV, None, Some(true)).call().await.unwrap();

        w.hide(MPV).call().await.unwrap();
        let windows = tv.windows();
        let hidden = mpv(&windows);
        assert!(hidden.hidden);
        assert!(!hidden.focused);
        assert_eq!((hidden.workspace, hidden.output.as_deref()), (None, None));
        assert!(hidden.fullscreen, "keeps its size for later");

        w.focus(MPV).call().await.unwrap();
        w.set_fullscreen(MPV, false).call().await.unwrap();
        w.hide(MPV).call().await.unwrap();
        let hidden = mpv(&tv.windows()).clone();
        assert!(hidden.hidden && !hidden.focused, "focus leaves it hidden");
        assert!(hidden.fullscreen, "so does set_fullscreen, as in niri");

        w.show(KITTY, None, None).call().await.unwrap();
        let current = tv
            .windows()
            .into_iter()
            .find(|w| w.id == KITTY)
            .unwrap()
            .workspace;
        w.show(MPV, None, None).call().await.unwrap();
        let shown = mpv(&tv.windows()).clone();
        assert!(!shown.hidden && shown.focused && shown.fullscreen);
        assert_eq!(shown.workspace, current, "back on the current workspace");
        assert!(shown.output.is_some());
    });
}
