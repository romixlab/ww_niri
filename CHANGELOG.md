# Changelog

Changes of ww_niri on top of upstream niri. Upstream's own changes are in its release notes.

## [Unreleased]

### Added

- `ww_niri_api`: the WireWeaver control API crate (`no_std`): windows, outputs, `apps.launch`, browser slots with a lifecycle (`start`, `open`, `stop`), every MPRIS player in `media.players[]`, a `Playback` trait shared by both, audio volume and mute, and an event stream. The server side follows.
- The control API server (`src/ww/`, cargo feature `ww`, on by default): `~/.config/niri/ww.kdl` (`listen "ADDR:7590"`, `browsers N`) makes niri listen at `ws://ADDR/ww`, never on every address; the file is checked every second and applied when it parses, and a bind that fails (the tailnet not up yet at boot) is retried. Everything runs on niri's calloop loop, no tokio. `WW_NIRI_CONFIG` points at another file.
- Control API: `info`, `windows` (list, show, focus, close, set_fullscreen; `show` takes `fullscreen` and a `Transition`, a duration and easing curve used for that one workspace switch and view move instead of the configured animations), `outputs.list`, and the `events` stream (windows opened, changed, closed and focused, from niri's own IPC events; `Resync` when the client fell behind). The rest answers `Unimplemented` for now.
- Control API: `apps.launch(argv)` runs a command line without a shell, as niri's `spawn` does, and answers with the pid of the started process (`EmptyArgv`, or `Spawn` with the reason, such as a missing binary). Every launch is logged with the peer address and argv. The `ww` feature now enables `systemd`, whose spawn hands that pid back.
- Control API: browser slots, `browsers[i]` (`browsers N` in ww.kdl, default 2). `start` launches the slot's own Firefox (kiosk unless `fullscreen: false`, profile `~/.local/share/ww_niri/browser-<n>`, WebDriver BiDi on 127.0.0.1:9300+n) and answers once a BiDi session drives its tab; the slot is `Running` once its window has mapped too (`BrowserStarted`). `open(url)` loads an http(s) URL in that same tab, `stop` closes Firefox (BiDi `browser.close`, then SIGTERM and SIGKILL after 5 s each), `show` shows its window, and `state` reads the phase, URL, title and window. A Firefox that exits, crashes or loses its BiDi connection ends the slot with `BrowserStopped` and the reason. Slot Firefoxes die with niri. Windows of a slot carry `browser`. Players and playback follow.
- Control API: MPRIS players. Every `org.mpris.MediaPlayer2.*` name on the session bus is listed in `media.players[]` (`media.list`, `PlayerAppeared` and `PlayerGone` events) with its identity and source: a browser slot, the window of its process or its parent, or the app alone. A name that comes back keeps its id. `playback` (state, play, pause, play_pause, stop, next, previous, seek, set_position) acts on the player over the bus; an action the player doesn't offer answers `NotSupported`. `browsers[i].playback` and `state.player` follow the slot's Firefox's player (`NoPlayer` while its page plays nothing).

### Fixed

- Spawning a command whose exec fails (a missing binary) no longer hangs a spawner thread and leaves a blocked niri child process behind, with the `systemd` feature on Linux. `Command::spawn()` waited for the intermediate child, which waited for niri to signal after `spawn()` returned. The intermediate child now also wakes up when the spawned process exits.
