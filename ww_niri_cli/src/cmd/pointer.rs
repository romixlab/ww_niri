//! `wwn pointer`: moves, clicks, drags and scrolls, as a mouse does.

use std::time::Duration;

use anyhow::Result;
use clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use ww_niri_api::{Modifiers, Point, WindowId};
use ww_niri_client::Tv;

use crate::args::ButtonArg;
use crate::registry::{BoxFuture, CommandDef};

fn xy_args() -> [Arg; 2] {
    [
        Arg::new("x")
            .required(true)
            .value_parser(value_parser!(i32)),
        Arg::new("y")
            .required(true)
            .value_parser(value_parser!(i32)),
    ]
}

/// `--at X Y`, optional: a point instead of wherever the pointer is.
fn at_arg() -> Arg {
    Arg::new("at")
        .long("at")
        .num_args(2)
        .value_names(["X", "Y"])
        .value_parser(value_parser!(i32))
}

fn window_arg() -> Arg {
    Arg::new("window")
        .long("window")
        .value_name("ID")
        .help("x, y are the window's own coordinates, not the global ones")
        .value_parser(value_parser!(u64))
}

fn modifier_args() -> [Arg; 4] {
    [
        Arg::new("ctrl").long("ctrl").action(ArgAction::SetTrue),
        Arg::new("shift").long("shift").action(ArgAction::SetTrue),
        Arg::new("alt").long("alt").action(ArgAction::SetTrue),
        Arg::new("super").long("super").action(ArgAction::SetTrue),
    ]
}

fn modifiers_of(m: &ArgMatches) -> Modifiers {
    Modifiers {
        ctrl: m.get_flag("ctrl"),
        shift: m.get_flag("shift"),
        alt: m.get_flag("alt"),
        super_key: m.get_flag("super"),
    }
}

fn point_of(m: &ArgMatches, x_id: &str, y_id: &str) -> Point {
    let x = exact_units::Fixed::new(*m.get_one::<i32>(x_id).expect("required"));
    let y = exact_units::Fixed::new(*m.get_one::<i32>(y_id).expect("required"));
    match m.get_one::<u64>("window") {
        Some(id) => Point::Window {
            id: WindowId(*id),
            x,
            y,
        },
        None => Point::Global { x, y },
    }
}

/// `None` means "where the pointer is".
fn at_of(m: &ArgMatches) -> Option<Point> {
    let mut at = m.get_many::<i32>("at")?;
    let (x, y) = (
        exact_units::Fixed::new(*at.next().expect("num_args(2)")),
        exact_units::Fixed::new(*at.next().expect("num_args(2)")),
    );
    Some(match m.get_one::<u64>("window") {
        Some(id) => Point::Window {
            id: WindowId(*id),
            x,
            y,
        },
        None => Point::Global { x, y },
    })
}

fn command() -> Command {
    Command::new("pointer")
        .about("Moves, clicks, drags and scrolls the pointer")
        .subcommand_required(true)
        .subcommand(
            Command::new("move-to")
                .about("Moves the pointer to a point")
                .allow_negative_numbers(true)
                .args(xy_args())
                .arg(window_arg()),
        )
        .subcommand(
            Command::new("move-by")
                .about("Moves the pointer by a distance, as a mouse does")
                .allow_negative_numbers(true)
                .arg(
                    Arg::new("dx")
                        .required(true)
                        .value_parser(value_parser!(i32)),
                )
                .arg(
                    Arg::new("dy")
                        .required(true)
                        .value_parser(value_parser!(i32)),
                ),
        )
        .subcommand(
            Command::new("click")
                .about("Presses and releases a button, at a point or where the pointer is")
                .allow_negative_numbers(true)
                .arg(at_arg())
                .arg(window_arg())
                .arg(
                    Arg::new("button")
                        .long("button")
                        .default_value("left")
                        .value_parser(value_parser!(ButtonArg)),
                )
                .arg(
                    Arg::new("count")
                        .long("count")
                        .default_value("1")
                        .value_parser(value_parser!(u8).range(1..=3)),
                )
                .args(modifier_args()),
        )
        .subcommand(
            Command::new("press")
                .about(
                    "Holds a button down for a while, then releases it: a one-shot `wwn` call \
                     cannot leave it held past its own connection, which the TV drops it on",
                )
                .arg(
                    Arg::new("button")
                        .required(true)
                        .value_parser(value_parser!(ButtonArg)),
                )
                .arg(
                    Arg::new("for_ms")
                        .long("for-ms")
                        .default_value("1000")
                        .value_parser(value_parser!(u64)),
                ),
        )
        .subcommand(
            Command::new("drag")
                .about("Presses at from, moves to to in steps, releases there")
                .allow_negative_numbers(true)
                .arg(
                    Arg::new("from_x")
                        .required(true)
                        .value_parser(value_parser!(i32)),
                )
                .arg(
                    Arg::new("from_y")
                        .required(true)
                        .value_parser(value_parser!(i32)),
                )
                .arg(
                    Arg::new("to_x")
                        .required(true)
                        .value_parser(value_parser!(i32)),
                )
                .arg(
                    Arg::new("to_y")
                        .required(true)
                        .value_parser(value_parser!(i32)),
                )
                .arg(
                    Arg::new("button")
                        .long("button")
                        .default_value("left")
                        .value_parser(value_parser!(ButtonArg)),
                )
                .arg(
                    Arg::new("duration_ms")
                        .long("duration-ms")
                        .default_value("300")
                        .value_parser(value_parser!(u16)),
                ),
        )
        .subcommand(
            Command::new("scroll")
                .about("Turns the wheel, in v120 units (120 is one notch)")
                .allow_negative_numbers(true)
                .arg(
                    Arg::new("dx")
                        .required(true)
                        .value_parser(value_parser!(i16)),
                )
                .arg(
                    Arg::new("dy")
                        .required(true)
                        .value_parser(value_parser!(i16)),
                )
                .arg(at_arg())
                .arg(window_arg())
                .args(modifier_args()),
        )
}

fn run<'a>(tv: &'a Tv, matches: &'a ArgMatches) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let pointer = tv.pointer();
        let (name, sub) = matches.subcommand().expect("subcommand_required");
        match name {
            "move-to" => pointer.move_to(point_of(sub, "x", "y")).await?,
            "move-by" => {
                let dx = *sub.get_one::<i32>("dx").expect("required");
                let dy = *sub.get_one::<i32>("dy").expect("required");
                pointer.move_by(dx, dy).await?;
            }
            "click" => {
                let button: ButtonArg = *sub.get_one("button").expect("has a default");
                let count = *sub.get_one::<u8>("count").expect("has a default");
                pointer
                    .click(at_of(sub), button.into(), count, modifiers_of(sub))
                    .await?;
            }
            "press" => {
                let button: ButtonArg = *sub.get_one("button").expect("required");
                let for_ms = *sub.get_one::<u64>("for_ms").expect("has a default");
                pointer.press(button.into()).await?;
                tokio::time::sleep(Duration::from_millis(for_ms)).await;
                pointer.release(button.into()).await?;
            }
            "drag" => {
                let get = |id| exact_units::Fixed::new(*sub.get_one::<i32>(id).expect("required"));
                let from = Point::Global {
                    x: get("from_x"),
                    y: get("from_y"),
                };
                let to = Point::Global {
                    x: get("to_x"),
                    y: get("to_y"),
                };
                let button: ButtonArg = *sub.get_one("button").expect("has a default");
                let duration_ms = *sub.get_one::<u16>("duration_ms").expect("has a default");
                pointer
                    .drag(
                        from,
                        to,
                        button.into(),
                        Duration::from_millis(u64::from(duration_ms)),
                    )
                    .await?;
            }
            "scroll" => {
                let dx = *sub.get_one::<i16>("dx").expect("required");
                let dy = *sub.get_one::<i16>("dy").expect("required");
                pointer
                    .scroll(at_of(sub), dx, dy, modifiers_of(sub))
                    .await?;
            }
            other => unreachable!("{other:?} is not a pointer subcommand"),
        }
        Ok(())
    })
}

inventory::submit! { CommandDef { name: "pointer", order: 40, command, run } }
