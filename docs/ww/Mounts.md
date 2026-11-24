# Mounted programs (ww_niri)

ww_niri's control API can carry the APIs of other local programs as branches of its own tree. A phone then keeps one link to the TV (one WebSocket and UDP address, one DNS-SD record, one reconnect) and reaches, say, a home hub at `ext["hub"]`. The program runs and restarts on its own: the compositor and the screen never wait for it.

## Mounting a program

A `mount` line of `ww.kdl` names the slot and the program's Unix socket:

```kdl
mount "hub" "unix:$XDG_RUNTIME_DIR/pult/hub.sock"
```

- The name is the slot's key in `ext`: at most 64 bytes, no spaces or control characters, each name once.
- `$VAR` and `${VAR}` in the path are taken from niri's environment when the file is read. The path must be absolute.
- The program serves a WireWeaver API on that socket with `wire_weaver_server` (preamble framing), for the same user as niri.
- Only `ww.kdl` adds a slot. A client never can, so no peer on the network can graft a branch onto the TV's tree.

ww.kdl is read again on each change, as for every other line: a new line mounts at once, a removed line unmounts, a changed path mounts the new socket at the same slot.

## What a client sees

`ext` is a keyed array (`mounts!(ext)` in `ww_niri_api`):

| Item | What it is |
|---|---|
| `ext_index_of(name)` | the slot's index, or none |
| `ext[i].presence` | `Absent`, or `Up` with the program's API crate, version, hashes and wire version; observable |
| `ext[i].api` | the program's own API: requests below it go to the program with the slot's part of the path taken off |
| `ext_keys` | stream of `Added` and `Removed`, when a `mount` line comes or goes |

A slot keeps its index while its line stays; indices are never reused within one run of niri.

With `wire_weaver_client`, a view of the slot works with the program's API as with a device of its own:

```rust
let slot = tv.ext_by(&name("hub")).await?.expect("ext[hub]");
let view = slot.view_for(&hub_api::Client::client_line()).await?;
let hub: hub_api::Client = view.client(&hub_api::Client::client_line())?;
```

The view learns the program's API when it comes up, and again when its hash changes. Its `status()` follows `presence`.

## When the program restarts

1. Its socket goes: the slot's `presence` turns `Absent`; observers hear it. The keys don't change.
2. Requests waiting on the program fail with `Unavailable`; its streams and observed levels end. ww_niri's own API goes on.
3. Requests into the slot meanwhile fail with `Unavailable` at once, never by timeout.
4. niri tries the socket again every second. Once the program answers, `presence` turns `Up` and the views resume their streams and observers.

## How it works

`wire_weaver_server::mount` runs on niri's calloop loop. A control link per slot learns the program's API and keeps `presence`. Each client connection opens its own link to each program it uses, so sequence numbers pass through unchanged and one slow phone never holds up another. Only the request and event envelopes are rewritten (the slot's path taken off and put back); arguments and values are copied as bytes.

The program can tell which client called: each forwarded request names the client as ww_niri's transport saw it (`RequestContext::forwarded_for`). A program whose server trusts niri's connections (`Server::trust_forwarders`) serves the call as that client's.

Mounted APIs' effects all pass: ww_niri sets no limit on them yet.

`ww_niri_fake` has the same slots (`FakeTv::mount`, `FakeTv::unmount`), and `Options::socket` serves a fake TV on a Unix socket, to be mounted in tests.
