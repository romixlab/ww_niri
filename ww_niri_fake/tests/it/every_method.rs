//! Every method and property of the API, answered by the fake as the TV would.

use std::collections::BTreeSet;
use std::time::Duration;

use exact_units::Fixed;
use wire_weaver_client::CallError;
use ww_niri_api::{
    BrowserErrorOwned, BrowserPhase, BrowserSlot, Button, InputErrorOwned, LaunchErrorOwned,
    MediaErrorOwned, Modifiers, MotionSeq, PlaybackStatus, PlayerSourceOwned, Point, PointerMotion,
    Volume, WindowError, WindowId,
};
use ww_niri_fake::{ErrorKind, InputEntry, ReleasedBy, METHODS};

use super::{connect, fake, run};

fn global(x: i32, y: i32) -> Point {
    Point::Global {
        x: Fixed::new(x),
        y: Fixed::new(y),
    }
}

#[test]
fn every_method_answers() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        let _events = client.events().call().await.unwrap();

        // info, windows, outputs, apps
        let info = client.info().call().await.unwrap();
        assert_eq!(info.hostname, "fake-tv");
        let windows = client.windows().list().call().await.unwrap();
        let apps: Vec<_> = windows.iter().map(|w| w.app_id.as_deref()).collect();
        assert_eq!(apps, [Some("kitty"), Some("mpv")]);
        assert!(windows[0].focused);
        let outputs = client.outputs().list().call().await.unwrap();
        assert_eq!(
            (outputs[0].width.get(), outputs[0].height.get()),
            (1920, 1080)
        );

        let w = client.windows();
        w.show(WindowId(2), None, Some(true)).call().await.unwrap();
        w.focus(WindowId(1)).call().await.unwrap();
        w.set_fullscreen(WindowId(2), false).call().await.unwrap();
        assert!(matches!(
            w.close(WindowId(99)).call().await,
            Err(CallError::Device(WindowError::NoSuchWindow))
        ));

        w.cover(WindowId(2), super::covers::card(), Fixed::new(1000), true)
            .call()
            .await
            .unwrap();
        w.uncover(WindowId(2)).call().await.unwrap();

        let apps = client.apps();
        let launched = apps.launch(vec!["true".to_owned()]).call().await.unwrap();
        assert!(launched.pid > 0);
        assert!(matches!(
            apps.launch(Vec::new()).call().await,
            Err(CallError::Device(LaunchErrorOwned::EmptyArgv))
        ));

        // A browser slot: start, open a page that plays, its playback, show, stop.
        let slot = client.browsers(0);
        slot.start(true).call().await.unwrap();
        assert_eq!(
            slot.read_state().read().await.unwrap().phase,
            BrowserPhase::Starting
        );
        assert!(matches!(
            slot.open("ftp://example.org/".to_owned()).call().await,
            Err(CallError::Device(BrowserErrorOwned::UrlNotHttp))
        ));
        assert!(matches!(
            slot.show(None, None).call().await,
            Err(CallError::Device(BrowserErrorOwned::NoWindow))
        ));
        tv.advance(Duration::from_secs(1));
        slot.open("https://example.org/watch".to_owned())
            .call()
            .await
            .unwrap();
        let state = slot.read_state().read().await.unwrap();
        assert_eq!(state.phase, BrowserPhase::Running);
        assert_eq!(state.url.as_deref(), Some("https://example.org/watch"));
        assert!(state.window.is_some() && state.player.is_some());
        slot.show(None, Some(false)).call().await.unwrap();
        slot.cover(super::covers::card(), Fixed::new(1000), false)
            .call()
            .await
            .unwrap();
        slot.uncover().call().await.unwrap();

        let playback = slot.playback();
        assert_eq!(
            playback.read_state().read().await.unwrap().status,
            PlaybackStatus::Playing
        );
        playback.pause().call().await.unwrap();
        playback.play().call().await.unwrap();
        playback.play_pause().call().await.unwrap();
        playback.next().call().await.unwrap();
        playback.previous().call().await.unwrap();
        playback.seek(Fixed::new(30_000_000)).call().await.unwrap();
        playback
            .set_position(Fixed::new(10_000_000))
            .call()
            .await
            .unwrap();
        let state = playback.read_state().read().await.unwrap();
        assert_eq!(
            (state.status, state.position.get()),
            (PlaybackStatus::Paused, 10_000_000)
        );
        playback.stop().call().await.unwrap();

        // The players: mpv's and the slot's.
        let media = client.media();
        let players = media.list().call().await.unwrap();
        assert_eq!(players.len(), 2);
        let browser_player = players
            .iter()
            .find(|p| p.source == PlayerSourceOwned::Browser(BrowserSlot(0)))
            .expect("the slot's player is listed");
        let player = media.players(browser_player.id.0);
        assert_eq!(
            player.read_source().read().await.unwrap(),
            PlayerSourceOwned::Browser(BrowserSlot(0))
        );
        let playback = player.playback();
        playback.play().call().await.unwrap();
        playback.pause().call().await.unwrap();
        playback.play_pause().call().await.unwrap();
        playback.next().call().await.unwrap();
        playback.previous().call().await.unwrap();
        playback.seek(Fixed::new(-5_000_000)).call().await.unwrap();
        playback.set_position(Fixed::new(0)).call().await.unwrap();
        playback.stop().call().await.unwrap();
        assert_eq!(
            playback.read_state().read().await.unwrap().status,
            PlaybackStatus::Stopped
        );

        slot.stop().call().await.unwrap();
        assert!(matches!(
            slot.stop().call().await,
            Err(CallError::Device(BrowserErrorOwned::NotRunning))
        ));
        assert!(matches!(
            slot.playback().play().call().await,
            Err(CallError::Device(MediaErrorOwned::NoPlayer))
        ));

        // Audio
        let audio = client.audio();
        assert_eq!(audio.read_volume().read().await.unwrap().get(), 40);
        audio
            .write_volume(Volume::try_new(125).unwrap())
            .write()
            .await
            .unwrap();
        audio.write_muted(true).write().await.unwrap();
        assert!(audio.read_muted().read().await.unwrap());
        assert_eq!(audio.read_sink().read().await.unwrap().name, "fake_sink");
        assert_eq!(tv.audio(), (125, true));

        // Input
        let input = client.input();
        let (pointer, keyboard) = (input.pointer(), input.keyboard());
        pointer.move_to(global(100, 200)).call().await.unwrap();
        assert!(matches!(
            pointer.move_to(global(1920, 0)).call().await,
            Err(CallError::Device(InputErrorOwned::OffScreen))
        ));
        pointer
            .move_by(Fixed::new(-500), Fixed::new(5000))
            .call()
            .await
            .unwrap();
        let at = pointer.read_position().read().await.unwrap();
        assert_eq!((at.x.get(), at.y.get()), (0, 1079));
        // Motion: the first write is where the totals start, a duplicate and a late write are
        // dropped, a lost one's distance comes with the next.
        let mut motion = pointer.motion().await.unwrap();
        for (seq, dx, dy) in [
            (7, 100, 100),
            (8, 110, 91),
            (8, 110, 91),
            (10, 111, 90),
            (9, 0, 0),
        ] {
            motion
                .send(PointerMotion {
                    seq: MotionSeq::new(seq),
                    dx_total: Fixed::new(dx),
                    dy_total: Fixed::new(dy),
                })
                .await
                .unwrap();
        }
        // Requests are handled in order: the writes are done once this answers.
        let at = pointer.read_position().read().await.unwrap();
        assert_eq!((at.x.get(), at.y.get()), (11, 1069));
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        pointer
            .click(Some(global(10, 10)), Button::Left, 2, Modifiers::default())
            .call()
            .await
            .unwrap();
        assert!(matches!(
            pointer
                .click(None, Button::Left, 4, Modifiers::default())
                .call()
                .await,
            Err(CallError::Device(InputErrorOwned::BadArgument))
        ));
        pointer.press(Button::Left, None).call().await.unwrap();
        pointer.release(Button::Left).call().await.unwrap();
        pointer
            .drag(global(0, 0), global(50, 60), Button::Left, Fixed::new(100))
            .call()
            .await
            .unwrap();
        pointer.scroll(None, 0, 60, ctrl).call().await.unwrap();
        keyboard.type_text("hi\n".to_owned()).call().await.unwrap();
        assert!(matches!(
            keyboard.type_text("ж".to_owned()).call().await,
            Err(CallError::Device(InputErrorOwned::NotTypeable(0x436)))
        ));
        keyboard
            .press_keys("ctrl+l".to_owned())
            .call()
            .await
            .unwrap();
        keyboard
            .key_down("XF86AudioPlay".to_owned(), Some(Fixed::new(500)))
            .call()
            .await
            .unwrap();
        keyboard
            .key_up("XF86AudioPlay".to_owned())
            .call()
            .await
            .unwrap();
        assert!(matches!(
            keyboard.key_down("Nope".to_owned(), None).call().await,
            Err(CallError::Device(InputErrorOwned::UnknownKey(k))) if k == "Nope"
        ));

        assert_eq!(
            tv.input_log(),
            [
                InputEntry::Click {
                    x: 10,
                    y: 10,
                    button: Button::Left,
                    count: 2,
                    modifiers: Modifiers::default()
                },
                InputEntry::Press(Button::Left),
                InputEntry::Release {
                    button: Button::Left,
                    by: ReleasedBy::Call
                },
                InputEntry::Drag {
                    from: (0, 0),
                    to: (50, 60),
                    button: Button::Left
                },
                InputEntry::Scroll {
                    x: 50,
                    y: 60,
                    dx: 0,
                    dy: 60,
                    modifiers: ctrl
                },
                InputEntry::Text("hi\n".to_owned()),
                InputEntry::Keys("ctrl+l".to_owned()),
                InputEntry::KeyDown("XF86AudioPlay".to_owned()),
                InputEntry::KeyUp {
                    key: "XF86AudioPlay".to_owned(),
                    by: ReleasedBy::Call
                },
            ]
        );
    });

    // Every method, and every writable property, is on the record.
    let called: BTreeSet<&str> = tv.calls().iter().map(|c| c.method).collect();
    let expected: BTreeSet<&str> = METHODS
        .iter()
        .filter(|(m, kind)| {
            *kind != ErrorKind::Property || ["audio.volume", "audio.muted"].contains(m)
        })
        .map(|(m, _)| *m)
        .collect();
    assert_eq!(called, expected);
    let shown: Vec<String> = tv.calls().iter().map(ToString::to_string).collect();
    assert!(shown.contains(&"browsers[0].open(\"https://example.org/watch\")".to_owned()));
    assert!(shown.contains(&"audio.volume = 125".to_owned()));
}
