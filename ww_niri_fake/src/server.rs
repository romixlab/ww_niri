//! Handlers of `ww_niri_api::WwNiri` on the fake's model, one [Server] per connection.

use std::sync::Arc;

use exact_units::prefix::{Micro, Milli};
use exact_units::units::Second;
use exact_units::{Fixed, Unbounded};
use wire_weaver::prelude::*;
use ww_client_server::ResourceIndex;
use ww_niri_api::{
    BrowserErrorOwned, BrowserStateOwned, Button, CardOwned, CoverError, InfoOwned,
    InputErrorOwned, LaunchErrorOwned, Launched, MediaErrorOwned, Modifiers, OutputOwned,
    PlaybackStateOwned, PlayerInfoOwned, PlayerSourceOwned, Point, PointerPosition, Px, SinkOwned,
    Transition, WindowError, WindowId, WindowOwned,
};

use crate::model::{Action, Mark, Model};
use crate::record::{Call, Fail};
use crate::Shared;

type Res<'a, E> = RpcResult<Result<(), Error<'a, E>>>;
type Micros<T> = Fixed<Second, Micro, T, Unbounded>;
type Millis<T> = Fixed<Second, Milli, T, Unbounded>;

pub(crate) struct Server {
    /// Observed properties: what the client observes and when it is due.
    pub(crate) observe: api_impl::ObserveState,
    shared: Arc<Shared>,
    /// The live players as `valid_indices` last answered them.
    player_ids: Vec<UNib32>,
    /// The client opened the `events` stream.
    pub(crate) events_open: bool,
}

/// The API error a [Fail] stands for, when it is of this error's kind.
trait FromFail: Sized {
    fn from_fail(fail: Fail) -> Option<Self>;
}

macro_rules! from_fail {
    ($($err:ty => $variant:ident),* $(,)?) => {$(
        impl FromFail for $err {
            fn from_fail(fail: Fail) -> Option<Self> {
                match fail {
                    Fail::$variant(err) => Some(err),
                    _ => None,
                }
            }
        }
    )*};
}

from_fail!(
    WindowError => Window,
    CoverError => Cover,
    BrowserErrorOwned => Browser,
    MediaErrorOwned => Media,
    InputErrorOwned => Input,
);

/// The index of `browsers[]` or `media.players[]`; the generated code checked it.
fn index(chain: [ResourceIndex; 1]) -> u32 {
    chain[0].get().0
}

fn slot(chain: [ResourceIndex; 1]) -> u8 {
    u8::try_from(index(chain)).expect("valid_indices answers slots that fit a u8")
}

fn refused(reason: &'static str) -> CommonError<'static> {
    CommonError::Refused {
        reason: MaxLen::new_unchecked(reason),
    }
}

impl Server {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self {
            observe: api_impl::ObserveState::new(Default::default()),
            shared,
            player_ids: Vec::new(),
            events_open: false,
        }
    }

    /// Records the call; the error a test asked this method to answer with.
    fn begin(&self, method: &'static str, index: Option<u32>, args: String) -> Option<Fail> {
        let mut record = self.shared.record();
        record.call(Call {
            method,
            index,
            args,
        });
        record.take_fail(method)
    }

    fn with<R>(&self, f: impl FnOnce(&mut Model) -> R) -> R {
        self.shared.with(f)
    }

    /// Sends an observed change to the client's observers.
    pub(crate) fn mark(&mut self, mark: Mark) {
        let i = |n: u32| [UNib32(n).into()];
        match mark {
            Mark::BrowserState(s) => self.mark_changed_browsers_state(i(u32::from(s))),
            Mark::BrowserPlayback(s) => {
                self.mark_changed_browsers_playback_state(i(u32::from(s)));
            }
            Mark::PlayerPlayback(id) => self.mark_changed_media_players_playback_state(i(id)),
            Mark::PlayerGone(id) => self.element_removed_media_players(i(id)),
            Mark::Audio => {
                self.mark_changed_audio_volume();
                self.mark_changed_audio_muted();
                self.mark_changed_audio_sink();
            }
            Mark::Pointer => self.mark_changed_input_pointer_position(),
        }
    }

    /// After missed events: every observed property may have changed.
    pub(crate) fn mark_all(&mut self) {
        let (slots, players) = self.with(|m| (m.slot_count(), m.player_ids()));
        for s in 0..slots {
            self.mark(Mark::BrowserState(s));
            self.mark(Mark::BrowserPlayback(s));
        }
        for id in players {
            self.mark(Mark::PlayerPlayback(id));
        }
        self.mark(Mark::Audio);
        self.mark(Mark::Pointer);
    }

    /// Records the call, then answers the error a test asked for or runs `f` on the model.
    fn op<E: FromFail>(
        &self,
        method: &'static str,
        index: Option<u32>,
        args: String,
        f: impl FnOnce(&mut Model) -> Result<(), E>,
    ) -> Res<'_, E>
    where
        for<'a> Error<'a, E>: From<E>,
    {
        let res = match self.begin(method, index, args).and_then(E::from_fail) {
            Some(err) => Err(err),
            None => self.with(f),
        };
        RpcResult::Ready(res.map_err(Into::into))
    }

    /// A player action on the slot's current player.
    fn browser_act(
        &self,
        method: &'static str,
        chain: [ResourceIndex; 1],
        args: String,
        action: Action,
    ) -> Res<'_, MediaErrorOwned> {
        self.op(method, Some(index(chain)), args, |m| {
            let id = m
                .slot_player(slot(chain))
                .ok_or(MediaErrorOwned::NoPlayer)?;
            m.act(id, action)
        })
    }

    fn player_act(
        &self,
        method: &'static str,
        chain: [ResourceIndex; 1],
        args: String,
        action: Action,
    ) -> Res<'_, MediaErrorOwned> {
        self.op(method, Some(index(chain)), args, |m| {
            m.act(index(chain), action)
        })
    }

    /// A property read: an error a test asked for, else the value.
    fn get<T>(
        &self,
        property: &'static str,
        value: impl FnOnce(&mut Model) -> T,
    ) -> GetResult<T, CommonError<'_>> {
        match self.shared.record().take_fail(property) {
            Some(Fail::Property(reason)) => GetResult::GetError(refused(reason)),
            _ => GetResult::Value(self.with(value)),
        }
    }

    fn set(
        &self,
        property: &'static str,
        args: String,
        f: impl FnOnce(&mut Model),
    ) -> SetResult<CommonError<'_>> {
        if let Some(Fail::Property(reason)) = self.begin(property, None, args) {
            return SetResult::SetError(refused(reason));
        }
        self.with(f);
        SetResult::Set
    }

    // ---- ww_niri_api::WwNiri at root ----

    async fn info(&mut self, _cx: &mut Context<'_, impl EventOut>) -> RpcResult<InfoOwned> {
        self.begin("info", None, "()".to_owned());
        RpcResult::Ready(self.shared.info.clone())
    }

    fn valid_indices_root_browsers(&mut self) -> ValidIndices<'_> {
        ValidIndices::range_u32(0..u32::from(self.with(|m| m.slot_count())))
    }

    async fn events(&mut self, _cx: &mut Context<'_, impl EventOut>) -> RpcResult<()> {
        self.begin("events", None, "()".to_owned());
        self.events_open = true;
        RpcResult::Ready(())
    }

    async fn close_events(&mut self, _cx: &mut Context<'_, impl EventOut>) {
        self.events_open = false;
    }

    async fn sideband_events(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _sideband: ww_client_server::StreamSideband,
    ) -> Option<ww_client_server::StreamSideband> {
        None
    }

    // ---- ww_niri_api::Windows at windows ----

    async fn windows_list(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> RpcResult<Vec<WindowOwned>> {
        self.begin("windows.list", None, "()".to_owned());
        RpcResult::Ready(self.with(|m| m.windows()))
    }

    async fn windows_show(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        id: WindowId,
        transition: Option<Transition>,
        fullscreen: Option<bool>,
    ) -> Res<'_, WindowError> {
        let args = format!("({id:?}, {transition:?}, {fullscreen:?})");
        self.op("windows.show", None, args, |m| {
            m.show_window(id.0, fullscreen)
        })
    }

    async fn windows_focus(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        id: WindowId,
    ) -> Res<'_, WindowError> {
        self.op("windows.focus", None, format!("({id:?})"), |m| {
            m.show_window(id.0, None)
        })
    }

    async fn windows_close(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        id: WindowId,
    ) -> Res<'_, WindowError> {
        self.op("windows.close", None, format!("({id:?})"), |m| {
            m.close_window(id.0)
        })
    }

    async fn windows_set_fullscreen(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        id: WindowId,
        on: bool,
    ) -> Res<'_, WindowError> {
        self.op(
            "windows.set_fullscreen",
            None,
            format!("({id:?}, {on})"),
            |m| m.set_fullscreen(id.0, on),
        )
    }

    async fn windows_cover(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        id: WindowId,
        card: CardOwned,
        lasts: Millis<u32>,
    ) -> Res<'_, CoverError> {
        let args = format!("({id:?}, {card:?}, {}ms)", lasts.get());
        self.op("windows.cover", None, args, |m| m.cover(id.0, card, lasts))
    }

    async fn windows_uncover(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        id: WindowId,
    ) -> Res<'_, CoverError> {
        self.op("windows.uncover", None, format!("({id:?})"), |m| {
            m.uncover(id.0)
        })
    }

    // ---- ww_niri_api::Outputs at outputs ----

    async fn outputs_list(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> RpcResult<Vec<OutputOwned>> {
        self.begin("outputs.list", None, "()".to_owned());
        RpcResult::Ready(Model::outputs())
    }

    // ---- ww_niri_api::Apps at apps ----

    async fn apps_launch(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        argv: Vec<String>,
    ) -> RpcResult<Result<Launched, Error<'_, LaunchErrorOwned>>> {
        if let Some(Fail::Launch(err)) = self.begin("apps.launch", None, format!("({argv:?})")) {
            return RpcResult::Ready(Err(err.into()));
        }
        if argv.is_empty() {
            return RpcResult::Ready(Err(LaunchErrorOwned::EmptyArgv.into()));
        }
        RpcResult::Ready(Ok(Launched {
            pid: self.with(Model::launch),
        }))
    }

    // ---- ww_niri_api::Browser at browsers[] ----

    async fn browsers_start(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
        fullscreen: bool,
    ) -> Res<'_, BrowserErrorOwned> {
        let args = format!("({fullscreen})");
        self.op("browsers.start", Some(index(chain)), args, |m| {
            m.start_browser(slot(chain), fullscreen)
        })
    }

    async fn browsers_open(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
        url: String,
    ) -> Res<'_, BrowserErrorOwned> {
        self.op(
            "browsers.open",
            Some(index(chain)),
            format!("({url:?})"),
            |m| m.open(slot(chain), &url),
        )
    }

    async fn browsers_stop(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, BrowserErrorOwned> {
        self.op("browsers.stop", Some(index(chain)), "()".to_owned(), |m| {
            m.stop_browser(slot(chain))
        })
    }

    async fn browsers_show(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
        transition: Option<Transition>,
        fullscreen: Option<bool>,
    ) -> Res<'_, BrowserErrorOwned> {
        let args = format!("({transition:?}, {fullscreen:?})");
        self.op("browsers.show", Some(index(chain)), args, |m| {
            m.show_browser(slot(chain), fullscreen)
        })
    }

    async fn browsers_cover(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
        card: CardOwned,
        lasts: Millis<u32>,
    ) -> Res<'_, CoverError> {
        let args = format!("({card:?}, {}ms)", lasts.get());
        self.op("browsers.cover", Some(index(chain)), args, |m| {
            let window = m.slot_window(slot(chain))?;
            m.cover(window, card, lasts)
        })
    }

    async fn browsers_uncover(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, CoverError> {
        self.op(
            "browsers.uncover",
            Some(index(chain)),
            "()".to_owned(),
            |m| {
                let window = m.slot_window(slot(chain))?;
                m.uncover(window)
            },
        )
    }

    async fn get_browsers_state(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> GetResult<BrowserStateOwned, CommonError<'_>> {
        self.get("browsers.state", |m| m.browser_state(slot(chain)))
    }

    // ---- ww_niri_api::Playback at browsers[].playback ----

    async fn get_browsers_playback_state(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> GetResult<PlaybackStateOwned, CommonError<'_>> {
        self.get("browsers.playback.state", |m| {
            m.playback(m.slot_player(slot(chain)))
        })
    }

    async fn browsers_playback_play(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        self.browser_act(
            "browsers.playback.play",
            chain,
            "()".to_owned(),
            Action::Play,
        )
    }

    async fn browsers_playback_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        self.browser_act(
            "browsers.playback.pause",
            chain,
            "()".to_owned(),
            Action::Pause,
        )
    }

    // spirit-level: allow(near-duplicate-fns) one handler per API method
    async fn browsers_playback_play_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        let method = "browsers.playback.play_pause";
        self.browser_act(method, chain, "()".to_owned(), Action::PlayPause)
    }

    async fn browsers_playback_stop(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        self.browser_act(
            "browsers.playback.stop",
            chain,
            "()".to_owned(),
            Action::Stop,
        )
    }

    async fn browsers_playback_next(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        self.browser_act(
            "browsers.playback.next",
            chain,
            "()".to_owned(),
            Action::Next,
        )
    }

    // spirit-level: allow(near-duplicate-fns) one handler per API method
    async fn browsers_playback_previous(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        let method = "browsers.playback.previous";
        self.browser_act(method, chain, "()".to_owned(), Action::Previous)
    }

    async fn browsers_playback_seek(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
        by: Micros<i64>,
    ) -> Res<'_, MediaErrorOwned> {
        let by = by.get();
        self.browser_act(
            "browsers.playback.seek",
            chain,
            format!("({by} us)"),
            Action::Seek(by),
        )
    }

    async fn browsers_playback_set_position(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
        at: Micros<u64>,
    ) -> Res<'_, MediaErrorOwned> {
        let at = at.get();
        let method = "browsers.playback.set_position";
        self.browser_act(method, chain, format!("({at} us)"), Action::SetPosition(at))
    }

    // ---- ww_niri_api::Media at media ----

    fn valid_indices_root_media_players(&mut self) -> ValidIndices<'_> {
        self.player_ids = self
            .with(|m| m.player_ids())
            .into_iter()
            .map(UNib32)
            .collect();
        ValidIndices::List(RefVec::Slice {
            slice: &self.player_ids,
        })
    }

    async fn media_list(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> RpcResult<Vec<PlayerInfoOwned>> {
        self.begin("media.list", None, "()".to_owned());
        RpcResult::Ready(self.with(|m| m.players()))
    }

    // ---- ww_niri_api::Player at media.players[] ----

    async fn get_media_players_source(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> GetResult<PlayerSourceOwned, CommonError<'_>> {
        // A player gone since the index check: as if its process was unknown.
        self.get("media.players.source", |m| {
            (m.player_source(index(chain))).unwrap_or(PlayerSourceOwned::App { pid: None })
        })
    }

    // ---- ww_niri_api::Playback at media.players[].playback ----

    async fn get_media_players_playback_state(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> GetResult<PlaybackStateOwned, CommonError<'_>> {
        self.get("media.players.playback.state", |m| {
            m.playback(Some(index(chain)))
        })
    }

    async fn media_players_playback_play(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        self.player_act(
            "media.players.playback.play",
            chain,
            "()".to_owned(),
            Action::Play,
        )
    }

    async fn media_players_playback_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        self.player_act(
            "media.players.playback.pause",
            chain,
            "()".to_owned(),
            Action::Pause,
        )
    }

    // spirit-level: allow(near-duplicate-fns) one handler per API method
    async fn media_players_playback_play_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        let method = "media.players.playback.play_pause";
        self.player_act(method, chain, "()".to_owned(), Action::PlayPause)
    }

    async fn media_players_playback_stop(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        self.player_act(
            "media.players.playback.stop",
            chain,
            "()".to_owned(),
            Action::Stop,
        )
    }

    async fn media_players_playback_next(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        self.player_act(
            "media.players.playback.next",
            chain,
            "()".to_owned(),
            Action::Next,
        )
    }

    // spirit-level: allow(near-duplicate-fns) one handler per API method
    async fn media_players_playback_previous(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
    ) -> Res<'_, MediaErrorOwned> {
        let method = "media.players.playback.previous";
        self.player_act(method, chain, "()".to_owned(), Action::Previous)
    }

    async fn media_players_playback_seek(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
        by: Micros<i64>,
    ) -> Res<'_, MediaErrorOwned> {
        let by = by.get();
        let method = "media.players.playback.seek";
        self.player_act(method, chain, format!("({by} us)"), Action::Seek(by))
    }

    async fn media_players_playback_set_position(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        chain: [ResourceIndex; 1],
        at: Micros<u64>,
    ) -> Res<'_, MediaErrorOwned> {
        let at = at.get();
        let method = "media.players.playback.set_position";
        self.player_act(method, chain, format!("({at} us)"), Action::SetPosition(at))
    }

    // ---- ww_niri_api::Audio at audio ----

    async fn set_audio_volume(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        value: ww_niri_api::Volume,
    ) -> SetResult<CommonError<'_>> {
        let volume = value.get();
        self.set("audio.volume", format!(" = {volume}"), |m| {
            let muted = m.muted;
            m.set_audio(volume, muted);
        })
    }

    async fn get_audio_volume(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> GetResult<ww_niri_api::Volume, CommonError<'_>> {
        match self.get("audio.volume", |m| m.volume) {
            GetResult::Value(v) => ww_niri_api::Volume::try_new(v).map_or(
                GetResult::GetError(refused("volume out of range")),
                GetResult::Value,
            ),
            GetResult::GetError(err) => GetResult::GetError(err),
            GetResult::Unimplemented => GetResult::Unimplemented,
        }
    }

    async fn set_audio_muted(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        value: bool,
    ) -> SetResult<CommonError<'_>> {
        self.set("audio.muted", format!(" = {value}"), |m| {
            let volume = m.volume;
            m.set_audio(volume, value);
        })
    }

    async fn get_audio_muted(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> GetResult<bool, CommonError<'_>> {
        self.get("audio.muted", |m| m.muted)
    }

    async fn get_audio_sink(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> GetResult<SinkOwned, CommonError<'_>> {
        match self.get("audio.sink", |m| m.sink.clone()) {
            GetResult::Value(Some(sink)) => GetResult::Value(SinkOwned {
                name: sink.name,
                description: sink.description,
            }),
            GetResult::Value(None) => GetResult::GetError(CommonError::WrongState {
                reason: MaxLen::new_unchecked("no default sink"),
            }),
            GetResult::GetError(err) => GetResult::GetError(err),
            GetResult::Unimplemented => GetResult::Unimplemented,
        }
    }

    // ---- ww_niri_api::Input at input ----

    async fn get_input_pointer_position(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> GetResult<PointerPosition, CommonError<'_>> {
        self.get("input.pointer.position", |m| m.pointer())
    }

    async fn input_pointer_move_to(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        to: Point,
    ) -> Res<'_, InputErrorOwned> {
        self.op("input.pointer.move_to", None, format!("({to:?})"), |m| {
            m.move_to(to)
        })
    }

    async fn input_pointer_move_by(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        dx: Px,
        dy: Px,
    ) -> Res<'_, InputErrorOwned> {
        let (dx, dy) = (dx.get(), dy.get());
        self.op(
            "input.pointer.move_by",
            None,
            format!("({dx}, {dy})"),
            |m| m.move_by(dx, dy),
        )
    }

    async fn input_pointer_click(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        at: Option<Point>,
        button: Button,
        count: u8,
        modifiers: Modifiers,
    ) -> Res<'_, InputErrorOwned> {
        let args = format!("({at:?}, {button:?}, {count}, {modifiers:?})");
        self.op("input.pointer.click", None, args, |m| {
            m.click(at, button, count, modifiers)
        })
    }

    async fn input_pointer_press(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        button: Button,
        hold: Option<Millis<u16>>,
    ) -> Res<'_, InputErrorOwned> {
        let hold = hold.map(exact_units::Fixed::get);
        let args = format!("({button:?}, {hold:?})");
        self.op("input.pointer.press", None, args, |m| m.press(button, hold))
    }

    async fn input_pointer_release(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        button: Button,
    ) -> Res<'_, InputErrorOwned> {
        self.op(
            "input.pointer.release",
            None,
            format!("({button:?})"),
            |m| m.release(button),
        )
    }

    async fn input_pointer_drag(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        from: Point,
        to: Point,
        button: Button,
        duration: Millis<u16>,
    ) -> Res<'_, InputErrorOwned> {
        let args = format!("({from:?}, {to:?}, {button:?}, {} ms)", duration.get());
        self.op("input.pointer.drag", None, args, |m| {
            m.drag(from, to, button)
        })
    }

    async fn input_pointer_scroll(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        at: Option<Point>,
        dx: i16,
        dy: i16,
        modifiers: Modifiers,
    ) -> Res<'_, InputErrorOwned> {
        let args = format!("({at:?}, {dx}, {dy}, {modifiers:?})");
        self.op("input.pointer.scroll", None, args, |m| {
            m.scroll(at, dx, dy, modifiers)
        })
    }

    async fn input_keyboard_type_text(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        text: String,
    ) -> Res<'_, InputErrorOwned> {
        self.op(
            "input.keyboard.type_text",
            None,
            format!("({text:?})"),
            |m| m.type_text(&text),
        )
    }

    async fn input_keyboard_press_keys(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        combo: String,
    ) -> Res<'_, InputErrorOwned> {
        let args = format!("({combo:?})");
        self.op("input.keyboard.press_keys", None, args, |m| {
            m.press_keys(&combo)
        })
    }

    async fn input_keyboard_key_down(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        key: String,
        hold: Option<Millis<u16>>,
    ) -> Res<'_, InputErrorOwned> {
        let hold = hold.map(exact_units::Fixed::get);
        let args = format!("({key:?}, {hold:?})");
        self.op("input.keyboard.key_down", None, args, |m| {
            m.key_down(&key, hold)
        })
    }

    async fn input_keyboard_key_up(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        key: String,
    ) -> Res<'_, InputErrorOwned> {
        self.op("input.keyboard.key_up", None, format!("({key:?})"), |m| {
            m.key_up(&key)
        })
    }

    /// Settings and send state of observed properties, for `Observe` requests and
    /// `poll_properties`.
    fn observe_state(&mut self) -> &mut api_impl::ObserveState {
        &mut self.observe
    }
}

impl WireWeaverAsyncApiBackend for Server {
    type Medium = ();

    async fn process_bytes<'a>(
        &mut self,
        out: &mut EventWriter<'_, impl MessageSink>,
        medium: (),
        data: &[u8],
        scratch: &'a mut [u8],
    ) -> Result<&'a [u8], ShrinkWrapError> {
        let latency = self.shared.latency();
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
        self.process_request_bytes(data, scratch, out, medium).await
    }

    fn version(&self) -> FullVersion<'_> {
        ww_niri_api::WW_NIRI_FULL_GID
    }
}

pub(crate) mod api_impl {
    wire_weaver::ww_codegen!(
        ww_niri_api :: WwNiri for super::Server,
        server = true, no_alloc = false, use_async = true,
        method_model = "_=immediate",
        property_model = "_=get_set",
    );
}
