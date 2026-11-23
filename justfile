# `just jobs=N <recipe>` caps build and test parallelism for that run; unset leaves CARGO_BUILD_JOBS /
# NEXTEST_TEST_THREADS as inherited, or cargo's own default (the CPU count).
jobs := ""
export CARGO_BUILD_JOBS := if jobs != "" { jobs } else { env_var_or_default('CARGO_BUILD_JOBS', num_cpus()) }
export NEXTEST_TEST_THREADS := if jobs != "" { jobs } else { env_var_or_default('NEXTEST_TEST_THREADS', 'num-cpus') }

default:
    @just --list

# `just test`: the whole workspace. `just test --changed`: only the crates changed against main and their dependents.
test *flags:
    {{ if flags == "--changed" { "tpm test changed -- cargo nextest run --workspace --locked" } else { "cargo nextest run --workspace --locked " + flags } }}

# Format (nightly rustfmt, as CI), clippy and spirit_level's lints (spl check)
lint:
    cargo +nightly fmt --all --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    spl check --progress never

# Build niri --profile deploy and install it through binkeep (the set is in deploy/binkeep.toml). Does not restart niri.
# Also refreshes the seatd unit where this PC already has it (the TV only).
install:
    cargo build --profile deploy --locked -p niri
    test -x target/deploy/niri
    # binkeep refuses a group-writable binary or folder (umask 0002 gives 775), so stage both at 755
    rm -rf ~/.cache/ww_niri-deploy && mkdir -m 755 -p ~/.cache/ww_niri-deploy
    install -m 755 target/deploy/niri ~/.cache/ww_niri-deploy/niri
    binkeep install niri ~/.cache/ww_niri-deploy
    {{just_executable()}} refresh-seatd-unit

# The TV's niri unit (deploy/niri-seatd.service) as ~/.config/systemd/user/niri.service, for a new seatd machine.
# Never on a desktop that runs niri through a display manager or niri-session. Does not restart niri.
install-seatd-unit:
    mkdir -p ~/.config/systemd/user
    install -m 644 deploy/niri-seatd.service ~/.config/systemd/user/niri.service
    systemctl --user daemon-reload

# Only where ~/.config/systemd/user/niri.service already says seatd: reinstall it when it differs from the repo's.
refresh-seatd-unit:
    @if grep -qs seatd ~/.config/systemd/user/niri.service && ! cmp -s deploy/niri-seatd.service ~/.config/systemd/user/niri.service; then {{just_executable()}} install-seatd-unit && echo "niri.service changed: niri needs a restart to use it"; fi

# Refuses on local changes. Does NOT restart niri: it is the session compositor and a restart closes every window.
# The new niri runs after the next session restart (README.md, "Deploying niri on a PC").
# Bring this PC's niri up to main
deploy:
    [ -z "$(git status --porcelain)" ] || { echo "local changes, not deploying:" >&2; git status --short >&2; exit 1; }
    git pull --ff-only
    {{just_executable()}} install
    target/deploy/niri --version | grep -qF "$(git describe --always --dirty=-modified)"
    @echo "niri installed, not restarted: the new niri runs after the next session restart"
