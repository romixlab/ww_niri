//! Handlers of `ww_niri_api::WwNiri`, one [Server] per connection. Started from `ww api scaffold
//! ww_niri_api --alloc --use-async`; handlers not done yet answer `Unimplemented`.

use std::net::SocketAddr;
use std::rc::Rc;

use wire_weaver::prelude::*;

use super::host::{call, Run, WwHost};
use super::hub::Hub;
use super::players::Action;
use super::{Audio, Browsers, Players, Services};

pub struct Server {
    /// Observed properties: what the client observes and when it is due.
    pub observe: api_impl::ObserveState,
    pub run: Run,
    pub peer: SocketAddr,
    pub hub: Rc<Hub>,
    pub browsers: Rc<Browsers>,
    pub players: Rc<Players>,
    pub audio: Rc<Audio>,
    /// The live players as `valid_indices` last answered them.
    player_ids: Vec<UNib32>,
    /// The client opened the `events` stream.
    pub events_open: bool,
}

impl Server {
    pub fn new(run: Run, peer: SocketAddr, services: Rc<Services>) -> Self {
        Self {
            observe: api_impl::ObserveState::new(Default::default()),
            run,
            peer,
            hub: services.hub.clone(),
            browsers: services.browsers.clone(),
            players: services.players.clone(),
            audio: services.audio.clone(),
            player_ids: Vec::new(),
            events_open: false,
        }
    }

    async fn window_op(
        &self,
        f: impl FnOnce(&mut dyn WwHost) -> bool + 'static,
    ) -> RpcResult<Result<(), ww_niri_api::WindowError>> {
        match call(&self.run, f).await {
            Some(true) => RpcResult::Ready(Ok(())),
            Some(false) => RpcResult::Ready(Err(ww_niri_api::WindowError::NoSuchWindow)),
            None => Unimplemented.into(),
        }
    }

    /// The slot's window, found by its Firefox's pid.
    async fn browser_window(&self, slot: u8) -> Option<ww_niri_api::WindowOwned> {
        let pid = self.browsers.pid(slot)?;
        call(&self.run, move |host| {
            host.windows().into_iter().find(|w| w.pid == Some(pid))
        })
        .await
        .flatten()
    }

    /// A browser slot's current player: the one of its Firefox process.
    fn browser_player(&self, slot: u8) -> Result<u32, ww_niri_api::MediaErrorOwned> {
        self.browsers
            .pid(slot)
            .and_then(|pid| self.players.of_pid(pid))
            .map(|id| id.0)
            .ok_or(ww_niri_api::MediaErrorOwned::NoPlayer)
    }

    async fn browser_act(
        &self,
        index_chain: [UNib32; 1],
        action: Action,
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        let res = match self.browser_player(index_chain[0].0 as u8) {
            // Gone in between: as if it had never played.
            Ok(id) => (self.players.act(id, action).await)
                .unwrap_or(Err(ww_niri_api::MediaErrorOwned::NoPlayer)),
            Err(err) => Err(err),
        };
        RpcResult::Ready(res)
    }

    async fn player_act(
        &self,
        index_chain: [UNib32; 1],
        action: Action,
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        // A player gone since the index check: as if it never played.
        RpcResult::Ready(
            (self.players.act(index_chain[0].0, action).await)
                .unwrap_or(Err(ww_niri_api::MediaErrorOwned::NoPlayer)),
        )
    }

    /// Marks a browser slot's window.
    pub fn mark_browser(&self, window: &mut ww_niri_api::WindowOwned) {
        window.browser = window.pid.and_then(|pid| self.browsers.slot_of_pid(pid));
    }

    // ---- ww_niri_api::WwNiri at root ----

    /// niri version, ww_niri version, hostname.
    async fn info(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> RpcResult<ww_niri_api::InfoOwned> {
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        RpcResult::Ready(ww_niri_api::InfoOwned {
            niri_version: crate::utils::version(),
            ww_niri_version: env!("CARGO_PKG_VERSION").to_owned(),
            hostname: hostname.trim().to_owned(),
        })
    }

    /// Valid indices of `browsers[]`.
    fn valid_indices_root_browsers(&mut self) -> ValidIndices<'_> {
        ValidIndices::range_u32(0..u32::from(self.browsers.count()))
    }

    /// Windows, outputs, browsers and players coming and going, in order. After
    /// [Event::Resync] the client lists again.
    ///
    /// Opens the stream (a new session): `Ok` / `Ready` starts it.
    async fn events(&mut self, _cx: &mut Context<'_, impl EventOut>) -> RpcResult<()> {
        self.events_open = true;
        RpcResult::Ready(())
    }

    /// The host closed the stream.
    async fn close_events(&mut self, _cx: &mut Context<'_, impl EventOut>) {
        self.events_open = false;
    }

    async fn sideband_events(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _sideband: ww_client_server::StreamSideband,
    ) -> Option<ww_client_server::StreamSideband> {
        // TODO: None sends no reply
        None
    }

    // ---- ww_niri_api::Windows at windows ----

    async fn windows_list(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> RpcResult<Vec<ww_niri_api::WindowOwned>> {
        match call(&self.run, |host| host.windows()).await {
            Some(mut windows) => {
                windows.iter_mut().for_each(|w| self.mark_browser(w));
                RpcResult::Ready(windows)
            }
            None => Unimplemented.into(),
        }
    }

    /// Brings the window on screen: switches to its workspace and output, scrolls to its column
    /// and focuses it. `transition: None` uses niri's configured animations; `fullscreen` sets
    /// fullscreen on the way when given.
    async fn windows_show(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        id: ww_niri_api::WindowId,
        transition: Option<ww_niri_api::Transition>,
        fullscreen: Option<bool>,
    ) -> RpcResult<Result<(), ww_niri_api::WindowError>> {
        self.window_op(move |host| host.show_window(id.0, transition, fullscreen))
            .await
    }

    /// Keyboard focus only, as niri's `focus-window`.
    async fn windows_focus(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        id: ww_niri_api::WindowId,
    ) -> RpcResult<Result<(), ww_niri_api::WindowError>> {
        self.window_op(move |host| host.focus(id.0)).await
    }

    /// Asks the app to close the window; the app may refuse or ask first.
    async fn windows_close(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        id: ww_niri_api::WindowId,
    ) -> RpcResult<Result<(), ww_niri_api::WindowError>> {
        self.window_op(move |host| host.close_window(id.0)).await
    }

    async fn windows_set_fullscreen(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        id: ww_niri_api::WindowId,
        on: bool,
    ) -> RpcResult<Result<(), ww_niri_api::WindowError>> {
        self.window_op(move |host| host.set_fullscreen(id.0, on))
            .await
    }

    // ---- ww_niri_api::Outputs at outputs ----

    async fn outputs_list(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> RpcResult<Vec<ww_niri_api::OutputOwned>> {
        match call(&self.run, |host| host.outputs()).await {
            Some(outputs) => RpcResult::Ready(outputs),
            None => Unimplemented.into(),
        }
    }

    // ---- ww_niri_api::Apps at apps ----

    /// Runs `argv` as the compositor's user, as niri's `spawn` does: no shell, `argv[0]` looked
    /// up in PATH. Any command is allowed.
    async fn apps_launch(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        argv: Vec<String>,
    ) -> RpcResult<Result<ww_niri_api::Launched, ww_niri_api::LaunchErrorOwned>> {
        use ww_niri_api::{LaunchErrorOwned, Launched};

        if argv.is_empty() {
            return RpcResult::Ready(Err(LaunchErrorOwned::EmptyArgv));
        }
        // Any command runs: every launch is on record with who asked.
        info!("ww: {} launches {argv:?}", self.peer);

        // Spawning blocks for some milliseconds: off the loop.
        let (tx, rx) = async_channel::bounded(1);
        let spawned = std::thread::Builder::new()
            .name("ww launch".to_owned())
            .spawn(move || {
                let _ = tx.try_send(crate::utils::spawning::spawn_with_pid(&argv));
            });
        let res = match spawned {
            Ok(_) => rx
                .recv()
                .await
                .unwrap_or_else(|_| Err(std::io::ErrorKind::Other.into())),
            Err(err) => Err(err),
        };
        RpcResult::Ready(match res {
            Ok(Some(pid)) => Ok(Launched { pid }),
            // The `ww` feature enables `systemd`, whose spawn hands the pid back.
            Ok(None) => Err(LaunchErrorOwned::Spawn("pid unknown".to_owned())),
            Err(err) => {
                debug!("ww: launch for {} failed: {err}", self.peer);
                Err(LaunchErrorOwned::Spawn(err.to_string()))
            }
        })
    }

    // ---- ww_niri_api::Browser at browsers[] ----

    /// Launches the slot's Firefox (kiosk, its profile, `about:blank`) and returns when it can be
    /// driven.
    async fn browsers_start(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
        fullscreen: bool,
    ) -> RpcResult<Result<(), ww_niri_api::BrowserErrorOwned>> {
        let slot = index_chain[0].0 as u8;
        RpcResult::Ready(self.browsers.start(slot, fullscreen).await)
    }

    /// Loads `url` (http or https) in the slot's tab and returns when the navigation is
    /// committed.
    async fn browsers_open(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
        url: String,
    ) -> RpcResult<Result<(), ww_niri_api::BrowserErrorOwned>> {
        let slot = index_chain[0].0 as u8;
        RpcResult::Ready(self.browsers.open(slot, &url).await)
    }

    /// Ends the slot's Firefox; its profile stays.
    async fn browsers_stop(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::BrowserErrorOwned>> {
        RpcResult::Ready(self.browsers.stop(index_chain[0].0 as u8).await)
    }

    /// Shows the slot's window, as [Windows::show].
    async fn browsers_show(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
        transition: Option<ww_niri_api::Transition>,
        fullscreen: Option<bool>,
    ) -> RpcResult<Result<(), ww_niri_api::BrowserErrorOwned>> {
        use ww_niri_api::BrowserErrorOwned;

        let slot = index_chain[0].0 as u8;
        if self.browsers.pid(slot).is_none() {
            return RpcResult::Ready(Err(BrowserErrorOwned::NotRunning));
        }
        let Some(window) = self.browser_window(slot).await else {
            return RpcResult::Ready(Err(BrowserErrorOwned::NoWindow));
        };
        let id = window.id.0;
        match call(&self.run, move |host| {
            host.show_window(id, transition, fullscreen)
        })
        .await
        {
            Some(true) => RpcResult::Ready(Ok(())),
            // Closed in between.
            Some(false) => RpcResult::Ready(Err(BrowserErrorOwned::NoWindow)),
            None => Unimplemented.into(),
        }
    }

    /// Phase, URL, window and current player of the slot.
    async fn get_browsers_state(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> GetResult<ww_niri_api::BrowserStateOwned, ()> {
        let slot = index_chain[0].0 as u8;
        let window = self.browser_window(slot).await;
        let mut state = self.browsers.state(slot, window.as_ref());
        state.player = self.browser_player(slot).ok().map(ww_niri_api::PlayerId);
        Value(state)
    }

    // ---- ww_niri_api::Playback at browsers[].playback ----

    /// Status, track, position and capabilities.
    async fn get_browsers_playback_state(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> GetResult<ww_niri_api::PlaybackStateOwned, ()> {
        let state = match self.browser_player(index_chain[0].0 as u8) {
            Ok(id) => self.players.state(id).await.and_then(Result::ok),
            Err(_) => None,
        };
        // Nothing playing: stopped, no track.
        Value(state.unwrap_or_else(stopped))
    }

    async fn browsers_playback_play(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.browser_act(index_chain, Action::Play).await
    }

    async fn browsers_playback_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.browser_act(index_chain, Action::Pause).await
    }

    async fn browsers_playback_play_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.browser_act(index_chain, Action::PlayPause).await
    }

    async fn browsers_playback_stop(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.browser_act(index_chain, Action::Stop).await
    }

    async fn browsers_playback_next(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.browser_act(index_chain, Action::Next).await
    }

    async fn browsers_playback_previous(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.browser_act(index_chain, Action::Previous).await
    }

    /// Relative, MPRIS `Seek`; negative goes back.
    async fn browsers_playback_seek(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
        by: exact_units::Fixed<
            exact_units::units::Second,
            exact_units::prefix::Micro,
            i64,
            exact_units::Unbounded,
        >,
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.browser_act(index_chain, Action::Seek(by.get())).await
    }

    /// Absolute, MPRIS `SetPosition` on the current track.
    async fn browsers_playback_set_position(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
        at: exact_units::Fixed<
            exact_units::units::Second,
            exact_units::prefix::Micro,
            u64,
            exact_units::Unbounded,
        >,
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.browser_act(index_chain, Action::SetPosition(at.get()))
            .await
    }

    // ---- ww_niri_api::Media at media ----

    /// Valid indices of `media.players[]`.
    fn valid_indices_root_media_players(&mut self) -> ValidIndices<'_> {
        self.player_ids = self.players.ids().into_iter().map(UNib32).collect();
        ValidIndices::List(RefVec::Slice {
            slice: &self.player_ids,
        })
    }

    async fn media_list(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> RpcResult<Vec<ww_niri_api::PlayerInfoOwned>> {
        RpcResult::Ready(self.players.list().await)
    }

    // ---- ww_niri_api::Player at media.players[] ----

    /// Where the player comes from: a browser slot, or a window or app.
    async fn get_media_players_source(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> GetResult<ww_niri_api::PlayerSourceOwned, ()> {
        let source = self.players.source_of(index_chain[0].0).await;
        Value(source.unwrap_or(ww_niri_api::PlayerSourceOwned::App { pid: None }))
    }

    // ---- ww_niri_api::Playback at media.players[].playback ----

    /// Status, track, position and capabilities.
    async fn get_media_players_playback_state(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> GetResult<ww_niri_api::PlaybackStateOwned, ()> {
        let state = self.players.state(index_chain[0].0).await;
        Value(state.and_then(Result::ok).unwrap_or_else(stopped))
    }

    async fn media_players_playback_play(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.player_act(index_chain, Action::Play).await
    }

    async fn media_players_playback_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.player_act(index_chain, Action::Pause).await
    }

    async fn media_players_playback_play_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.player_act(index_chain, Action::PlayPause).await
    }

    async fn media_players_playback_stop(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.player_act(index_chain, Action::Stop).await
    }

    async fn media_players_playback_next(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.player_act(index_chain, Action::Next).await
    }

    async fn media_players_playback_previous(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.player_act(index_chain, Action::Previous).await
    }

    /// Relative, MPRIS `Seek`; negative goes back.
    async fn media_players_playback_seek(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
        by: exact_units::Fixed<
            exact_units::units::Second,
            exact_units::prefix::Micro,
            i64,
            exact_units::Unbounded,
        >,
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.player_act(index_chain, Action::Seek(by.get())).await
    }

    /// Absolute, MPRIS `SetPosition` on the current track.
    async fn media_players_playback_set_position(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        index_chain: [UNib32; 1],
        at: exact_units::Fixed<
            exact_units::units::Second,
            exact_units::prefix::Micro,
            u64,
            exact_units::Unbounded,
        >,
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        self.player_act(index_chain, Action::SetPosition(at.get()))
            .await
    }

    // ---- ww_niri_api::Audio at audio ----

    /// The default sink's volume, up to 150 %.
    async fn set_audio_volume(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        value: ww_niri_api::Volume,
    ) -> SetResult<()> {
        // observers get it after `SetResult::Set`: the generated code calls `mark_changed_audio_volume()`
        audio_set(self.audio.set_volume(value.get()).await)
    }

    /// The default sink's volume, up to 150 %.
    async fn get_audio_volume(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> GetResult<ww_niri_api::Volume, ()> {
        match self.audio.sink() {
            Some(sink) => ww_niri_api::Volume::try_new(sink.volume)
                .map_or(GetResult::GetError(()), GetResult::Value),
            None => GetResult::GetError(()),
        }
    }

    async fn set_audio_muted(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        value: bool,
    ) -> SetResult<()> {
        // observers get it after `SetResult::Set`: the generated code calls `mark_changed_audio_muted()`
        audio_set(self.audio.set_muted(value).await)
    }

    async fn get_audio_muted(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> GetResult<bool, ()> {
        match self.audio.sink() {
            Some(sink) => GetResult::Value(sink.muted),
            None => GetResult::GetError(()),
        }
    }

    /// The default sink, to show where the sound goes.
    async fn get_audio_sink(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> GetResult<ww_niri_api::SinkOwned, ()> {
        match self.audio.sink() {
            Some(sink) => GetResult::Value(ww_niri_api::SinkOwned {
                name: sink.name,
                description: sink.description,
            }),
            None => GetResult::GetError(()),
        }
    }

    /// Settings and send state of observed properties, for `Observe` requests and `poll_properties`.
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
        self.process_request_bytes(data, scratch, out, medium).await
    }

    fn version(&self) -> FullVersion<'_> {
        ww_niri_api::WW_NIRI_FULL_GID
    }
}

pub mod api_impl {
    wire_weaver::ww_codegen!(
        ww_niri_api :: WwNiri for super::Server,
        server = true, no_alloc = false, use_async = true,
        method_model = "_=immediate",
        property_model = "_=get_set",
    );
}

/// The audio errors carry no detail on the wire (the properties' error type is `()`): log it here.
fn audio_set(res: Result<(), super::audio::AudioError>) -> SetResult<()> {
    match res {
        Ok(()) => SetResult::Set,
        Err(err) => {
            debug!("ww: error setting audio: {err}");
            SetResult::SetError(())
        }
    }
}

/// The state of a player that plays nothing.
fn stopped() -> ww_niri_api::PlaybackStateOwned {
    ww_niri_api::PlaybackStateOwned {
        status: ww_niri_api::PlaybackStatus::Stopped,
        title: None,
        artist: None,
        length: None,
        position: exact_units::Fixed::new(0),
        rate: exact_units::Fixed::new(1000),
        can_seek: false,
        can_pause: false,
    }
}
