//! The snapshot follows the TV: lists, observers and events, whoever made the change.

use std::time::{Duration, Instant};

use ww_niri_api::{BrowserPhase, BrowserSlot, PlaybackStatus, PlayerSourceOwned, WindowId};
use ww_niri_client::ConnectionState;

use super::{connect, fake, run, until};

#[test]
fn the_snapshot_lists_the_tv() {
    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        let ConnectionState::Connected { info, .. } = tv.state() else {
            panic!("connected");
        };
        assert_eq!(info.hostname, "fake-tv");
        // Observers deliver their first values right after the lists.
        let s = until(&tv, "audio and pointer", |s| {
            s.audio.volume.is_some() && s.pointer.is_some() && s.audio.sink.is_some()
        })
        .await;
        let apps: Vec<_> = s.windows.iter().map(|w| w.app_id.as_deref()).collect();
        assert_eq!(apps, [Some("kitty"), Some("mpv")]);
        assert_eq!(s.focused().map(|w| w.id), Some(WindowId(1)));
        assert_eq!(s.outputs.len(), 1);
        assert_eq!(s.slots.iter().map(|s| s.index).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(s.players.len(), 1);
        assert_eq!(s.audio.volume, Some(40));
        assert_eq!(s.pointer.map(|p| (p.x.get(), p.y.get())), Some((960, 540)));
        let s = until(&tv, "the slots' states", |s| {
            s.slots.iter().all(|slot| slot.state.is_some())
        })
        .await;
        assert_eq!(
            s.slot(0).unwrap().state.as_ref().unwrap().phase,
            BrowserPhase::Stopped
        );
    });
}

#[test]
fn changes_on_the_tv_reach_the_snapshot() {
    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        fake.set_audio(70, true);
        until(&tv, "volume 70, muted", |s| {
            s.audio.volume == Some(70) && s.audio.muted == Some(true)
        })
        .await;

        let id = fake.open_window("org.telegram.desktop", "Telegram");
        let s = until(&tv, "Telegram's window", |s| s.window(id).is_some()).await;
        assert_eq!(s.focused().map(|w| w.id), Some(id));

        // The mpv window closes: its player goes with it.
        fake.close_window(WindowId(2)).unwrap();
        until(&tv, "no mpv", |s| {
            s.window(WindowId(2)).is_none() && s.players.is_empty()
        })
        .await;
    });
}

#[test]
fn a_slot_starts_plays_and_its_position_moves() {
    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        let slot = tv.slot(1);
        slot.start(false).await.unwrap();
        until(&tv, "Starting", |s| {
            s.slot(1).and_then(|x| x.state.as_ref()).map(|x| x.phase)
                == Some(BrowserPhase::Starting)
        })
        .await;
        fake.advance(Duration::from_millis(500));
        let s = until(&tv, "Running with its window", |s| {
            s.slot(1)
                .and_then(|x| x.state.as_ref())
                .is_some_and(|x| x.phase == BrowserPhase::Running)
                && s.windows.iter().any(|w| w.browser == Some(BrowserSlot(1)))
        })
        .await;
        assert!(s.slot(0).unwrap().state.as_ref().unwrap().phase == BrowserPhase::Stopped);

        slot.open("https://example.org/a".to_owned()).await.unwrap();
        let s = until(&tv, "the slot plays", |s| {
            s.slot(1)
                .and_then(|x| x.playback.as_ref())
                .is_some_and(|p| p.state.status == PlaybackStatus::Playing)
                && s.players.len() == 2
        })
        .await;
        assert!(
            s.players
                .iter()
                .any(|p| p.info.source == PlayerSourceOwned::Browser(BrowserSlot(1)))
        );

        // Between updates the position moves on with the clock of the client.
        let playback = s.slot(1).unwrap().playback.clone().unwrap();
        let at = playback.received + Duration::from_secs(3);
        let moved = playback.position_at(at) - playback.position_at(playback.received);
        assert_eq!(moved, Duration::from_secs(3));
        assert!(playback.position_at(Instant::now()) >= playback.position_at(playback.received));

        slot.playback().pause().await.unwrap();
        until(&tv, "Paused", |s| {
            s.slot(1)
                .and_then(|x| x.playback.as_ref())
                .is_some_and(|p| p.state.status == PlaybackStatus::Paused)
        })
        .await;

        slot.stop().await.unwrap();
        until(&tv, "Stopped, its player gone", |s| {
            s.slot(1)
                .and_then(|x| x.state.as_ref())
                .is_some_and(|x| x.phase == BrowserPhase::Stopped)
                && s.players.len() == 1
        })
        .await;
    });
}

#[test]
fn a_resync_lists_again() {
    let fake = fake();
    run(async {
        let tv = connect(&fake).await;
        let before = until(&tv, "volume", |s| s.audio.volume.is_some()).await;
        fake.resync();
        let after = until(&tv, "a new snapshot", |s| s.generation > before.generation).await;
        assert_eq!(after.windows, before.windows);
        assert!(tv.state().is_connected());
        // It still follows the TV after the resync.
        fake.set_audio(12, false);
        until(&tv, "volume 12", |s| s.audio.volume == Some(12)).await;
    });
}
