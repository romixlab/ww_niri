# Changelog

Changes of ww_niri on top of upstream niri. Upstream's own changes are in its release notes.

## [Unreleased]

### Added

- `ww_niri_api`: the WireWeaver control API crate (`no_std`): windows, outputs, `apps.launch`, browser slots with a lifecycle (`start`, `open`, `stop`), every MPRIS player in `media.players[]`, a `Playback` trait shared by both, audio volume and mute, and an event stream. The server side follows.
- The control API server (`src/ww/`, cargo feature `ww`, on by default): `~/.config/niri/ww.kdl` (`listen "ADDR:7590"`, `browsers N`) makes niri listen at `ws://ADDR/ww`, never on every address; the file is checked every second and applied when it parses, and a bind that fails (the tailnet not up yet at boot) is retried. Everything runs on niri's calloop loop, no tokio. `WW_NIRI_CONFIG` points at another file.
- Control API: `info`, `windows` (list, show, focus, close, set_fullscreen; `show` takes `fullscreen` and a `Transition`, a duration and easing curve used for that one workspace switch and view move instead of the configured animations), `outputs.list`, and the `events` stream (windows opened, changed, closed and focused, from niri's own IPC events; `Resync` when the client fell behind). The rest answers `Unimplemented` for now.
