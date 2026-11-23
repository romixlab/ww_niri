//! Typed calls and the send queue reach the fake in order, merged where one call does both.

use std::time::Duration;

use exact_units::Fixed;
use ww_niri_api::{BrowserErrorOwned, Button, Modifiers, Point};
use ww_niri_client::{Command, TvError};
use ww_niri_fake::{ClockMode, Fail, FakeTv, InputEntry, Options, ReleasedBy};

use super::{config, connect, fake, run, until_fake};

fn calls_of(fake: &FakeTv, method: &str) -> Vec<String> {
    fake.calls()
        .iter()
        .filter(|c| c.method == method)
        .map(ToString::to_string)
        .collect()
}

#[test]
fn typed_calls_reach_the_tv() {
    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        let pointer = tv.pointer();
        pointer.move_by(-10, 5).await.unwrap();
        assert_eq!((fake.pointer().x.get(), fake.pointer().y.get()), (950, 545));
        pointer
            .click(None, Button::Left, 2, Modifiers::default())
            .await
            .unwrap();
        pointer
            .scroll(None, 0, 120, Modifiers::default())
            .await
            .unwrap();
        let keyboard = tv.keyboard();
        keyboard.type_text("hi".to_owned()).await.unwrap();
        keyboard.press_keys("ctrl+l".to_owned()).await.unwrap();
        tv.audio().set_volume(55).await.unwrap();
        assert_eq!(fake.audio(), (55, false));
        assert!(matches!(
            tv.audio().set_volume(151).await,
            Err(TvError::BadArgument { what: "volume" })
        ));
        assert!(matches!(
            tv.slot(0).open("ftp://example.org/".to_owned()).await,
            Err(TvError::Browser(BrowserErrorOwned::NotRunning))
        ));
        let log = fake.take_input_log();
        assert!(matches!(log[0], InputEntry::Click { count: 2, .. }));
        assert!(matches!(log[1], InputEntry::Scroll { dy: 120, .. }));
        assert_eq!(log[2], InputEntry::Text("hi".to_owned()));
        assert!(tv.launch(vec!["true".to_owned()]).await.unwrap() > 0);
        let mpv = tv.window(ww_niri_api::WindowId(2));
        mpv.hide().await.unwrap();
        assert!(fake.windows()[1].hidden);
        mpv.show(None, Some(true)).await.unwrap();
        assert!(!fake.windows()[1].hidden);
    });
}

#[test]
fn moves_merge_while_a_call_is_in_flight() {
    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        fake.set_latency(Duration::from_millis(30));
        for _ in 0..20 {
            tv.send(Command::MoveBy { dx: 1, dy: 0 }).unwrap();
        }
        tv.send(Command::Click {
            at: None,
            button: Button::Left,
            count: 1,
            modifiers: Modifiers::default(),
        })
        .unwrap();
        for v in 41..=60 {
            tv.send(Command::SetVolume(v)).unwrap();
        }
        until_fake("the click", || !fake.input_log().is_empty()).await;
        until_fake("volume 60", || fake.audio().0 == 60).await;
        // The click lands where all the moves went, and few calls carried them.
        let InputEntry::Click { x, .. } = fake.input_log()[0] else {
            panic!("a click");
        };
        assert_eq!(x, 980);
        let moves = calls_of(&fake, "input.pointer.move_by");
        assert!(moves.len() <= 3, "{moves:?}");
        let volumes = calls_of(&fake, "audio.volume");
        assert!(volumes.len() <= 3, "{volumes:?}");
        // Commands of other lanes don't wait for the input lane.
        let order: Vec<_> = fake.calls().iter().map(|c| c.method).collect();
        let click = order
            .iter()
            .position(|m| *m == "input.pointer.click")
            .unwrap();
        let volume = order.iter().position(|m| *m == "audio.volume").unwrap();
        assert!(volume < click, "{order:?}");
    });
}

#[test]
fn failed_sends_come_on_the_error_channel() {
    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        let mut errors = tv.errors();
        fake.fail_next(
            "browsers.start",
            Fail::Browser(BrowserErrorOwned::Spawn("no firefox".to_owned())),
        )
        .unwrap();
        tv.send(Command::SlotStart {
            slot: 0,
            fullscreen: true,
        })
        .unwrap();
        let failed = super::soon("an error", errors.recv()).await.unwrap();
        assert_eq!(
            failed.command,
            Command::SlotStart {
                slot: 0,
                fullscreen: true
            }
        );
        assert!(matches!(
            failed.error,
            TvError::Browser(BrowserErrorOwned::Spawn(ref e)) if e == "no firefox"
        ));
    });
}

#[test]
fn held_buttons_are_renewed_until_released() {
    // The hold limit runs on the fake's clock: a real one here.
    let fake = FakeTv::start(Options {
        clock: ClockMode::Real,
        ..Options::default()
    });
    run(async {
        let tv = ww_niri_client::Tv::start(config(&fake).hold_limit(Duration::from_millis(300)))
            .unwrap();
        super::connected(&tv).await;
        tv.pointer().press(Button::Left).await.unwrap();
        tv.keyboard().key_down("shift".to_owned()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1000)).await;
        // Still held after three hold limits: renewed.
        assert_eq!(
            fake.input_log(),
            [
                InputEntry::Press(Button::Left),
                InputEntry::KeyDown("shift".to_owned())
            ]
        );
        assert!(calls_of(&fake, "input.pointer.press").len() >= 3);
        tv.release_all().await.unwrap();
        assert_eq!(
            fake.input_log()[2..],
            [
                InputEntry::Release {
                    button: Button::Left,
                    by: ReleasedBy::Call
                },
                InputEntry::KeyUp {
                    key: "shift".to_owned(),
                    by: ReleasedBy::Call
                }
            ]
        );
        // Nothing is renewed after the release.
        let presses = calls_of(&fake, "input.pointer.press").len();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(calls_of(&fake, "input.pointer.press").len(), presses);
    });
}

#[test]
fn a_point_in_a_window() {
    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        let to = Point::Window {
            id: ww_niri_api::WindowId(2),
            x: Fixed::new(10),
            y: Fixed::new(20),
        };
        tv.pointer().move_to(to).await.unwrap();
        let at = fake.pointer().window.unwrap();
        assert_eq!((at.id.0, at.x.get(), at.y.get()), (2, 10, 20));
    });
}

/// An ad cover over `on` for `lasts_ms`.
fn ad_cover(on: ww_niri_api::SurfaceOwned, lasts_ms: u32, mute: bool) -> ww_niri_api::ItemOwned {
    use ww_niri_api::{At, BackdropOwned, CardOwned, ContentOwned, ItemOwned, PlaceOwned};

    ItemOwned {
        place: PlaceOwned { on, at: At::Fill },
        content: ContentOwned::Card(CardOwned {
            title: "Реклама".to_owned(),
            countdown: Some("Передача вернётся через".to_owned()),
            clock: true,
            footer: None,
        }),
        backdrop: BackdropOwned {
            blur: 2,
            dim: 65,
            ..BackdropOwned::default()
        },
        priority: 150,
        lasts: Some(Fixed::new(lasts_ms)),
        mute,
        dismissible: true,
        report_shown: false,
    }
}

#[test]
fn overlay_items_reach_the_tv_and_lift_with_the_connection() {
    use ww_niri_api::{BrowserSlot, ItemEnd, OverlayError, SurfaceOwned, WindowId};
    use ww_niri_client::OverlayEnded;

    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        let mut ended = tv.overlay_ended();
        let mut next_ended = async || {
            super::soon("an item lifts", ended.recv())
                .await
                .expect("no ended item missed")
        };
        let window = SurfaceOwned::Window(WindowId(2));
        let overlay = tv.overlay();
        let key = || "ad".to_owned();
        overlay
            .put(key(), ad_cover(window.clone(), 130_000, true))
            .await
            .unwrap();
        let item = fake.overlay()[0].clone();
        assert_eq!(item.window, Some(WindowId(2)));
        assert!(item.item.mute);
        until_fake("the sink is muted", || fake.audio().1).await;
        assert!(matches!(
            overlay
                .put(
                    key(),
                    ad_cover(SurfaceOwned::Slot(BrowserSlot(0)), 1000, false)
                )
                .await,
            Err(TvError::Overlay(OverlayError::NoSuchWindow))
        ));
        overlay.lift(key()).await.unwrap();
        assert!(fake.overlay().is_empty());
        assert!(!fake.audio().1);
        assert_eq!(
            next_ended().await,
            OverlayEnded {
                id: item.id,
                key: Some(key()),
                why: ItemEnd::Lifted
            }
        );

        // Its end.
        overlay
            .put(key(), ad_cover(window.clone(), 5_000, false))
            .await
            .unwrap();
        fake.advance(Duration::from_secs(5));
        assert_eq!(next_ended().await.why, ItemEnd::Deadline);

        // Another client's item comes without a key.
        let other = connect(&fake).await;
        other
            .overlay()
            .put(key(), ad_cover(window.clone(), 60_000, false))
            .await
            .unwrap();
        let theirs = fake.overlay()[0].id;
        overlay.dismiss(theirs).await.unwrap();
        assert_eq!(
            next_ended().await,
            OverlayEnded {
                id: theirs,
                key: None,
                why: ItemEnd::Dismissed
            }
        );

        // A reconnect lifts it, and the client tells: it puts up again what should stay.
        overlay
            .put(key(), ad_cover(window, 130_000, true))
            .await
            .unwrap();
        fake.drop_connection();
        until_fake("the item lifts", || fake.overlay().is_empty()).await;
        let got = next_ended().await;
        assert_eq!((got.key, got.why), (Some(key()), ItemEnd::Disconnected));
        assert!(!fake.audio().1);
        super::connected(&tv).await;
        assert!(ended.try_recv().is_err());
    });
}

#[test]
fn the_owner_alone_hears_its_item_shown() {
    use ww_niri_api::{SurfaceOwned, WindowId};

    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        let other = connect(&fake).await;
        let (mut shown, mut theirs) = (tv.overlay_shown(), other.overlay_shown());
        let window = SurfaceOwned::Window(WindowId(2));
        fake.advance(Duration::from_millis(1500));
        let mut cover = ad_cover(window.clone(), 60_000, false);
        cover.report_shown = true;
        tv.overlay().put("ad".to_owned(), cover).await.unwrap();
        let id = fake.overlay()[0].id;
        let got = super::soon("the item shows", shown.recv()).await.unwrap();
        assert_eq!((got.id, got.at.get()), (id, 1_500_000));
        // Without `report_shown`, nothing.
        tv.overlay()
            .put("quiet".to_owned(), ad_cover(window, 60_000, false))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(shown.try_recv().is_err());
        assert!(theirs.try_recv().is_err(), "only the owner hears it");
    });
}

#[test]
fn show_only_reaches_the_tv() {
    use ww_niri_api::{Band, ShowOnlyOwned};

    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        let status = ShowOnlyOwned {
            bands: vec![Band {
                lowest: 192,
                highest: 255,
            }],
            ..ShowOnlyOwned::default()
        };
        tv.overlay().show_only(Some(status.clone())).await.unwrap();
        assert_eq!(fake.show_only(), Some(status));
        tv.overlay().show_only(None).await.unwrap();
        assert_eq!(fake.show_only(), None);
    });
}
