# ww_niri_fake

An in-memory television box (TV) serving ww_niri's control API (`ww_niri_api`) in the same process, for the tests of clients and apps and for their demo modes. No socket and no compositor: the generated server runs on `ww_device::Server` in a thread of its own, reached through `wire_weaver_client::in_process`.

```rust
let tv = FakeTv::start(Options { clock: ClockMode::Manual, ..Options::default() });
let client = Client::config(|c| c.in_process_path(tv.path().to_owned())).connect().await?;
client.browsers(0).start(true).call().await?;
tv.advance(Duration::from_secs(1)); // the slot's window maps: Running
```

## What the TV has

- One output, `HDMI-A-1`, 1920x1080. The pointer stays on it: `move_by` stops at the edges, `move_to` off it answers `OffScreen`.
- A terminal (`kitty`, window 1) and a video player (`mpv`, window 2) side by side on the first workspace.
- The video player's paused Media Player Remote Interfacing Specification (MPRIS) player.
- Browser slots (2 by default) that go Stopped, Starting (`BrowserStarted` at once), Running when the window maps after the start delay. A page opened in a slot plays: the slot gets a playing player.
- Players whose position moves with the clock while they play, and stop at the end of the track.
- Volume (40 %), mute and a default sink.
- A log of the keys, text, buttons and wheel turns injected ([`FakeTv::input_log`]). Text outside printable ASCII is `NotTypeable`, as with the us keymap.
- Held buttons and keys come up after their hold limit or when the connection ends.
- Covers over windows ([`FakeTv::covers`]): set and renewed by `cover`, lifted at their end, on `uncover`, when the window closes or when the connection ends, each with a `CoverLifted` event. A cover with `mute` mutes the sink while it holds, as the compositor does.

## Knobs for tests

| Knob | Does |
|---|---|
| `ClockMode::Manual` and `advance` | Time stands until the test moves it: start delays, positions and hold limits follow. |
| `calls`, `take_calls` | Every method call and property write, shown as `browsers[0].open("https://...")`. |
| `fail_next(method, fail)` | The next call of a method, or read or write of a property, answers this error. |
| `set_latency` | Every request waits this long, so calls stay in flight. |
| `drop_connection` | Ends the client's connection; a new one may come at once. |
| `set_reachable(false)` | Drops the connection and refuses new ones until reachable again. |
| `resync` | Sends `Resync` on the `events` stream and refreshes every observer. |
| `open_window`, `close_window`, `set_audio`, `set_sink` | Changes made on the TV itself. |

Serving the fake over a real WebSocket is left out: ww_niri's own server tests cover that path.

## License

Licensed under either of:

- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

at your option.
