//! The record of every call a client made, and the errors a test has the fake answer with.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use ww_niri_api::{
    BrowserErrorOwned, InputErrorOwned, LaunchErrorOwned, MediaErrorOwned, WindowError,
};

/// Calls the record keeps; older ones go.
pub(crate) const CALLS_LEN_MAX: usize = 10_000;

/// Errors waiting per method.
const FAILS_PER_METHOD_MAX: usize = 64;

/// A call a client made: a method, or a property write. Reads are not recorded: observers read
/// on their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The method's path without indices, as [METHODS] names it: `browsers.playback.play`.
    pub method: &'static str,
    /// The index of `browsers[]` or `media.players[]` on the path.
    pub index: Option<u32>,
    /// The arguments as Rust debug output, `= value` for a property write.
    pub args: String,
}

impl fmt::Display for Call {
    /// `browsers[0].open("https://example.org/")`, `audio.volume = 40`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut indexed = false;
        for (i, part) in self.method.split('.').enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            f.write_str(part)?;
            if let (false, Some(index), "browsers" | "players") = (indexed, self.index, part) {
                write!(f, "[{index}]")?;
                indexed = true;
            }
        }
        f.write_str(&self.args)
    }
}

/// Which error a method answers with.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Window,
    Launch,
    Browser,
    Media,
    Input,
    /// A property read or write: a common error.
    Property,
    /// The method has no error of its own.
    None,
}

/// Every method and property of the API, by path without indices, with the error it answers.
pub const METHODS: &[(&str, ErrorKind)] = &[
    ("info", ErrorKind::None),
    ("events", ErrorKind::None),
    ("windows.list", ErrorKind::None),
    ("windows.show", ErrorKind::Window),
    ("windows.focus", ErrorKind::Window),
    ("windows.close", ErrorKind::Window),
    ("windows.set_fullscreen", ErrorKind::Window),
    ("outputs.list", ErrorKind::None),
    ("apps.launch", ErrorKind::Launch),
    ("browsers.start", ErrorKind::Browser),
    ("browsers.open", ErrorKind::Browser),
    ("browsers.stop", ErrorKind::Browser),
    ("browsers.show", ErrorKind::Browser),
    ("browsers.state", ErrorKind::Property),
    ("browsers.playback.state", ErrorKind::Property),
    ("browsers.playback.play", ErrorKind::Media),
    ("browsers.playback.pause", ErrorKind::Media),
    ("browsers.playback.play_pause", ErrorKind::Media),
    ("browsers.playback.stop", ErrorKind::Media),
    ("browsers.playback.next", ErrorKind::Media),
    ("browsers.playback.previous", ErrorKind::Media),
    ("browsers.playback.seek", ErrorKind::Media),
    ("browsers.playback.set_position", ErrorKind::Media),
    ("media.list", ErrorKind::None),
    ("media.players.source", ErrorKind::Property),
    ("media.players.playback.state", ErrorKind::Property),
    ("media.players.playback.play", ErrorKind::Media),
    ("media.players.playback.pause", ErrorKind::Media),
    ("media.players.playback.play_pause", ErrorKind::Media),
    ("media.players.playback.stop", ErrorKind::Media),
    ("media.players.playback.next", ErrorKind::Media),
    ("media.players.playback.previous", ErrorKind::Media),
    ("media.players.playback.seek", ErrorKind::Media),
    ("media.players.playback.set_position", ErrorKind::Media),
    ("audio.volume", ErrorKind::Property),
    ("audio.muted", ErrorKind::Property),
    ("audio.sink", ErrorKind::Property),
    ("input.pointer.position", ErrorKind::Property),
    ("input.pointer.move_to", ErrorKind::Input),
    ("input.pointer.move_by", ErrorKind::Input),
    ("input.pointer.click", ErrorKind::Input),
    ("input.pointer.press", ErrorKind::Input),
    ("input.pointer.release", ErrorKind::Input),
    ("input.pointer.drag", ErrorKind::Input),
    ("input.pointer.scroll", ErrorKind::Input),
    ("input.keyboard.type_text", ErrorKind::Input),
    ("input.keyboard.press_keys", ErrorKind::Input),
    ("input.keyboard.key_down", ErrorKind::Input),
    ("input.keyboard.key_up", ErrorKind::Input),
];

/// An error the next call of a method answers with, whatever the fake would have done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fail {
    Window(WindowError),
    Launch(LaunchErrorOwned),
    Browser(BrowserErrorOwned),
    Media(MediaErrorOwned),
    Input(InputErrorOwned),
    /// A property read or write refused with this reason.
    Property(&'static str),
}

impl Fail {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Window(_) => ErrorKind::Window,
            Self::Launch(_) => ErrorKind::Launch,
            Self::Browser(_) => ErrorKind::Browser,
            Self::Media(_) => ErrorKind::Media,
            Self::Input(_) => ErrorKind::Input,
            Self::Property(_) => ErrorKind::Property,
        }
    }
}

/// Why [FakeTv::fail_next](crate::FakeTv::fail_next) refused an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailError {
    /// No method or property of the API has this path.
    UnknownMethod(String),
    /// The method answers another kind of error, or none.
    WrongKind {
        method: &'static str,
        answers: ErrorKind,
    },
    /// Errors waiting for this method reached the limit.
    TooMany(&'static str),
}

impl fmt::Display for FailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownMethod(m) => write!(f, "no method or property `{m}`"),
            Self::WrongKind { method, answers } => {
                write!(f, "`{method}` answers errors of kind {answers:?}")
            }
            Self::TooMany(m) => write!(f, "{FAILS_PER_METHOD_MAX} errors already wait for `{m}`"),
        }
    }
}

impl std::error::Error for FailError {}

/// The calls made and the errors waiting.
#[derive(Default)]
pub(crate) struct Record {
    calls: VecDeque<Call>,
    fails: HashMap<&'static str, VecDeque<Fail>>,
}

impl Record {
    pub(crate) fn call(&mut self, call: Call) {
        if self.calls.len() >= CALLS_LEN_MAX {
            self.calls.pop_front();
        }
        self.calls.push_back(call);
    }

    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls.iter().cloned().collect()
    }

    pub(crate) fn take_calls(&mut self) -> Vec<Call> {
        self.calls.drain(..).collect()
    }

    pub(crate) fn fail_next(&mut self, method: &str, fail: Fail) -> Result<(), FailError> {
        let (method, answers) = (METHODS.iter())
            .find(|(m, _)| *m == method)
            .copied()
            .ok_or_else(|| FailError::UnknownMethod(method.to_owned()))?;
        if fail.kind() != answers {
            return Err(FailError::WrongKind { method, answers });
        }
        let queue = self.fails.entry(method).or_default();
        if queue.len() >= FAILS_PER_METHOD_MAX {
            return Err(FailError::TooMany(method));
        }
        queue.push_back(fail);
        Ok(())
    }

    /// The error waiting for `method`, if any.
    pub(crate) fn take_fail(&mut self, method: &str) -> Option<Fail> {
        self.fails.get_mut(method).and_then(VecDeque::pop_front)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_show_their_index() {
        let call = |method, index, args: &str| Call {
            method,
            index,
            args: args.to_owned(),
        };
        assert_eq!(
            call("browsers.open", Some(0), "(\"https://x/\")").to_string(),
            "browsers[0].open(\"https://x/\")"
        );
        assert_eq!(
            call("media.players.playback.play", Some(3), "()").to_string(),
            "media.players[3].playback.play()"
        );
        assert_eq!(
            call("audio.volume", None, " = 40").to_string(),
            "audio.volume = 40"
        );
    }

    #[test]
    fn fails_are_checked() {
        let mut record = Record::default();
        assert_eq!(
            record.fail_next("nope", Fail::Property("x")),
            Err(FailError::UnknownMethod("nope".to_owned()))
        );
        assert_eq!(
            record.fail_next("info", Fail::Property("x")),
            Err(FailError::WrongKind {
                method: "info",
                answers: ErrorKind::None
            })
        );
        record
            .fail_next("windows.show", Fail::Window(WindowError::NoSuchWindow))
            .unwrap();
        assert_eq!(
            record.take_fail("windows.show"),
            Some(Fail::Window(WindowError::NoSuchWindow))
        );
        assert_eq!(record.take_fail("windows.show"), None);
    }
}
