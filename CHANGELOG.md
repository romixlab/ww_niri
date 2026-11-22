# Changelog

Changes of ww_niri on top of upstream niri. Upstream's own changes are in its release notes.

## [Unreleased]

## [26.23.0] - 2026-11-22

### Changed

- Control API: every method declares its `#[effect]`; wire_weaver moves to fc01c46a (API-58).

## [26.22.0] - 2026-11-22

### Added

- Control API `overlay`: items each client owns by key, stacked by priority, on windows, slots or outputs (P2618).
- Overlay kinds: blurred cover with a card, text, clock and notice, with configurable blur, dim and anchor (P2618).
- `ww_niri_client`: `tv.overlay()` puts, lifts and dismisses items; `Tv::overlay_ended` tells each end (P2618).
- `ww_niri_fake`: the overlay with the same rules; `FakeTv::overlay` lists the items (P2618).

### Removed

- **Breaking:** control API: `windows.cover`, `windows.uncover`, `browsers[].cover`, `browsers[].uncover` and `CoverLifted`; use `overlay` (P2618).

## [26.21.1] - 2026-11-22

### Added

- `ww_niri_fake`: `restart_reporting_hash` restarts its servers under another API hash (P2618#21nov-2322).

### Changed

- Takes wire_weaver 3faea4f58: a reconnecting client relearns the API revisions (P2618#21nov-2322).

## [26.21.0] - 2026-11-22

### Changed

- The control API server runs on wire_weaver_server over ww_calloop, with its connection and request limits (P2618).
- A stopped listener frees its address at once; its open connections go on until their links end (P2618).
- `ww_niri_fake` serves on wire_weaver_server: many clients, each releasing only what it held; `rebind` moves its paths (P2618).
- `ww_niri_client`: a `Tv` races its addresses; `MotionSender` cuts a reconnect wait short on the next move (P2618).
- **Breaking:** control API over WebSocket: only the paths `/ww` and `/` reach the API, others get 404 (P2618).

## [26.20.0] - 2026-11-22

### Added

- `ww.kdl`: `browser N owner=... user-agent=...` gives a slot its person and the user agent its Firefox sends (P2618#21nov-1232b).
- Control API: `BrowserState.owner` names the slot's person; `ww_niri_fake` takes `slot_owners`, `wwn` prints them (P2618#21nov-1232b).

### Changed

- The control API server serves full introspection data: clients stay compatible by shape, not version (P2618).
- **Breaking:** control API: `input.pointer.motion` takes `Totals<Delta>`, a session per link (P2618).
- `MotionSender` sends through wire_weaver's `TotalsSender`, repeating the last write after 50 ms without moves (P2618).

## [26.19.0] - 2026-11-21

### Added

- Control API: `windows.hide` keeps a window running off screen, never drawn or focused; `windows.show` brings it back (P2618).
- `ww_niri_client`, `wwn windows hide` and `ww_niri_fake`: hide a window; `Window.hidden` tells which are hidden (P2618).

## [26.18.0] - 2026-11-21

### Added

- `justfile`: `test`, `lint`, `install` and `deploy` recipes; `deploy/binkeep.toml` keeps niri as a binkeep set. `deploy` never restarts niri (P2618#21nov-1625).
- Control API: `input.pointer.motion` sink moves the pointer by running totals, so a lost UDP datagram loses no distance (P2618#21nov-0944).
- Control API over UDP on each `listen` address and port, beside the WebSocket, several hosts at once (P2618#21nov-0944).
- `ww_niri_client`: `MotionSender` moves the pointer over UDP without waiting, and `ww_niri_fake` serves it on `motion_path` (P2618#21nov-0944).
- `ww_niri_fake`: `set_screen_connected` takes the screen off HDMI and back; the output list changes and `OutputsChanged` is sent (P2618).

## [26.17.0] - 2026-11-20

### Added

- `ww_niri_client`: `Tv::cover_lifted` tells of each lifted cover and why, its own lost with the link as `Disconnected` (P2618#20nov-0043).
- Control API: `cover` takes `mute`, muting the default sink while the cover holds and restoring its mute after (P2618#20nov-0043b).

## [26.16.0] - 2026-11-19

### Added

- `wwn`: a CLI on `ww_niri_client`, every control API call as a subcommand, `--json`, `watch`, shell completion (P2618#18nov-1316c).

### Fixed

- `ww_niri_client`: a slot's `start`/`open`/`stop` no longer time out after 1 s while Firefox is still launching, loading a page, or closing; they now wait up to 60 s (P2618#18nov-1316c).

## [26.15.0] - 2026-11-19

### Added

- Control API: `windows.cover` and `browsers[].cover` blur a window under a card with a countdown and clock, held as a lease (P2618#19nov-2040b).
- `ww_niri_client` and `ww_niri_fake`: cover and uncover, with covers lifting when the connection ends (P2618#19nov-2040b).

## [26.14.0] - 2026-11-19

### Added

- Control API: `lan:` listeners are announced over DNS-SD as `_wireweaver._tcp`, named by `advertise` in `ww.kdl` (P2618#18nov-1357b).

## [26.13.0] - 2026-11-19

### Added

- `ww_niri_client`: a TV handle that reconnects, follows the TV live, and sends typed, merged commands (P2618#12nov-1559).

## [26.12.0] - 2026-11-18

### Added

- Control API: `type_text` types characters of the keymap's other layouts, such as Cyrillic with `us,ru`, and switches back after (P2618#18nov-1349).

## [26.11.0] - 2026-11-18

### Added

- `ww_niri_fake`: an in-memory TV serving the control API in-process, with test knobs, for client and app tests (P2618#18nov-1316b).

## [26.10.0] - 2026-11-18

### Changed

- `ww_niri_api` relicensed from GPL-3.0-or-later to MIT OR Apache-2.0, so clients like pult can link it.

## [26.9.0] - 2026-11-18

### Added

- Control API: `pointer.move_by` relative moves, observed pointer `position`, media and remote keys (P2618#18nov-1316).

### Changed

- Control API: `scroll` takes v120 units and modifiers; `press` and `key_down` take a hold limit, default 10 s (breaks clients).

## [26.8.0] - 2026-11-18

### Added

- Control API: `listen "lan:PORT"` and `"tailnet:PORT"` in `ww.kdl` bind the current LAN and tailnet addresses, following changes (P2618#18nov-1357).

## [26.7.0] - 2026-11-18

### Added

- Control API: `input.pointer` and `input.keyboard` (P2618#12nov-1548). Move the pointer to a point in global or window-local coordinates, click (any button, double click, modifiers held), press and release, drag, scroll, read where the pointer is and the window under it; type text, press keys and combinations (`ctrl+l`), hold keys. The input goes in below niri, through a keyboard and an absolute pointer that niri makes as kernel uinput devices on first use, so libinput, xkb, niri's binds and the apps see real devices. Needs write access to `/dev/uinput` (else `Unavailable`). Buttons and keys a connection holds are released when it ends. Text outside the configured keymap answers `NotTypeable`.

### Changed

- Control API: `ww.kdl` takes several `listen` lines, such as the tailnet and LAN addresses; each is retried until its address exists.

### Fixed

- Control API audio: with no PulseAudio cookie file (`~/.config/pulse/cookie`), the client sent an empty cookie and PipeWire refused AUTH with "Invalid argument", so volume and mute never worked. It now sends 256 zero bytes, which PipeWire accepts (it ignores the cookie but checks its length).

## [26.6.0] - 2026-11-17

### Changed

- Control API: ported to the WireWeaver layers. `ww_niri_api` depends on `ww_api` and `ww_si` from ww_base; its error enums declare `api_error!`; audio property errors are `CommonError` (`WrongState`, `Fault`, `Refused`) instead of `()`. Wire unchanged.

## [26.5.1] - 2026-11-12

### Fixed

- Control API: `browsers[i].open` no longer overwrites a newer URL the browser reported while the call was answered (a redirect, a link followed at once), and observers of `browsers[i].state` now hear the URL `open` sets. Seen as `browser_slot_lifecycle` failing about every other full test run in the build container.

## [26.5.0] - 2026-11-12

### Added

- Control API: observed properties under the resource arrays (wire_weaver API-63). `browsers[i].state` follows the slot's phase, URL, window and window title, `browsers[i].playback.state` and `media.players[j].playback.state` follow the player's status, track, rate and capabilities (MPRIS `PropertiesChanged`); observers of a player that leaves the bus get `Ended`. Changes reach observers whether or not the client has the events stream open.

### Removed

- The `PlaybackChanged` event: observe `playback.state` instead (breaks clients that matched on it).

### Fixed

- Cargo.lock takes up wire_weaver master (82438565, with API-63) and exact_units main (58cc839), which build together again; 26.4.0's lock had exact_units 1e825f4, which ww_si and ww_self don't build against.

## [26.4.0] - 2026-11-11

### Added

- `ww_niri_api`: the WireWeaver control API crate (`no_std`): windows, outputs, `apps.launch`, browser slots with a lifecycle (`start`, `open`, `stop`), every MPRIS player in `media.players[]`, a `Playback` trait shared by both, audio volume and mute, and an event stream. The server side follows.
- The control API server (`src/ww/`, cargo feature `ww`, on by default): `~/.config/niri/ww.kdl` (`listen "ADDR:7590"`, `browsers N`) makes niri listen at `ws://ADDR/ww`, never on every address; the file is checked every second and applied when it parses, and a bind that fails (the tailnet not up yet at boot) is retried. Everything runs on niri's calloop loop, no tokio. `WW_NIRI_CONFIG` points at another file.
- Control API: `info`, `windows` (list, show, focus, close, set_fullscreen; `show` takes `fullscreen` and a `Transition`, a duration and easing curve used for that one workspace switch and view move instead of the configured animations), `outputs.list`, and the `events` stream (windows opened, changed, closed and focused, from niri's own IPC events; `Resync` when the client fell behind). The rest answers `Unimplemented` for now.
- Control API: `apps.launch(argv)` runs a command line without a shell, as niri's `spawn` does, and answers with the pid of the started process (`EmptyArgv`, or `Spawn` with the reason, such as a missing binary). Every launch is logged with the peer address and argv. The `ww` feature now enables `systemd`, whose spawn hands that pid back.
- Control API: browser slots, `browsers[i]` (`browsers N` in ww.kdl, default 2). `start` launches the slot's own Firefox (kiosk unless `fullscreen: false`, profile `~/.local/share/ww_niri/browser-<n>`, WebDriver BiDi on 127.0.0.1:9300+n) and answers once a BiDi session drives its tab; the slot is `Running` once its window has mapped too (`BrowserStarted`). `open(url)` loads an http(s) URL in that same tab, `stop` closes Firefox (BiDi `browser.close`, then SIGTERM and SIGKILL after 5 s each), `show` shows its window, and `state` reads the phase, URL, title and window. A Firefox that exits, crashes or loses its BiDi connection ends the slot with `BrowserStopped` and the reason. Slot Firefoxes die with niri. Windows of a slot carry `browser`. Players and playback follow.
- Control API: MPRIS players. Every `org.mpris.MediaPlayer2.*` name on the session bus is listed in `media.players[]` (`media.list`, `PlayerAppeared` and `PlayerGone` events) with its identity and source: a browser slot, the window of its process or its parent, or the app alone. A name that comes back keeps its id. `playback` (state, play, pause, play_pause, stop, next, previous, seek, set_position) acts on the player over the bus; an action the player doesn't offer answers `NotSupported`. `browsers[i].playback` and `state.player` follow the slot's Firefox's player (`NoPlayer` while its page plays nothing).
- Control API: `audio` volume (0 to 150 %), mute and the default sink's name, over the PulseAudio protocol (PipeWire's `pipewire-pulse`; the `pulseaudio` crate's messages, pure Rust, on niri's loop). The server is found as libpulse finds it (`PULSE_SERVER`, `XDG_RUNTIME_DIR/pulse/native`); niri follows its sink and server events, so a change made elsewhere is seen at once, and connects again every 5 s when the server goes away. A volume sets every channel of the default sink; the reported volume is the loudest channel. Without a server or default sink the properties answer an error.
- Control API: observed properties are pushed. Each connection wakes when an observed property is due and sends the updates; `audio.volume`, `audio.muted` and `audio.sink` observed on change get every change of the default sink, also one made elsewhere.
- Control API: `PlaybackChanged { player, state }` event when a player's status, track, rate or capabilities change (its MPRIS `PropertiesChanged`), so clients follow playback of `media.players[]` and of browser slots without polling. It stands in for observing `playback.state`, which wire_weaver can't yet do under a resource array.

### Fixed

- Spawning a command whose exec fails (a missing binary) no longer hangs a spawner thread and leaves a blocked niri child process behind, with the `systemd` feature on Linux. `Command::spawn()` waited for the intermediate child, which waited for niri to signal after `spawn()` returned. The intermediate child now also wakes up when the spawned process exits.
