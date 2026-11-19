//! Value parsers shared by several commands: buttons, transitions, durations and on/off.

use clap::ValueEnum;
use exact_units::Fixed;
use ww_niri_api::{Button, Curve, Transition};

/// `--button`.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum ButtonArg {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl From<ButtonArg> for Button {
    fn from(b: ButtonArg) -> Self {
        match b {
            ButtonArg::Left => Button::Left,
            ButtonArg::Right => Button::Right,
            ButtonArg::Middle => Button::Middle,
            ButtonArg::Back => Button::Back,
            ButtonArg::Forward => Button::Forward,
        }
    }
}

/// `--on`/`--off`, for `windows set-fullscreen` and `mute`.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

impl OnOff {
    pub fn as_bool(self) -> bool {
        matches!(self, OnOff::On)
    }
}

/// `--transition`'s easing curve; `--transition-ms` gives its duration.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum CurveArg {
    Linear,
    EaseOutQuad,
    EaseOutCubic,
    EaseOutExpo,
}

impl CurveArg {
    pub fn transition(self, duration_ms: u16) -> Transition {
        let curve = match self {
            CurveArg::Linear => Curve::Linear,
            CurveArg::EaseOutQuad => Curve::EaseOutQuad,
            CurveArg::EaseOutCubic => Curve::EaseOutCubic,
            CurveArg::EaseOutExpo => Curve::EaseOutExpo,
        };
        Transition {
            duration: Fixed::new(duration_ms),
            curve,
        }
    }
}

/// A signed duration like `10`, `10s`, `-500ms`, `2000us` (no suffix: seconds), as microseconds.
///
/// # Errors
///
/// When there is no number, or the unit is none of `s`, `ms`, `us`.
pub fn parse_signed_micros(s: &str) -> Result<i64, String> {
    let (number, unit) = split_unit(s);
    let value: f64 = number
        .parse()
        .map_err(|_e| format!("{s:?} is not a number"))?;
    let micros = micros_of(value, unit)?;
    Ok(clamp_to_i64(micros))
}

/// An unsigned duration (same units as [parse_signed_micros]), as microseconds.
///
/// # Errors
///
/// When the value is negative, there is no number, or the unit is unknown.
pub fn parse_unsigned_micros(s: &str) -> Result<u64, String> {
    let micros = parse_signed_micros(s)?;
    u64::try_from(micros).map_err(|_e| format!("{s:?} must not be negative"))
}

fn split_unit(s: &str) -> (&str, &str) {
    let at = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(s.len());
    (&s[..at], &s[at..])
}

fn micros_of(value: f64, unit: &str) -> Result<f64, String> {
    match unit {
        "" | "s" => Ok(value * 1_000_000.0),
        "ms" => Ok(value * 1_000.0),
        "us" | "µs" => Ok(value),
        other => Err(format!("unknown unit {other:?}: use s, ms or us")),
    }
}

fn clamp_to_i64(v: f64) -> i64 {
    if v.is_nan() {
        0
    } else {
        v.round().clamp(i64::MIN as f64, i64::MAX as f64) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_numbers_as_seconds() {
        assert_eq!(parse_signed_micros("10").unwrap(), 10_000_000);
        assert_eq!(parse_signed_micros("-2.5").unwrap(), -2_500_000);
    }

    #[test]
    fn parses_units() {
        assert_eq!(parse_signed_micros("500ms").unwrap(), 500_000);
        assert_eq!(parse_signed_micros("-1500us").unwrap(), -1500);
    }

    #[test]
    fn rejects_unknown_units_and_negatives_when_unsigned() {
        assert!(parse_signed_micros("3x").is_err());
        assert!(parse_unsigned_micros("-1").is_err());
    }
}
