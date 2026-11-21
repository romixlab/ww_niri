//! The injected input: pointer moves, clicks, the wheel, keys and text, and what is held.

use exact_units::Fixed;
use ww_niri_api::{
    Button, InputErrorOwned, Modifiers, Point, PointerPosition, WindowId, WindowPoint,
};

use super::{
    InputEntry, Mark, Model, Rect, ReleasedBy, HOLD_DEFAULT_US, INPUT_LOG_LEN_MAX,
    OUTPUT_HEIGHT_PX, OUTPUT_WIDTH_PX,
};
use crate::keys;

impl Model {
    pub(super) fn log(&mut self, entry: InputEntry) {
        if self.input_log.len() >= INPUT_LOG_LEN_MAX {
            self.input_log.pop_front();
        }
        self.input_log.push_back(entry);
    }

    pub(crate) fn input_log(&self) -> Vec<InputEntry> {
        self.input_log.iter().cloned().collect()
    }

    pub(crate) fn take_input_log(&mut self) -> Vec<InputEntry> {
        self.input_log.drain(..).collect()
    }

    pub(crate) fn pointer(&self) -> PointerPosition {
        let (x, y) = self.pointer;
        let under = (self.focused.iter())
            .chain(self.windows.iter().map(|w| &w.id))
            .filter_map(|id| self.window(*id))
            .find(|w| !w.hidden && w.workspace == self.workspace && w.rect().contains(x, y));
        PointerPosition {
            x: Fixed::new(x),
            y: Fixed::new(y),
            window: under.map(|w| WindowPoint {
                id: WindowId(w.id),
                x: Fixed::new(x - w.rect().x),
                y: Fixed::new(y - w.rect().y),
            }),
        }
    }

    /// A point in global space, on the output.
    fn global(&self, point: Point) -> Result<(i32, i32), InputErrorOwned> {
        let (x, y) = match point {
            Point::Global { x, y } => (x.get(), y.get()),
            Point::Window { id, x, y } => {
                let w = self.window(id.0).ok_or(InputErrorOwned::NoSuchWindow)?;
                if w.hidden || w.workspace != self.workspace {
                    return Err(InputErrorOwned::WindowNotVisible);
                }
                let r = w.rect();
                (r.x.saturating_add(x.get()), r.y.saturating_add(y.get()))
            }
            Point::Unknown(_) => return Err(InputErrorOwned::BadArgument),
        };
        if Rect::OUTPUT.contains(x, y) {
            Ok((x, y))
        } else {
            Err(InputErrorOwned::OffScreen)
        }
    }

    fn move_pointer(&mut self, to: (i32, i32)) {
        if self.pointer != to {
            self.pointer = to;
            self.mark(Mark::Pointer);
        }
    }

    /// Moves to `at` when given; where the pointer is then.
    fn at(&mut self, at: Option<Point>) -> Result<(i32, i32), InputErrorOwned> {
        if let Some(at) = at {
            let to = self.global(at)?;
            self.move_pointer(to);
        }
        Ok(self.pointer)
    }

    pub(crate) fn move_to(&mut self, to: Point) -> Result<(), InputErrorOwned> {
        let to = self.global(to)?;
        self.move_pointer(to);
        Ok(())
    }

    /// Stops at the output's edges, as a mouse does.
    pub(crate) fn move_by(&mut self, dx: i32, dy: i32) -> Result<(), InputErrorOwned> {
        let (x, y) = self.pointer;
        let to = (
            x.saturating_add(dx).clamp(0, OUTPUT_WIDTH_PX - 1),
            y.saturating_add(dy).clamp(0, OUTPUT_HEIGHT_PX - 1),
        );
        self.move_pointer(to);
        Ok(())
    }

    pub(crate) fn click(
        &mut self,
        at: Option<Point>,
        button: Button,
        count: u8,
        modifiers: Modifiers,
    ) -> Result<(), InputErrorOwned> {
        check_button(button)?;
        if !(1..=3).contains(&count) {
            return Err(InputErrorOwned::BadArgument);
        }
        let (x, y) = self.at(at)?;
        self.log(InputEntry::Click {
            x,
            y,
            button,
            count,
            modifiers,
        });
        Ok(())
    }

    /// Presses a button, or renews the hold limit of one held.
    pub(crate) fn press(
        &mut self,
        button: Button,
        hold_ms: Option<u16>,
    ) -> Result<(), InputErrorOwned> {
        check_button(button)?;
        let until = self.now_us + hold_limit_us(hold_ms)?;
        if let Some((_, at)) = self.held_buttons.iter_mut().find(|(b, _)| *b == button) {
            *at = until;
        } else {
            self.held_buttons.push((button, until));
            self.log(InputEntry::Press(button));
        }
        Ok(())
    }

    pub(crate) fn release(&mut self, button: Button) -> Result<(), InputErrorOwned> {
        check_button(button)?;
        self.held_buttons.retain(|(b, _)| *b != button);
        self.log(InputEntry::Release {
            button,
            by: ReleasedBy::Call,
        });
        Ok(())
    }

    pub(crate) fn drag(
        &mut self,
        from: Point,
        to: Point,
        button: Button,
    ) -> Result<(), InputErrorOwned> {
        check_button(button)?;
        let from = self.global(from)?;
        let to = self.global(to)?;
        self.move_pointer(to);
        self.log(InputEntry::Drag { from, to, button });
        Ok(())
    }

    pub(crate) fn scroll(
        &mut self,
        at: Option<Point>,
        dx: i16,
        dy: i16,
        modifiers: Modifiers,
    ) -> Result<(), InputErrorOwned> {
        let (x, y) = self.at(at)?;
        self.log(InputEntry::Scroll {
            x,
            y,
            dx,
            dy,
            modifiers,
        });
        Ok(())
    }

    pub(crate) fn type_text(&mut self, text: &str) -> Result<(), InputErrorOwned> {
        keys::check_text(text)?;
        self.log(InputEntry::Text(text.to_owned()));
        Ok(())
    }

    pub(crate) fn press_keys(&mut self, combo: &str) -> Result<(), InputErrorOwned> {
        keys::check_combo(combo)?;
        self.log(InputEntry::Keys(combo.to_owned()));
        Ok(())
    }

    pub(crate) fn key_down(
        &mut self,
        key: &str,
        hold_ms: Option<u16>,
    ) -> Result<(), InputErrorOwned> {
        if !keys::is_key(key) {
            return Err(InputErrorOwned::UnknownKey(key.to_owned()));
        }
        let until = self.now_us + hold_limit_us(hold_ms)?;
        if let Some((_, at)) = self.held_keys.iter_mut().find(|(k, _)| k == key) {
            *at = until;
        } else {
            self.held_keys.push((key.to_owned(), until));
            self.log(InputEntry::KeyDown(key.to_owned()));
        }
        Ok(())
    }

    pub(crate) fn key_up(&mut self, key: &str) -> Result<(), InputErrorOwned> {
        if !keys::is_key(key) {
            return Err(InputErrorOwned::UnknownKey(key.to_owned()));
        }
        self.held_keys.retain(|(k, _)| k != key);
        self.log(InputEntry::KeyUp {
            key: key.to_owned(),
            by: ReleasedBy::Call,
        });
        Ok(())
    }

    /// The connection ended: nothing it held stays down.
    /// The connection ended: what it held comes up, its covers lift.
    pub(crate) fn release_all(&mut self) {
        self.lift_all_covers();
        for (button, _) in std::mem::take(&mut self.held_buttons) {
            self.log(InputEntry::Release {
                button,
                by: ReleasedBy::LinkDown,
            });
        }
        for (key, _) in std::mem::take(&mut self.held_keys) {
            self.log(InputEntry::KeyUp {
                key,
                by: ReleasedBy::LinkDown,
            });
        }
    }
}

fn check_button(button: Button) -> Result<(), InputErrorOwned> {
    match button {
        Button::Unknown(_) => Err(InputErrorOwned::BadArgument),
        _ => Ok(()),
    }
}

/// A press's hold limit: the default for `None`, a zero limit refused.
fn hold_limit_us(hold_ms: Option<u16>) -> Result<u64, InputErrorOwned> {
    match hold_ms {
        None => Ok(HOLD_DEFAULT_US),
        Some(0) => Err(InputErrorOwned::BadArgument),
        Some(ms) => Ok(u64::from(ms) * 1000),
    }
}
