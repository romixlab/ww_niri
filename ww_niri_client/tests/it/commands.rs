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

#[test]
fn covers_reach_the_tv_and_lift_with_the_connection() {
    use ww_niri_api::{CardOwned, CoverError, WindowId};

    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        let card = CardOwned {
            title: "Реклама".to_owned(),
            countdown: Some("Передача вернётся через".to_owned()),
            clock: true,
            footer: None,
        };
        let window = tv.window(WindowId(2));
        window
            .cover(card.clone(), Duration::from_secs(130))
            .await
            .unwrap();
        assert_eq!(fake.covers()[0].window, WindowId(2));
        assert!(matches!(
            tv.slot(0).cover(card.clone(), Duration::from_secs(1)).await,
            Err(TvError::Cover(CoverError::NoSuchWindow))
        ));
        assert!(matches!(
            window
                .cover(card.clone(), Duration::from_secs(u64::from(u32::MAX)))
                .await,
            Err(TvError::BadArgument {
                what: "cover lasts"
            })
        ));
        window.uncover().await.unwrap();
        assert!(fake.covers().is_empty());

        // A reconnect lifts it: the client renews what should stay.
        window.cover(card, Duration::from_secs(130)).await.unwrap();
        fake.drop_connection();
        until_fake("the cover lifts", || fake.covers().is_empty()).await;
    });
}
