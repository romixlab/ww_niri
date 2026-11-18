//! A keyboard and an absolute pointer as kernel uinput devices: libinput finds them as it finds
//! any device, so injected input goes through libinput, xkb, niri's binds and on to the apps.
//! The pointer is absolute like a VM's tablet (ABS_X and ABS_Y with mouse buttons and a wheel):
//! libinput maps it to the bounding box of all outputs, as niri does for such a device.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

pub const KEYBOARD_NAME: &str = "ww_niri keyboard";
pub const POINTER_NAME: &str = "ww_niri pointer";

/// The pointer's ABS_X and ABS_Y run 0..=ABS_MAX over the outputs' bounding box.
pub const ABS_MAX: i32 = 65535;

// <linux/input-event-codes.h>
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;
/// KEY_ESC to KEY_MICMUTE: the keys of a full keyboard.
pub const KEYS: std::ops::RangeInclusive<u16> = 1..=248;
/// BTN_LEFT to BTN_TASK.
const BUTTONS: std::ops::RangeInclusive<u16> = 0x110..=0x117;
const BUS_VIRTUAL: u16 = 0x06;

// <linux/uinput.h>: _IO('U', n) and _IOW('U', n, size).
const fn io(nr: u64) -> u64 {
    (b'U' as u64) << 8 | nr
}
const fn iow(nr: u64, size: usize) -> u64 {
    1 << 30 | (size as u64) << 16 | io(nr)
}
const UI_DEV_CREATE: u64 = io(1);
const UI_DEV_SETUP: u64 = iow(3, size_of::<libc::uinput_setup>());
const UI_ABS_SETUP: u64 = iow(4, size_of::<libc::uinput_abs_setup>());
const UI_SET_EVBIT: u64 = iow(100, size_of::<libc::c_int>());
const UI_SET_KEYBIT: u64 = iow(101, size_of::<libc::c_int>());
const UI_SET_RELBIT: u64 = iow(102, size_of::<libc::c_int>());
const UI_SET_ABSBIT: u64 = iow(103, size_of::<libc::c_int>());

/// One input event: type, code, value. The kernel stamps the time.
pub type Ev = (u16, u16, i32);

pub fn key(code: u16, pressed: bool) -> [Ev; 2] {
    [(EV_KEY, code, i32::from(pressed)), (EV_SYN, SYN_REPORT, 0)]
}

pub fn abs(x: i32, y: i32) -> [Ev; 3] {
    [
        (EV_ABS, ABS_X, x),
        (EV_ABS, ABS_Y, y),
        (EV_SYN, SYN_REPORT, 0),
    ]
}

/// A wheel turn in v120 units (120 a notch), positive down and right; whole notches also go as
/// the low-resolution events.
pub fn wheel(dx: i32, dy: i32) -> Vec<Ev> {
    let mut evs = Vec::new();
    if dy != 0 {
        // The wheel's axis points up.
        evs.push((EV_REL, REL_WHEEL_HI_RES, -dy));
        if dy % 120 == 0 {
            evs.push((EV_REL, REL_WHEEL, -dy / 120));
        }
    }
    if dx != 0 {
        evs.push((EV_REL, REL_HWHEEL_HI_RES, dx));
        if dx % 120 == 0 {
            evs.push((EV_REL, REL_HWHEEL, dx / 120));
        }
    }
    evs.push((EV_SYN, SYN_REPORT, 0));
    evs
}

fn ioctl(file: &File, request: u64, arg: libc::c_ulong) -> io::Result<()> {
    // SAFETY: the requests used here take an int by value or a pointer to a struct of the size
    // their number encodes, which the callers pass.
    let res = unsafe { libc::ioctl(file.as_raw_fd(), request as _, arg) };
    if res < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn setup(file: &File, name: &str) -> io::Result<()> {
    let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
    setup.id.bustype = BUS_VIRTUAL;
    setup.id.vendor = 0x7777;
    for (dst, src) in setup.name.iter_mut().zip(name.bytes()) {
        *dst = src as libc::c_char;
    }
    ioctl(file, UI_DEV_SETUP, &setup as *const _ as libc::c_ulong)?;
    ioctl(file, UI_DEV_CREATE, 0)
}

fn open() -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open("/dev/uinput")
}

/// The two devices; they go away when this is dropped (niri exits).
pub struct Uinput {
    keyboard: File,
    pointer: File,
}

impl Uinput {
    pub fn create() -> io::Result<Self> {
        let keyboard = open()?;
        ioctl(&keyboard, UI_SET_EVBIT, EV_KEY.into())?;
        for code in KEYS {
            ioctl(&keyboard, UI_SET_KEYBIT, code.into())?;
        }
        setup(&keyboard, KEYBOARD_NAME)?;

        let pointer = open()?;
        ioctl(&pointer, UI_SET_EVBIT, EV_KEY.into())?;
        for code in BUTTONS {
            ioctl(&pointer, UI_SET_KEYBIT, code.into())?;
        }
        ioctl(&pointer, UI_SET_EVBIT, EV_REL.into())?;
        for code in [REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
            ioctl(&pointer, UI_SET_RELBIT, code.into())?;
        }
        ioctl(&pointer, UI_SET_EVBIT, EV_ABS.into())?;
        for code in [ABS_X, ABS_Y] {
            ioctl(&pointer, UI_SET_ABSBIT, code.into())?;
            let mut abs: libc::uinput_abs_setup = unsafe { std::mem::zeroed() };
            abs.code = code;
            abs.absinfo.maximum = ABS_MAX;
            ioctl(&pointer, UI_ABS_SETUP, &abs as *const _ as libc::c_ulong)?;
        }
        setup(&pointer, POINTER_NAME)?;

        Ok(Self { keyboard, pointer })
    }

    pub fn write_keyboard(&self, evs: &[Ev]) -> io::Result<()> {
        write(&self.keyboard, evs)
    }

    pub fn write_pointer(&self, evs: &[Ev]) -> io::Result<()> {
        write(&self.pointer, evs)
    }
}

fn write(mut file: &File, evs: &[Ev]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(evs.len() * size_of::<libc::input_event>());
    for &(type_, code, value) in evs {
        let ev = libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        // SAFETY: input_event is plain old data.
        let bytes = unsafe {
            std::slice::from_raw_parts(&ev as *const _ as *const u8, size_of::<libc::input_event>())
        };
        buf.extend_from_slice(bytes);
    }
    file.write_all(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_numbers() {
        // From <linux/uinput.h> on 64-bit Linux.
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_SETUP, 0x405c5503);
        assert_eq!(UI_ABS_SETUP, 0x401c5504);
        assert_eq!(UI_SET_EVBIT, 0x40045564);
        assert_eq!(UI_SET_ABSBIT, 0x40045567);
    }

    #[test]
    fn wheel_events() {
        // Two notches down: the wheel turns back by two.
        assert_eq!(
            wheel(0, 240),
            [
                (EV_REL, REL_WHEEL_HI_RES, -240),
                (EV_REL, REL_WHEEL, -2),
                (EV_SYN, SYN_REPORT, 0)
            ]
        );
        assert_eq!(
            wheel(60, 0),
            [(EV_REL, REL_HWHEEL_HI_RES, 60), (EV_SYN, SYN_REPORT, 0)]
        );
    }
}
