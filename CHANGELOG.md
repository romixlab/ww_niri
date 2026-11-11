# Changelog

Changes of ww_niri on top of upstream niri. Upstream's own changes are in its release notes.

## [Unreleased]

### Added

- `ww_niri_api`: the WireWeaver control API crate (`no_std`): windows, outputs, `apps.launch`, browser slots with a lifecycle (`start`, `open`, `stop`), every MPRIS player in `media.players[]`, a `Playback` trait shared by both, audio volume and mute, and an event stream. The server side follows.
