//! Pointer motion over the fake's simulated UDP device.

use std::time::Duration;

use ww_niri_client::{Backoff, KeepTrying, MotionAddress, MotionSender};
use ww_niri_fake::FakeTv;

use super::{fake, run, until_fake};

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

/// The motion writes the fake got, as `input.pointer.motion(seq, dx_total, dy_total)`.
fn writes(fake: &FakeTv) -> Vec<String> {
    fake.calls()
        .iter()
        .filter(|c| c.method == "input.pointer.motion")
        .map(ToString::to_string)
        .collect()
}

fn at(fake: &FakeTv) -> (i32, i32) {
    let p = fake.pointer();
    (p.x.get(), p.y.get())
}

#[test]
fn moves_add_up_and_a_new_link_starts_from_its_baseline() {
    let fake = fake();
    run(async {
        let motion = sender(&fake);
        // The first write of a link is where its totals start: it moves nothing.
        until_fake("the baseline", || writes(&fake).len() == 1).await;
        assert!(motion.is_connected());
        assert_eq!(writes(&fake), ["input.pointer.motion(1, 0, 0)"]);
        assert_eq!(at(&fake), (960, 540));

        // Moves made faster than they go add up in the totals: the pointer gets all of them.
        for _ in 0..30 {
            motion.move_by(1, -1);
        }
        until_fake("30 moves", || at(&fake) == (990, 510)).await;
        let sent = writes(&fake);
        assert!(sent.last().unwrap().ends_with(", 30, -30)"), "{sent:?}");

        // A new link: its first write is the totals so far, which move nothing again.
        let before = writes(&fake).len();
        fake.drop_connection();
        until_fake("a new baseline", || writes(&fake).len() > before).await;
        assert!(writes(&fake)[before].ends_with(", 30, -30)"));
        assert_eq!(at(&fake), (990, 510));
        motion.move_by(-10, 0);
        until_fake("a move on the new link", || at(&fake) == (980, 510)).await;
    });
}
