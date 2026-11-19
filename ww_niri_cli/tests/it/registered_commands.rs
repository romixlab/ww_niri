//! Every command comes from the registry: no central match grows a name this list misses.

#[test]
fn every_module_registered_itself() {
    let names: Vec<&str> = ww_niri_cli::registry::commands()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(
        names,
        [
            "info", "windows", "outputs", "launch", "slot", "player", "audio", "volume", "mute",
            "pointer", "type", "keys", "key-down", "watch",
        ]
    );
}

#[test]
fn every_registered_command_builds_and_help_does_not_panic() {
    for def in ww_niri_cli::registry::commands() {
        let mut command = (def.command)();
        // `--help` renders every arg's value parser (ranges, enums): a bad one panics here.
        let _ = command.render_long_help();
    }
}
