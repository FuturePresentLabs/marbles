#!/bin/sh
# `mb` over `soil:records`.
#
# This is the *host* half of the Marbles CLI. The guest is the installed `mb`
# command package: it owns the commands, the claim race and the record shapes.
# This file owns the endpoint, the actor and the project scope, which is the
# split `soil/docs/peer-sync.md` requires - a Spore names a collection, the host
# names the book.
#
# Configuration, in increasing precedence:
#   ~/.config/fungos/marbles/records.env     written by scripts/install-records-cli.sh
#   FUNGOS_RECORDS_ROOT / _BOOK / _COLLECTION
#   SOIL_COMMAND                             path to the command runner
#
# The book has no default. Guessing one means writing a task store next to the
# real one and finding out weeks later.
set -eu

BINDING=${XDG_CONFIG_HOME:-$HOME/.config}/fungos/marbles/records.env
if [ -r "$BINDING" ]; then
    # shellcheck disable=SC1090  # the path is the point
    . "$BINDING"
fi

RECORDS_ROOT=${FUNGOS_RECORDS_ROOT:-${RECORDS_ROOT:-$HOME/.local/share/fungos/records}}
RECORDS_BOOK=${FUNGOS_RECORDS_BOOK:-${RECORDS_BOOK:-}}
RECORDS_COLLECTION=${FUNGOS_RECORDS_COLLECTION:-${RECORDS_COLLECTION:-marbles}}
COMMANDS=${FUNGOS_COMMAND_ROOT:-${COMMANDS:-$HOME/.local/share/fungos/commands}}
ACTOR=${MARBLES_ACTOR:-${USER:-${LOGNAME:-agent}}}

if [ -z "$RECORDS_BOOK" ]; then
    cat >&2 <<'MSG'
mb: no records book configured.

Set one with either:
  scripts/install-records-cli.sh --records-book personal/<you>/marbles
  export FUNGOS_RECORDS_BOOK=personal/<you>/marbles
MSG
    exit 2
fi

if [ -n "${SOIL_COMMAND:-}" ]; then
    RUNNER=$SOIL_COMMAND
elif RUNNER=$(command -v soil-command 2>/dev/null); then
    :
else
    cat >&2 <<'MSG'
mb: soil-command not found. It is the native runner that instantiates the `mb`
    command package with a soil:records binding; build it from a Soil checkout:
      cargo build --release -p soil-command && install -m755 \
        target/release/soil-command ~/.local/bin/soil-command
MSG
    exit 2
fi

# The project a command runs against: `.marbles/project.toml` nearest the
# working directory, the same discovery the native CLI does. Finding none is not
# an error: an unscoped query means every project, which is what `mb ready` in a
# scratch directory should answer.
project=
here=$(pwd -P)
while :; do
    for marker in .marbles .marbles-local; do
        manifest=$here/$marker/project.toml
        if [ -r "$manifest" ]; then
            project=$(sed -n 's/^[[:space:]]*slug[[:space:]]*=[[:space:]]*"\(.*\)".*/\1/p' "$manifest" | head -n 1)
            [ -n "$project" ] && break 2
        fi
    done
    [ "$here" = / ] && break
    here=$(dirname "$here")
    [ "$here" = . ] && break
done

exec "$RUNNER" \
    --root "$COMMANDS" \
    --records-root "$RECORDS_ROOT" \
    --book "$RECORDS_BOOK" \
    --collection "$RECORDS_COLLECTION" \
    --env "MARBLES_ACTOR=$ACTOR" \
    --env "MARBLES_PROJECT=$project" \
    run mb "$@"
