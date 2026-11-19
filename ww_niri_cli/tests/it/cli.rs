//! `wwn`'s commands, through the real dispatch, against `ww_niri_fake`.

use std::time::Duration;

use ww_niri_api::Button;
use ww_niri_fake::ReleasedBy;

use super::{connect, fake, run, wwn};

fn calls_of(fake: &ww_niri_fake::FakeTv, method: &str) -> Vec<String> {
    fake.calls()
        .iter()
        .filter(|c| c.method == method)
        .map(ToString::to_string)
        .collect()
}

/// The done-criterion: open a video in a slot, pause it, change the volume, click and type.
#[test]
fn drives_the_tv_end_to_end() {
    let fake = fake();
    run(async {
        wwn(&fake, &["slot", "0", "start"]).await.unwrap();
        wwn(&fake, &["slot", "0", "open", "https://example.org/video"])
            .await
            .unwrap();
        assert_eq!(calls_of(&fake, "browsers.open").len(), 1);

        wwn(&fake, &["slot", "0", "pause"]).await.unwrap();
        let paused = fake
            .calls()
            .iter()
            .any(|c| c.method == "browsers.playback.pause");
        assert!(paused, "{:?}", fake.calls());

        wwn(&fake, &["volume", "40"]).await.unwrap();
        assert_eq!(fake.audio(), (40, false));

        wwn(&fake, &["pointer", "click"]).await.unwrap();
        let log = fake.input_log();
        assert!(matches!(
            log.last(),
            Some(ww_niri_fake::InputEntry::Click {
                button: Button::Left,
                count: 1,
                ..
            })
        ));

        wwn(&fake, &["type", "hi"]).await.unwrap();
        assert_eq!(
            fake.input_log().last(),
            Some(&ww_niri_fake::InputEntry::Text("hi".to_owned()))
        );
    });
}

#[test]
fn slot_playback_actions_reach_the_slots_player() {
    let fake = fake();
    run(async {
        wwn(&fake, &["slot", "0", "start"]).await.unwrap();
        wwn(&fake, &["slot", "0", "open", "https://example.org/"])
            .await
            .unwrap();
        wwn(&fake, &["slot", "0", "seek", "10s"]).await.unwrap();
        assert!(
            calls_of(&fake, "browsers.playback.seek")
                .iter()
                .any(|c| c.contains("10000000"))
        );
        wwn(&fake, &["slot", "0", "set-position", "5s"])
            .await
            .unwrap();
        assert!(
            calls_of(&fake, "browsers.playback.set_position")
                .iter()
                .any(|c| c.contains("5000000"))
        );
    });
}

#[test]
fn player_actions_take_an_mpris_id() {
    let fake = fake();
    run(async {
        wwn(&fake, &["slot", "0", "start"]).await.unwrap();
        wwn(&fake, &["slot", "0", "open", "https://example.org/"])
            .await
            .unwrap();
        let players = {
            let tv = connect(&fake).await;
            tv.list_players().await.unwrap()
        };
        let id = players.first().expect("the slot's player").id.0;
        wwn(&fake, &["player", "pause", &id.to_string()])
            .await
            .unwrap();
        assert_eq!(calls_of(&fake, "media.players.playback.pause").len(), 1);
    });
}

#[test]
fn pointer_moves_and_drags_reach_the_fake() {
    let fake = fake();
    run(async {
        wwn(&fake, &["pointer", "move-to", "100", "200"])
            .await
            .unwrap();
        assert_eq!((fake.pointer().x.get(), fake.pointer().y.get()), (100, 200));
        wwn(&fake, &["pointer", "move-by", "10", "-5"])
            .await
            .unwrap();
        assert_eq!((fake.pointer().x.get(), fake.pointer().y.get()), (110, 195));
        wwn(&fake, &["pointer", "drag", "0", "0", "50", "50"])
            .await
            .unwrap();
        assert!(matches!(
            fake.input_log().last(),
            Some(ww_niri_fake::InputEntry::Drag { .. })
        ));
        wwn(&fake, &["pointer", "scroll", "0", "120", "--ctrl"])
            .await
            .unwrap();
        assert!(matches!(
            fake.input_log().last(),
            Some(ww_niri_fake::InputEntry::Scroll { dy: 120, modifiers, .. }) if modifiers.ctrl
        ));
    });
}

/// `press`/`key-down` hold for `--for-ms` and release themselves within the one call: a later,
/// separate `wwn` invocation has no connection left to release anything on.
#[test]
fn press_holds_for_the_given_time_then_releases_itself() {
    let fake = fake();
    run(async {
        wwn(&fake, &["pointer", "press", "left", "--for-ms", "20"])
            .await
            .unwrap();
        let log = fake.input_log();
        assert!(matches!(
            log[log.len() - 2],
            ww_niri_fake::InputEntry::Press(Button::Left)
        ));
        assert!(matches!(
            log.last(),
            Some(ww_niri_fake::InputEntry::Release {
                button: Button::Left,
                by: ReleasedBy::Call
            })
        ));
    });
}

#[test]
fn keys_and_key_down_reach_the_keyboard() {
    let fake = fake();
    run(async {
        wwn(&fake, &["keys", "ctrl+l"]).await.unwrap();
        assert_eq!(
            fake.input_log().last(),
            Some(&ww_niri_fake::InputEntry::Keys("ctrl+l".to_owned()))
        );
        wwn(&fake, &["key-down", "shift", "--for-ms", "20"])
            .await
            .unwrap();
        let log = fake.input_log();
        assert_eq!(
            log[log.len() - 2],
            ww_niri_fake::InputEntry::KeyDown("shift".to_owned())
        );
        assert!(
            matches!(log.last(), Some(ww_niri_fake::InputEntry::KeyUp { key, .. }) if key == "shift")
        );
    });
}

#[test]
fn mute_reaches_the_sink() {
    let fake = fake();
    run(async {
        wwn(&fake, &["mute", "on"]).await.unwrap();
        assert_eq!(fake.audio().1, true);
        wwn(&fake, &["mute", "off"]).await.unwrap();
        assert_eq!(fake.audio().1, false);
    });
}

#[test]
fn launch_answers_a_pid() {
    let fake = fake();
    run(async {
        wwn(&fake, &["launch", "--", "true"]).await.unwrap();
        assert!(fake.calls().iter().any(|c| c.method == "apps.launch"));
    });
}

#[test]
fn windows_and_outputs_list_without_error() {
    let fake = fake();
    run(async {
        wwn(&fake, &["windows", "list"]).await.unwrap();
        wwn(&fake, &["outputs", "list"]).await.unwrap();
        wwn(&fake, &["info"]).await.unwrap();
    });
}

#[test]
fn an_offline_tv_fails_within_the_connect_timeout() {
    run(async {
        let matches = super::matches([
            "--tv",
            "ws://127.0.0.1:1/ww",
            "--connect-timeout-ms",
            "50",
            "info",
        ]);
        let started = std::time::Instant::now();
        let err = ww_niri_cli::connect::connect(&matches)
            .await
            .err()
            .expect("an offline TV fails to connect");
        // generous margin: the 50 ms timeout itself is what this test checks, not wall-clock
        // under a loaded test runner
        assert!(started.elapsed() < Duration::from_secs(10), "{err}");
    });
}
