use std::time::{Duration, Instant};

use exact_units::Fixed;
use ww_niri_api::{CardOwned, CoverError};

use super::*;

fn card() -> CardOwned {
    CardOwned {
        title: "Реклама".to_owned(),
        countdown: Some("Передача вернётся через".to_owned()),
        clock: true,
        footer: Some("Первый канал".to_owned()),
    }
}

fn look(until: Instant) -> Look {
    Look {
        card: card(),
        until,
    }
}

#[test]
fn check_limits() {
    assert_eq!(
        card().check_cover(Fixed::new(1)),
        Ok(Duration::from_millis(1))
    );
    assert_eq!(
        card().check_cover(Fixed::new(15 * 60 * 1000)),
        Ok(Duration::from_secs(15 * 60))
    );
    assert_eq!(
        card().check_cover(Fixed::new(0)),
        Err(CoverError::BadArgument)
    );
    assert_eq!(
        card().check_cover(Fixed::new(15 * 60 * 1000 + 1)),
        Err(CoverError::BadArgument)
    );
    let mut long = card();
    long.footer = Some("я".repeat(101));
    assert_eq!(
        long.check_cover(Fixed::new(1000)),
        Err(CoverError::BadArgument)
    );
    long.footer = Some("я".repeat(100));
    assert!(long.check_cover(Fixed::new(1000)).is_ok());
}

#[test]
fn countdown_rounds_up() {
    let now = Instant::now();
    let at = |left_ms| look(now + Duration::from_millis(left_ms)).countdown(now);
    assert_eq!(at(130_000).as_deref(), Some("Передача вернётся через 2:10"));
    assert_eq!(at(129_001).as_deref(), Some("Передача вернётся через 2:10"));
    assert_eq!(at(9_000).as_deref(), Some("Передача вернётся через 0:09"));
    assert_eq!(at(1).as_deref(), Some("Передача вернётся через 0:01"));
    assert_eq!(at(0).as_deref(), Some("Передача вернётся через 0:00"));
    // Past the end it stays at zero.
    assert_eq!(
        look(now).countdown(now + Duration::from_secs(5)).as_deref(),
        Some("Передача вернётся через 0:00")
    );

    let mut silent = look(now);
    silent.card.countdown = None;
    assert_eq!(silent.countdown(now), None);
}

#[test]
fn leases_lift_by_end_holder_and_window() {
    let now = Instant::now();
    let mut covers = Covers::default();
    assert!(covers.is_empty());
    assert_eq!(covers.next_wake(now), None);

    covers.set(1, 10, look(now + Duration::from_secs(2)), false);
    covers.set(2, 10, look(now + Duration::from_secs(60)), false);
    covers.set(3, 11, look(now + Duration::from_secs(60)), false);

    // Window 2 is covered again by connection 11: it holds it from now on.
    covers.set(2, 11, look(now + Duration::from_secs(30)), false);
    assert_eq!(covers.lift_held_by(10), [1]);
    assert_eq!(covers.lift_held_by(10), Vec::<u64>::new());

    assert_eq!(
        covers.lift_due(now + Duration::from_secs(29)),
        Vec::<u64>::new()
    );
    assert_eq!(covers.lift_due(now + Duration::from_secs(30)), [2]);

    assert!(covers.lift(3));
    assert!(!covers.lift(3));
    assert!(covers.is_empty());

    covers.set(4, 12, look(now + Duration::from_secs(5)), false);
    covers.set(5, 12, look(now + Duration::from_secs(5)), false);
    assert_eq!(covers.lift_gone(|w| w == 5), [4]);
    assert_eq!(covers.lift_held_by(12), [5]);
}

#[test]
fn wakes_at_each_countdown_second_and_the_end() {
    let now = Instant::now();
    let mut covers = Covers::default();
    covers.set(1, 1, look(now + Duration::from_millis(2_300)), false);
    assert_eq!(
        covers.next_wake(now),
        Some(now + Duration::from_millis(300))
    );
    // On a whole second: the next one.
    assert_eq!(
        covers.next_wake(now + Duration::from_millis(300)),
        Some(now + Duration::from_millis(1_300))
    );
    assert_eq!(
        covers.next_wake(now + Duration::from_millis(1_800)),
        Some(now + Duration::from_millis(2_300))
    );
    // Due: at once.
    assert_eq!(
        covers.next_wake(now + Duration::from_secs(3)),
        Some(now + Duration::from_secs(3))
    );

    covers.set(2, 1, look(now + Duration::from_millis(150)), false);
    assert_eq!(
        covers.next_wake(now),
        Some(now + Duration::from_millis(150))
    );
}

#[test]
fn mutes_while_a_muting_cover_holds() {
    let now = Instant::now();
    let mut covers = Covers::default();
    assert!(!covers.mutes());
    covers.set(1, 1, look(now + Duration::from_secs(5)), false);
    assert!(!covers.mutes());
    covers.set(2, 1, look(now + Duration::from_secs(5)), true);
    covers.set(3, 2, look(now + Duration::from_secs(5)), true);
    assert!(covers.mutes());
    assert!(covers.lift(2));
    assert!(covers.mutes());
    // Replaced by a cover without mute.
    covers.set(3, 2, look(now + Duration::from_secs(5)), false);
    assert!(!covers.mutes());
}
