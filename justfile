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
install:
    cargo build --profile deploy --locked -p niri
    test -x target/deploy/niri
    # binkeep refuses a group-writable binary (cargo leaves 775 under umask 0002), so stage it at 755
    mkdir -p ~/.cache/ww_niri-deploy
    install -m 755 target/deploy/niri ~/.cache/ww_niri-deploy/niri
    binkeep install niri ~/.cache/ww_niri-deploy

# Refuses on local changes. Does NOT restart niri: it is the session compositor and a restart closes every window.
# The new niri runs after the next session restart (README.md, "Deploying niri on a PC").
# Bring this PC's niri up to main
deploy:
    [ -z "$(git status --porcelain)" ] || { echo "local changes, not deploying:" >&2; git status --short >&2; exit 1; }
    git pull --ff-only
    {{just_executable()}} install
    target/deploy/niri --version | grep -qF "$(git describe --always --dirty=-modified)"
    @echo "niri installed, not restarted: the new niri runs after the next session restart"
