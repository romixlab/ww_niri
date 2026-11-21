//! Observers and the `events` stream follow what happens on the fake, whoever made it happen.

use std::time::Duration;

use exact_units::Fixed;
use wire_weaver_client::observe::{on_change, ObserveEvent};
use ww_niri_api::{
    BrowserPhase, BrowserSlot, Button, EventOwned, PlaybackStatus, PlayerId, PlayerSourceOwned,
    StopReason, WindowId,
};
use ww_niri_fake::{InputEntry, ReleasedBy};

use super::{connect, fake, run, soon};

#[test]
fn a_slot_starts_plays_and_stops() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        let mut events = client.events().call().await.unwrap();
        let mut next = async || soon("an event", events.recv()).await.unwrap();
        let slot = client.browsers(1);
        let mut phases = slot
            .observe_state(on_change(Duration::ZERO))
            .subscribe()
            .await
            .unwrap();
        let mut phase = async || soon("a slot state", phases.recv()).await.unwrap().value;
        assert_eq!(phase().await.phase, BrowserPhase::Stopped);

        // Driven at once, Running once its window maps after the start delay.
        slot.start(false).call().await.unwrap();
        assert_eq!(next().await, EventOwned::BrowserStarted(BrowserSlot(1)));
        assert_eq!(phase().await.phase, BrowserPhase::Starting);
        tv.advance(Duration::from_millis(499));
        assert_eq!(
            slot.read_state().read().await.unwrap().phase,
            BrowserPhase::Starting
        );
        tv.advance(Duration::from_millis(1));
        let EventOwned::WindowOpenedOrChanged(window) = next().await else {
            panic!("the slot's window opens");
        };
        assert_eq!(window.browser, Some(BrowserSlot(1)));
        assert_eq!(
            next().await,
            EventOwned::WindowFocusChanged(Some(window.id))
        );
        let running = loop {
            let state = phase().await;
            if state.phase == BrowserPhase::Running {
                break state;
            }
        };
        assert_eq!(running.window, Some(window.id));

        // The page plays: a player appears, and its position moves with the clock.
        let mut playing = slot
            .playback()
            .observe_state(on_change(Duration::ZERO))
            .subscribe()
            .await
            .unwrap();
        slot.open("https://example.org/a".to_owned())
            .call()
            .await
            .unwrap();
        let player = loop {
            if let EventOwned::PlayerAppeared(info) = next().await {
                break info;
            }
        };
        assert_eq!(player.source, PlayerSourceOwned::Browser(BrowserSlot(1)));
        soon("Playing", async {
            while playing.recv().await.unwrap().value.status != PlaybackStatus::Playing {}
        })
        .await;
        tv.advance(Duration::from_secs(10));
        let state = slot.playback().read_state().read().await.unwrap();
        assert_eq!(state.position.get(), 10_000_000);
        assert_eq!(state.title.as_deref(), Some("example.org/a"));

        slot.playback().pause().call().await.unwrap();
        soon("Paused", async {
            while playing.recv().await.unwrap().value.status != PlaybackStatus::Paused {}
        })
        .await;

        // Stopped: its window and player go with it.
        slot.stop().call().await.unwrap();
        let mut rest = Vec::new();
        while !matches!(rest.last(), Some(EventOwned::BrowserStopped { .. })) {
            rest.push(next().await);
        }
        assert!(rest.contains(&EventOwned::WindowClosed(window.id)));
        assert!(rest.contains(&EventOwned::PlayerGone(player.id)));
        assert_eq!(
            rest.last(),
            Some(&EventOwned::BrowserStopped {
                slot: BrowserSlot(1),
                reason: StopReason::Stopped
            })
        );
        loop {
            if phase().await.phase == BrowserPhase::Stopped {
                break;
            }
        }
    });
}

#[test]
fn changes_made_on_the_tv_reach_the_client() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        let mut events = client.events().call().await.unwrap();
        let mut next = async || soon("an event", events.recv()).await.unwrap();

        // Volume and mute changed on the TV.
        let audio = client.audio();
        let mut volumes = audio
            .observe_volume(on_change(Duration::ZERO))
            .subscribe()
            .await
            .unwrap();
        tv.set_audio(70, false);
        soon("volume 70", async {
            while volumes.recv().await.unwrap().value.get() != 70 {}
        })
        .await;

        // The mpv window closes by itself: its player ends, and the player's observers.
        let mut mpv = client
            .media()
            .players(0)
            .playback()
            .observe_state(on_change(Duration::ZERO))
            .subscribe()
            .await
            .unwrap();
        tv.close_window(WindowId(2)).unwrap();
        assert_eq!(next().await, EventOwned::WindowClosed(WindowId(2)));
        assert_eq!(next().await, EventOwned::PlayerGone(PlayerId(0)));
        soon("the player's observer ends", async {
            while !matches!(mpv.recv_event().await, Ok(ObserveEvent::Ended)) {}
        })
        .await;

        // A window opened on the TV is shown and focused.
        let id = tv.open_window("org.telegram.desktop", "Telegram");
        let EventOwned::WindowOpenedOrChanged(window) = next().await else {
            panic!("the window opens");
        };
        assert_eq!((window.id, window.title.as_deref()), (id, Some("Telegram")));
        assert_eq!(next().await, EventOwned::WindowFocusChanged(Some(id)));
    });
}

#[test]
fn pointer_observers_and_hold_limits() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        let input = client.input();
        let pointer = input.pointer();
        let mut positions = pointer
            .observe_position(on_change(Duration::ZERO))
            .subscribe()
            .await
            .unwrap();
        // At the start: the middle of the output, the mpv window's left edge.
        let at = soon("the position", positions.recv()).await.unwrap().value;
        assert_eq!((at.x.get(), at.y.get()), (960, 540));
        assert_eq!(at.window.map(|w| (w.id, w.x.get())), Some((WindowId(2), 0)));

        pointer
            .move_by(Fixed::new(-10), Fixed::new(0))
            .call()
            .await
            .unwrap();
        let at = soon("the moved position", positions.recv())
            .await
            .unwrap()
            .value;
        assert_eq!(
            at.window.map(|w| (w.id, w.x.get())),
            Some((WindowId(1), 950))
        );

        // A held button comes up when its hold limit passes without a renewing press.
        pointer
            .press(Button::Left, Some(Fixed::new(1000)))
            .call()
            .await
            .unwrap();
        tv.advance(Duration::from_millis(900));
        pointer
            .press(Button::Left, Some(Fixed::new(1000)))
            .call()
            .await
            .unwrap();
        tv.advance(Duration::from_millis(900));
        assert_eq!(tv.input_log(), [InputEntry::Press(Button::Left)]);
        tv.advance(Duration::from_millis(100));
        assert_eq!(
            tv.take_input_log(),
            [
                InputEntry::Press(Button::Left),
                InputEntry::Release {
                    button: Button::Left,
                    by: ReleasedBy::HoldLimit
                }
            ]
        );

        // A key held by a connection that ends comes up with it.
        client
            .input()
            .keyboard()
            .key_down("shift".to_owned(), None)
            .call()
            .await
            .unwrap();
        tv.drop_connection();
        soon("the key comes up", async {
            while tv.input_log().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(
            tv.input_log()[1],
            InputEntry::KeyUp {
                key: "shift".to_owned(),
                by: ReleasedBy::LinkDown
            }
        );
    });
}

#[test]
fn a_screen_that_goes_and_comes_back_changes_the_outputs() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        let mut events = client.events().call().await.unwrap();
        let mut next = async || soon("an event", events.recv()).await.unwrap();
        assert_eq!(client.outputs().list().call().await.unwrap().len(), 1);

        tv.set_screen_connected(false);
        assert_eq!(next().await, EventOwned::OutputsChanged);
        assert!(client.outputs().list().call().await.unwrap().is_empty());
        assert!(tv.windows().iter().all(|w| w.output.is_none()));

        tv.set_screen_connected(true);
        loop {
            if next().await == EventOwned::OutputsChanged {
                break;
            }
        }
        let outputs = client.outputs().list().call().await.unwrap();
        assert_eq!(outputs.len(), 1);
        assert!(tv
            .windows()
            .iter()
            .all(|w| w.output.as_deref() == Some(outputs[0].name.as_str())));
    });
}

#[test]
fn slot_owners_stay_through_start_and_stop() {
    let tv = ww_niri_fake::FakeTv::start(ww_niri_fake::Options {
        clock: ww_niri_fake::ClockMode::Manual,
        start_delay: Duration::ZERO,
        slot_owners: vec![None, Some("Роман".to_owned())],
        ..ww_niri_fake::Options::default()
    });
    run(async {
        let client = connect(&tv).await.unwrap();
        let owner = async |slot: u32| {
            let state = client.browsers(slot).read_state().read().await.unwrap();
            state.owner
        };
        assert_eq!(owner(0).await, None);
        assert_eq!(owner(1).await.as_deref(), Some("Роман"));
        let slot = client.browsers(1);
        slot.start(true).call().await.unwrap();
        assert_eq!(owner(1).await.as_deref(), Some("Роман"));
        slot.stop().call().await.unwrap();
        assert_eq!(owner(1).await.as_deref(), Some("Роман"));
    });
}
