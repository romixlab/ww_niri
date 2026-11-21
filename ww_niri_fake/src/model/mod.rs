//! The TV the fake pretends to be: one 1920x1080 output, windows on workspaces, browser slots,
//! media players, audio and the injected input. Time comes in as microseconds; what changes goes
//! to an outbox the connection sends from.

use std::collections::{BTreeMap, VecDeque};

use exact_units::Fixed;
use ww_niri_api::{
    BrowserErrorOwned, BrowserPhase, BrowserSlot, BrowserStateOwned, Button, EventOwned,
    MediaErrorOwned, Modifiers, OutputOwned, PlaybackStateOwned, PlaybackStatus, PlayerId,
    PlayerInfoOwned, PlayerSourceOwned, StopReason, WindowError, WindowId, WindowOwned,
    WorkspaceId,
};

mod cover;
mod input;

pub use cover::Cover;

/// The output's size in logical pixels.
pub(crate) const OUTPUT_WIDTH_PX: i32 = 1920;
pub(crate) const OUTPUT_HEIGHT_PX: i32 = 1080;
const OUTPUT_NAME: &str = "HDMI-A-1";

/// A held button or key comes up when this passes without a renewing press.
const HOLD_DEFAULT_US: u64 = 10_000_000;

/// Changes waiting for the connection; past this the client is told to list again.
const OUTBOX_LEN_MAX: usize = 1024;

/// Entries the input log keeps; older ones go.
pub(crate) const INPUT_LOG_LEN_MAX: usize = 10_000;

/// The length of what an opened page plays.
const PAGE_MEDIA_LENGTH_US: u64 = 300_000_000;

/// Workspace of the windows there at the start; browser slot `n` gets `BROWSER_WORKSPACE + n`.
const START_WORKSPACE: u64 = 1;
const BROWSER_WORKSPACE: u64 = 10;

/// An observed property whose value changed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    BrowserState(u8),
    BrowserPlayback(u8),
    PlayerPlayback(u32),
    PlayerGone(u32),
    Audio,
    Pointer,
}

/// What the connection sends: an event of the `events` stream, or an observed change.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Out {
    Event(EventOwned),
    Mark(Mark),
}

/// Why a held button or key came up.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReleasedBy {
    /// `release` or `key_up`.
    Call,
    /// Its hold limit passed without a renewing press.
    HoldLimit,
    /// The connection that held it ended.
    LinkDown,
}

/// What the client injected, in order: the fake's record of keys, text, buttons and the wheel.
/// Pointer moves are not here: [FakeTv::pointer](crate::FakeTv::pointer) says where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEntry {
    Click {
        x: i32,
        y: i32,
        button: Button,
        count: u8,
        modifiers: Modifiers,
    },
    Press(Button),
    Release {
        button: Button,
        by: ReleasedBy,
    },
    Drag {
        from: (i32, i32),
        to: (i32, i32),
        button: Button,
    },
    Scroll {
        x: i32,
        y: i32,
        dx: i16,
        dy: i16,
        modifiers: Modifiers,
    },
    Text(String),
    Keys(String),
    KeyDown(String),
    KeyUp {
        key: String,
        by: ReleasedBy,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    const OUTPUT: Self = Self {
        x: 0,
        y: 0,
        w: OUTPUT_WIDTH_PX,
        h: OUTPUT_HEIGHT_PX,
    };

    fn contains(&self, x: i32, y: i32) -> bool {
        (self.x..self.x + self.w).contains(&x) && (self.y..self.y + self.h).contains(&y)
    }
}

struct Win {
    id: u64,
    app_id: String,
    title: String,
    pid: u32,
    workspace: u64,
    rect: Rect,
    fullscreen: bool,
    slot: Option<u8>,
    /// Hidden by `windows.hide`: in no workspace, never drawn or focused.
    hidden: bool,
}

impl Win {
    fn rect(&self) -> Rect {
        if self.fullscreen {
            Rect::OUTPUT
        } else {
            self.rect
        }
    }
}

#[derive(Default)]
struct Slot {
    phase: Option<BrowserPhase>,
    pid: Option<u32>,
    url: Option<String>,
    window: Option<u64>,
    player: Option<u32>,
    /// When the window maps and the slot is Running.
    maps_at_us: Option<u64>,
    fullscreen: bool,
}

struct Player {
    identity: String,
    source: PlayerSourceOwned,
    pid: u32,
    status: PlaybackStatus,
    title: String,
    artist: Option<String>,
    length_us: u64,
    /// The position at `since_us`; while playing it moves on from there.
    position_us: u64,
    since_us: u64,
    track: u32,
}

impl Player {
    fn position(&self, now_us: u64) -> u64 {
        let moved = match self.status {
            PlaybackStatus::Playing => now_us.saturating_sub(self.since_us),
            _ => 0,
        };
        self.position_us.saturating_add(moved).min(self.length_us)
    }

    /// When a playing track reaches its end.
    fn ends_at(&self) -> Option<u64> {
        (self.status == PlaybackStatus::Playing)
            .then(|| self.since_us + (self.length_us.saturating_sub(self.position_us)))
    }

    fn state(&self, now_us: u64) -> PlaybackStateOwned {
        PlaybackStateOwned {
            status: self.status,
            title: Some(self.title.clone()),
            artist: self.artist.clone(),
            length: Some(Fixed::new(self.length_us)),
            position: Fixed::new(self.position(now_us)),
            rate: Fixed::new(1000),
            can_seek: true,
            can_pause: true,
        }
    }

    fn set(&mut self, now_us: u64, status: PlaybackStatus, position_us: u64) {
        self.status = status;
        self.position_us = position_us.min(self.length_us);
        self.since_us = now_us;
    }
}

/// A player action of the `Playback` interface.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Action {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    Seek(i64),
    SetPosition(u64),
}

/// The default sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sink {
    pub name: String,
    pub description: String,
}

/// Settings the model starts from.
pub(crate) struct Setup {
    pub slots: u8,
    pub start_delay_us: u64,
    pub pages_play: bool,
}

pub(crate) struct Model {
    now_us: u64,
    windows: Vec<Win>,
    focused: Option<u64>,
    workspace: u64,
    next_window_id: u64,
    next_pid: u32,
    slots: Vec<Slot>,
    players: BTreeMap<u32, Player>,
    next_player_id: u32,
    pub volume: u16,
    pub muted: bool,
    pub sink: Option<Sink>,
    pointer: (i32, i32),
    held_buttons: Vec<(Button, u64)>,
    held_keys: Vec<(String, u64)>,
    /// Covered windows by id.
    covers: BTreeMap<u64, Cover>,
    /// While a cover mutes the sink: its mute before.
    mute_held: Option<bool>,
    input_log: VecDeque<InputEntry>,
    outbox: Vec<Out>,
    /// The outbox overflowed: the client lists again.
    behind: bool,
    start_delay_us: u64,
    pages_play: bool,
    /// The TV's screen is on the HDMI port; without it niri lists no output.
    screen_connected: bool,
}

impl Model {
    /// Two windows on the first workspace, a terminal left and a paused video player right;
    /// every browser slot stopped; volume 40 %, not muted.
    pub(crate) fn new(setup: &Setup) -> Self {
        let mut model = Self {
            now_us: 0,
            windows: Vec::new(),
            focused: None,
            workspace: START_WORKSPACE,
            next_window_id: 1,
            next_pid: 1000,
            slots: (0..setup.slots).map(|_| Slot::default()).collect(),
            players: BTreeMap::new(),
            next_player_id: 0,
            volume: 40,
            muted: false,
            sink: Some(Sink {
                name: "fake_sink".to_owned(),
                description: "Fake HDMI".to_owned(),
            }),
            pointer: (OUTPUT_WIDTH_PX / 2, OUTPUT_HEIGHT_PX / 2),
            held_buttons: Vec::new(),
            held_keys: Vec::new(),
            covers: BTreeMap::new(),
            mute_held: None,
            input_log: VecDeque::new(),
            outbox: Vec::new(),
            behind: false,
            start_delay_us: setup.start_delay_us,
            pages_play: setup.pages_play,
            screen_connected: true,
        };
        let half = Rect {
            x: 0,
            y: 0,
            w: OUTPUT_WIDTH_PX / 2,
            h: OUTPUT_HEIGHT_PX,
        };
        let terminal = model.add_window("kitty", "~", START_WORKSPACE, half, None, false);
        let right = Rect {
            x: OUTPUT_WIDTH_PX / 2,
            ..half
        };
        let video = model.add_window(
            "mpv",
            "big_buck_bunny.mp4 - mpv",
            START_WORKSPACE,
            right,
            None,
            false,
        );
        let pid = model.window(video).map_or(0, |w| w.pid);
        model.add_player(
            "mpv",
            PlayerSourceOwned::Window {
                id: WindowId(video),
                app_id: Some("mpv".to_owned()),
            },
            pid,
            "Big Buck Bunny",
            Some("Blender Foundation"),
            596_000_000,
            PlaybackStatus::Paused,
        );
        model.focused = Some(terminal);
        model.outbox.clear();
        model
    }

    // ---- time ----

    /// Moves time on to `now_us` and acts on what came due.
    pub(crate) fn tick(&mut self, now_us: u64) {
        self.now_us = self.now_us.max(now_us);
        let now = self.now_us;
        for slot in 0..self.slot_count() {
            if self.slots[usize::from(slot)]
                .maps_at_us
                .is_some_and(|at| at <= now)
            {
                self.map_browser(slot);
            }
        }
        let ended: Vec<u32> = (self.players.iter())
            .filter(|(_, p)| p.ends_at().is_some_and(|at| at <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in ended {
            if let Some(player) = self.players.get_mut(&id) {
                let length = player.length_us;
                player.set(now, PlaybackStatus::Stopped, length);
            }
            self.mark_player(id);
        }
        self.lift_due_covers();
        let (expired, held): (Vec<_>, Vec<_>) =
            (self.held_buttons.drain(..)).partition(|(_, at)| *at <= now);
        self.held_buttons = held;
        for (button, _) in expired {
            self.log(InputEntry::Release {
                button,
                by: ReleasedBy::HoldLimit,
            });
        }
        let (expired, held): (Vec<_>, Vec<_>) =
            (self.held_keys.drain(..)).partition(|(_, at)| *at <= now);
        self.held_keys = held;
        for (key, _) in expired {
            self.log(InputEntry::KeyUp {
                key,
                by: ReleasedBy::HoldLimit,
            });
        }
    }

    /// The next time something comes due by itself.
    pub(crate) fn next_deadline(&self) -> Option<u64> {
        let maps = self.slots.iter().filter_map(|s| s.maps_at_us);
        let ends = self.players.values().filter_map(Player::ends_at);
        let held = (self.held_buttons.iter().map(|(_, at)| *at))
            .chain(self.held_keys.iter().map(|(_, at)| *at));
        maps.chain(ends)
            .chain(held)
            .chain(self.next_cover_end())
            .min()
    }

    // ---- outbox ----

    fn push(&mut self, out: Out) {
        if self.outbox.len() >= OUTBOX_LEN_MAX {
            self.outbox.clear();
            self.behind = true;
        }
        if !self.behind {
            self.outbox.push(out);
        }
    }

    fn event(&mut self, event: EventOwned) {
        self.push(Out::Event(event));
    }

    fn mark(&mut self, mark: Mark) {
        self.push(Out::Mark(mark));
    }

    /// What changed since the last call, and whether some of it was lost.
    pub(crate) fn drain(&mut self) -> (Vec<Out>, bool) {
        let behind = std::mem::take(&mut self.behind);
        (std::mem::take(&mut self.outbox), behind)
    }

    /// Has the client list everything again, as after missed events.
    pub(crate) fn resync(&mut self) {
        self.outbox.clear();
        self.behind = true;
    }

    // ---- windows and outputs ----

    fn window(&self, id: u64) -> Option<&Win> {
        self.windows.iter().find(|w| w.id == id)
    }

    fn window_mut(&mut self, id: u64) -> Option<&mut Win> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    fn add_window(
        &mut self,
        app_id: &str,
        title: &str,
        workspace: u64,
        rect: Rect,
        slot: Option<u8>,
        fullscreen: bool,
    ) -> u64 {
        let id = self.next_window_id;
        self.next_window_id += 1;
        let pid = match slot.and_then(|s| self.slots[usize::from(s)].pid) {
            Some(pid) => pid,
            None => self.new_pid(),
        };
        self.windows.push(Win {
            id,
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            pid,
            workspace,
            rect,
            fullscreen,
            slot,
            hidden: false,
        });
        self.window_changed(id);
        id
    }

    /// Opens a window on a workspace of its own and shows it, as a launched app's would.
    pub(crate) fn open_window(&mut self, app_id: &str, title: &str) -> u64 {
        let workspace = self.windows.iter().map(|w| w.workspace).max().unwrap_or(0) + 1;
        let id = self.add_window(app_id, title, workspace, Rect::OUTPUT, None, false);
        self.show_window(id, None)
            .expect("the window was just added");
        id
    }

    /// A launched app's pid; the fake runs nothing.
    pub(crate) fn launch(&mut self) -> u32 {
        self.new_pid()
    }

    fn new_pid(&mut self) -> u32 {
        let pid = self.next_pid;
        self.next_pid += 1;
        pid
    }

    fn window_owned(&self, w: &Win) -> WindowOwned {
        WindowOwned {
            id: WindowId(w.id),
            app_id: Some(w.app_id.clone()),
            title: Some(w.title.clone()),
            pid: Some(w.pid),
            workspace: (!w.hidden).then_some(WorkspaceId(w.workspace)),
            output: (self.screen_connected && !w.hidden).then(|| OUTPUT_NAME.to_owned()),
            focused: self.focused == Some(w.id),
            fullscreen: w.fullscreen,
            browser: w.slot.map(BrowserSlot),
            hidden: w.hidden,
        }
    }

    fn window_changed(&mut self, id: u64) {
        if let Some(w) = self.window(id) {
            let window = self.window_owned(w);
            self.event(EventOwned::WindowOpenedOrChanged(window));
        }
        // The window under the pointer may be another now.
        self.mark(Mark::Pointer);
    }

    fn set_focus(&mut self, id: Option<u64>) {
        if self.focused == id {
            return;
        }
        let old = std::mem::replace(&mut self.focused, id);
        self.event(EventOwned::WindowFocusChanged(id.map(WindowId)));
        for id in [old, id].into_iter().flatten() {
            self.window_changed(id);
        }
    }

    pub(crate) fn windows(&self) -> Vec<WindowOwned> {
        self.windows.iter().map(|w| self.window_owned(w)).collect()
    }

    /// The screen came on the HDMI port, or went (TV off or unplugged): the output list changes, and every
    /// window's output with it.
    pub(crate) fn set_screen_connected(&mut self, connected: bool) {
        if self.screen_connected == connected {
            return;
        }
        self.screen_connected = connected;
        self.event(EventOwned::OutputsChanged);
        let ids: Vec<u64> = self.windows.iter().map(|w| w.id).collect();
        for id in ids {
            self.window_changed(id);
        }
    }

    /// One output while the screen is connected, none without it.
    pub(crate) fn outputs(&self) -> Vec<OutputOwned> {
        if !self.screen_connected {
            return Vec::new();
        }
        vec![OutputOwned {
            name: OUTPUT_NAME.to_owned(),
            make: "Fake".to_owned(),
            model: "TV".to_owned(),
            width: Fixed::new(u16::try_from(OUTPUT_WIDTH_PX).expect("the width fits a u16")),
            height: Fixed::new(u16::try_from(OUTPUT_HEIGHT_PX).expect("the height fits a u16")),
            refresh: Fixed::new(60_000),
            scale: Fixed::new(100),
            focused: true,
        }]
    }

    /// Switches to the window's workspace and focuses it; `fullscreen` on the way when given. A
    /// hidden window comes back on the current workspace, as niri puts it on the active one.
    pub(crate) fn show_window(
        &mut self,
        id: u64,
        fullscreen: Option<bool>,
    ) -> Result<(), WindowError> {
        let current = self.workspace;
        let w = self.window_mut(id).ok_or(WindowError::NoSuchWindow)?;
        let unhidden = std::mem::take(&mut w.hidden);
        if unhidden {
            w.workspace = current;
        }
        let workspace = w.workspace;
        let changed = unhidden || fullscreen.is_some_and(|on| on != w.fullscreen);
        if let Some(on) = fullscreen {
            w.fullscreen = on;
        }
        self.workspace = workspace;
        if changed {
            self.window_changed(id);
        }
        self.set_focus(Some(id));
        self.mark(Mark::Pointer);
        Ok(())
    }

    /// Keyboard focus, as niri's `focus-window`: a hidden window stays hidden and unfocused.
    pub(crate) fn focus_window(&mut self, id: u64) -> Result<(), WindowError> {
        let w = self.window(id).ok_or(WindowError::NoSuchWindow)?;
        if w.hidden {
            return Ok(());
        }
        self.show_window(id, None)
    }

    /// Takes the window out of its workspace, alive but never drawn or focused, until
    /// `show_window`; focus goes to another window of the current workspace, if any.
    pub(crate) fn hide_window(&mut self, id: u64) -> Result<(), WindowError> {
        let w = self.window_mut(id).ok_or(WindowError::NoSuchWindow)?;
        if w.hidden {
            return Ok(());
        }
        w.hidden = true;
        self.window_changed(id);
        if self.focused == Some(id) {
            self.set_focus(self.focus_after(id));
        }
        self.mark(Mark::Pointer);
        Ok(())
    }

    /// What niri focuses when `gone` leaves: another shown window of the current workspace.
    fn focus_after(&self, gone: u64) -> Option<u64> {
        (self.windows.iter())
            .find(|w| w.id != gone && !w.hidden && w.workspace == self.workspace)
            .map(|w| w.id)
    }

    /// Sets fullscreen; a hidden window is out of the layout and keeps what it had, as in niri.
    pub(crate) fn set_fullscreen(&mut self, id: u64, on: bool) -> Result<(), WindowError> {
        let w = self.window_mut(id).ok_or(WindowError::NoSuchWindow)?;
        if !w.hidden && w.fullscreen != on {
            w.fullscreen = on;
            self.window_changed(id);
        }
        Ok(())
    }

    /// The app closes the window: a browser slot's Firefox and a player's process end with it.
    pub(crate) fn close_window(&mut self, id: u64) -> Result<(), WindowError> {
        let pos =
            (self.windows.iter().position(|w| w.id == id)).ok_or(WindowError::NoSuchWindow)?;
        let w = self.windows.remove(pos);
        self.lift(id, ww_niri_api::CoverEnd::WindowClosed);
        self.event(EventOwned::WindowClosed(WindowId(id)));
        if self.focused == Some(id) {
            self.set_focus(self.focus_after(id));
        }
        self.mark(Mark::Pointer);
        if let Some(slot) = w.slot {
            self.end_browser(slot, StopReason::Exited(0));
        } else {
            let gone: Vec<u32> = (self.players.iter())
                .filter(|(_, p)| p.pid == w.pid)
                .map(|(id, _)| *id)
                .collect();
            for id in gone {
                self.remove_player(id);
            }
        }
        Ok(())
    }

    // ---- browsers ----

    pub(crate) fn slot_count(&self) -> u8 {
        u8::try_from(self.slots.len()).expect("the slot count came as a u8")
    }

    fn slot(&self, slot: u8) -> Option<&Slot> {
        self.slots.get(usize::from(slot))
    }

    fn slot_mut(&mut self, slot: u8) -> Option<&mut Slot> {
        self.slots.get_mut(usize::from(slot))
    }

    pub(crate) fn browser_state(&self, slot: u8) -> BrowserStateOwned {
        let s = self.slot(slot);
        let window = s.and_then(|s| s.window);
        BrowserStateOwned {
            phase: s.and_then(|s| s.phase).unwrap_or(BrowserPhase::Stopped),
            url: s.and_then(|s| s.url.clone()),
            title: window
                .and_then(|id| self.window(id))
                .map(|w| w.title.clone()),
            window: window.map(WindowId),
            player: s.and_then(|s| s.player).map(PlayerId),
        }
    }

    /// Driven at once (`BrowserStarted`), Running when its window maps after the start delay.
    pub(crate) fn start_browser(
        &mut self,
        slot: u8,
        fullscreen: bool,
    ) -> Result<(), BrowserErrorOwned> {
        let pid = self.new_pid();
        let maps_at = self.now_us + self.start_delay_us;
        let s = self.slot_mut(slot).ok_or(BrowserErrorOwned::NotRunning)?;
        if s.phase.is_some_and(|p| p != BrowserPhase::Stopped) {
            return Err(BrowserErrorOwned::AlreadyRunning);
        }
        *s = Slot {
            phase: Some(BrowserPhase::Starting),
            pid: Some(pid),
            url: Some("about:blank".to_owned()),
            maps_at_us: Some(maps_at),
            fullscreen,
            ..Slot::default()
        };
        self.event(EventOwned::BrowserStarted(BrowserSlot(slot)));
        self.mark(Mark::BrowserState(slot));
        if self.start_delay_us == 0 {
            self.map_browser(slot);
        }
        Ok(())
    }

    fn map_browser(&mut self, slot: u8) {
        let title = self.page_title(slot);
        let fullscreen = self.slot(slot).is_some_and(|s| s.fullscreen);
        let workspace = BROWSER_WORKSPACE + u64::from(slot);
        let id = self.add_window(
            "firefox",
            &title,
            workspace,
            Rect::OUTPUT,
            Some(slot),
            fullscreen,
        );
        if let Some(s) = self.slot_mut(slot) {
            s.maps_at_us = None;
            s.phase = Some(BrowserPhase::Running);
            s.window = Some(id);
        }
        self.workspace = workspace;
        self.set_focus(Some(id));
        self.mark(Mark::BrowserState(slot));
    }

    fn page_title(&self, slot: u8) -> String {
        let url = self.slot(slot).and_then(|s| s.url.as_deref()).unwrap_or("");
        let page = url.split_once("://").map_or(url, |(_, rest)| rest);
        format!("{page} — Mozilla Firefox")
    }

    fn running_pid(&self, slot: u8) -> Result<u32, BrowserErrorOwned> {
        let s = self.slot(slot).ok_or(BrowserErrorOwned::NotRunning)?;
        match s.phase {
            Some(BrowserPhase::Starting | BrowserPhase::Running) => {
                s.pid.ok_or(BrowserErrorOwned::NotRunning)
            }
            _ => Err(BrowserErrorOwned::NotRunning),
        }
    }

    /// Loads the page; a page plays media when the fake is set up so.
    pub(crate) fn open(&mut self, slot: u8, url: &str) -> Result<(), BrowserErrorOwned> {
        let pid = self.running_pid(slot)?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(BrowserErrorOwned::UrlNotHttp);
        }
        let s = self.slot_mut(slot).expect("running_pid found the slot");
        s.url = Some(url.to_owned());
        let (window, player) = (s.window, s.player);
        let title = self.page_title(slot);
        if let Some(w) = window.and_then(|id| self.window_mut(id)) {
            w.title.clone_from(&title);
            let id = w.id;
            self.window_changed(id);
        }
        self.mark(Mark::BrowserState(slot));
        if !self.pages_play {
            return Ok(());
        }
        let page = title.trim_end_matches(" — Mozilla Firefox").to_owned();
        if let Some((id, p)) = player.and_then(|id| self.players.get_mut(&id).map(|p| (id, p))) {
            p.title = page;
            p.length_us = PAGE_MEDIA_LENGTH_US;
            p.set(self.now_us, PlaybackStatus::Playing, 0);
            self.mark_player(id);
        } else {
            let id = self.add_player(
                "Mozilla Firefox",
                PlayerSourceOwned::Browser(BrowserSlot(slot)),
                pid,
                &page,
                None,
                PAGE_MEDIA_LENGTH_US,
                PlaybackStatus::Playing,
            );
            if let Some(s) = self.slot_mut(slot) {
                s.player = Some(id);
            }
            self.mark(Mark::BrowserState(slot));
            self.mark(Mark::BrowserPlayback(slot));
        }
        Ok(())
    }

    pub(crate) fn stop_browser(&mut self, slot: u8) -> Result<(), BrowserErrorOwned> {
        self.running_pid(slot)?;
        if let Some(id) = self.slot(slot).and_then(|s| s.window) {
            self.windows.retain(|w| w.id != id);
            self.event(EventOwned::WindowClosed(WindowId(id)));
            if self.focused == Some(id) {
                self.set_focus(None);
            }
            self.mark(Mark::Pointer);
        }
        self.end_browser(slot, StopReason::Stopped);
        Ok(())
    }

    /// The slot's Firefox is gone: its player too.
    fn end_browser(&mut self, slot: u8, reason: StopReason) {
        let player = self.slot(slot).and_then(|s| s.player);
        if let Some(id) = player {
            self.remove_player(id);
        }
        if let Some(s) = self.slot_mut(slot) {
            *s = Slot::default();
        }
        self.event(EventOwned::BrowserStopped {
            slot: BrowserSlot(slot),
            reason,
        });
        self.mark(Mark::BrowserState(slot));
        self.mark(Mark::BrowserPlayback(slot));
    }

    pub(crate) fn show_browser(
        &mut self,
        slot: u8,
        fullscreen: Option<bool>,
    ) -> Result<(), BrowserErrorOwned> {
        self.running_pid(slot)?;
        let id = (self.slot(slot).and_then(|s| s.window)).ok_or(BrowserErrorOwned::NoWindow)?;
        if self.window(id).is_none() {
            return Err(BrowserErrorOwned::NoWindow);
        }
        self.show_window(id, fullscreen)
            .expect("the window was just found");
        Ok(())
    }

    // ---- players ----

    #[expect(
        clippy::too_many_arguments,
        reason = "a player's fields, called from two places"
    )]
    fn add_player(
        &mut self,
        identity: &str,
        source: PlayerSourceOwned,
        pid: u32,
        title: &str,
        artist: Option<&str>,
        length_us: u64,
        status: PlaybackStatus,
    ) -> u32 {
        let id = self.next_player_id;
        self.next_player_id += 1;
        let player = Player {
            identity: identity.to_owned(),
            source,
            pid,
            status,
            title: title.to_owned(),
            artist: artist.map(str::to_owned),
            length_us,
            position_us: 0,
            since_us: self.now_us,
            track: 1,
        };
        let info = Self::player_info(id, &player);
        self.players.insert(id, player);
        self.event(EventOwned::PlayerAppeared(info));
        id
    }

    fn remove_player(&mut self, id: u32) {
        if self.players.remove(&id).is_none() {
            return;
        }
        for s in &mut self.slots {
            if s.player == Some(id) {
                s.player = None;
            }
        }
        self.event(EventOwned::PlayerGone(PlayerId(id)));
        self.mark(Mark::PlayerGone(id));
    }

    fn player_info(id: u32, p: &Player) -> PlayerInfoOwned {
        PlayerInfoOwned {
            id: PlayerId(id),
            identity: p.identity.clone(),
            source: p.source.clone(),
        }
    }

    pub(crate) fn player_ids(&self) -> Vec<u32> {
        self.players.keys().copied().collect()
    }

    pub(crate) fn players(&self) -> Vec<PlayerInfoOwned> {
        (self.players.iter())
            .map(|(id, p)| Self::player_info(*id, p))
            .collect()
    }

    pub(crate) fn player_source(&self, id: u32) -> Option<PlayerSourceOwned> {
        self.players.get(&id).map(|p| p.source.clone())
    }

    pub(crate) fn slot_player(&self, slot: u8) -> Option<u32> {
        self.slot(slot).and_then(|s| s.player)
    }

    pub(crate) fn playback(&self, id: Option<u32>) -> PlaybackStateOwned {
        match id.and_then(|id| self.players.get(&id)) {
            Some(p) => p.state(self.now_us),
            None => PlaybackStateOwned {
                status: PlaybackStatus::Stopped,
                title: None,
                artist: None,
                length: None,
                position: Fixed::new(0),
                rate: Fixed::new(1000),
                can_seek: false,
                can_pause: false,
            },
        }
    }

    fn mark_player(&mut self, id: u32) {
        self.mark(Mark::PlayerPlayback(id));
        if let Some(PlayerSourceOwned::Browser(BrowserSlot(slot))) = self.player_source(id) {
            self.mark(Mark::BrowserPlayback(slot));
        }
    }

    pub(crate) fn act(&mut self, id: u32, action: Action) -> Result<(), MediaErrorOwned> {
        let now = self.now_us;
        let p = self.players.get_mut(&id).ok_or(MediaErrorOwned::NoPlayer)?;
        let at = p.position(now);
        match action {
            Action::Play => p.set(now, PlaybackStatus::Playing, at),
            Action::Pause => p.set(now, PlaybackStatus::Paused, at),
            Action::PlayPause => {
                let status = match p.status {
                    PlaybackStatus::Playing => PlaybackStatus::Paused,
                    _ => PlaybackStatus::Playing,
                };
                p.set(now, status, at);
            }
            Action::Stop => p.set(now, PlaybackStatus::Stopped, 0),
            Action::Next | Action::Previous => {
                p.track = match action {
                    Action::Next => p.track.saturating_add(1),
                    _ => p.track.saturating_sub(1).max(1),
                };
                let base = p.title.split(" #").next().unwrap_or_default().to_owned();
                p.title = format!("{base} #{}", p.track);
                let status = p.status;
                p.set(now, status, 0);
            }
            Action::Seek(by_us) => {
                let to = at.saturating_add_signed(by_us);
                let status = p.status;
                p.set(now, status, to);
            }
            // As MPRIS: a position past the track is ignored.
            Action::SetPosition(to) if to > p.length_us => return Ok(()),
            Action::SetPosition(to) => {
                let status = p.status;
                p.set(now, status, to);
            }
        }
        self.mark_player(id);
        Ok(())
    }

    // ---- audio ----

    pub(crate) fn set_audio(&mut self, volume: u16, muted: bool) {
        self.volume = volume;
        self.muted = muted;
        self.mark(Mark::Audio);
    }

    pub(crate) fn set_sink(&mut self, sink: Option<Sink>) {
        self.sink = sink;
        self.mark(Mark::Audio);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model::new(&Setup {
            slots: 1,
            start_delay_us: 0,
            pages_play: true,
        })
    }

    #[test]
    fn an_overflowing_outbox_resyncs() {
        let mut m = model();
        for _ in 0..=OUTBOX_LEN_MAX {
            m.set_audio(50, false);
        }
        let (outs, behind) = m.drain();
        assert!(behind, "the client lists again");
        assert!(outs.is_empty(), "what was lost is listed again: {outs:?}");
        m.set_audio(51, false);
        assert_eq!(m.drain(), (vec![Out::Mark(Mark::Audio)], false));
    }

    #[test]
    fn a_track_ends_by_itself() {
        let mut m = model();
        m.start_browser(0, true).unwrap();
        m.open(0, "https://example.org/").unwrap();
        let id = m.slot_player(0).expect("the page plays");
        assert_eq!(m.next_deadline(), Some(PAGE_MEDIA_LENGTH_US));
        m.tick(PAGE_MEDIA_LENGTH_US + 1);
        let state = m.playback(Some(id));
        assert_eq!(state.status, PlaybackStatus::Stopped);
        assert_eq!(state.position.get(), PAGE_MEDIA_LENGTH_US);
        assert_eq!(m.next_deadline(), None);
    }
}
