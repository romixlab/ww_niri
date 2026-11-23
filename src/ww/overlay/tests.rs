use std::time::{Duration, Instant, SystemTime};

use exact_units::Fixed;
use ww_niri_api::{
    Anchor, At, BackdropOwned, CardOwned, ContentOwned, ItemOwned, OverlayError, PlaceOwned, Px,
    SurfaceOwned, TextSize, WindowId,
};

use super::*;

fn card() -> CardOwned {
    CardOwned {
        title: "Реклама".to_owned(),
        countdown: Some("Передача вернётся через".to_owned()),
        clock: false,
        footer: Some("Первый канал".to_owned()),
    }
}

/// A cover on window 1 for `lasts_ms`, at `priority`.
fn cover(priority: u8, lasts_ms: u32) -> ItemOwned {
    ItemOwned {
        place: PlaceOwned {
            on: SurfaceOwned::Window(WindowId(1)),
            at: At::Fill,
        },
        content: ContentOwned::Card(card()),
        backdrop: BackdropOwned {
            blur: 2,
            dim: 65,
            ..BackdropOwned::default()
        },
        priority,
        lasts: Some(Fixed::new(lasts_ms)),
        mute: false,
        dismissible: false,
        report_shown: false,
    }
}

fn text(priority: u8) -> ItemOwned {
    ItemOwned {
        place: PlaceOwned {
            on: SurfaceOwned::Window(WindowId(1)),
            at: At::Anchor {
                anchor: Anchor::Bottom,
                dx: Px::new(0),
                dy: Px::new(-80),
            },
        },
        content: ContentOwned::Text {
            text: "Переключаю выход".to_owned(),
            size: TextSize::Medium,
        },
        backdrop: BackdropOwned {
            blur: 0,
            dim: 60,
            ..BackdropOwned::default()
        },
        priority,
        lasts: None,
        mute: false,
        dismissible: false,
        report_shown: false,
    }
}

fn look(content: ContentOwned, until: Option<Instant>) -> Look {
    Look {
        id: 1,
        priority: 0,
        at: At::Fill,
        content,
        backdrop: BackdropOwned::default(),
        image: None,
        until,
        since: Instant::now(),
        drawn: Default::default(),
    }
}

const W1: Target = Target::Window(1);

#[test]
fn check_limits() {
    let ok = |item: &ItemOwned| item.check("ad").map(|l| l.map(|l| l.as_millis()));
    assert_eq!(ok(&cover(0, 1)), Ok(Some(1)));
    assert_eq!(ok(&cover(0, 15 * 60 * 1000)), Ok(Some(15 * 60 * 1000)));
    assert_eq!(ok(&text(0)), Ok(None));
    let bad = Err(OverlayError::BadArgument);
    assert_eq!(ok(&cover(0, 0)), bad);
    assert_eq!(ok(&cover(0, 15 * 60 * 1000 + 1)), bad);
    assert_eq!(
        ok(&cover(0, 1)).and(cover(0, 1).check("").map(|_| None)),
        bad
    );
    assert_eq!(
        cover(0, 1).check(&"k".repeat(65)),
        Err(OverlayError::BadArgument)
    );
    let mut long = cover(0, 1000);
    let ContentOwned::Card(c) = &mut long.content else {
        unreachable!()
    };
    c.footer = Some("я".repeat(251));
    assert_eq!(ok(&long), bad);
    let mut blur = cover(0, 1000);
    blur.backdrop.blur = 5;
    assert_eq!(ok(&blur), bad);
    let mut dim = cover(0, 1000);
    dim.backdrop.dim = 101;
    assert_eq!(ok(&dim), bad);
    // A card only fills.
    let mut anchored = cover(0, 1000);
    anchored.place.at = text(0).place.at;
    assert_eq!(ok(&anchored), bad);
}

#[test]
fn countdown_rounds_up() {
    let now = Instant::now();
    let at = |left_ms| {
        look(
            ContentOwned::Card(card()),
            Some(now + Duration::from_millis(left_ms)),
        )
        .countdown(now)
    };
    assert_eq!(at(130_000).as_deref(), Some("Передача вернётся через 2:10"));
    assert_eq!(at(129_001).as_deref(), Some("Передача вернётся через 2:10"));
    assert_eq!(at(1).as_deref(), Some("Передача вернётся через 0:01"));
    assert_eq!(at(0).as_deref(), Some("Передача вернётся через 0:00"));
    // No end, no countdown.
    assert_eq!(look(ContentOwned::Card(card()), None).countdown(now), None);
}

#[test]
fn owners_items_stack_and_never_replace_each_other() {
    let now = Instant::now();
    let mut o = Overlay::default();
    let ad = o.put(10, "ad", W1, cover(150, 60_000), None).unwrap();
    // The same key of another owner is another item.
    let other = o.put(11, "ad", W1, cover(150, 60_000), None).unwrap();
    let exit = o.put(11, "exit", W1, text(200), None).unwrap();
    assert_ne!(ad, other);
    let front_to_back = |o: &Overlay| o.on_window(1).iter().map(|l| l.id).collect::<Vec<_>>();
    assert_eq!(front_to_back(&o), [exit, other, ad]);

    // A change keeps the id; a priority change moves it in the stack.
    assert_eq!(o.put(10, "ad", W1, cover(250, 1_000), None), Ok(ad));
    assert_eq!(front_to_back(&o), [ad, exit, other]);
    assert_eq!(o.on_window(2), []);

    // An owner lifts only its own.
    assert_eq!(o.lift(10, "exit"), None);
    assert_eq!(o.lift(11, "exit"), Some(exit));
    assert_eq!(front_to_back(&o), [ad, other]);

    let list = o.list(10);
    assert_eq!(
        list.iter().map(|i| (i.id.0, i.own)).collect::<Vec<_>>(),
        [(ad, true), (other, false)]
    );
    assert_eq!(o.lift_due(now), Vec::<u64>::new());
}

#[test]
fn items_lift_by_end_owner_window_and_dismiss() {
    let now = Instant::now();
    let mut o = Overlay::default();
    let soon = o
        .put(10, "a", W1, cover(0, 0), Some(now + Duration::from_secs(2)))
        .unwrap();
    let late = o
        .put(
            10,
            "b",
            W1,
            cover(0, 0),
            Some(now + Duration::from_secs(60)),
        )
        .unwrap();
    let other = o.put(11, "a", Target::Window(2), text(0), None).unwrap();
    assert_eq!(o.lift_due(now + Duration::from_secs(1)), Vec::<u64>::new());
    assert_eq!(o.lift_due(now + Duration::from_secs(2)), [soon]);
    assert_eq!(o.lift_gone(|w| w == 1), [other]);
    assert_eq!(o.lift_owned_by(11), Vec::<u64>::new());
    assert_eq!(o.lift_owned_by(10), [late]);
    assert!(o.is_empty());

    let mut dismissible = text(0);
    dismissible.dismissible = true;
    let kept = o.put(10, "kept", W1, text(0), None).unwrap();
    let shown = o.put(10, "shown", W1, dismissible, None).unwrap();
    assert_eq!(o.dismiss(kept), Err(OverlayError::NotDismissible));
    assert_eq!(o.dismiss(shown), Ok(()));
    assert_eq!(o.dismiss(shown), Err(OverlayError::NoSuchItem));
}

#[test]
fn an_owner_has_at_most_its_share() {
    let mut o = Overlay::default();
    for i in 0..ITEMS_PER_OWNER_MAX {
        o.put(1, &i.to_string(), W1, text(0), None).unwrap();
    }
    assert_eq!(
        o.put(1, "one more", W1, text(0), None),
        Err(OverlayError::TooMany)
    );
    // A change is no new item; another owner has its own share.
    assert!(o.put(1, "0", W1, text(1), None).is_ok());
    assert!(o.put(2, "0", W1, text(1), None).is_ok());
}

#[test]
fn output_items_go_on_their_output_or_the_focused_one() {
    let mut o = Overlay::default();
    let named = o
        .put(
            1,
            "a",
            Target::Output(Some("HDMI-A-1".to_owned())),
            text(0),
            None,
        )
        .unwrap();
    let focused = o.put(1, "b", Target::Output(None), text(9), None).unwrap();
    let ids = |looks: Vec<Rc<Look>>| looks.iter().map(|l| l.id).collect::<Vec<_>>();
    assert_eq!(ids(o.on_output("HDMI-A-1", true)), [focused, named]);
    assert_eq!(ids(o.on_output("HDMI-A-1", false)), [named]);
    assert_eq!(ids(o.on_output("DP-1", true)), [focused]);
    assert_eq!(ids(o.on_window(1)), Vec::<u64>::new());
}

#[test]
fn wakes_at_each_countdown_second_the_end_and_the_minute() {
    let now = Instant::now();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_millis(60_000 * 1_000 + 20_500);
    let mut o = Overlay::default();
    assert_eq!(o.next_wake(now, wall), None);
    o.put(
        1,
        "a",
        W1,
        cover(0, 0),
        Some(now + Duration::from_millis(2_300)),
    )
    .unwrap();
    assert_eq!(
        o.next_wake(now, wall),
        Some(now + Duration::from_millis(300))
    );
    let later = now + Duration::from_millis(300);
    assert_eq!(
        o.next_wake(later, wall),
        Some(later + Duration::from_millis(1_000))
    );
    // Due: at once.
    let past = now + Duration::from_secs(3);
    assert_eq!(o.next_wake(past, wall), Some(past));

    // A clock wakes at the next minute; an item without an end or clock never.
    let mut o = Overlay::default();
    o.put(1, "t", W1, text(0), None).unwrap();
    assert_eq!(o.next_wake(now, wall), None);
    let mut clock = text(0);
    clock.content = ContentOwned::Clock {
        size: TextSize::Small,
    };
    o.put(1, "c", W1, clock, None).unwrap();
    assert_eq!(
        o.next_wake(now, wall),
        Some(now + Duration::from_millis(39_500))
    );
}

#[test]
fn mutes_while_a_muting_item_is_up() {
    let mut o = Overlay::default();
    let mut muting = cover(0, 1000);
    muting.mute = true;
    o.put(1, "a", W1, cover(0, 1000), None).unwrap();
    assert!(!o.mutes());
    o.put(1, "b", W1, muting.clone(), None).unwrap();
    o.put(2, "b", W1, muting, None).unwrap();
    assert!(o.mutes());
    o.lift(1, "b");
    assert!(o.mutes());
    // Changed to an item without mute.
    o.put(2, "b", W1, cover(0, 1000), None).unwrap();
    assert!(!o.mutes());
}

fn progress() -> ItemOwned {
    let mut item = text(200);
    item.place.on = SurfaceOwned::Output(None);
    item.content = ContentOwned::Progress {
        title: "Загружаю плеер".to_owned(),
        detail: Some("Первый канал".to_owned()),
    };
    item
}

#[test]
fn progress_counts_up_from_the_first_put() {
    let mut o = Overlay::default();
    let id = o
        .put(1, "status", Target::Output(None), text(200), None)
        .unwrap();
    let since = o.on_output("HDMI-A-1", true)[0].since;
    // The mark turns into progress: the count starts at the mark.
    assert_eq!(
        o.put(1, "status", Target::Output(None), progress(), None),
        Ok(id)
    );
    let look = o.on_output("HDMI-A-1", true)[0].clone();
    assert_eq!(look.since, since);
    let at = |ms| look.elapsed(since + Duration::from_millis(ms));
    assert_eq!(at(0).as_deref(), Some("0:00"));
    assert_eq!(at(999).as_deref(), Some("0:00"));
    assert_eq!(at(61_000).as_deref(), Some("1:01"));
    assert_eq!(
        look.elapsed(since - Duration::from_secs(1)).as_deref(),
        Some("0:00")
    );
    // Not a progress, no count.
    o.put(2, "t", W1, text(0), None).unwrap();
    assert_eq!(o.on_window(1)[0].elapsed(since), None);

    // Wakes at each whole second counted from the start.
    let wall = SystemTime::UNIX_EPOCH;
    let now = since + Duration::from_millis(2_300);
    assert_eq!(o.next_wake(now, wall), Some(since + Duration::from_secs(3)));
    assert_eq!(
        o.next_wake(since + Duration::from_secs(3), wall),
        Some(since + Duration::from_secs(4))
    );
}

#[test]
fn shown_reports_a_drawn_put_once_to_its_owner() {
    use crate::render_helpers::RenderTarget;

    let mut o = Overlay::default();
    let mut asks = progress();
    asks.report_shown = true;
    let id = o
        .put(7, "status", Target::Output(None), asks.clone(), None)
        .unwrap();
    o.put(8, "quiet", Target::Output(None), progress(), None)
        .unwrap();

    // Not drawn yet, or drawn only for a screencast: nothing to report.
    o.frame_started();
    assert_eq!(o.take_shown(), []);
    render::mark_drawn(&o.on_output("DP-1", true), RenderTarget::Screencast);
    assert_eq!(o.take_shown(), []);

    o.frame_started();
    render::mark_drawn(&o.on_output("DP-1", true), RenderTarget::Output);
    assert_eq!(o.take_shown(), [(7, id)]);
    // Once per put.
    o.frame_started();
    render::mark_drawn(&o.on_output("DP-1", true), RenderTarget::Output);
    assert_eq!(o.take_shown(), []);

    // A new put asks again; a frame that did not draw it does not report it.
    o.put(7, "status", Target::Output(None), asks, None)
        .unwrap();
    render::mark_drawn(&o.on_output("DP-1", true), RenderTarget::Output);
    o.frame_started();
    assert_eq!(o.take_shown(), []);
    render::mark_drawn(&o.on_output("DP-1", true), RenderTarget::Output);
    assert_eq!(o.take_shown(), [(7, id)]);
}

#[test]
fn show_only_hides_what_it_does_not_match() {
    use ww_niri_api::{Band, ShowOnlyOwned};

    let mut o = Overlay::default();
    let end = Instant::now() + Duration::from_secs(1);
    let cover_id = o.put(1, "ad", W1, cover(150, 1000), Some(end)).unwrap();
    let text_id = o.put(2, "status", W1, text(200), None).unwrap();
    let clock_id = o.put(2, "clock", W1, text(10), None).unwrap();
    assert_eq!(o.on_window(1).len(), 3);

    let filter = ShowOnlyOwned {
        bands: vec![Band {
            lowest: 192,
            highest: 255,
        }],
        keys: vec!["clock".to_owned()],
        ids: Vec::new(),
    };
    o.set_show_only(Some(filter.clone()));
    assert_eq!(o.show_only(), Some(&filter));
    let drawn: Vec<u64> = o.on_window(1).iter().map(|l| l.id).collect();
    assert_eq!(drawn, [text_id, clock_id]);
    let hidden: Vec<(u64, bool)> = (o.list(1).iter()).map(|i| (i.id.0, i.hidden)).collect();
    assert_eq!(
        hidden,
        [(text_id, false), (cover_id, true), (clock_id, false)]
    );

    // By id; a hidden item still ends at its time.
    o.set_show_only(Some(ShowOnlyOwned {
        ids: vec![ItemId(cover_id)],
        ..ShowOnlyOwned::default()
    }));
    assert_eq!(o.on_window(1)[0].id, cover_id);
    o.set_show_only(Some(ShowOnlyOwned::default()));
    assert!(o.on_window(1).is_empty());
    assert_eq!(o.lift_due(end), [cover_id]);

    o.set_show_only(None);
    assert_eq!(o.on_window(1).len(), 2);
}

#[test]
fn a_fill_image_comes_from_the_backgrounds_folder() {
    let mut item = text(0);
    item.backdrop.fill = FillOwned::Image("sky.png".to_owned());
    let mut o = Overlay::default();
    // No folder: no images.
    assert_eq!(
        o.put(1, "a", W1, item.clone(), None),
        Err(OverlayError::NoSuchImage)
    );

    let dir = std::env::temp_dir().join(format!("ww-overlay-fill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    o.backgrounds = Some(dir.clone());
    assert_eq!(
        o.put(1, "a", W1, item.clone(), None),
        Err(OverlayError::NoSuchImage)
    );
    let mut encoder = png::Encoder::new(std::fs::File::create(dir.join("sky.png")).unwrap(), 2, 1);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&[0, 0, 255, 255, 0, 0]).unwrap();
    writer.finish().unwrap();

    o.put(1, "a", W1, item.clone(), None).unwrap();
    o.put(2, "a", W1, item, None).unwrap();
    let looks = o.on_window(1);
    let images: Vec<&Rc<image::Image>> = looks.iter().map(|l| l.image.as_ref().unwrap()).collect();
    // Read once for both items.
    assert!(Rc::ptr_eq(images[0], images[1]));
    assert_eq!(&images[0].pixels.data()[..4], &[255, 0, 0, 255]);
    std::fs::remove_dir_all(&dir).unwrap();
}
