//! Covers: set, renewed, lifted at their end, on `uncover`, with the connection and the window.

use std::time::Duration;

use exact_units::Fixed;
use wire_weaver_client::CallError;
use ww_niri_api::{CardOwned, CoverEnd, CoverError, EventOwned, WindowId};

use super::{connect, fake, run, soon};

pub fn card() -> CardOwned {
    CardOwned {
        title: "Реклама".to_owned(),
        countdown: Some("Передача вернётся через".to_owned()),
        clock: true,
        footer: Some("Первый канал".to_owned()),
    }
}

#[test]
fn covers_lift_by_end_uncover_window_and_connection() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        let mut events = client.events().call().await.unwrap();
        let mut next = async || soon("an event", events.recv()).await.unwrap();
        let w = client.windows();

        // Lifts at its end; a renewal moves the end.
        w.cover(WindowId(2), card(), Fixed::new(10_000), false)
            .call()
            .await
            .unwrap();
        tv.advance(Duration::from_secs(8));
        w.cover(WindowId(2), card(), Fixed::new(5_000), false)
            .call()
            .await
            .unwrap();
        let covers = tv.covers();
        assert_eq!(covers.len(), 1);
        assert_eq!(
            (covers[0].window, covers[0].until_us),
            (WindowId(2), 13_000_000)
        );
        tv.advance(Duration::from_secs(5));
        assert_eq!(
            next().await,
            EventOwned::CoverLifted {
                window: WindowId(2),
                why: CoverEnd::Deadline
            }
        );
        assert!(tv.covers().is_empty());

        // Uncovered, then not covered.
        w.cover(WindowId(2), card(), Fixed::new(10_000), false)
            .call()
            .await
            .unwrap();
        w.uncover(WindowId(2)).call().await.unwrap();
        assert_eq!(
            next().await,
            EventOwned::CoverLifted {
                window: WindowId(2),
                why: CoverEnd::Uncovered
            }
        );
        let device = |res| match res {
            Err(CallError::Device(err)) => err,
            other => panic!("not a device error: {other:?}"),
        };
        assert_eq!(
            device(w.uncover(WindowId(2)).call().await),
            CoverError::NotCovered
        );
        assert_eq!(
            device(
                w.cover(WindowId(99), card(), Fixed::new(1000), false)
                    .call()
                    .await
            ),
            CoverError::NoSuchWindow
        );
        assert_eq!(
            device(
                w.cover(WindowId(2), card(), Fixed::new(0), false)
                    .call()
                    .await
            ),
            CoverError::BadArgument
        );
        assert_eq!(
            device(
                client
                    .browsers(0)
                    .cover(card(), Fixed::new(1000), false)
                    .call()
                    .await
            ),
            CoverError::NoSuchWindow
        );

        // The window closes under it.
        w.cover(WindowId(2), card(), Fixed::new(10_000), false)
            .call()
            .await
            .unwrap();
        tv.close_window(WindowId(2)).unwrap();
        assert_eq!(
            next().await,
            EventOwned::CoverLifted {
                window: WindowId(2),
                why: CoverEnd::WindowClosed
            }
        );

        // The connection that held it goes.
        w.cover(WindowId(1), card(), Fixed::new(10_000), false)
            .call()
            .await
            .unwrap();
        tv.drop_connection();
        soon("the cover lifts with the connection", async {
            while !tv.covers().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    });
}

#[test]
fn muting_covers_hold_the_mute() {
    let tv = fake();
    run(async {
        let client = connect(&tv).await.unwrap();
        let w = client.windows();
        let audio = client.audio();
        let muted = async || audio.read_muted().read().await.unwrap();

        // Muted with the first muting cover, back when the last one lifts.
        w.cover(WindowId(1), card(), Fixed::new(10_000), false)
            .call()
            .await
            .unwrap();
        assert!(!muted().await);
        w.cover(WindowId(1), card(), Fixed::new(10_000), true)
            .call()
            .await
            .unwrap();
        w.cover(WindowId(2), card(), Fixed::new(20_000), true)
            .call()
            .await
            .unwrap();
        assert!(muted().await);
        tv.advance(Duration::from_secs(10));
        assert!(muted().await);
        w.uncover(WindowId(2)).call().await.unwrap();
        assert!(!muted().await);

        // A user's unmute holds through renewals.
        w.cover(WindowId(2), card(), Fixed::new(10_000), true)
            .call()
            .await
            .unwrap();
        assert!(muted().await);
        audio.write_muted(false).write().await.unwrap();
        w.cover(WindowId(2), card(), Fixed::new(10_000), true)
            .call()
            .await
            .unwrap();
        assert!(!muted().await);
        w.uncover(WindowId(2)).call().await.unwrap();

        // Muted before: stays muted after.
        audio.write_muted(true).write().await.unwrap();
        w.cover(WindowId(2), card(), Fixed::new(10_000), true)
            .call()
            .await
            .unwrap();
        w.uncover(WindowId(2)).call().await.unwrap();
        assert!(muted().await);
        audio.write_muted(false).write().await.unwrap();

        // The connection that held it goes: the sink is unmuted.
        w.cover(WindowId(2), card(), Fixed::new(10_000), true)
            .call()
            .await
            .unwrap();
        assert!(muted().await);
        tv.drop_connection();
        soon("the mute lifts with the connection", async {
            while !tv.covers().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let client = connect(&tv).await.unwrap();
        assert!(!client.audio().read_muted().read().await.unwrap());
    });
}
