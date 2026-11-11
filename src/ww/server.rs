//! Handlers of `ww_niri_api::WwNiri`, one [Server] per connection. Started from `ww api scaffold
//! ww_niri_api --alloc --use-async`; handlers not done yet answer `Unimplemented`.

use wire_weaver::prelude::*;

use super::host::{call, Run, WwHost};

pub struct Server {
    /// Observed properties: what the client observes and when it is due.
    pub observe: api_impl::ObserveState,
    pub run: Run,
    /// The client opened the `events` stream.
    pub events_open: bool,
}

impl Server {
    pub fn new(run: Run) -> Self {
        Self {
            observe: api_impl::ObserveState::new(Default::default()),
            run,
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
        // TODO: no index is valid until implemented
        ValidIndices::range_u32(0..0)
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
            Some(windows) => RpcResult::Ready(windows),
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
        _argv: Vec<String>,
    ) -> RpcResult<Result<ww_niri_api::Launched, ww_niri_api::LaunchErrorOwned>> {
        Unimplemented.into()
    }

    // ---- ww_niri_api::Browser at browsers[] ----

    /// Launches the slot's Firefox (kiosk, its profile, `about:blank`) and returns when it can be
    /// driven.
    async fn browsers_start(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
        _fullscreen: bool,
    ) -> RpcResult<Result<(), ww_niri_api::BrowserErrorOwned>> {
        Unimplemented.into()
    }

    /// Loads `url` (http or https) in the slot's tab and returns when the navigation is
    /// committed.
    async fn browsers_open(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
        _url: String,
    ) -> RpcResult<Result<(), ww_niri_api::BrowserErrorOwned>> {
        Unimplemented.into()
    }

    /// Ends the slot's Firefox; its profile stays.
    async fn browsers_stop(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::BrowserErrorOwned>> {
        Unimplemented.into()
    }

    /// Shows the slot's window, as [Windows::show].
    async fn browsers_show(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
        _transition: Option<ww_niri_api::Transition>,
        _fullscreen: Option<bool>,
    ) -> RpcResult<Result<(), ww_niri_api::BrowserErrorOwned>> {
        Unimplemented.into()
    }

    /// Phase, URL, window and current player of the slot.
    async fn get_browsers_state(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> GetResult<ww_niri_api::BrowserStateOwned, ()> {
        Unimplemented.into()
    }

    // ---- ww_niri_api::Playback at browsers[].playback ----

    /// Status, track, position and capabilities.
    async fn get_browsers_playback_state(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> GetResult<ww_niri_api::PlaybackStateOwned, ()> {
        Unimplemented.into()
    }

    async fn browsers_playback_play(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    async fn browsers_playback_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    async fn browsers_playback_play_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    async fn browsers_playback_stop(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    async fn browsers_playback_next(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    async fn browsers_playback_previous(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    /// Relative, MPRIS `Seek`; negative goes back.
    async fn browsers_playback_seek(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
        _by: exact_units::Fixed<
            exact_units::units::Second,
            exact_units::prefix::Micro,
            i64,
            exact_units::Unbounded,
        >,
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    /// Absolute, MPRIS `SetPosition` on the current track.
    async fn browsers_playback_set_position(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
        _at: exact_units::Fixed<
            exact_units::units::Second,
            exact_units::prefix::Micro,
            u64,
            exact_units::Unbounded,
        >,
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    // ---- ww_niri_api::Media at media ----

    /// Valid indices of `media.players[]`.
    fn valid_indices_root_media_players(&mut self) -> ValidIndices<'_> {
        // TODO: no index is valid until implemented
        ValidIndices::range_u32(0..0)
    }

    async fn media_list(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> RpcResult<Vec<ww_niri_api::PlayerInfoOwned>> {
        Unimplemented.into()
    }

    // ---- ww_niri_api::Player at media.players[] ----

    /// Where the player comes from: a browser slot, or a window or app.
    async fn get_media_players_source(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> GetResult<ww_niri_api::PlayerSourceOwned, ()> {
        Unimplemented.into()
    }

    // ---- ww_niri_api::Playback at media.players[].playback ----

    /// Status, track, position and capabilities.
    async fn get_media_players_playback_state(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> GetResult<ww_niri_api::PlaybackStateOwned, ()> {
        Unimplemented.into()
    }

    async fn media_players_playback_play(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    async fn media_players_playback_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    async fn media_players_playback_play_pause(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    async fn media_players_playback_stop(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    async fn media_players_playback_next(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    async fn media_players_playback_previous(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    /// Relative, MPRIS `Seek`; negative goes back.
    async fn media_players_playback_seek(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
        _by: exact_units::Fixed<
            exact_units::units::Second,
            exact_units::prefix::Micro,
            i64,
            exact_units::Unbounded,
        >,
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    /// Absolute, MPRIS `SetPosition` on the current track.
    async fn media_players_playback_set_position(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _index_chain: [UNib32; 1],
        _at: exact_units::Fixed<
            exact_units::units::Second,
            exact_units::prefix::Micro,
            u64,
            exact_units::Unbounded,
        >,
    ) -> RpcResult<Result<(), ww_niri_api::MediaErrorOwned>> {
        Unimplemented.into()
    }

    // ---- ww_niri_api::Audio at audio ----

    /// The default sink's volume, up to 150 %.
    async fn set_audio_volume(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _value: exact_units::Fixed<
            exact_units::units::Ratio,
            exact_units::prefix::Centi,
            u16,
            exact_units::Range<0, 150>,
        >,
    ) -> SetResult<()> {
        // observers get it after `SetResult::Set`: the generated code calls `mark_changed_audio_volume()`
        Unimplemented.into()
    }

    /// The default sink's volume, up to 150 %.
    async fn get_audio_volume(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> GetResult<
        exact_units::Fixed<
            exact_units::units::Ratio,
            exact_units::prefix::Centi,
            u16,
            exact_units::Range<0, 150>,
        >,
        (),
    > {
        Unimplemented.into()
    }

    async fn set_audio_muted(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
        _value: bool,
    ) -> SetResult<()> {
        // observers get it after `SetResult::Set`: the generated code calls `mark_changed_audio_muted()`
        Unimplemented.into()
    }

    async fn get_audio_muted(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> GetResult<bool, ()> {
        Unimplemented.into()
    }

    /// The default sink, to show where the sound goes.
    async fn get_audio_sink(
        &mut self,
        _cx: &mut Context<'_, impl EventOut>,
    ) -> GetResult<ww_niri_api::SinkOwned, ()> {
        Unimplemented.into()
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
