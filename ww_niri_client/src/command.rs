//! Commands to the TV, one per API call that answers nothing but success, and how each is sent.

use std::time::Duration;

use exact_units::{Fixed, Micros, Millis};
use ww_niri_api::{Button, CardOwned, Modifiers, PlayerId, Point, Transition, Volume, WindowId};

use crate::api::NiriClient;
use crate::error::TvError;

/// Whose playback a [Command::Playback] drives.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum PlaybackTarget {
    /// The current player of a browser slot.
    Slot(u8),
    /// Any MPRIS player.
    Player(PlayerId),
}

/// A playback control.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PlaybackAction {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    /// Relative; negative goes back.
    SeekBy {
        by: Micros<i64>,
    },
    /// Absolute, from the start of the track.
    SetPosition {
        at: Micros<u64>,
    },
}

/// One call to the TV, for [Tv::send](crate::Tv::send) and the typed calls.
///
/// Commands of one resource keep their order; see [Tv::send](crate::Tv::send) for those that
/// merge while a call is in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    ShowWindow {
        id: WindowId,
        transition: Option<Transition>,
        fullscreen: Option<bool>,
    },
    FocusWindow(WindowId),
    CloseWindow(WindowId),
    SetFullscreen {
        id: WindowId,
        on: bool,
    },
    /// Covers the window for `lasts`, held by this client's connection: it lifts when the
    /// connection ends, also on a reconnect. With `mute` the TV's sink is muted while it holds.
    Cover {
        id: WindowId,
        card: CardOwned,
        lasts: Duration,
        mute: bool,
    },
    Uncover(WindowId),
    SlotStart {
        slot: u8,
        fullscreen: bool,
    },
    SlotOpen {
        slot: u8,
        url: String,
    },
    SlotStop {
        slot: u8,
    },
    SlotShow {
        slot: u8,
        transition: Option<Transition>,
        fullscreen: Option<bool>,
    },
    /// Covers the slot's window, as [Command::Cover].
    SlotCover {
        slot: u8,
        card: CardOwned,
        lasts: Duration,
        mute: bool,
    },
    SlotUncover {
        slot: u8,
    },
    Playback {
        target: PlaybackTarget,
        action: PlaybackAction,
    },
    /// Percent, 0 to 150.
    SetVolume(u16),
    SetMuted(bool),
    MoveTo(Point),
    MoveBy {
        dx: i32,
        dy: i32,
    },
    Click {
        at: Option<Point>,
        button: Button,
        count: u8,
        modifiers: Modifiers,
    },
    /// Holds the button down, renewed within the hold limit until [Command::Release] or
    /// [Command::ReleaseAll].
    Press(Button),
    Release(Button),
    Drag {
        from: Point,
        to: Point,
        button: Button,
        duration: Duration,
    },
    /// v120 units, positive down and right.
    Scroll {
        at: Option<Point>,
        dx: i16,
        dy: i16,
        modifiers: Modifiers,
    },
    TypeText(String),
    PressKeys(String),
    /// Holds the key down, renewed within the hold limit until [Command::KeyUp] or
    /// [Command::ReleaseAll].
    KeyDown(String),
    KeyUp(String),
    /// Releases every button and key held through this client: the app goes to the background.
    ReleaseAll,
}

/// Which resource a command drives: commands of one lane go one at a time, in order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Lane {
    Windows,
    Slot(u8),
    Playback(PlaybackTarget),
    Volume,
    Muted,
    /// Pointer and keyboard: one lane, so clicks land where the moves before them went.
    Input,
}

impl Command {
    pub(crate) fn lane(&self) -> Lane {
        match self {
            Command::ShowWindow { .. }
            | Command::FocusWindow(_)
            | Command::CloseWindow(_)
            | Command::SetFullscreen { .. }
            | Command::Cover { .. }
            | Command::Uncover(_) => Lane::Windows,
            Command::SlotStart { slot, .. }
            | Command::SlotOpen { slot, .. }
            | Command::SlotStop { slot }
            | Command::SlotShow { slot, .. }
            | Command::SlotCover { slot, .. }
            | Command::SlotUncover { slot } => Lane::Slot(*slot),
            Command::Playback { target, .. } => Lane::Playback(*target),
            Command::SetVolume(_) => Lane::Volume,
            Command::SetMuted(_) => Lane::Muted,
            Command::MoveTo(_)
            | Command::MoveBy { .. }
            | Command::Click { .. }
            | Command::Press(_)
            | Command::Release(_)
            | Command::Drag { .. }
            | Command::Scroll { .. }
            | Command::TypeText(_)
            | Command::PressKeys(_)
            | Command::KeyDown(_)
            | Command::KeyUp(_)
            | Command::ReleaseAll => Lane::Input,
        }
    }

    /// Folds `next`, queued right after `self`, into `self` when one call does both: moves and
    /// wheel turns add up, a volume or mute replaces the one before, a cover of the same window
    /// replaces the one before, seeks add up and a position replaces what came before it. Gives
    /// `next` back otherwise.
    pub(crate) fn merge(&mut self, next: Command) -> Result<(), Command> {
        use PlaybackAction::{SeekBy, SetPosition};
        match (self, next) {
            (this @ Command::Cover { .. }, next @ Command::Cover { .. })
            | (this @ Command::SlotCover { .. }, next @ Command::SlotCover { .. })
                if this.lane() == next.lane() && this.cover_target() == next.cover_target() =>
            {
                *this = next;
                Ok(())
            }
            (Command::MoveBy { dx, dy }, Command::MoveBy { dx: ndx, dy: ndy }) => {
                match (dx.checked_add(ndx), dy.checked_add(ndy)) {
                    (Some(x), Some(y)) => {
                        (*dx, *dy) = (x, y);
                        Ok(())
                    }
                    _ => Err(Command::MoveBy { dx: ndx, dy: ndy }),
                }
            }
            (
                Command::Scroll {
                    at: None,
                    dx,
                    dy,
                    modifiers,
                },
                Command::Scroll {
                    at: None,
                    dx: ndx,
                    dy: ndy,
                    modifiers: nm,
                },
            ) if *modifiers == nm => match (dx.checked_add(ndx), dy.checked_add(ndy)) {
                (Some(x), Some(y)) => {
                    (*dx, *dy) = (x, y);
                    Ok(())
                }
                _ => Err(Command::Scroll {
                    at: None,
                    dx: ndx,
                    dy: ndy,
                    modifiers: nm,
                }),
            },
            (Command::SetVolume(v), Command::SetVolume(n)) => {
                *v = n;
                Ok(())
            }
            (Command::SetMuted(m), Command::SetMuted(n)) => {
                *m = n;
                Ok(())
            }
            (
                Command::Playback { target, action },
                Command::Playback {
                    target: nt,
                    action: next_action,
                },
            ) if *target == nt => match (*action, next_action) {
                (SeekBy { .. } | SetPosition { .. }, SetPosition { at }) => {
                    *action = SetPosition { at };
                    Ok(())
                }
                (SeekBy { by }, SeekBy { by: n }) if by.get().checked_add(n.get()).is_some() => {
                    *action = SeekBy {
                        by: Fixed::new(by.get() + n.get()),
                    };
                    Ok(())
                }
                (SetPosition { at }, SeekBy { by: n }) => {
                    *action = SetPosition {
                        at: Fixed::new(at.get().saturating_add_signed(n.get())),
                    };
                    Ok(())
                }
                (_, next_action) => Err(Command::Playback {
                    target: nt,
                    action: next_action,
                }),
            },
            (_, next) => Err(next),
        }
    }
}

impl Command {
    /// The window or slot a cover command is about.
    fn cover_target(&self) -> Option<(Option<WindowId>, Option<u8>)> {
        match self {
            Command::Cover { id, .. } => Some((Some(*id), None)),
            Command::SlotCover { slot, .. } => Some((None, Some(*slot))),
            _ => None,
        }
    }
}

/// A duration as the API sends it, in whole milliseconds; `what` names it when it does not fit.
fn millis<T: TryFrom<u128> + exact_units::Storage>(
    duration: Duration,
    what: &'static str,
) -> Result<Millis<T>, TvError> {
    let ms = T::try_from(duration.as_millis()).map_err(|_e| TvError::BadArgument { what })?;
    Ok(Fixed::new(ms))
}

/// The hold limit a press or key-down sends.
fn hold_limit(hold: Duration) -> Millis<u16> {
    Fixed::new(u16::try_from(hold.as_millis()).expect("Config checks the hold limit fits"))
}

fn px(v: i32) -> ww_niri_api::Px {
    Fixed::new(v)
}

/// Sends one command and waits for its answer.
pub(crate) async fn execute(
    client: &NiriClient,
    command: &Command,
    hold: Duration,
) -> Result<(), TvError> {
    match command {
        Command::Playback { target, action } => playback(client, *target, *action).await,
        Command::SetVolume(percent) => {
            let volume =
                Volume::try_new(*percent).map_err(|_e| TvError::BadArgument { what: "volume" })?;
            Ok(client.audio().write_volume(volume).write().await?)
        }
        Command::SetMuted(on) => Ok(client.audio().write_muted(*on).write().await?),
        Command::TypeText(_) | Command::PressKeys(_) | Command::KeyDown(_) | Command::KeyUp(_) => {
            keyboard(client, command, hold).await
        }
        command => match command.lane() {
            Lane::Windows => window(client, command).await,
            Lane::Slot(slot) => browser(client, slot, command).await,
            _ => pointer(client, command, hold).await,
        },
    }
}

async fn window(client: &NiriClient, command: &Command) -> Result<(), TvError> {
    let windows = client.windows();
    match command {
        Command::ShowWindow {
            id,
            transition,
            fullscreen,
        } => Ok(windows.show(*id, *transition, *fullscreen).call().await?),
        Command::FocusWindow(id) => Ok(windows.focus(*id).call().await?),
        Command::CloseWindow(id) => Ok(windows.close(*id).call().await?),
        Command::SetFullscreen { id, on } => Ok(windows.set_fullscreen(*id, *on).call().await?),
        Command::Cover {
            id,
            card,
            lasts,
            mute,
        } => Ok(windows
            .cover(*id, card.clone(), millis(*lasts, "cover lasts")?, *mute)
            .call()
            .await?),
        Command::Uncover(id) => Ok(windows.uncover(*id).call().await?),
        other => unreachable!("{other:?} is not in the windows lane"),
    }
}

/// `SlotStart`/`SlotOpen`/`SlotStop` launch or close Firefox and load a page: far past
/// `wire_weaver_client`'s 1 s default request timeout, and past the slot's own 5 s stop grace.
const SLOT_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(60);

async fn browser(client: &NiriClient, slot: u8, command: &Command) -> Result<(), TvError> {
    let browser = client.browsers(u32::from(slot));
    match command {
        Command::SlotStart { fullscreen, .. } => Ok(browser
            .start(*fullscreen)
            .with_timeout(SLOT_LIFECYCLE_TIMEOUT)
            .call()
            .await?),
        Command::SlotOpen { url, .. } => Ok(browser
            .open(url.clone())
            .with_timeout(SLOT_LIFECYCLE_TIMEOUT)
            .call()
            .await?),
        Command::SlotStop { .. } => Ok(browser
            .stop()
            .with_timeout(SLOT_LIFECYCLE_TIMEOUT)
            .call()
            .await?),
        Command::SlotShow {
            transition,
            fullscreen,
            ..
        } => Ok(browser.show(*transition, *fullscreen).call().await?),
        Command::SlotCover {
            card, lasts, mute, ..
        } => Ok(browser
            .cover(card.clone(), millis(*lasts, "cover lasts")?, *mute)
            .call()
            .await?),
        Command::SlotUncover { .. } => Ok(browser.uncover().call().await?),
        other => unreachable!("{other:?} is not in a slot's lane"),
    }
}

async fn pointer(client: &NiriClient, command: &Command, hold: Duration) -> Result<(), TvError> {
    let input = client.input();
    let pointer = input.pointer();
    match command {
        Command::MoveTo(to) => Ok(pointer.move_to(*to).call().await?),
        Command::MoveBy { dx, dy } => Ok(pointer.move_by(px(*dx), px(*dy)).call().await?),
        Command::Scroll {
            at,
            dx,
            dy,
            modifiers,
        } => Ok(pointer.scroll(*at, *dx, *dy, *modifiers).call().await?),
        command => buttons(client, command, hold).await,
    }
}

async fn buttons(client: &NiriClient, command: &Command, hold: Duration) -> Result<(), TvError> {
    let input = client.input();
    let pointer = input.pointer();
    match command {
        Command::Click {
            at,
            button,
            count,
            modifiers,
        } => Ok(pointer
            .click(*at, *button, *count, *modifiers)
            .call()
            .await?),
        Command::Press(button) => Ok(pointer
            .press(*button, Some(hold_limit(hold)))
            .call()
            .await?),
        Command::Release(button) => Ok(pointer.release(*button).call().await?),
        Command::Drag {
            from,
            to,
            button,
            duration,
        } => Ok(pointer
            .drag(*from, *to, *button, millis(*duration, "drag duration")?)
            .call()
            .await?),
        other => unreachable!("{other:?} is not a pointer command"),
    }
}

async fn keyboard(client: &NiriClient, command: &Command, hold: Duration) -> Result<(), TvError> {
    let input = client.input();
    let keyboard = input.keyboard();
    match command {
        Command::TypeText(text) => Ok(keyboard.type_text(text.clone()).call().await?),
        Command::PressKeys(combo) => Ok(keyboard.press_keys(combo.clone()).call().await?),
        Command::KeyDown(key) => Ok(keyboard
            .key_down(key.clone(), Some(hold_limit(hold)))
            .call()
            .await?),
        Command::KeyUp(key) => Ok(keyboard.key_up(key.clone()).call().await?),
        other => unreachable!("{other:?} is not a keyboard command"),
    }
}

async fn playback(
    client: &NiriClient,
    target: PlaybackTarget,
    action: PlaybackAction,
) -> Result<(), TvError> {
    macro_rules! on {
        ($($playback:tt)*) => {{
            match action {
                PlaybackAction::Play => $($playback)*.play().call().await?,
                PlaybackAction::Pause => $($playback)*.pause().call().await?,
                PlaybackAction::PlayPause => $($playback)*.play_pause().call().await?,
                PlaybackAction::Stop => $($playback)*.stop().call().await?,
                PlaybackAction::Next => $($playback)*.next().call().await?,
                PlaybackAction::Previous => $($playback)*.previous().call().await?,
                PlaybackAction::SeekBy { by } => $($playback)*.seek(by).call().await?,
                PlaybackAction::SetPosition { at } => {
                    $($playback)*.set_position(at).call().await?
                }
            }
            Ok(())
        }};
    }
    match target {
        PlaybackTarget::Slot(slot) => on!(client.browsers(u32::from(slot)).playback()),
        PlaybackTarget::Player(id) => on!(client.media().players(id.0).playback()),
    }
}
