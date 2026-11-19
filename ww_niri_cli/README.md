# ww_niri_cli

`wwn`, the command line for a television (TV) box running ww_niri's control API
(`ww_niri_api`). Every call is a subcommand. The typed calls themselves are
[ww_niri_client](../ww_niri_client)'s.

```sh
wwn --tv ws://alma-2tv:7590/ww info
wwn slot 0 start
wwn slot 0 open https://example.org/video
wwn slot 0 pause
wwn volume 40
wwn pointer click
wwn type "hello"
wwn keys ctrl+l
wwn watch
```

`--tv URL` (env `WWN_TV`, default `ws://alma-2tv:7590/ww`) and `--json` for scripts are global.
`--help` on any subcommand lists its own flags; `source <(COMPLETE=bash wwn)` registers shell
completion (also zsh, fish, elvish, powershell).

## Commands

Each API resource is its own module under `src/cmd/`, registering its subcommand(s) into
`registry::CommandDef` with `inventory::submit!`. Nothing names every command in one place.
`tests/it/registered_commands.rs` checks the registered list against what this README promises.

| Command | What |
|---|---|
| `info` | niri version, ww_niri version, hostname |
| `windows list\|show\|focus\|close\|set-fullscreen` | niri's windows |
| `outputs list` | the TV's monitors |
| `launch -- ARGV..` | runs a command as the TV's user |
| `slot INDEX start\|open\|stop\|show\|play\|pause\|play-pause\|next\|previous\|seek\|set-position` | a browser slot and its player |
| `player list\|play\|pause\|play-pause\|stop\|next\|previous\|seek\|set-position` | any MPRIS player, by id |
| `audio`, `volume PERCENT`, `mute on\|off` | the default sink |
| `pointer move-to\|move-by\|click\|press [--for-ms]\|drag\|scroll` | the TV's pointer |
| `type TEXT`, `keys COMBO`, `key-down KEY [--for-ms]` | the TV's keyboard |
| `watch` | the live model, one line per change, until Ctrl+C |

`pointer press`/`key-down` hold for `--for-ms` (default 1000) then release themselves, all
within one call.

A one-shot `wwn` invocation cannot hold past its own connection: the TV releases what a
connection held as soon as it ends. So `pointer release`/`key-up`/`release-all` as separate
calls would be no-ops on a fresh connection, and are not offered here.

`ww_niri_client::Tv` still has them, for an app that keeps its connection open.

## Tests

Against `ww_niri_fake` in-process, the same way `ww_niri_client`'s own tests do:

```sh
cargo nextest run -p ww_niri_cli
```

## License

Licensed under either of:

- The Massachusetts Institute of Technology (MIT) license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

at your option.
