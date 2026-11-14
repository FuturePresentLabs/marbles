#!/bin/sh
# Install the records-backed `mb`: the command package, the host shim, and the
# records binding they share.
#
# What it does, in order:
#   1. builds `mb.wasm` for wasm32-wasip2 with the `soil` feature
#   2. installs it as the `mb` command package under the fungOS command root
#   3. installs `scripts/mb-records.sh` as `mb` on PATH
#   4. records the binding in ~/.config/fungos/marbles/records.env
#
# It does not install `soil-command`; that binary belongs to Soil. Pass
# --soil-command if it is not already on PATH.
#
#   scripts/install-records-cli.sh --records-book personal/avery/marbles
#   scripts/install-records-cli.sh --dry-run
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd -P)
RECORDS_ROOT=${FUNGOS_RECORDS_ROOT:-$HOME/.local/share/fungos/records}
BOOK=${FUNGOS_RECORDS_BOOK:-}
COLLECTION=${FUNGOS_RECORDS_COLLECTION:-marbles}
COMMAND_ROOT=${FUNGOS_COMMAND_ROOT:-$HOME/.local/share/fungos/commands}
BIN_DIR=${MARBLES_BIN_DIR:-$HOME/.local/bin}
CONFIG=${XDG_CONFIG_HOME:-$HOME/.config}/fungos/marbles/records.env
SOIL_COMMAND=${SOIL_COMMAND:-}
DRY_RUN=no

need() { [ $# -ge 2 ] || { echo "$1 needs a value" >&2; exit 2; }; }

while [ $# -gt 0 ]; do
    case $1 in
        --records-root) need "$@"; RECORDS_ROOT=$2; shift 2 ;;
        --records-book) need "$@"; BOOK=$2; shift 2 ;;
        --records-collection) need "$@"; COLLECTION=$2; shift 2 ;;
        --command-root) need "$@"; COMMAND_ROOT=$2; shift 2 ;;
        --bin-dir) need "$@"; BIN_DIR=$2; shift 2 ;;
        --soil-command) need "$@"; SOIL_COMMAND=$2; shift 2 ;;
        --dry-run) DRY_RUN=yes; shift ;;
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done

if [ -z "$BOOK" ]; then
    # Seed from the binding Mush already uses, so the two cannot disagree by
    # accident. A file is read, not guessed at: if it is absent, ask.
    MUSH_CONFIG=${XDG_CONFIG_HOME:-$HOME/.config}/fungos/mush/config.toml
    if [ -r "$MUSH_CONFIG" ]; then
        BOOK=$(sed -n 's/^[[:space:]]*records_book[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' "$MUSH_CONFIG" | head -n 1)
    fi
fi
if [ -z "$BOOK" ]; then
    echo "no records book: pass --records-book (for example personal/\$USER/marbles)" >&2
    exit 2
fi

if [ -z "$SOIL_COMMAND" ]; then
    SOIL_COMMAND=$(command -v soil-command 2>/dev/null || true)
fi
if [ -z "$SOIL_COMMAND" ]; then
    echo "soil-command is not on PATH; pass --soil-command PATH" >&2
    exit 2
fi

run() {
    if [ "$DRY_RUN" = yes ]; then
        printf 'would: %s\n' "$*"
    else
        "$@"
    fi
}

command -v cargo >/dev/null 2>&1 || { echo "cargo is required to build mb.wasm" >&2; exit 2; }

echo "building mb.wasm (wasm32-wasip2, --features soil)"
run cargo build --release --target wasm32-wasip2 --no-default-features --features soil \
    --manifest-path "$REPO/Cargo.toml" --bin mb

WASM=$REPO/target/wasm32-wasip2/release/mb.wasm
[ "$DRY_RUN" = yes ] || [ -f "$WASM" ] || { echo "build produced no $WASM" >&2; exit 1; }

echo "installing the mb command package into $COMMAND_ROOT/mb"
run mkdir -p "$COMMAND_ROOT/mb"
run cp "$WASM" "$COMMAND_ROOT/mb/module.wasm"

# The package manifest the command registry reads.
VERSION=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' "$REPO/Cargo.toml" | head -n 1)
if [ "$DRY_RUN" = yes ]; then
    printf 'would write: %s\n' "$COMMAND_ROOT/mb/command.toml"
else
    cat > "$COMMAND_ROOT/mb/command.toml" <<EOF
schema = 1

[command]
name = "mb"
version = "$VERSION"
summary = "Marbles task tracker over soil:records"
module = "module.wasm"

[limits]
timeout_ms = 300000
# A whole-fleet list --all is a legitimate query, and the JSON for a few
# thousand issues runs to megabytes, so the cap is sized for the query rather
# than for one page. It still exists so a runaway command cannot fill memory.
max_stdout_bytes = 67108864
max_stderr_bytes = 1048576
max_memory_bytes = 268435456
EOF
fi

echo "installing the mb shim into $BIN_DIR/mb"
run mkdir -p "$BIN_DIR"
run install -m755 "$REPO/scripts/mb-records.sh" "$BIN_DIR/mb"

echo "writing the records binding to $CONFIG"
if [ "$DRY_RUN" = yes ]; then
    printf 'would write: %s\n' "$CONFIG"
else
    mkdir -p "$(dirname "$CONFIG")"
    cat > "$CONFIG" <<EOF
# Written by marbles scripts/install-records-cli.sh.
# Read by the mb shim. The environment overrides each of these.
RECORDS_ROOT="$RECORDS_ROOT"
RECORDS_BOOK="$BOOK"
RECORDS_COLLECTION="$COLLECTION"
COMMANDS="$COMMAND_ROOT"
# The runner the installer verified. Naming it here means an install cannot
# silently start using a different one that happens to be earlier on PATH.
SOIL_COMMAND="$SOIL_COMMAND"
EOF
    chmod 644 "$CONFIG"
fi

echo
echo "done. mb now runs the soil:records store at $RECORDS_ROOT, book $BOOK."
printf 'the native binary is still `marbles`: init, import, plan and portfolio live there.\n'
