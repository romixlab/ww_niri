//! The connection: a TV that comes up late, drops and comes back, and a Tv without a runtime.

use std::time::Duration;

use ww_niri_client::{ConnectionState, Tv, TvError};

use super::{config, connected, fake, run, soon, until};

#[test]
fn a_dropped_link_comes_back_and_the_model_goes_on() {
    let fake = fake();
    run(async {
        let tv = super::connect(&fake).await;
        until(&tv, "volume", |s| s.audio.volume.is_some()).await;
        let mut state = tv.watch_state();

        fake.set_reachable(false);
        soon("offline", state.wait_for(|s| !s.is_connected()))
            .await
            .unwrap();
        let ConnectionState::Connecting {
            offline_since,
            last_error,
            ..
        } = tv.state()
        else {
            panic!("trying again");
        };
        assert!(offline_since.is_some() && last_error.is_some());
        assert!(matches!(tv.info().await, Err(TvError::Offline)));
        assert!(matches!(
            tv.pointer().move_by(1, 1).await,
            Err(TvError::Offline)
        ));
        // The last state stays to show while offline.
        assert_eq!(tv.snapshot().audio.volume, Some(40));

        fake.set_audio(66, false);
        fake.set_reachable(true);
        connected(&tv).await;
        // Listed again, with what changed during the gap; the observers go on after.
        until(&tv, "volume 66", |s| s.audio.volume == Some(66)).await;
        fake.set_audio(67, false);
        until(&tv, "volume 67", |s| s.audio.volume == Some(67)).await;
        assert!(fake.links_up() >= 2);

        // A drop with an immediate reconnect.
        fake.drop_connection();
        soon("links up again", async {
            while fake.links_up() < 3 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
        connected(&tv).await;
        fake.set_audio(68, false);
        until(&tv, "volume 68", |s| s.audio.volume == Some(68)).await;
    });
}

#[test]
fn a_tv_that_comes_up_late() {
    let fake = fake();
    fake.set_reachable(false);
    run(async {
        let tv = Tv::start(config(&fake)).unwrap();
        let mut state = tv.watch_state();
        soon(
            "a failed attempt",
            state.wait_for(
                |s| matches!(s, ConnectionState::Connecting { attempt, .. } if *attempt >= 2),
            ),
        )
        .await
        .unwrap();
        fake.set_reachable(true);
        connected(&tv).await;
        until(&tv, "volume", |s| s.audio.volume == Some(40)).await;
    });
}

#[test]
fn without_a_runtime_it_runs_its_own() {
    let fake = fake();
    let changes = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counted = changes.clone();
    let tv = Tv::start(config(&fake).on_change(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }))
    .unwrap();
    let mut waited = Duration::ZERO;
    while tv.snapshot().audio.volume.is_none() {
        assert!(waited < Duration::from_secs(5), "volume within 5 s");
        std::thread::sleep(Duration::from_millis(5));
        waited += Duration::from_millis(5);
    }
    assert!(tv.state().is_connected());
    assert!(changes.load(std::sync::atomic::Ordering::Relaxed) > 0);
    tv.send(ww_niri_client::Command::SetVolume(77)).unwrap();
    while fake.audio().0 != 77 {
        assert!(waited < Duration::from_secs(5), "volume 77 within 5 s");
        std::thread::sleep(Duration::from_millis(5));
        waited += Duration::from_millis(5);
    }
}
