//! Pointer motion over the fake's simulated UDP device.

use std::time::Duration;

use wire_weaver_client::ConnectionEvent;
use ww_niri_client::{Backoff, KeepTrying, MotionAddress, MotionSender};
use ww_niri_fake::FakeTv;

use super::{fake, run, soon, until_fake};

/// Connects to the fake's motion path, retrying fast.
fn sender(fake: &FakeTv) -> MotionSender {
    let keep_trying = KeepTrying {
        backoff: Backoff {
            first: Duration::from_millis(5),
            max: Duration::from_millis(50),
        },
        give_up: None,
    };
    MotionSender::start(
        MotionAddress::InProcess(fake.motion_path().to_owned()),
        keep_trying,
    )
    .unwrap()
}

/// The motion writes the fake got, as `(session, dx_total, dy_total)`, repeats included.
fn writes(fake: &FakeTv) -> Vec<(u16, i32, i32)> {
    fake.calls()
        .iter()
        .filter(|c| c.method == "input.pointer.motion")
        .map(|c| {
            let text = c.to_string();
            let args = text
                .strip_prefix("input.pointer.motion(")
                .and_then(|rest| rest.strip_suffix(')'))
                .unwrap_or_else(|| panic!("a motion write: {text}"));
            let [session, dx, dy] = args.split(", ").collect::<Vec<_>>()[..] else {
                panic!("three numbers: {text}");
            };
            (
                session.parse().unwrap(),
                dx.parse().unwrap(),
                dy.parse().unwrap(),
            )
        })
        .collect()
}

fn at(fake: &FakeTv) -> (i32, i32) {
    let p = fake.pointer();
    (p.x.get(), p.y.get())
}

#[test]
fn moves_add_up_and_a_new_link_starts_a_session_from_its_baseline() {
    let fake = fake();
    run(async {
        let motion = sender(&fake);
        // The first write of a session is where its totals start: it moves nothing.
        until_fake("the baseline", || !writes(&fake).is_empty()).await;
        assert!(motion.is_connected());
        let (first, dx, dy) = writes(&fake)[0];
        assert_eq!((dx, dy), (0, 0));
        assert_eq!(at(&fake), (960, 540));

        // Moves made faster than they go add up in the totals: the pointer gets all of them.
        for _ in 0..30 {
            motion.move_by(1, -1);
        }
        until_fake("30 moves", || at(&fake) == (990, 510)).await;
        assert_eq!(writes(&fake).last(), Some(&(first, 30, -30)));

        // A new link is a new session: its first write is the totals so far, which move
        // nothing again.
        fake.drop_connection();
        let new_session = || writes(&fake).into_iter().find(|w| w.0 != first);
        until_fake("a new session", || new_session().is_some()).await;
        let (second, dx, dy) = new_session().unwrap();
        assert_eq!((dx, dy), (30, -30));
        assert_eq!(at(&fake), (990, 510));
        motion.move_by(-10, 0);
        until_fake("a move on the new link", || at(&fake) == (980, 510)).await;
        assert_eq!(writes(&fake).last(), Some(&(second, 20, -30)));
    });
}

#[test]
fn a_move_while_the_link_waits_starts_the_next_attempt_at_once() {
    let fake = fake();
    run(async {
        // Without a hurry the next attempt would come in an hour.
        let hour = Duration::from_secs(3600);
        let motion = MotionSender::start(
            MotionAddress::InProcess(fake.motion_path().to_owned()),
            KeepTrying {
                backoff: Backoff {
                    first: hour,
                    max: hour,
                },
                give_up: None,
            },
        )
        .unwrap();
        until_fake("connected", || motion.is_connected()).await;

        let mut events = motion.connection_events();
        fake.set_reachable(false);
        soon("an attempt failed", async {
            loop {
                let event = events.recv().await.unwrap();
                if let ConnectionEvent::AttemptFailed { .. } = event {
                    break;
                }
            }
        })
        .await;
        fake.set_reachable(true);
        motion.move_by(1, 0);
        until_fake("connected again", || motion.is_connected()).await;
    });
}
