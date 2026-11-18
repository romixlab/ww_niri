//! Pointer and keyboard from the API, injected below niri through kernel uinput devices
//! ([super::uinput]): libinput, xkb, niri's binds and the apps see them as real devices. Window
//! points and keysyms are resolved in niri. Each connection keeps what it holds down ([Input])
//! and lets it go when it ends or when a hold limit passes without a renewing press.

use std::rc::Rc;
use std::time::{Duration, Instant};

use exact_units::Millis;
use smithay::input::keyboard::{xkb, Keysym};
use smithay::utils::{Logical, Rectangle};
use ww_niri_api::{Button, InputErrorOwned, Modifiers, Point, Px};

use super::browsers::LoopIo;
use super::host::{call, Run};
use super::uinput;

/// One step of injected input, in niri's global logical space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Motion {
        x: f64,
        y: f64,
    },
    /// A Linux button code, `BTN_LEFT` and on.
    Button {
        code: u32,
        pressed: bool,
    },
    /// Wheel notches in v120 units.
    Wheel {
        dx: f64,
        dy: f64,
    },
    /// An xkb keycode of the configured keymap.
    Key {
        code: u32,
        pressed: bool,
    },
}

/// A key of the keymap that makes a keysym: its xkb keycode, and whether Shift goes with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: u32,
    pub shift: bool,
}

/// Drags move in steps this far apart, about a frame.
const STEP: Duration = Duration::from_millis(16);

/// Steps written at once, and the pause between such chunks.
const CHUNK: usize = 16;
const CHUNK_GAP: Duration = Duration::from_millis(2);

/// How long new devices may take to reach niri.
const DEVICE_WAIT: Duration = Duration::from_secs(3);

/// A held button or key comes up when this passes without a renewing press, unless the call
/// names its own limit.
const HOLD_DEFAULT: Duration = Duration::from_secs(10);

type Res = Result<(), InputErrorOwned>;

/// The modifiers by name, as `press_keys` takes them.
fn modifier(name: &str) -> Option<Keysym> {
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(Keysym::Control_L),
        "shift" => Some(Keysym::Shift_L),
        "alt" => Some(Keysym::Alt_L),
        "super" | "logo" | "mod4" => Some(Keysym::Super_L),
        _ => None,
    }
}

/// A modifier or an xkb keysym name: exact first, then in any case (`return` is `Return`).
fn keysym(name: &str) -> Option<Keysym> {
    if let Some(sym) = modifier(name) {
        return Some(sym);
    }
    [xkb::KEYSYM_NO_FLAGS, xkb::KEYSYM_CASE_INSENSITIVE]
        .into_iter()
        .map(|flags| xkb::keysym_from_name(name, flags))
        .find(|sym| sym.raw() != 0)
}

/// `ctrl+shift+Tab`: the keysyms in the order they go down.
fn parse_combo(combo: &str) -> Result<Vec<Keysym>, InputErrorOwned> {
    let parts: Vec<&str> = combo.split('+').map(str::trim).collect();
    // `ctrl++` is ctrl with plus.
    let parts: Vec<&str> = match parts.as_slice() {
        [head @ .., "", ""] => head.iter().copied().chain(["plus"]).collect(),
        _ => parts,
    };
    if parts.iter().any(|p| p.is_empty()) {
        return Err(InputErrorOwned::UnknownKey(combo.to_owned()));
    }
    let (last, mods) = parts.split_last().expect("a split has at least one part");
    let mut syms = Vec::new();
    for name in mods {
        syms.push(modifier(name).ok_or_else(|| InputErrorOwned::UnknownKey((*name).to_owned()))?);
    }
    syms.push(keysym(last).ok_or_else(|| InputErrorOwned::UnknownKey((*last).to_owned()))?);
    Ok(syms)
}

/// The first key of `layout` that makes `sym` on its first or second (Shift) level.
pub fn find_key(keymap: &xkb::Keymap, layout: u32, sym: Keysym) -> Option<Key> {
    let (min, max) = (keymap.min_keycode().raw(), keymap.max_keycode().raw());
    for level in 0..2 {
        for code in min..=max {
            let kc = xkb::Keycode::new(code);
            if level >= keymap.num_levels_for_key(kc, layout) {
                continue;
            }
            if keymap.key_get_syms_by_level(kc, layout, level) == [sym] {
                return Some(Key {
                    code,
                    shift: level == 1,
                });
            }
        }
    }
    None
}

fn button_code(button: Button) -> Option<u32> {
    // <linux/input-event-codes.h>
    match button {
        Button::Left => Some(0x110),
        Button::Right => Some(0x111),
        Button::Middle => Some(0x112),
        Button::Back => Some(0x116),
        Button::Forward => Some(0x115),
        Button::Unknown(_) => None,
    }
}

fn px(v: Px) -> f64 {
    f64::from(v.get())
}

/// A press's hold limit: the default for `None`, a zero limit refused.
fn hold_limit(hold: Option<Millis<u16>>) -> Result<Duration, InputErrorOwned> {
    match hold.map(|ms| ms.get()) {
        None => Ok(HOLD_DEFAULT),
        Some(0) => Err(InputErrorOwned::BadArgument),
        Some(ms) => Ok(Duration::from_millis(u64::from(ms))),
    }
}

/// Where a relative move of the pointer from `from` ends, as niri moves a mouse's pointer: past
/// the outputs it stops at the edge of the output it was on; from off every output it goes to
/// the middle of the first one. `None` without outputs.
pub fn moved_by(
    outputs: &[Rectangle<i32, Logical>],
    from: (f64, f64),
    dx: f64,
    dy: f64,
) -> Option<(f64, f64)> {
    let on = |(x, y): (f64, f64), r: &Rectangle<i32, Logical>| r.to_f64().contains((x, y));
    let to = (from.0 + dx, from.1 + dy);
    if outputs.iter().any(|r| on(to, r)) {
        return Some(to);
    }
    match outputs.iter().find(|r| on(from, r)) {
        Some(r) => Some((
            to.0.clamp(f64::from(r.loc.x), f64::from(r.loc.x + r.size.w - 1)),
            to.1.clamp(f64::from(r.loc.y), f64::from(r.loc.y + r.size.h - 1)),
        )),
        None => {
            let r = outputs.first()?;
            Some((
                f64::from(r.loc.x) + f64::from(r.size.w) / 2.,
                f64::from(r.loc.y) + f64::from(r.size.h) / 2.,
            ))
        }
    }
}

/// A button or key held down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Button(u32),
    Key(u32),
}

impl Held {
    fn release(self) -> Op {
        match self {
            Self::Button(code) => Op::Button {
                code,
                pressed: false,
            },
            Self::Key(code) => Op::Key {
                code,
                pressed: false,
            },
        }
    }
}

/// What a connection holds down, in the order it went down, each with the time it comes up by
/// itself: `None` while a call holds it (the modifiers around a click).
#[derive(Debug, Default)]
struct Holds {
    held: Vec<(Held, Option<Instant>)>,
}

impl Holds {
    /// Follows an injected step: a press holds, a release lets go.
    fn follow(&mut self, op: Op) {
        let (held, pressed) = match op {
            Op::Button { code, pressed } => (Held::Button(code), pressed),
            Op::Key { code, pressed } => (Held::Key(code), pressed),
            Op::Motion { .. } | Op::Wheel { .. } => return,
        };
        let was = self.held.iter().position(|(h, _)| *h == held);
        match (was, pressed) {
            (None, true) => self.held.push((held, None)),
            (Some(i), false) => {
                self.held.remove(i);
            }
            _ => (),
        }
    }

    fn holds(&self, held: Held) -> bool {
        self.held.iter().any(|(h, _)| *h == held)
    }

    /// The held button or key comes up at `until` unless pressed again before.
    fn limit(&mut self, held: Held, until: Instant) {
        for (h, at) in &mut self.held {
            if *h == held {
                *at = Some(until);
            }
        }
    }

    /// When the next hold limit passes.
    fn next_release(&self) -> Option<Instant> {
        self.held.iter().filter_map(|(_, at)| *at).min()
    }

    /// The releases of what is held past its limit at `now`.
    fn expired(&self, now: Instant) -> Vec<Op> {
        self.held
            .iter()
            .filter(|(_, at)| at.is_some_and(|at| at <= now))
            .map(|(h, _)| h.release())
            .collect()
    }

    /// The releases of everything held: buttons first, then keys in reverse.
    fn all(&self) -> Vec<Op> {
        let buttons = self
            .held
            .iter()
            .filter(|(h, _)| matches!(h, Held::Button(_)));
        let keys = self
            .held
            .iter()
            .rev()
            .filter(|(h, _)| matches!(h, Held::Key(_)));
        buttons.chain(keys).map(|(h, _)| h.release()).collect()
    }
}

/// A connection's input: the steps it injects and what it holds down.
pub struct Input {
    run: Run,
    io: Rc<LoopIo>,
    holds: Holds,
    /// niri has the devices.
    ready: bool,
}

impl Input {
    pub fn new(run: Run, io: Rc<LoopIo>) -> Self {
        Self {
            run,
            io,
            holds: Holds::default(),
            ready: false,
        }
    }

    async fn inject(&mut self, ops: Vec<Op>) -> Res {
        self.ready().await?;
        for (i, chunk) in ops.chunks(CHUNK).enumerate() {
            if i > 0 {
                // niri's loop reads the events in between: a long text never overflows the
                // device's buffer.
                self.io.sleep(CHUNK_GAP).await;
            }
            let chunk = chunk.to_vec();
            let held = chunk.clone();
            call(&self.run, move |host| host.inject(&chunk))
                .await
                .unwrap_or(Ok(()))?;
            for op in held {
                self.holds.follow(op);
            }
        }
        Ok(())
    }

    /// Creates the devices on first use and waits until niri has them: libinput opens a new
    /// device only once udev has announced it, and events written before that are lost.
    async fn ready(&mut self) -> Res {
        if self.ready {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + DEVICE_WAIT;
        loop {
            match call(&self.run, |host| host.input_devices()).await {
                Some(Ok(true)) | None => break,
                Some(Ok(false)) => (),
                Some(Err(err)) => return Err(InputErrorOwned::Unavailable(err)),
            }
            if std::time::Instant::now() > deadline {
                return Err(InputErrorOwned::Unavailable(
                    "niri did not pick up the uinput devices".to_owned(),
                ));
            }
            self.io.sleep(Duration::from_millis(20)).await;
        }
        self.ready = true;
        Ok(())
    }

    async fn global(&self, point: Point) -> Result<(f64, f64), InputErrorOwned> {
        match point {
            Point::Global { x, y } => Ok((px(x), px(y))),
            Point::Window { id, x, y } => {
                call(&self.run, move |host| host.window_point(id.0, px(x), px(y)))
                    .await
                    .unwrap_or(Err(InputErrorOwned::NoSuchWindow))
            }
            Point::Unknown(_) => Err(InputErrorOwned::BadArgument),
        }
    }

    async fn motion(&self, at: Option<Point>) -> Result<Vec<Op>, InputErrorOwned> {
        Ok(match at {
            Some(point) => {
                let (x, y) = self.global(point).await?;
                vec![Op::Motion { x, y }]
            }
            None => Vec::new(),
        })
    }

    async fn key(&self, sym: Keysym) -> Result<Key, InputErrorOwned> {
        call(&self.run, move |host| host.key_for(sym.raw()))
            .await
            .flatten()
            .ok_or_else(|| InputErrorOwned::UnknownKey(xkb::keysym_get_name(sym)))
    }

    async fn shift(&self) -> Result<u32, InputErrorOwned> {
        Ok(self.key(Keysym::Shift_L).await?.code)
    }

    pub async fn position(&self) -> Option<ww_niri_api::PointerPosition> {
        call(&self.run, |host| host.pointer_position()).await
    }

    pub async fn move_to(&mut self, to: Point) -> Res {
        let ops = self.motion(Some(to)).await?;
        self.inject(ops).await
    }

    pub async fn move_by(&mut self, dx: Px, dy: Px) -> Res {
        let (x, y) = call(&self.run, move |host| host.pointer_moved_by(px(dx), px(dy)))
            .await
            .unwrap_or(Err(InputErrorOwned::OffScreen))?;
        self.inject(vec![Op::Motion { x, y }]).await
    }

    /// The keys of the modifiers, in the order they go down.
    async fn modifier_keys(&self, modifiers: Modifiers) -> Result<Vec<u32>, InputErrorOwned> {
        let mut codes = Vec::new();
        for (on, sym) in [
            (modifiers.ctrl, Keysym::Control_L),
            (modifiers.shift, Keysym::Shift_L),
            (modifiers.alt, Keysym::Alt_L),
            (modifiers.super_key, Keysym::Super_L),
        ] {
            if on {
                codes.push(self.key(sym).await?.code);
            }
        }
        Ok(codes)
    }

    pub async fn click(
        &mut self,
        at: Option<Point>,
        button: Button,
        count: u8,
        modifiers: Modifiers,
    ) -> Res {
        let code = button_code(button).ok_or(InputErrorOwned::BadArgument)?;
        if !(1..=3).contains(&count) {
            return Err(InputErrorOwned::BadArgument);
        }
        let mods = self.modifier_keys(modifiers).await?;
        let mut ops = self.motion(at).await?;
        let steps =
            (0..count).flat_map(|_| [true, false].map(|pressed| Op::Button { code, pressed }));
        ops.extend(with_modifiers(&mods, steps));
        self.inject(ops).await
    }

    /// Presses a button, or renews the hold limit of one this connection holds.
    pub async fn press(&mut self, button: Button, hold: Option<Millis<u16>>) -> Res {
        let code = button_code(button).ok_or(InputErrorOwned::BadArgument)?;
        let limit = hold_limit(hold)?;
        let op = Op::Button {
            code,
            pressed: true,
        };
        self.hold(Held::Button(code), op, limit).await
    }

    pub async fn release(&mut self, button: Button) -> Res {
        let code = button_code(button).ok_or(InputErrorOwned::BadArgument)?;
        self.inject(vec![Op::Button {
            code,
            pressed: false,
        }])
        .await
    }

    async fn hold(&mut self, held: Held, press: Op, limit: Duration) -> Res {
        // A renewing press: the device has it down already.
        if !self.holds.holds(held) {
            self.inject(vec![press]).await?;
        }
        self.holds.limit(held, Instant::now() + limit);
        Ok(())
    }

    pub async fn drag(&mut self, from: Point, to: Point, button: Button, time: Duration) -> Res {
        let code = button_code(button).ok_or(InputErrorOwned::BadArgument)?;
        let (x0, y0) = self.global(from).await?;
        let (x1, y1) = self.global(to).await?;
        // Off screen at the end: refused before anything is pressed.
        call(&self.run, move |host| host.on_screen(x1, y1))
            .await
            .unwrap_or(true)
            .then_some(())
            .ok_or(InputErrorOwned::OffScreen)?;

        self.inject(vec![
            Op::Motion { x: x0, y: y0 },
            Op::Button {
                code,
                pressed: true,
            },
        ])
        .await?;
        let steps = (time.as_millis() / STEP.as_millis()).max(1) as u32;
        for i in 1..=steps {
            self.io.sleep(STEP).await;
            let t = f64::from(i) / f64::from(steps);
            let (x, y) = (x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
            self.inject(vec![Op::Motion { x, y }]).await?;
        }
        self.inject(vec![Op::Button {
            code,
            pressed: false,
        }])
        .await
    }

    /// `dx` and `dy` in v120 units.
    pub async fn scroll(
        &mut self,
        at: Option<Point>,
        dx: i16,
        dy: i16,
        modifiers: Modifiers,
    ) -> Res {
        let mods = self.modifier_keys(modifiers).await?;
        let mut ops = self.motion(at).await?;
        let wheel = Op::Wheel {
            dx: f64::from(dx),
            dy: f64::from(dy),
        };
        ops.extend(with_modifiers(&mods, [wheel]));
        self.inject(ops).await
    }

    pub async fn type_text(&mut self, text: &str) -> Res {
        let shift = self.shift().await?;
        let mut ops = Vec::new();
        for ch in text.chars() {
            let sym = match ch {
                '\n' => Keysym::Return,
                '\t' => Keysym::Tab,
                _ => xkb::utf32_to_keysym(u32::from(ch)),
            };
            let key = match self.key(sym).await {
                Ok(key) if sym.raw() != 0 => key,
                _ => return Err(InputErrorOwned::NotTypeable(u32::from(ch))),
            };
            tap(&mut ops, key, shift);
        }
        self.inject(ops).await
    }

    pub async fn press_keys(&mut self, combo: &str) -> Res {
        let mut codes = Vec::new();
        for sym in parse_combo(combo)? {
            codes.push(self.key(sym).await?);
        }
        let shift = self.shift().await?;
        // A lone key that needs Shift (`press_keys("A")`) gets it; in a combination the client
        // names its modifiers.
        if let [key] = codes[..] {
            let mut ops = Vec::new();
            tap(&mut ops, key, shift);
            return self.inject(ops).await;
        }
        let down = codes.iter().map(|k| Op::Key {
            code: k.code,
            pressed: true,
        });
        let up = codes.iter().rev().map(|k| Op::Key {
            code: k.code,
            pressed: false,
        });
        let ops = down.chain(up).collect();
        self.inject(ops).await
    }

    async fn named_key(&self, name: &str) -> Result<u32, InputErrorOwned> {
        let sym = keysym(name).ok_or_else(|| InputErrorOwned::UnknownKey(name.to_owned()))?;
        Ok(self.key(sym).await?.code)
    }

    /// Presses a key, or renews the hold limit of one this connection holds.
    pub async fn key_down(&mut self, name: &str, hold: Option<Millis<u16>>) -> Res {
        let limit = hold_limit(hold)?;
        let code = self.named_key(name).await?;
        let op = Op::Key {
            code,
            pressed: true,
        };
        self.hold(Held::Key(code), op, limit).await
    }

    pub async fn key_up(&mut self, name: &str) -> Res {
        let code = self.named_key(name).await?;
        self.inject(vec![Op::Key {
            code,
            pressed: false,
        }])
        .await
    }

    /// Comes when the next hold limit passes; never while nothing held has one.
    pub async fn hold_limit_passes(&self) {
        match self.holds.next_release() {
            Some(at) => self.io.sleep_until(at).await,
            None => std::future::pending().await,
        }
    }

    /// Lets go of what is held past its limit at `now`; answers how many went up.
    pub async fn release_expired(&mut self, now: Instant) -> Result<usize, InputErrorOwned> {
        let ops = self.holds.expired(now);
        let count = ops.len();
        if count > 0 {
            self.inject(ops).await?;
        }
        Ok(count)
    }

    /// Lets go of every button and key this connection still holds.
    pub async fn release_all(&mut self) -> Res {
        let ops = self.holds.all();
        if ops.is_empty() {
            return Ok(());
        }
        self.inject(ops).await
    }
}

/// `steps` with the keys of `mods` held around them.
fn with_modifiers(mods: &[u32], steps: impl IntoIterator<Item = Op>) -> Vec<Op> {
    let down = mods.iter().map(|&code| Op::Key {
        code,
        pressed: true,
    });
    let up = mods.iter().rev().map(|&code| Op::Key {
        code,
        pressed: false,
    });
    down.chain(steps).chain(up).collect()
}

fn tap(ops: &mut Vec<Op>, key: Key, shift: u32) {
    if key.shift {
        ops.push(Op::Key {
            code: shift,
            pressed: true,
        });
    }
    ops.push(Op::Key {
        code: key.code,
        pressed: true,
    });
    ops.push(Op::Key {
        code: key.code,
        pressed: false,
    });
    if key.shift {
        ops.push(Op::Key {
            code: shift,
            pressed: false,
        });
    }
}

/// The pointer's ABS value for `v` on an axis of the outputs' box. libinput maps it back as
/// `units * size / (ABS_MAX + 1)`; rounding up lands it on `v` or a hair past, never below, so
/// apps that floor the position see pixel `v`.
fn abs_units(v: f64, origin: i32, size: i32) -> i32 {
    let units = (v - f64::from(origin)) * f64::from(uinput::ABS_MAX + 1);
    (units / f64::from(size)).ceil() as i32
}

/// A motion niri may not have read yet: where it goes, and where niri had the pointer when it
/// was written.
pub struct Sent {
    to: (f64, f64),
    from: smithay::utils::Point<f64, Logical>,
}

impl Sent {
    pub fn new(to: (f64, f64), from: smithay::utils::Point<f64, Logical>) -> Self {
        Self { to, from }
    }

    /// Where a relative move starts with the pointer at `at`: where the last motion goes while
    /// niri has not read it (the pointer is still where it was then), else at `at`.
    fn start(sent: Option<&Self>, at: smithay::utils::Point<f64, Logical>) -> (f64, f64) {
        match sent {
            Some(sent) if sent.from == at => sent.to,
            _ => (at.x, at.y),
        }
    }
}

/// The pointer's place and the window under it with that window's place, as observers last
/// heard them.
pub struct Seen {
    at: smithay::utils::Point<f64, Logical>,
    window: Option<(smithay::desktop::Window, crate::layout::HitType)>,
}

/// niri's side of the host: injection, window points, the pointer and the keymap.
mod niri_state {
    use smithay::input::keyboard::Keysym;
    use smithay::utils::{Logical, Point as SPoint, Rectangle};
    use ww_niri_api::{InputErrorOwned, PointerPosition, WindowId, WindowPoint};

    use super::{find_key, moved_by, Key, Op, Seen, Sent};
    use crate::layout::HitType;
    use crate::niri::State;
    use crate::window::mapped::MappedId;
    use crate::ww::uinput::{self, Uinput};

    fn px(v: f64) -> ww_niri_api::Px {
        (v.round() as i32).into()
    }

    impl State {
        fn ww_pointer_location(&self) -> SPoint<f64, Logical> {
            let pointer = self
                .niri
                .seat
                .get_pointer()
                .expect("niri's seat has a pointer");
            pointer.current_location()
        }

        /// Tells observers of the pointer's position when it or the window under it moved, and
        /// forgets an injected motion niri has read; once per loop iteration, after niri found
        /// what is under the pointer.
        pub fn ww_refresh_pointer(&mut self) {
            let at = self.ww_pointer_location();
            let ww = &mut self.niri.ww;
            if ww.pointer_sent.as_ref().is_some_and(|sent| sent.from != at) {
                ww.pointer_sent = None;
            }
            let window = &self.niri.pointer_contents.window;
            if ww
                .pointer_seen
                .as_ref()
                .is_some_and(|seen| seen.at == at && &seen.window == window)
            {
                return;
            }
            ww.pointer_seen = Some(Seen {
                at,
                window: window.clone(),
            });
            ww.services().hub.pointer_moved();
        }

        pub fn ww_pointer_moved_by(
            &mut self,
            dx: f64,
            dy: f64,
        ) -> Result<(f64, f64), InputErrorOwned> {
            let from = Sent::start(
                self.niri.ww.pointer_sent.as_ref(),
                self.ww_pointer_location(),
            );
            let space = &self.niri.global_space;
            let outputs: Vec<_> = space
                .outputs()
                .filter_map(|o| space.output_geometry(o))
                .collect();
            moved_by(&outputs, from, dx, dy).ok_or(InputErrorOwned::OffScreen)
        }

        pub fn ww_on_screen(&self, x: f64, y: f64) -> bool {
            self.niri.output_under(SPoint::from((x, y))).is_some()
        }

        /// The outputs' bounding box, which libinput maps an absolute pointer to.
        fn ww_bounds(&self) -> Option<Rectangle<i32, Logical>> {
            let space = &self.niri.global_space;
            space
                .outputs()
                .filter_map(|o| space.output_geometry(o))
                .reduce(|a, b| a.merge(b))
        }

        /// Creates the uinput devices when missing; whether niri has them (libinput added them).
        pub fn ww_input_devices(&mut self) -> Result<bool, String> {
            if self.niri.ww.uinput.is_none() {
                let uinput = Uinput::create().map_err(|err| format!("/dev/uinput: {err}"))?;
                info!("ww: created the uinput keyboard and pointer");
                self.niri.ww.uinput = Some(uinput);
            }
            let names = [uinput::KEYBOARD_NAME, uinput::POINTER_NAME];
            let seen = |name: &str| self.niri.devices.iter().any(|d| d.name() == name);
            Ok(names.into_iter().all(seen))
        }

        pub fn ww_inject(&mut self, ops: &[Op]) -> Result<(), InputErrorOwned> {
            for op in ops {
                if let Op::Motion { x, y } = *op {
                    if !self.ww_on_screen(x, y) {
                        return Err(InputErrorOwned::OffScreen);
                    }
                }
            }
            let bounds = self.ww_bounds().ok_or(InputErrorOwned::OffScreen)?;
            let Some(uinput) = &self.niri.ww.uinput else {
                return Err(InputErrorOwned::Unavailable("no uinput devices".to_owned()));
            };
            let at = self.ww_pointer_location();
            for &op in ops {
                if let Op::Motion { x, y } = op {
                    self.niri.ww.pointer_sent = Some(Sent::new((x, y), at));
                }
                let res = match op {
                    Op::Motion { x, y } => uinput.write_pointer(&uinput::abs(
                        super::abs_units(x, bounds.loc.x, bounds.size.w),
                        super::abs_units(y, bounds.loc.y, bounds.size.h),
                    )),
                    Op::Button { code, pressed } => {
                        uinput.write_pointer(&uinput::key(code as u16, pressed))
                    }
                    Op::Wheel { dx, dy } => {
                        uinput.write_pointer(&uinput::wheel(dx as i32, dy as i32))
                    }
                    // xkb keycodes are evdev's plus 8.
                    Op::Key { code, pressed } => {
                        uinput.write_keyboard(&uinput::key((code - 8) as u16, pressed))
                    }
                };
                res.map_err(|err| InputErrorOwned::Unavailable(format!("uinput: {err}")))?;
            }
            Ok(())
        }

        /// A point of the window's surface in global space, as niri renders the window now.
        pub fn ww_window_point(
            &self,
            id: u64,
            x: f64,
            y: f64,
        ) -> Result<(f64, f64), InputErrorOwned> {
            let layout = &self.niri.layout;
            if !layout.is_overview_open() {
                for mon in layout.monitors() {
                    let Some(out) = self.niri.global_space.output_geometry(mon.output()) else {
                        continue;
                    };
                    for (ws, geo) in mon.workspaces_with_render_geo() {
                        for (tile, pos, visible) in ws.tiles_with_render_positions() {
                            if visible && tile.window().id().get() == id {
                                let p = out.loc.to_f64() + geo.loc + pos + tile.buf_loc();
                                return Ok((p.x + x, p.y + y));
                            }
                        }
                    }
                }
            }
            match self.niri.find_window_by_id(MappedId::specific(id)) {
                Some(_) => Err(InputErrorOwned::WindowNotVisible),
                None => Err(InputErrorOwned::NoSuchWindow),
            }
        }

        pub fn ww_pointer_position(&self) -> PointerPosition {
            let pos = self.ww_pointer_location();
            let under = self.niri.contents_under(pos);
            let window = under.window.and_then(|(window, hit)| {
                let HitType::Input { win_pos } = hit else {
                    return None;
                };
                let mut id = None;
                self.niri.layout.with_windows(|mapped, _, _, _| {
                    if mapped.window == window {
                        id = Some(mapped.id().get());
                    }
                });
                Some(WindowPoint {
                    id: WindowId(id?),
                    x: px(pos.x - win_pos.x),
                    y: px(pos.y - win_pos.y),
                })
            });
            PointerPosition {
                x: px(pos.x),
                y: px(pos.y),
                window,
            }
        }

        pub fn ww_key_for(&mut self, sym: u32) -> Option<Key> {
            let keyboard = self
                .niri
                .seat
                .get_keyboard()
                .expect("niri's seat has a keyboard");
            keyboard.with_xkb_state(self, |context| {
                let xkb = context.xkb().lock().expect("the xkb lock is not poisoned");
                let layout = xkb.active_layout();
                // SAFETY: the keymap is used only while the lock is held.
                let keymap = unsafe { xkb.keymap() };
                find_key(keymap, layout.0, Keysym::new(sym))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn relative_moves_stop_at_the_edge_of_their_output() {
        // Two outputs side by side, the right one shorter.
        let outputs = [rect(0, 0, 1920, 1080), rect(1920, 0, 1280, 720)];
        assert_eq!(moved_by(&outputs, (100., 100.), 5., -3.), Some((105., 97.)));
        // Across to the other output.
        assert_eq!(
            moved_by(&outputs, (1900., 100.), 50., 0.),
            Some((1950., 100.))
        );
        // Off every output: stops at the edge of the one it was on.
        assert_eq!(
            moved_by(&outputs, (100., 100.), -500., 5000.),
            Some((0., 1079.))
        );
        assert_eq!(
            moved_by(&outputs, (2000., 700.), 0., 100.),
            Some((2000., 719.))
        );
        // From off every output: the middle of the first one.
        assert_eq!(
            moved_by(&outputs, (5000., 5000.), 1., 1.),
            Some((960., 540.))
        );
        assert_eq!(moved_by(&[], (0., 0.), 1., 1.), None);
    }

    #[test]
    fn relative_moves_start_where_the_last_motion_goes_until_niri_has_it() {
        let at = smithay::utils::Point::from((10., 20.));
        let sent = Sent {
            to: (500., 300.),
            from: at,
        };
        assert_eq!(Sent::start(None, at), (10., 20.));
        // Not read yet: the pointer is still where it was.
        assert_eq!(Sent::start(Some(&sent), at), (500., 300.));
        // Read, or something else moved the pointer since.
        let moved = smithay::utils::Point::from((500.1, 300.1));
        assert_eq!(Sent::start(Some(&sent), moved), (500.1, 300.1));
    }

    #[test]
    fn hold_limits() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let press = |code| Op::Button {
            code,
            pressed: true,
        };
        let key = |code, pressed| Op::Key { code, pressed };
        let mut holds = Holds::default();

        // Held by a call: no limit.
        holds.follow(key(37, true));
        assert_eq!(holds.next_release(), None);
        assert!(holds.expired(ms(1_000_000)).is_empty());

        holds.follow(press(0x110));
        holds.limit(Held::Button(0x110), ms(100));
        holds.follow(key(50, true));
        holds.limit(Held::Key(50), ms(300));
        assert_eq!(holds.next_release(), Some(ms(100)));
        assert!(holds.expired(ms(99)).is_empty());
        assert_eq!(
            holds.expired(ms(100)),
            [Op::Button {
                code: 0x110,
                pressed: false
            }]
        );

        // A renewing press moves the limit on and holds once.
        holds.follow(press(0x110));
        holds.limit(Held::Button(0x110), ms(400));
        assert_eq!(holds.next_release(), Some(ms(300)));
        assert_eq!(holds.expired(ms(399)), [key(50, false)]);
        holds.follow(key(50, false));
        assert_eq!(holds.next_release(), Some(ms(400)));

        // Everything at the end: buttons, then keys in reverse.
        assert_eq!(
            holds.all(),
            [
                Op::Button {
                    code: 0x110,
                    pressed: false
                },
                key(37, false)
            ]
        );
        holds.follow(Op::Button {
            code: 0x110,
            pressed: false,
        });
        assert_eq!(holds.next_release(), None);

        assert_eq!(hold_limit(None), Ok(HOLD_DEFAULT));
        assert_eq!(
            hold_limit(Some(Millis::new(250))),
            Ok(Duration::from_millis(250))
        );
        assert_eq!(
            hold_limit(Some(Millis::new(0))),
            Err(InputErrorOwned::BadArgument)
        );
    }

    #[test]
    fn abs_units_map_back_to_the_pixel() {
        for size in [720, 1080, 1440, 1920, 2160, 3840] {
            for v in 0..size {
                let units = abs_units(f64::from(v + 100), 100, size);
                assert!(units <= uinput::ABS_MAX, "{v} of {size} past ABS_MAX");
                // libinput's mapping, then the floor an app takes.
                let back = f64::from(units) * f64::from(size) / f64::from(uinput::ABS_MAX + 1);
                assert_eq!(back.floor() as i32, v, "{v} of {size} came back as {back}");
            }
        }
    }

    #[test]
    fn combos() {
        let names = |combo: &str| {
            parse_combo(combo).map(|syms| {
                syms.into_iter()
                    .map(xkb::keysym_get_name)
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(names("Return").unwrap(), ["Return"]);
        assert_eq!(names("return").unwrap(), ["Return"]);
        assert_eq!(names("ctrl+l").unwrap(), ["Control_L", "l"]);
        assert_eq!(
            names("Ctrl+Shift+Tab").unwrap(),
            ["Control_L", "Shift_L", "Tab"]
        );
        assert_eq!(names("super+Page_Down").unwrap(), ["Super_L", "Next"]);
        assert_eq!(names("ctrl++").unwrap(), ["Control_L", "plus"]);
        assert_eq!(
            names("hyper+x"),
            Err(InputErrorOwned::UnknownKey("hyper".to_owned()))
        );
        assert_eq!(
            names("ctrl+NoSuchKey"),
            Err(InputErrorOwned::UnknownKey("NoSuchKey".to_owned()))
        );
        assert_eq!(names(""), Err(InputErrorOwned::UnknownKey("".to_owned())));
    }

    #[test]
    fn keys_of_the_us_keymap() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap =
            xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkb::COMPILE_NO_FLAGS)
                .unwrap();
        let key = |ch: char| find_key(&keymap, 0, xkb::utf32_to_keysym(u32::from(ch)));
        // evdev codes + 8: KEY_A 30, KEY_1 2, KEY_SPACE 57.
        assert_eq!(
            key('a'),
            Some(Key {
                code: 38,
                shift: false
            })
        );
        assert_eq!(
            key('A'),
            Some(Key {
                code: 38,
                shift: true
            })
        );
        assert_eq!(
            key('!'),
            Some(Key {
                code: 10,
                shift: true
            })
        );
        assert_eq!(
            key(' '),
            Some(Key {
                code: 65,
                shift: false
            })
        );
        assert_eq!(key('ж'), None);
        // Media and remote keys: evdev KEY_PLAYPAUSE 164, KEY_MUTE 113, KEY_VOLUMEUP 115,
        // KEY_BACK 158, KEY_HOMEPAGE 172, KEY_SEARCH 217.
        for (name, evdev) in [
            ("XF86AudioPlay", 164),
            ("XF86AudioMute", 113),
            ("XF86AudioRaiseVolume", 115),
            ("XF86Back", 158),
            ("XF86HomePage", 172),
            ("XF86Search", 217),
        ] {
            let sym = keysym(name).unwrap_or_else(|| panic!("{name} is a keysym"));
            assert_eq!(
                find_key(&keymap, 0, sym),
                Some(Key {
                    code: evdev + 8,
                    shift: false
                }),
                "{name}"
            );
            assert!(
                uinput::KEYS.contains(&(evdev as u16)),
                "{name} on the keyboard device"
            );
        }
        assert_eq!(
            find_key(&keymap, 0, Keysym::Return),
            Some(Key {
                code: 36,
                shift: false
            })
        );
    }
}
