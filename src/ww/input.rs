//! Pointer and keyboard from the API, injected below niri through kernel uinput devices
//! ([super::uinput]): libinput, xkb, niri's binds and the apps see them as real devices. Window
//! points and keysyms are resolved in niri. Each connection keeps what it holds down ([Input])
//! and lets it go when it ends.

use std::rc::Rc;
use std::time::Duration;

use smithay::input::keyboard::{xkb, Keysym};
use ww_niri_api::{Button, InputErrorOwned, Modifiers, Point};

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

/// A wheel notch in v120 units.
const NOTCH: f64 = 120.;

/// Steps written at once, and the pause between such chunks.
const CHUNK: usize = 16;
const CHUNK_GAP: Duration = Duration::from_millis(2);

/// How long new devices may take to reach niri.
const DEVICE_WAIT: Duration = Duration::from_secs(3);

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
    let (last, mods) = parts.split_last().unwrap();
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

fn px(v: ww_niri_api::Px) -> f64 {
    f64::from(v.get())
}

/// A connection's input: the steps it injects and what it holds down.
pub struct Input {
    run: Run,
    io: Rc<LoopIo>,
    buttons: Vec<u32>,
    keys: Vec<u32>,
    /// niri has the devices.
    ready: bool,
}

impl Input {
    pub fn new(run: Run, io: Rc<LoopIo>) -> Self {
        Self {
            run,
            io,
            buttons: Vec::new(),
            keys: Vec::new(),
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
                match op {
                    Op::Button { code, pressed } => hold(&mut self.buttons, code, pressed),
                    Op::Key { code, pressed } => hold(&mut self.keys, code, pressed),
                    _ => (),
                }
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
        let mut names = Vec::new();
        for (on, sym) in [
            (modifiers.ctrl, Keysym::Control_L),
            (modifiers.shift, Keysym::Shift_L),
            (modifiers.alt, Keysym::Alt_L),
            (modifiers.super_key, Keysym::Super_L),
        ] {
            if on {
                names.push(self.key(sym).await?.code);
            }
        }
        let mut ops = self.motion(at).await?;
        ops.extend(names.iter().map(|&code| Op::Key {
            code,
            pressed: true,
        }));
        for _ in 0..count {
            ops.push(Op::Button {
                code,
                pressed: true,
            });
            ops.push(Op::Button {
                code,
                pressed: false,
            });
        }
        ops.extend(names.iter().rev().map(|&code| Op::Key {
            code,
            pressed: false,
        }));
        self.inject(ops).await
    }

    pub async fn button(&mut self, button: Button, pressed: bool) -> Res {
        let code = button_code(button).ok_or(InputErrorOwned::BadArgument)?;
        self.inject(vec![Op::Button { code, pressed }]).await
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

    pub async fn scroll(&mut self, at: Option<Point>, dx: i16, dy: i16) -> Res {
        let mut ops = self.motion(at).await?;
        ops.push(Op::Wheel {
            dx: f64::from(dx) * NOTCH,
            dy: f64::from(dy) * NOTCH,
        });
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

    pub async fn key_updown(&mut self, name: &str, pressed: bool) -> Res {
        let sym = keysym(name).ok_or_else(|| InputErrorOwned::UnknownKey(name.to_owned()))?;
        let key = self.key(sym).await?;
        self.inject(vec![Op::Key {
            code: key.code,
            pressed,
        }])
        .await
    }

    /// Lets go of every button and key this connection still holds.
    pub async fn release_all(&mut self) {
        let buttons = self.buttons.iter().map(|&code| Op::Button {
            code,
            pressed: false,
        });
        let keys = self.keys.iter().rev().map(|&code| Op::Key {
            code,
            pressed: false,
        });
        let ops: Vec<Op> = buttons.chain(keys).collect();
        if !ops.is_empty() {
            let _ = self.inject(ops).await;
        }
    }
}

fn hold(held: &mut Vec<u32>, code: u32, pressed: bool) {
    held.retain(|&c| c != code);
    if pressed {
        held.push(code);
    }
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

/// niri's side of the host: injection, window points, the pointer and the keymap.
mod niri_state {
    use smithay::input::keyboard::Keysym;
    use smithay::utils::{Logical, Point as SPoint, Rectangle};
    use ww_niri_api::{InputErrorOwned, PointerPosition, WindowId, WindowPoint};

    use super::{find_key, Key, Op};
    use crate::layout::HitType;
    use crate::niri::State;
    use crate::window::mapped::MappedId;
    use crate::ww::uinput::{self, Uinput};

    fn px(v: f64) -> ww_niri_api::Px {
        (v.round() as i32).into()
    }

    impl State {
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
            for &op in ops {
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
            let pos = self.niri.seat.get_pointer().unwrap().current_location();
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
            let keyboard = self.niri.seat.get_keyboard().unwrap();
            keyboard.with_xkb_state(self, |context| {
                let xkb = context.xkb().lock().unwrap();
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
        assert_eq!(
            find_key(&keymap, 0, Keysym::Return),
            Some(Key {
                code: 36,
                shift: false
            })
        );
    }
}
