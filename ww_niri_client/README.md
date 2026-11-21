# ww_niri_client

Client of ww_niri's control API (`ww_niri_api`) for apps that drive a television (TV) box. Its `Tv` handle connects in the background, follows the TV in a live model, and sends typed commands.

It wraps the generated WireWeaver client: an app never touches resource paths, observers or the event stream.

```rust
let tv = Tv::start(Config::websocket("ws://192.168.1.95:7590/ww"))?;
tv.watch_state().wait_for(|s| s.is_connected()).await?;
tv.slot(0).open("https://example.org/".to_owned()).await?;
tv.slot(0).playback().play_pause().await?;
tv.pointer().move_by(20, -5).await?;
let volume = tv.snapshot().audio.volume;
```

## Connection

- `Tv::start` returns at once and connects on wire_weaver's keep-trying connection; it connects again whenever the link goes.
- Each attempt tries the configured addresses in order, and the first that answers wins.
- It runs on the caller's tokio runtime, or on a runtime thread of its own when there is none (an egui app, a sync CLI).
- `Tv::state` and `Tv::watch_state` say where the connection is:
  - `Connecting`: the attempt, since when offline, and why;
  - `Connected`: the address and the TV's `info()`;
  - `Offline`: the give-up time of `KeepTrying` passed.
- While offline every call fails at once with `TvError::Offline`; none is sent again by itself.

## Pointer motion over UDP

A touchpad's moves go over UDP with a `MotionSender`. Over a lossy Wi-Fi a lost move is skipped, while TCP would hold the later ones up. ww_niri serves UDP on the address and port of its WebSocket.

```rust
let addr = MotionAddress::beside("ws://192.168.1.95:7590/ww").expect("a ws URL with a port");
let motion = MotionSender::start(addr, KeepTrying::default())?;
motion.move_by(3, -1); // never waits
```

- Each datagram carries the sum of all moves so far, so the next datagram brings the distance of a lost one.
- Moves made while one datagram is on its way add up into the next.
- It connects in the background and again whenever the link goes. Moves made while offline, or as the link comes up, are not sent: `is_connected` and `watch_state` tell when to move over the `Tv` instead.
- Clicks, keys and everything else stay on the `Tv`.

## The live model

`Tv::snapshot` is one immutable `Snapshot`, replaced on every change. `Tv::watch_snapshot` and `Config::on_change` tell of each new one; an egui app passes `ctx.request_repaint`. It holds:

- windows and outputs, from the lists and the `events` stream;
- browser slots with their observed state and their player's playback;
- every Media Player Remote Interfacing Specification (MPRIS) player with its observed playback;
- volume, mute and sink; the pointer position.

On each link, and after a `Resync` event, the client lists everything again and opens the observers anew. The last snapshot stays while the TV is offline.

`Playback::position_at` moves the position on with the rate, so a seek bar moves smoothly between updates.

## Commands

- Typed calls mirror the API and answer `Result<_, TvError>`: `tv.slot(0).start(true)`, `tv.player(id).seek(..)`, `tv.audio().set_volume(60)`, `tv.pointer().click(..)`, `tv.keyboard().press_keys("ctrl+l".into())`.
- `Tv::send(Command)` queues and returns at once, for a UI; failures come on `Tv::errors`.
- Each resource's commands go one call at a time, in order. Pointer and keyboard share one lane; each slot, each player, windows, volume and mute have their own.
- While a call is in flight, the commands behind it merge where one call does both:
  - pointer moves and wheel turns add up;
  - a volume or mute replaces the one before;
  - seeks add up, and a position replaces them;
  - a cover replaces the one before of the same window or slot.
- So a fast UI never floods the link, and the rate follows the round trip. Clicks, keys and text keep their order.
- A button or key held with `press` or `key_down` is renewed every third of the hold limit until released. `Config::hold_limit` sets it, 3 s by default.
- `tv.window(id).cover(card, lasts, mute)` and `tv.slot(0).cover(..)` blur the window under a card for `lasts`. The cover belongs to the connection: it lifts when the link goes, also on a reconnect, so a client renews it while it should stay.
- With `mute` the TV's default sink is muted when the first muting cover starts. When the last one lifts, the sink gets its mute from before. Renewals leave the mute as it is, so a user's unmute during the cover holds. A client that hangs or crashes never leaves the TV muted: its connection ends and the cover lifts.
- `Tv::cover_lifted` tells of every cover that lifts on the TV, whoever set it, with the window and why: `Deadline`, `Uncovered`, `WindowClosed` or `Disconnected`. The covers this client holds when its link goes come as `Disconnected`, made by the client itself.
- `tv.window(id).hide()` keeps the window running off screen: niri never draws or focuses it until `show`, which brings it back on the active workspace. A hidden window is a lasting state, not a lease: it stays hidden when the link goes.
- `Tv::release_all` lets go of everything when the app goes to the background. A phone that vanishes leaves nothing held past the hold limit.

## Tests

The tests run the client against `ww_niri_fake` in the same process. They cover:

- the model following lists, observers and events, and `Resync`;
- a dropped link and a refused one;
- merging under latency, held input and the error channel;
- a `Tv` without a runtime.

```sh
cargo nextest run -p ww_niri_client
```

## License

Licensed under either of:

- The Massachusetts Institute of Technology (MIT) license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

at your option.
