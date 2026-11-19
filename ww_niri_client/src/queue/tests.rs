//! Lanes and merging, without a TV.

use exact_units::Fixed;
use ww_niri_api::{Button, Modifiers, PlayerId};

use super::{Held, LANE_PENDING_MAX, Lanes};
use crate::command::{Command, Lane, PlaybackAction, PlaybackTarget};

fn queued(lanes: &Lanes, lane: &Lane) -> Vec<Command> {
    lanes.lanes[lane]
        .pending
        .iter()
        .map(|p| p.command.clone())
        .collect()
}

fn scroll(dy: i16, ctrl: bool) -> Command {
    Command::Scroll {
        at: None,
        dx: 0,
        dy,
        modifiers: Modifiers {
            ctrl,
            ..Modifiers::default()
        },
    }
}

#[test]
fn moves_and_turns_add_up_until_a_click() {
    let mut lanes = Lanes::default();
    for c in [
        Command::MoveBy { dx: 1, dy: 2 },
        Command::MoveBy { dx: 3, dy: -1 },
        Command::Click {
            at: None,
            button: Button::Left,
            count: 1,
            modifiers: Modifiers::default(),
        },
        Command::MoveBy { dx: 5, dy: 0 },
        scroll(30, false),
        scroll(30, false),
        scroll(60, true),
    ] {
        lanes.push(c, None).unwrap();
    }
    let input = queued(&lanes, &Lane::Input);
    assert_eq!(input[0], Command::MoveBy { dx: 4, dy: 1 });
    assert!(matches!(input[1], Command::Click { .. }));
    assert_eq!(input[2], Command::MoveBy { dx: 5, dy: 0 });
    // The same modifiers add up; ctrl + scroll is a zoom of its own.
    assert_eq!(input[3], scroll(60, false));
    assert_eq!(input[4], scroll(60, true));
    assert_eq!(input.len(), 5);
}

#[test]
fn a_move_that_would_overflow_stays_apart() {
    let mut lanes = Lanes::default();
    lanes
        .push(
            Command::MoveBy {
                dx: i32::MAX,
                dy: 0,
            },
            None,
        )
        .unwrap();
    lanes.push(Command::MoveBy { dx: 1, dy: 0 }, None).unwrap();
    assert_eq!(queued(&lanes, &Lane::Input).len(), 2);
}

#[test]
fn volume_and_position_are_latest_wins_seeks_add_up() {
    let mut lanes = Lanes::default();
    for v in [10, 20, 30] {
        lanes.push(Command::SetVolume(v), None).unwrap();
    }
    assert_eq!(queued(&lanes, &Lane::Volume), [Command::SetVolume(30)]);

    let target = PlaybackTarget::Player(PlayerId(3));
    let play = |action| Command::Playback { target, action };
    let lane = Lane::Playback(target);
    lanes
        .push(play(PlaybackAction::SeekBy { by: Fixed::new(10) }), None)
        .unwrap();
    lanes
        .push(play(PlaybackAction::SeekBy { by: Fixed::new(-4) }), None)
        .unwrap();
    assert_eq!(
        queued(&lanes, &lane),
        [play(PlaybackAction::SeekBy { by: Fixed::new(6) })]
    );
    lanes
        .push(
            play(PlaybackAction::SetPosition {
                at: Fixed::new(100),
            }),
            None,
        )
        .unwrap();
    lanes
        .push(
            play(PlaybackAction::SeekBy {
                by: Fixed::new(-150),
            }),
            None,
        )
        .unwrap();
    assert_eq!(
        queued(&lanes, &lane),
        [play(PlaybackAction::SetPosition { at: Fixed::new(0) })]
    );
    lanes.push(play(PlaybackAction::Pause), None).unwrap();
    lanes
        .push(play(PlaybackAction::SeekBy { by: Fixed::new(1) }), None)
        .unwrap();
    assert_eq!(queued(&lanes, &lane).len(), 3);
}

#[test]
fn one_call_in_flight_per_lane() {
    let mut lanes = Lanes::default();
    lanes.push(Command::MoveBy { dx: 1, dy: 0 }, None).unwrap();
    lanes.push(Command::TypeText("a".to_owned()), None).unwrap();
    lanes.push(Command::SetVolume(5), None).unwrap();
    let (first, _) = lanes.next_ready().unwrap();
    let (second, _) = lanes.next_ready().unwrap();
    assert_ne!(first, second);
    assert!(lanes.next_ready().is_none(), "both lanes are busy");
    // A move queued while the first is in flight does not merge into it.
    lanes.push(Command::MoveBy { dx: 1, dy: 0 }, None).unwrap();
    lanes.done(&Lane::Input);
    let (lane, pending) = lanes.next_ready().unwrap();
    assert_eq!(lane, Lane::Input);
    assert_eq!(pending.command, Command::TypeText("a".to_owned()));
}

#[test]
fn a_full_lane_refuses() {
    let mut lanes = Lanes::default();
    for _ in 0..LANE_PENDING_MAX {
        lanes
            .push(Command::PressKeys("a".to_owned()), None)
            .unwrap();
    }
    assert!(
        lanes
            .push(Command::PressKeys("a".to_owned()), None)
            .is_err()
    );
    // Another lane still takes commands.
    lanes.push(Command::SetMuted(true), None).unwrap();
}

#[test]
fn held_input_renews_and_releases() {
    let mut held = Held::default();
    held.note(&Command::Press(Button::Left));
    held.note(&Command::KeyDown("shift".to_owned()));
    held.note(&Command::Press(Button::Right));
    held.note(&Command::Release(Button::Right));
    assert_eq!(
        held.renewals(),
        [
            Command::Press(Button::Left),
            Command::KeyDown("shift".to_owned())
        ]
    );
    assert_eq!(
        held.release_all(),
        [
            Command::Release(Button::Left),
            Command::KeyUp("shift".to_owned())
        ]
    );
    assert!(held.renewals().is_empty());
}
