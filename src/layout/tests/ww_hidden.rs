use super::*;

fn two_windows() -> Layout<TestWindow> {
    check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
    ])
}

fn workspace_of(layout: &Layout<TestWindow>, id: usize) -> Option<WorkspaceId> {
    let mut found = None;
    layout.with_windows(|win, _, ws, _| {
        if *win.id() == id {
            found = ws;
        }
    });
    found
}

#[test]
fn a_hidden_window_leaves_every_workspace_and_focus_but_stays_known() {
    let mut layout = two_windows();
    assert_eq!(layout.focus().map(|w| *w.id()), Some(2));

    assert!(layout.ww_hide_window(&2));
    layout.verify_invariants();
    assert!(layout.ww_is_hidden(&2));
    assert!(layout.has_window(&2), "still alive");
    assert_eq!(workspace_of(&layout, 2), None);
    assert!(layout.workspaces().all(|(_, _, ws)| !ws.has_window(&2)));
    assert_eq!(layout.focus().map(|w| *w.id()), Some(1));
    assert!(!layout.ww_hide_window(&2), "already hidden");
}

#[test]
fn showing_a_hidden_window_puts_it_back_focused() {
    let mut layout = two_windows();
    let ws = workspace_of(&layout, 2);
    layout.ww_hide_window(&2);

    assert!(layout.ww_unhide_window(&2));
    layout.verify_invariants();
    assert!(!layout.ww_is_hidden(&2));
    assert_eq!(workspace_of(&layout, 2), ws);
    assert_eq!(layout.focus().map(|w| *w.id()), Some(2));
    assert!(!layout.ww_unhide_window(&2), "not hidden any more");
}

#[test]
fn a_hidden_fullscreen_window_comes_back_fullscreen() {
    let mut layout = two_windows();
    layout.set_fullscreen(&2, true);
    layout.ww_hide_window(&2);
    layout.ww_unhide_window(&2);
    layout.verify_invariants();
    let win = layout.windows().find(|(_, w)| *w.id() == 2).unwrap().1;
    assert!(win.pending_sizing_mode().is_fullscreen());
}

#[test]
fn a_hidden_window_can_close() {
    let mut layout = two_windows();
    layout.ww_hide_window(&2);
    assert!(layout.remove_window(&2, Transaction::new()).is_some());
    layout.verify_invariants();
    assert!(!layout.has_window(&2));
    assert!(!layout.ww_is_hidden(&2));
}

#[test]
fn a_hidden_window_outlives_its_output_and_shows_without_one() {
    let mut layout = two_windows();
    layout.ww_hide_window(&2);
    check_ops_on_layout(&mut layout, [Op::RemoveOutput(1)]);
    assert!(layout.ww_is_hidden(&2));

    assert!(layout.ww_unhide_window(&2));
    layout.verify_invariants();
    assert!(workspace_of(&layout, 2).is_some());

    check_ops_on_layout(&mut layout, [Op::AddOutput(1)]);
    assert_eq!(layout.focus().map(|w| *w.id()), Some(2));
}

#[test]
fn the_only_window_hides_and_comes_back() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ]);
    layout.ww_hide_window(&1);
    layout.verify_invariants();
    assert!(layout.focus().is_none());
    layout.ww_unhide_window(&1);
    layout.verify_invariants();
    assert_eq!(layout.focus().map(|w| *w.id()), Some(1));
}
