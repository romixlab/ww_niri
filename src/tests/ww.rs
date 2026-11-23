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

/// An overlay item: a cover over `window` filling it, or a text at its bottom.
fn overlay_item(window: u64, cover: bool, priority: u8) -> ww_niri_api::ItemOwned {
    use ww_niri_api::{
        Anchor, At, Backdrop, CardOwned, ContentOwned, ItemOwned, PlaceOwned, Px, SurfaceOwned,
        TextSize, WindowId,
    };

    let (at, content, backdrop) = if cover {
        let card = CardOwned {
            title: "Реклама".to_owned(),
            countdown: Some("Передача вернётся через".to_owned()),
            clock: true,
            footer: Some("Первый канал".to_owned()),
        };
        (
            At::Fill,
            ContentOwned::Card(card),
            Backdrop { blur: 2, dim: 65 },
        )
    } else {
        let at = At::Anchor {
            anchor: Anchor::Bottom,
            dx: Px::new(0),
            dy: Px::new(-80),
        };
        let text = ContentOwned::Text {
            text: "Переключаю выход".to_owned(),
            size: TextSize::Large,
        };
        (at, text, Backdrop { blur: 0, dim: 100 })
    };
    ItemOwned {
        place: PlaceOwned {
            on: SurfaceOwned::Window(WindowId(window)),
            at,
        },
        content,
        backdrop,
        priority,
        lasts: None,
        mute: false,
        dismissible: false,
        report_shown: false,
    }
}

#[test]
fn host_overlay_items_stack_lift_by_owner_end_and_window() {
    use std::time::{Duration, Instant};

    use ww_niri_api::OverlayError;

    use crate::layout::LayoutElement as _;
    use crate::ww::overlay::Target;

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
    let wid = f.niri_state().windows()[0].id.0;

    // The window's items, front to back.
    let drawn = |f: &mut Fixture| {
        let mut ids = Vec::new();
        f.niri().layout.with_windows(|mapped, _, _, _| {
            ids = mapped.overlay().iter().map(|l| l.id).collect();
        });
        ids
    };
    let put = |f: &mut Fixture, owner, key: &str, cover, until| {
        let item = overlay_item(wid, cover, if cover { 150 } else { 200 });
        f.niri_state()
            .overlay_put(owner, key.to_owned(), Target::Window(wid), item, until)
            .map(|id| id.0)
    };

    assert_eq!(
        f.niri_state().overlay_put(
            1,
            "ad".to_owned(),
            Target::Window(9999),
            overlay_item(9999, true, 0),
            None
        ),
        Err(OverlayError::NoSuchWindow)
    );

    // Two owners' items stack; the text of owner 2 goes over the cover of owner 1.
    let ad = put(&mut f, 1, "ad", true, None).unwrap();
    let text = put(&mut f, 2, "exit", false, None).unwrap();
    assert_eq!(drawn(&mut f), [text, ad]);
    // An owner lifts only its own.
    assert_eq!(
        f.niri_state().overlay_lift(2, "ad".to_owned()),
        Err(OverlayError::NoSuchItem)
    );
    f.niri_state().overlay_lift(2, "exit".to_owned()).unwrap();
    assert_eq!(drawn(&mut f), [ad]);
    // An owner's end lifts its items only.
    put(&mut f, 2, "exit", false, None).unwrap();
    f.niri_state().overlay_lift_owned_by(1);
    assert_eq!(drawn(&mut f).len(), 1);
    f.niri_state().overlay_lift_owned_by(2);
    assert_eq!(drawn(&mut f), Vec::<u64>::new());

    // Lifted by the timer at its end.
    put(
        &mut f,
        1,
        "ad",
        true,
        Some(Instant::now() + Duration::from_millis(50)),
    )
    .unwrap();
    assert_eq!(drawn(&mut f).len(), 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !drawn(&mut f).is_empty() {
        assert!(Instant::now() < deadline, "the item outlived its end");
        // niri's own loop: its timers fire without waking the fixture's.
        f.state
            .server
            .event_loop
            .dispatch(Duration::from_millis(10), &mut f.state.server.state)
            .unwrap();
    }
    assert!(f.niri().ww.overlay.is_empty());
    assert!(f.niri().ww.overlay_timer.is_none());

    // The window closes: its items lift.
    put(&mut f, 1, "ad", true, None).unwrap();
    let window = f.client(id).window(&surface);
    window.attach_null();
    window.commit();
    f.double_roundtrip(id);
    f.niri_state().refresh_and_flush_clients();
    assert!(f.niri().ww.overlay.is_empty());
}

/// Renders a red window under a cover of one owner and a text of another, and a notice on the
/// output, on a surfaceless EGL renderer: the blur, the dim, the card, the text and the notice
/// show. `WW_COVER_PNG=path` also writes the frame there to look at.
#[test]
fn overlay_renders_cover_text_and_output_notice() {
    use smithay::backend::allocator::Fourcc;
    use smithay::utils::Transform;
    use ww_niri_api::{
        Anchor, At, Backdrop, ContentOwned, ItemOwned, PlaceOwned, Px, SurfaceOwned,
    };

    use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget};
    use crate::ww::overlay::Target;

    let config = niri_config::Config::parse_mem("hotkey-overlay { skip-at-startup; }").unwrap();
    let mut f = Fixture::with_config(config);
    if let Err(err) = f.niri_state().backend.headless().add_renderer() {
        // No EGL here (a build host without Mesa): nothing to render with.
        eprintln!("skipped: no surfaceless EGL renderer: {err:?}");
        return;
    }
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_fullscreen(None);
    window.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    let (w, h) = window.configures_received.last().unwrap().1.size;
    assert_eq!((w, h), (1920, 1080));
    window.attach_color_buffer([u32::MAX, 0, 0, u32::MAX]);
    window.set_size(1920, 1080);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    let wid = f.niri_state().windows()[0].id.0;

    let frame = |f: &mut Fixture| -> Vec<u8> {
        let output = f.niri_output(1);
        let state = f.niri_state();
        let niri = &state.niri;
        state
            .backend
            .with_primary_renderer(|renderer| {
                let ctx = RenderCtx {
                    renderer: &mut *renderer,
                    target: RenderTarget::Output,
                    xray: None,
                };
                let elements = niri.render_to_vec(ctx, &output, false);
                render_to_vec(
                    renderer,
                    (1920, 1080).into(),
                    1.0.into(),
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements.iter().rev(),
                )
                .unwrap()
            })
            .unwrap()
    };
    let px = |frame: &[u8], x: usize, y: usize| {
        let i = (y * 1920 + x) * 4;
        [frame[i], frame[i + 1], frame[i + 2]]
    };
    let lit = |frame: &[u8], xs: std::ops::Range<usize>, ys: std::ops::Range<usize>| {
        ys.flat_map(|y| xs.clone().map(move |x| (x, y)))
            .any(|(x, y)| {
                let [r, g, _] = px(frame, x, y);
                r > 200 && g > 180
            })
    };

    let plain = frame(&mut f);
    assert_eq!(px(&plain, 960, 100), [255, 0, 0]);

    let until = std::time::Instant::now() + std::time::Duration::from_secs(130);
    let state = f.niri_state();
    let window = Target::Window(wid);
    (state.overlay_put(
        1,
        "ad".to_owned(),
        window.clone(),
        overlay_item(wid, true, 150),
        Some(until),
    ))
    .unwrap();
    (state.overlay_put(
        2,
        "exit".to_owned(),
        window,
        overlay_item(wid, false, 200),
        None,
    ))
    .unwrap();
    let notice = ItemOwned {
        place: PlaceOwned {
            on: SurfaceOwned::Output(None),
            at: At::Anchor {
                anchor: Anchor::TopRight,
                dx: Px::new(-40),
                dy: Px::new(40),
            },
        },
        content: ContentOwned::Notice {
            title: "Звонок".to_owned(),
            body: Some("Мама".to_owned()),
        },
        // The blur needs framebuffer blits; without them the box is the dim alone.
        backdrop: Backdrop { blur: 2, dim: 100 },
        priority: 100,
        lasts: None,
        mute: false,
        dismissible: true,
        report_shown: false,
    };
    (state.overlay_put(3, "call".to_owned(), Target::Output(None), notice, None)).unwrap();
    let covered = frame(&mut f);
    if let Some(path) = std::env::var_os("WW_COVER_PNG") {
        let file = std::fs::File::create(path).unwrap();
        crate::utils::write_png_rgba8(file, 1920, 1080, &covered).unwrap();
    }

    // Away from the text: the blurred red under the warm dim.
    let [r, g, b] = px(&covered, 960, 100);
    let near = |v: u8, want: f32| (f32::from(v) - want).abs() <= 6.;
    assert!(
        near(r, 255. * 0.35 + 42. * 0.65) && near(g, 29. * 0.65) && near(b, 22. * 0.65),
        "dimmed red: {:?}",
        [r, g, b]
    );
    // Each line of the card shows: light pixels across its band.
    for (line, y) in [("title", 380), ("countdown", 520), ("clock", 720)] {
        assert!(
            lit(&covered, 0..1920, y - 30..y + 30),
            "the {line} line is drawn"
        );
    }
    // The text sits in its opaque box at the bottom, over the card's footer.
    assert!(lit(&covered, 600..1320, 900..1000), "the text is drawn");
    let [r, g, b] = px(&covered, 960, 1080 - 81);
    assert!(
        near(r, 42.) && near(g, 29.) && near(b, 22.),
        "the text's box: {:?}",
        [r, g, b]
    );
    // The notice is in the output's top right corner, in its box.
    assert!(lit(&covered, 1500..1880, 40..200), "the notice is drawn");
    assert!(
        !lit(&covered, 0..400, 0..200),
        "nothing in the top left corner"
    );

    f.niri_state().overlay_lift_owned_by(1);
    f.niri_state().overlay_lift_owned_by(2);
    f.niri_state().overlay_lift_owned_by(3);
    assert_eq!(px(&frame(&mut f), 960, 100), [255, 0, 0]);
}

/// Blurs sharp stripes through the cover renderer: the edges go soft and nothing sharp is left.
/// `WW_COVER_STRIPES_PNG=path` writes the frame there to look at.
#[test]
fn cover_blurs_detail_away() {
    use std::rc::Rc;

    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::element::Kind;
    use smithay::backend::renderer::gles::GlesRenderer;
    use smithay::utils::{Point, Scale, Size, Transform};
    use ww_niri_api::{At, Backdrop, ContentOwned, TextSize};

    use crate::backend::Headless;
    use crate::layout::LayoutElementRenderElement;
    use crate::render_helpers::render_to_vec;
    use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
    use crate::ww::overlay::render::{Area, OverlayElement, OverlayRender};
    use crate::ww::overlay::Look;

    let mut headless = Headless::new();
    if let Err(err) = headless.add_renderer() {
        eprintln!("skipped: no surfaceless EGL renderer: {err:?}");
        return;
    }
    let look = Rc::new(Look {
        id: 1,
        priority: 0,
        at: At::Fill,
        content: ContentOwned::Text {
            text: String::new(),
            size: TextSize::Small,
        },
        backdrop: Backdrop { blur: 2, dim: 65 },
        until: None,
        since: std::time::Instant::now(),
        drawn: Default::default(),
    });
    let render = OverlayRender::default();
    let area = Area {
        loc: Point::from((0., 0.)),
        size: Size::from((1920., 1080.)),
        scale: Scale::from(1.),
        alpha: 1.,
    };
    let frame = headless
        .with_primary_renderer(|renderer| {
            let mut elements = Vec::new();
            // White and blue stripes, 40 px wide, at the scale asked for.
            let mut window = |_: &mut GlesRenderer, _| {
                (0..48)
                    .map(|i| {
                        let color = if i % 2 == 0 {
                            [1., 1., 1., 1.]
                        } else {
                            [0., 0., 1., 1.]
                        };
                        let buffer = SolidColorBuffer::new((40., 1080.), color);
                        let at = Point::from((f64::from(i) * 40., 0.));
                        let elem = SolidColorRenderElement::from_buffer(
                            &buffer,
                            at,
                            1.,
                            Kind::Unspecified,
                        );
                        LayoutElementRenderElement::SolidColor(elem)
                    })
                    .collect()
            };
            let drawn = render.render(renderer, &[look], area, Some(&mut window), &mut |elem| {
                elements.push(elem)
            });
            assert!(drawn.covered, "a fill with a blur covers the window");
            let elements: Vec<_> = elements
                .into_iter()
                .map(|elem| match elem {
                    OverlayElement::Texture(elem) => {
                        crate::niri::OutputRenderElements::Texture(elem)
                    }
                    OverlayElement::SolidColor(elem) => elem.into(),
                    OverlayElement::Blur(_) => unreachable!("a cover blurs the window's copy"),
                })
                .collect();
            render_to_vec(
                renderer,
                (1920, 1080).into(),
                1.0.into(),
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )
            .unwrap()
        })
        .unwrap();
    if let Some(path) = std::env::var_os("WW_COVER_STRIPES_PNG") {
        let file = std::fs::File::create(path).unwrap();
        crate::utils::write_png_rgba8(file, 1920, 1080, &frame).unwrap();
    }

    // Unblurred, red would swing by 255 * 0.35 = 89 between the stripes.
    let row: Vec<u8> = (0..1920).map(|x| frame[(540 * 1920 + x) * 4]).collect();
    let swing = row[400..1500].iter().max().unwrap() - row[400..1500].iter().min().unwrap();
    assert!(
        swing < 30,
        "red swings by {swing} along the row: not blurred"
    );
    // Between neighbours there is no sharp edge left.
    let step = row[400..1500]
        .windows(2)
        .map(|w| w[0].abs_diff(w[1]))
        .max()
        .unwrap();
    assert!(step <= 4, "a step of {step} between neighbours");
}

/// A put queues a redraw of the outputs at once; a progress is drawn with its count, and the
/// owner alone hears when the frame that first drew it reached the screen.
/// `WW_PROGRESS_PNG=path` writes the frame there to look at.
#[test]
fn overlay_put_redraws_at_once_and_reports_the_shown_frame() {
    use std::time::Duration;

    use smithay::backend::allocator::Fourcc;
    use smithay::utils::Transform;
    use ww_niri_api::{
        Anchor, At, Backdrop, ContentOwned, EventOwned, ItemOwned, PlaceOwned, Px, SurfaceOwned,
    };

    use crate::niri::RedrawState;
    use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget};
    use crate::ww::overlay::Target;

    let config = niri_config::Config::parse_mem("hotkey-overlay { skip-at-startup; }").unwrap();
    let mut f = Fixture::with_config(config);
    if let Err(err) = f.niri_state().backend.headless().add_renderer() {
        eprintln!("skipped: no surfaceless EGL renderer: {err:?}");
        return;
    }
    f.add_output(1, (1920, 1080));
    f.dispatch();
    let output = f.niri_output(1);
    let redraw = |f: &mut Fixture| {
        let output = f.niri_output(1);
        matches!(
            f.niri().output_state[&output].redraw_state,
            RedrawState::Idle
        )
    };
    assert!(redraw(&mut f), "the output is drawn and idle");

    let hub = f.niri().ww.services().hub.clone();
    let (owner, other) = (hub.subscribe(3), hub.subscribe(4));
    let item = ItemOwned {
        place: PlaceOwned {
            on: SurfaceOwned::Output(None),
            at: At::Anchor {
                anchor: Anchor::BottomRight,
                dx: Px::new(-40),
                dy: Px::new(-40),
            },
        },
        content: ContentOwned::Progress {
            title: "Загружаю плеер".to_owned(),
            detail: Some("Первый канал".to_owned()),
        },
        backdrop: Backdrop { blur: 0, dim: 100 },
        priority: 200,
        lasts: None,
        mute: false,
        dismissible: false,
        report_shown: true,
    };
    let id = (f.niri_state())
        .overlay_put(3, "status".to_owned(), Target::Output(None), item, None)
        .unwrap();
    assert!(!redraw(&mut f), "the put queued a redraw at once");

    let frame = {
        let state = f.niri_state();
        let niri = &state.niri;
        state
            .backend
            .with_primary_renderer(|renderer| {
                let ctx = RenderCtx {
                    renderer: &mut *renderer,
                    target: RenderTarget::Output,
                    xray: None,
                };
                let elements = niri.render_to_vec(ctx, &output, false);
                render_to_vec(
                    renderer,
                    (1920, 1080).into(),
                    1.0.into(),
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements.iter().rev(),
                )
                .unwrap()
            })
            .unwrap()
    };
    if let Some(path) = std::env::var_os("WW_PROGRESS_PNG") {
        let file = std::fs::File::create(path).unwrap();
        crate::utils::write_png_rgba8(file, 1920, 1080, &frame).unwrap();
    }
    // Title, detail and count are light lines in the bottom right corner, nowhere else.
    let light_rows = |xs: std::ops::Range<usize>, ys: std::ops::Range<usize>| {
        ys.filter(|y| {
            xs.clone().any(|x| {
                let i = (y * 1920 + x) * 4;
                frame[i] > 200 && frame[i + 1] > 160
            })
        })
        .count()
    };
    let rows = light_rows(1300..1880, 700..1040);
    assert!(rows > 60, "{rows} rows of text in the corner");
    assert_eq!(
        light_rows(0..600, 0..400),
        0,
        "nothing in the top left corner"
    );

    assert!(owner.try_recv().is_err(), "not on the screen yet");
    f.niri().ww.overlay_frame_queued(&output);
    f.niri()
        .ww
        .overlay_presented(&output, Duration::from_millis(1234));
    assert_eq!(
        owner.try_recv(),
        Ok(EventOwned::OverlayShown {
            id,
            at: exact_units::Fixed::new(1_234_000),
        })
    );
    assert!(other.try_recv().is_err(), "only the owner hears it");
    // Once: the next frame reports nothing.
    f.niri()
        .ww
        .overlay_presented(&output, Duration::from_millis(1250));
    assert!(owner.try_recv().is_err());
}
