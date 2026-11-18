use super::*;

fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
    Rectangle::new((x, y).into(), (w, h).into())
}

#[test]
fn relative_moves_stop_at_the_edge_of_their_output() {
    // Two outputs side by side, the right one shorter.
    let outputs = [rect(0, 0, 1920, 1080), rect(1920, 0, 1280, 720)];
    assert_eq!(moved_by(&outputs, (100., 100.), 5., -3.), Some((105., 97.)));
    // Across to the other output.
    assert_eq!(
        moved_by(&outputs, (1900., 100.), 50., 0.),
        Some((1950., 100.))
    );
    // Off every output: stops at the edge of the one it was on.
    assert_eq!(
        moved_by(&outputs, (100., 100.), -500., 5000.),
        Some((0., 1079.))
    );
    assert_eq!(
        moved_by(&outputs, (2000., 700.), 0., 100.),
        Some((2000., 719.))
    );
    // From off every output: the middle of the first one.
    assert_eq!(
        moved_by(&outputs, (5000., 5000.), 1., 1.),
        Some((960., 540.))
    );
    assert_eq!(moved_by(&[], (0., 0.), 1., 1.), None);
}

#[test]
fn relative_moves_start_where_the_last_motion_goes_until_niri_has_it() {
    let at = smithay::utils::Point::from((10., 20.));
    let sent = Sent {
        to: (500., 300.),
        from: at,
    };
    assert_eq!(Sent::start(None, at), (10., 20.));
    // Not read yet: the pointer is still where it was.
    assert_eq!(Sent::start(Some(&sent), at), (500., 300.));
    // Read, or something else moved the pointer since.
    let moved = smithay::utils::Point::from((500.1, 300.1));
    assert_eq!(Sent::start(Some(&sent), moved), (500.1, 300.1));
}

#[test]
fn hold_limits() {
    let t0 = Instant::now();
    let ms = |n| t0 + Duration::from_millis(n);
    let press = |code| Op::Button {
        code,
        pressed: true,
    };
    let key = |code, pressed| Op::Key { code, pressed };
    let mut holds = Holds::default();

    // Held by a call: no limit.
    holds.follow(key(37, true));
    assert_eq!(holds.next_release(), None);
    assert!(holds.expired(ms(1_000_000)).is_empty());

    holds.follow(press(0x110));
    holds.limit(Held::Button(0x110), ms(100));
    holds.follow(key(50, true));
    holds.limit(Held::Key(50), ms(300));
    assert_eq!(holds.next_release(), Some(ms(100)));
    assert!(holds.expired(ms(99)).is_empty());
    assert_eq!(
        holds.expired(ms(100)),
        [Op::Button {
            code: 0x110,
            pressed: false
        }]
    );

    // A renewing press moves the limit on and holds once.
    holds.follow(press(0x110));
    holds.limit(Held::Button(0x110), ms(400));
    assert_eq!(holds.next_release(), Some(ms(300)));
    assert_eq!(holds.expired(ms(399)), [key(50, false)]);
    holds.follow(key(50, false));
    assert_eq!(holds.next_release(), Some(ms(400)));

    // Everything at the end: buttons, then keys in reverse.
    assert_eq!(
        holds.all(),
        [
            Op::Button {
                code: 0x110,
                pressed: false
            },
            key(37, false)
        ]
    );
    holds.follow(Op::Button {
        code: 0x110,
        pressed: false,
    });
    assert_eq!(holds.next_release(), None);

    assert_eq!(hold_limit(None), Ok(HOLD_DEFAULT));
    assert_eq!(
        hold_limit(Some(Millis::new(250))),
        Ok(Duration::from_millis(250))
    );
    assert_eq!(
        hold_limit(Some(Millis::new(0))),
        Err(InputErrorOwned::BadArgument)
    );
}

#[test]
fn abs_units_map_back_to_the_pixel() {
    for size in [720, 1080, 1440, 1920, 2160, 3840] {
        for v in 0..size {
            let units = abs_units(f64::from(v + 100), 100, size);
            assert!(units <= uinput::ABS_MAX, "{v} of {size} past ABS_MAX");
            // libinput's mapping, then the floor an app takes.
            let back = f64::from(units) * f64::from(size) / f64::from(uinput::ABS_MAX + 1);
            assert_eq!(back.floor() as i32, v, "{v} of {size} came back as {back}");
        }
    }
}

#[test]
fn combos() {
    let names = |combo: &str| {
        parse_combo(combo).map(|syms| {
            syms.into_iter()
                .map(xkb::keysym_get_name)
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(names("Return").unwrap(), ["Return"]);
    assert_eq!(names("return").unwrap(), ["Return"]);
    assert_eq!(names("ctrl+l").unwrap(), ["Control_L", "l"]);
    assert_eq!(
        names("Ctrl+Shift+Tab").unwrap(),
        ["Control_L", "Shift_L", "Tab"]
    );
    assert_eq!(names("super+Page_Down").unwrap(), ["Super_L", "Next"]);
    assert_eq!(names("ctrl++").unwrap(), ["Control_L", "plus"]);
    assert_eq!(
        names("hyper+x"),
        Err(InputErrorOwned::UnknownKey("hyper".to_owned()))
    );
    assert_eq!(
        names("ctrl+NoSuchKey"),
        Err(InputErrorOwned::UnknownKey("NoSuchKey".to_owned()))
    );
    assert_eq!(names(""), Err(InputErrorOwned::UnknownKey("".to_owned())));
}

#[test]
fn text_on_a_second_layout() {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap =
        xkb::Keymap::new_from_names(&context, "", "", "us,ru", "", None, xkb::COMPILE_NO_FLAGS)
            .unwrap();
    let syms = |text: &str| -> Vec<Keysym> {
        text.chars()
            .map(|ch| xkb::utf32_to_keysym(u32::from(ch)))
            .collect()
    };
    let plan = |text: &str, active| {
        text_keys(&syms(text), 2, active, |sym, layout| {
            find_key(&keymap, layout, sym)
        })
    };
    let layouts = |text: &str, active| -> Vec<u32> {
        plan(text, active)
            .unwrap()
            .keys
            .iter()
            .map(|t| t.layout)
            .collect()
    };
    // evdev KEY_SEMICOLON 39 makes ж, KEY_Q 16 makes й; + 8.
    let keys = plan("aЖ", 0).unwrap();
    assert_eq!(keys.active, 0);
    assert_eq!(
        keys.keys[1],
        Typed {
            key: Key {
                code: 47,
                shift: true
            },
            layout: 1
        }
    );
    // The space and digits stay on the layout before them.
    assert_eq!(
        layouts("ok привет 42 ok", 0),
        [0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0]
    );
    // From ru active, Latin letters go to us.
    assert_eq!(layouts("й q", 1), [1, 1, 0]);
    assert_eq!(plan("ok 你", 0), Err(3));
    assert_eq!(plan("a\u{0}", 0), Err(1));

    let ops = text_ops(&plan("aй", 0).unwrap(), 50);
    assert_eq!(
        ops,
        [
            Op::Key {
                code: 38,
                pressed: true
            },
            Op::Key {
                code: 38,
                pressed: false
            },
            Op::Layout(1),
            Op::Key {
                code: 24,
                pressed: true
            },
            Op::Key {
                code: 24,
                pressed: false
            },
            Op::Layout(0),
        ]
    );
    // Only us: no switch.
    assert!(!text_ops(&plan("ok", 0).unwrap(), 50)
        .iter()
        .any(|op| matches!(op, Op::Layout(_))));
}

#[test]
fn layout_marker_makes_no_keysym() {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap =
        xkb::Keymap::new_from_names(&context, "", "", "us,ru", "", None, xkb::COMPILE_NO_FLAGS)
            .unwrap();
    let kc = xkb::Keycode::new(u32::from(uinput::LAYOUT_MARKER) + 8);
    for layout in 0..2 {
        for level in 0..keymap.num_levels_for_key(kc, layout) {
            assert!(keymap.key_get_syms_by_level(kc, layout, level).is_empty());
        }
    }
    assert!(uinput::KEYS.contains(&uinput::LAYOUT_MARKER));
}

#[test]
fn keys_of_the_us_keymap() {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap =
        xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkb::COMPILE_NO_FLAGS)
            .unwrap();
    let key = |ch: char| find_key(&keymap, 0, xkb::utf32_to_keysym(u32::from(ch)));
    // evdev codes + 8: KEY_A 30, KEY_1 2, KEY_SPACE 57.
    assert_eq!(
        key('a'),
        Some(Key {
            code: 38,
            shift: false
        })
    );
    assert_eq!(
        key('A'),
        Some(Key {
            code: 38,
            shift: true
        })
    );
    assert_eq!(
        key('!'),
        Some(Key {
            code: 10,
            shift: true
        })
    );
    assert_eq!(
        key(' '),
        Some(Key {
            code: 65,
            shift: false
        })
    );
    assert_eq!(key('ж'), None);
    // Media and remote keys: evdev KEY_PLAYPAUSE 164, KEY_MUTE 113, KEY_VOLUMEUP 115,
    // KEY_BACK 158, KEY_HOMEPAGE 172, KEY_SEARCH 217.
    for (name, evdev) in [
        ("XF86AudioPlay", 164),
        ("XF86AudioMute", 113),
        ("XF86AudioRaiseVolume", 115),
        ("XF86Back", 158),
        ("XF86HomePage", 172),
        ("XF86Search", 217),
    ] {
        let sym = keysym(name).unwrap_or_else(|| panic!("{name} is a keysym"));
        assert_eq!(
            find_key(&keymap, 0, sym),
            Some(Key {
                code: evdev + 8,
                shift: false
            }),
            "{name}"
        );
        assert!(
            uinput::KEYS.contains(&(evdev as u16)),
            "{name} on the keyboard device"
        );
    }
    assert_eq!(
        find_key(&keymap, 0, Keysym::Return),
        Some(Key {
            code: 36,
            shift: false
        })
    );
}
