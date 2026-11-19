#!/bin/sh
# Install the records-backed `mb`: the command package, the policy the shim
# reads, and the shim itself.
#
# What it does, in order:
#   1. builds `mb.wasm` for wasm32-wasip2 with the `soil` feature
#   2. installs it as the `mb` command package under the fungOS command root
#   3. writes the command policy (~/.config/fungos/commands/mb.env)
#   4. asks `soil-command shims` to generate `mb` on PATH from that policy
#
# The shim is Soil's, not marbles': the package, the binding and the project
# discovery are policy, and any command that imports soil:records is installed
# the same way.
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
POLICY=${XDG_CONFIG_HOME:-$HOME/.config}/fungos/commands/mb.env
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
# The two capabilities this command composes: records to write, the bus to
# publish what it wrote. Without the second the command still works and simply
# never announces anything, which is why it is listed here rather than assumed.
requires = ["soil:records", "soil:unibus"]

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

echo "writing the command policy to $POLICY"
if [ "$DRY_RUN" = yes ]; then
    printf 'would write: %s\n' "$POLICY"
else
    mkdir -p "$(dirname "$POLICY")"
    sed -e "s|@RECORDS_ROOT@|$RECORDS_ROOT|" \
        -e "s|@RECORDS_BOOK@|$BOOK|" \
        -e "s|@RECORDS_COLLECTION@|$COLLECTION|" > "$POLICY" <<'POLICY'
# Written by marbles scripts/install-records-cli.sh; read by the shim that
# `soil-command shims` generates for this command. The environment overrides
# any of the RECORDS_* values below.
RECORDS_ROOT="@RECORDS_ROOT@"
RECORDS_BOOK="@RECORDS_BOOK@"
RECORDS_COLLECTION="@RECORDS_COLLECTION@"
# The variables the command may see. The shim forwards exactly these, by name.
FUNGOS_COMMAND_ENV="MARBLES_ACTOR MARBLES_PROJECT"
MARBLES_ACTOR=${MARBLES_ACTOR:-${USER:-${LOGNAME:-agent}}}
# The project is the nearest `.marbles/project.toml`, the same discovery the
# native CLI does. Finding none is not an error: an unscoped query means every
# project, which is what `mb ready` in a scratch directory should answer.
here=$PWD
while :; do
    for marker in .marbles .marbles-local; do
        manifest=$here/$marker/project.toml
        if [ -r "$manifest" ]; then
            MARBLES_PROJECT=$(sed -n 's/^[[:space:]]*slug[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' "$manifest" | head -n 1)
            [ -n "${MARBLES_PROJECT:-}" ] && break 2
        fi
    done
    [ "$here" = / ] && break
    here=$(dirname "$here")
    [ "$here" = . ] && break
done
unset here marker manifest
# Forwarded by name, so these have to be exported.
export MARBLES_ACTOR MARBLES_PROJECT
POLICY
    chmod 644 "$POLICY"
fi

echo "generating the mb shim with soil-command"
run mkdir -p "$BIN_DIR"
if [ "$DRY_RUN" = yes ]; then
    printf 'would: %s\n' "$SOIL_COMMAND --root $COMMAND_ROOT shims --dir $BIN_DIR"
else
    # Anything already at that path which Soil did not generate is somebody
    # else's, so it is moved aside rather than overwritten. That covers the
    # shim this installer used to write, and a symlink to the native binary.
    if [ -e "$BIN_DIR/mb" ] && ! grep -q "generated by soil-command" "$BIN_DIR/mb" 2>/dev/null; then
        stamp=$(date +%Y%m%dT%H%M%S)
        echo "keeping the existing mb as $BIN_DIR/mb.bak-$stamp"
        mv "$BIN_DIR/mb" "$BIN_DIR/mb.bak-$stamp"
    fi
    if ! "$SOIL_COMMAND" --root "$COMMAND_ROOT" shims --dir "$BIN_DIR"; then
        echo "mb: this soil-command predates \`shims\`; update soil and re-run" >&2
        exit 2
    fi
fi

echo
echo "done. mb now runs the soil:records store at $RECORDS_ROOT, book $BOOK."
printf 'the native binary is still `marbles`: init, import, plan and portfolio live there.\n'
