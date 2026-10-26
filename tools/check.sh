#!/usr/bin/env bash
# tools/check.sh — the repository gate.
#
# This is the *only* verification command: local runs and CI invoke it verbatim.
# It is fail-closed on purpose:
#
#   * the transcript is bound to a repo path, the full HEAD sha, and a clean/dirty
#     file count; if the binding cannot be read the gate fails rather than guessing;
#   * a missing tool (`git`, `cargo`, `rustfmt`, `cargo-clippy`, `df`) is a named
#     failure, never a skip;
#   * a print of `SKIP`/`SKIPPED` from a required step is converted to a failure —
#     a check that did nothing must not look green;
#   * the disk floor is enforced and printed with its exact measurement;
#   * every step prints its exact command and exit code.
#
# Usage:
#   tools/check.sh [--transcript PATH] [--allow-dirty] [--disk-floor-gib N]
#   tools/check.sh --self-test
#
# `--self-test` plants defects in throwaway fixture repos and proves the gate goes
# red for the expected reason (naming the step or tool). It exits 0 only when every
# planted defect was caught.
#
# See docs/ci.md.

set -uo pipefail

PROG="$(basename "$0")"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ALLOW_DIRTY=0
TRANSCRIPT=""
SELF_TEST=0
DISK_FLOOR_GIB="15"

usage() {
    cat <<EOF
usage: $PROG [--transcript PATH] [--allow-dirty] [--disk-floor-gib N]
       $PROG --self-test

  --transcript PATH    also write the transcript to PATH
  --allow-dirty        local diagnostic only: tolerate a dirty tree (still recorded)
  --disk-floor-gib N   required free space in GiB (default 15)
  --self-test          prove the gate goes red on planted defects
EOF
}

die_usage() {
    printf 'FAIL usage: %s\n' "$1" >&2
    usage >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --transcript)
            [ $# -ge 2 ] || die_usage "--transcript needs a path"
            TRANSCRIPT="$2"
            shift 2
            ;;
        --transcript=*)
            TRANSCRIPT="${1#--transcript=}"
            shift
            ;;
        --allow-dirty)
            ALLOW_DIRTY=1
            shift
            ;;
        --disk-floor-gib)
            [ $# -ge 2 ] || die_usage "--disk-floor-gib needs a number"
            DISK_FLOOR_GIB="$2"
            shift 2
            ;;
        --disk-floor-gib=*)
            DISK_FLOOR_GIB="${1#--disk-floor-gib=}"
            shift
            ;;
        --self-test)
            SELF_TEST=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die_usage "unknown argument: $1"
            ;;
    esac
done

case "$DISK_FLOOR_GIB" in
    ''|*[!0-9.]*) die_usage "--disk-floor-gib must be a number, got: $DISK_FLOOR_GIB" ;;
esac

fail_closed() {
    # $1 = what failed (step / tool / binding), $2 = the evidence.
    printf 'FAIL %s: %s\n' "$1" "$2"
    printf 'RESULT: FAIL\n'
    exit 1
}

# ---------------------------------------------------------------------------
# The gate proper
# ---------------------------------------------------------------------------

gate() {
    # Resolve the repository root. If git cannot even find the toplevel, fall back to
    # the current directory and let transcript binding produce the named failure.
    local ROOT
    ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || ROOT="$(pwd)"
    [ -n "$ROOT" ] || ROOT="$(pwd)"

    # 1. Transcript binding: repo, full HEAD, clean/dirty count. Never optional.
    printf 'repo: %s\n' "$ROOT"

    local head_sha
    if ! head_sha="$(cd "$ROOT" && git rev-parse HEAD 2>/dev/null)" || [ -z "$head_sha" ]; then
        fail_closed "transcript binding" "git rev-parse HEAD failed; the gate will not run unbound"
    fi
    printf 'head: %s\n' "$head_sha"

    local status
    if ! status="$(cd "$ROOT" && git status --porcelain 2>/dev/null)"; then
        fail_closed "transcript binding" "git status --porcelain failed; the gate will not run unbound"
    fi

    local file_count
    file_count="$(printf '%s\n' "$status" | awk 'NF > 0 { n++ } END { print n + 0 }')"
    if [ "$file_count" -eq 0 ]; then
        printf 'tree: clean\n'
    else
        printf 'tree: dirty (%s files)\n' "$file_count"
        if [ "$ALLOW_DIRTY" -ne 1 ]; then
            fail_closed "tree" "dirty ($file_count files); commit, stash, or pass --allow-dirty for a local diagnostic only"
        fi
    fi

    # 2. Dependencies: fail closed, named. A missing tool never becomes a skip.
    local dep
    for dep in git cargo rustfmt cargo-clippy df; do
        if ! command -v "$dep" >/dev/null 2>&1; then
            fail_closed "missing dependency" "$dep"
        fi
    done
    printf 'deps: git cargo rustfmt cargo-clippy df\n'

    # 3. Disk floor on the filesystem that holds the repo.
    local df_out avail_kb gib floor
    if ! df_out="$(df -Pk "$ROOT" 2>/dev/null)"; then
        fail_closed "disk floor" "df failed for $ROOT"
    fi
    avail_kb="$(printf '%s\n' "$df_out" | awk 'NR == 2 { print $4 }')"
    if [ -z "$avail_kb" ]; then
        fail_closed "disk floor" "df produced no available-KiB field for $ROOT"
    fi
    gib="$(awk -v k="$avail_kb" 'BEGIN { printf "%.1f", k / 1048576 }')"
    floor="$(awk -v f="$DISK_FLOOR_GIB" 'BEGIN { printf "%.1f", f }')"
    printf 'disk free: %s GiB (floor %s GiB)\n' "$gib" "$floor"
    if awk -v k="$avail_kb" -v f="$DISK_FLOOR_GIB" 'BEGIN { exit !(k / 1048576 < f) }'; then
        fail_closed "disk floor" "disk free: $gib GiB (floor $floor GiB) is below the required floor"
    fi

    # 4-6. Required cargo steps. `eval` is safe here: the commands are fixed literals.
    run_step "cargo fmt" \
        "cargo fmt --all -- --check"
    run_step "cargo clippy" \
        "cargo clippy --all-targets --all-features"
    run_step "cargo test" \
        "cargo test --all-targets --all-features"

    # 7. Shell syntax over the repo's scripts.
    run_shell_syntax_step "$ROOT"

    printf '\nRESULT: PASS\n'
}

run_step() {
    local name="$1" cmd="$2" out rc
    printf '\n=== step: %s\n' "$name"
    printf '$ %s\n' "$cmd"
    out="$(cd "$ROOT" && eval "$cmd" 2>&1)"
    rc=$?
    if [ -n "$out" ]; then
        printf '%s\n' "$out"
    fi
    printf 'exit: %d\n' "$rc"
    if [ "$rc" -ne 0 ]; then
        fail_closed "step" "$name exited $rc"
    fi
    if printf '%s\n' "$out" | grep -Eq '(^|[^[:alnum:]_])(SKIP|SKIPPED)([^[:alnum:]_]|$)'; then
        fail_closed "step" "$name reported SKIP/SKIPPED; a skipped check is not a pass"
    fi
}

run_shell_syntax_step() {
    local ROOT="$1"
    printf '\n=== step: shell syntax (bash -n)\n'

    shopt -s nullglob
    local files=( "$ROOT"/scripts/*.sh "$ROOT"/deploy/*.sh "$ROOT"/.github/scripts/*.sh )
    shopt -u nullglob

    if [ "${#files[@]}" -eq 0 ]; then
        printf '$ bash -n (no shell scripts present)\n'
        printf 'exit: 0\n'
        return 0
    fi

    local f rel out rc overall=0
    for f in "${files[@]}"; do
        rel="${f#"$ROOT"/}"
        printf '$ bash -n %s\n' "$rel"
        out="$(bash -n "$f" 2>&1)"
        rc=$?
        if [ -n "$out" ]; then
            printf '%s\n' "$out"
        fi
        printf 'exit: %d\n' "$rc"
        if [ "$rc" -ne 0 ]; then
            overall=$rc
        fi
    done
    printf 'step exit: %d\n' "$overall"
    if [ "$overall" -ne 0 ]; then
        fail_closed "step" "shell syntax (bash -n) exited $overall"
    fi
}

# ---------------------------------------------------------------------------
# Self-test: plant defects, prove the gate names them
# ---------------------------------------------------------------------------

SELFTEST_FAILURES=0
ST_ROOT=""

selftest_cleanup() {
    if [ -n "$ST_ROOT" ] && [ -d "$ST_ROOT" ]; then
        rm -rf "$ST_ROOT"
    fi
}
trap selftest_cleanup EXIT

selftest_note_fail() {
    printf 'SELF-TEST FAIL %s: %s\n' "$1" "$2"
    if [ -n "${3:-}" ]; then
        printf '%s\n' "$3" | sed 's/^/    /'
    fi
    SELFTEST_FAILURES=$((SELFTEST_FAILURES + 1))
}

assert_red() {
    # $1 case name, $2 expected regex, $3 rc, $4 transcript
    local name="$1" pattern="$2" rc="$3" out="$4"
    if [ "$rc" -eq 0 ]; then
        selftest_note_fail "$name" "gate exited 0 on a planted defect (expected red)" "$out"
        return 1
    fi
    if ! printf '%s\n' "$out" | grep -Eq -- "$pattern"; then
        selftest_note_fail "$name" "transcript did not match /$pattern/" "$out"
        return 1
    fi
    printf 'mutation %-24s red: matched /%s/ (exit %d)\n' "$name" "$pattern" "$rc"
    return 0
}

assert_contains() {
    local name="$1" needle="$2" out="$3"
    if ! printf '%s\n' "$out" | grep -Fq -- "$needle"; then
        selftest_note_fail "$name" "transcript did not contain '$needle'" "$out"
        return 1
    fi
    return 0
}

new_fixture() {
    # A dependency-free crate: no network, no lock contention with the outer build.
    local dir="$1"
    mkdir -p "$dir/src"
    cat > "$dir/Cargo.toml" <<'TOML'
[package]
name = "gatefixture"
version = "0.0.0"
edition = "2021"

[dependencies]
TOML
    cat > "$dir/.gitignore" <<'GI'
/target
GI
    cat > "$dir/src/lib.rs" <<'RS'
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(1, 2), 3);
    }
}
RS
    git -C "$dir" init -q
    git -C "$dir" -c user.email=gate@test.invalid -c user.name=gate add -A
    git -C "$dir" -c user.email=gate@test.invalid -c user.name=gate commit -qm init
}

selftest() {
    local need
    for need in git cargo rustfmt cargo-clippy df mktemp; do
        if ! command -v "$need" >/dev/null 2>&1; then
            fail_closed "missing dependency" "$need"
        fi
    done

    ST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/marbles-gate-selftest.XXXXXX")" \
        || fail_closed "self-test" "mktemp failed"

    local gate="$SCRIPT_DIR/check.sh"
    local d out rc

    printf 'self-test: fixtures under %s\n' "$ST_ROOT"

    # 1. Bad formatting: committed, so the failure must come from the fmt step.
    d="$ST_ROOT/bad-format"
    new_fixture "$d"
    cat > "$d/src/lib.rs" <<'RS'
pub fn add(a:i32,b:i32)->i32{a+b}
RS
    git -C "$d" -c user.email=gate@test.invalid -c user.name=gate commit -qam "bad formatting"
    out="$(cd "$d" && env -u CARGO_TARGET_DIR bash "$gate" 2>&1)"
    rc=$?
    assert_red "bad formatting" 'FAIL step: cargo fmt exited [0-9]+' "$rc" "$out"

    # 2. Failing test: formatted code, so the failure must come from the test step.
    d="$ST_ROOT/bad-test"
    new_fixture "$d"
    cat > "$d/src/lib.rs" <<'RS'
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(2, 2), 5);
    }
}
RS
    git -C "$d" -c user.email=gate@test.invalid -c user.name=gate commit -qam "failing test"
    out="$(cd "$d" && env -u CARGO_TARGET_DIR bash "$gate" 2>&1)"
    rc=$?
    assert_red "failing test" 'FAIL step: cargo test exited [0-9]+' "$rc" "$out"

    # 3. Missing dependency: a PATH without cargo must name cargo and exit non-zero.
    d="$ST_ROOT/no-cargo"
    new_fixture "$d"
    local bindir="$ST_ROOT/no-cargo-bin"
    mkdir -p "$bindir"
    ln -s "$(command -v git)" "$bindir/git"
    ln -s "$(command -v df)" "$bindir/df"
    out="$(cd "$d" && env -u CARGO_TARGET_DIR PATH="$bindir:/usr/bin:/bin" /bin/bash "$gate" 2>&1)"
    rc=$?
    assert_red "missing dependency: cargo" 'FAIL missing dependency: cargo' "$rc" "$out"

    # 4. Disk floor: an impossible floor must print the measured free space + threshold.
    d="$ST_ROOT/disk-floor"
    new_fixture "$d"
    out="$(cd "$d" && env -u CARGO_TARGET_DIR bash "$gate" --disk-floor-gib 999999 2>&1)"
    rc=$?
    assert_red "disk floor" \
        'FAIL disk floor: disk free: [0-9]+\.[0-9] GiB \(floor 999999\.0 GiB\)' "$rc" "$out"

    # 5. Broken binding: a git that cannot report HEAD must fail by name, never skip.
    d="$ST_ROOT/broken-git"
    new_fixture "$d"
    bindir="$ST_ROOT/broken-git-bin"
    mkdir -p "$bindir"
    cat > "$bindir/git" <<'SH'
#!/bin/sh
exit 1
SH
    chmod +x "$bindir/git"
    out="$(cd "$d" && env -u CARGO_TARGET_DIR PATH="$bindir:/usr/bin:/bin" /bin/bash "$gate" 2>&1)"
    rc=$?
    assert_red "transcript binding" 'FAIL transcript binding: git rev-parse HEAD failed' "$rc" "$out"

    # 6. Skipped check: a step that exits 0 while printing SKIP must become a FAIL.
    d="$ST_ROOT/skip-step"
    new_fixture "$d"
    bindir="$ST_ROOT/skip-step-bin"
    mkdir -p "$bindir"
    cat > "$bindir/cargo" <<'SH'
#!/bin/sh
echo "SKIP: pretending there is nothing to check"
exit 0
SH
    chmod +x "$bindir/cargo"
    ln -s "$(command -v git)" "$bindir/git"
    ln -s "$(command -v df)" "$bindir/df"
    local toolbin
    toolbin="$(dirname "$(command -v rustfmt)")"
    out="$(cd "$d" && env -u CARGO_TARGET_DIR PATH="$bindir:$toolbin:/usr/bin:/bin" /bin/bash "$gate" 2>&1)"
    rc=$?
    assert_red "skipped check" 'FAIL step: cargo fmt reported SKIP/SKIPPED' "$rc" "$out"

    # 7. Dirty tree is refused, and --allow-dirty records the dirt instead of hiding it.
    d="$ST_ROOT/dirty-tree"
    new_fixture "$d"
    printf '\n// uncommitted edit\n' >> "$d/src/lib.rs"
    out="$(cd "$d" && env -u CARGO_TARGET_DIR bash "$gate" 2>&1)"
    rc=$?
    assert_red "dirty tree" 'FAIL tree: dirty \(1 files\)' "$rc" "$out"
    out="$(cd "$d" && env -u CARGO_TARGET_DIR bash "$gate" --allow-dirty --disk-floor-gib 999999 2>&1)"
    rc=$?
    if [ "$rc" -eq 0 ]; then
        selftest_note_fail "allow-dirty diagnostic" "expected the disk-floor stop" "$out"
    elif printf '%s\n' "$out" | grep -Fq -- 'tree: dirty (1 files)' \
        && printf '%s\n' "$out" | grep -Fq -- 'FAIL disk floor:'; then
        printf 'mutation %-24s red: dirt recorded as "tree: dirty (1 files)"\n' "allow-dirty diagnostic"
    else
        selftest_note_fail "allow-dirty diagnostic" "transcript did not record the dirty tree" "$out"
    fi

    if [ "$SELFTEST_FAILURES" -ne 0 ]; then
        printf 'SELF-TEST: FAIL (%d mutation(s) went green)\n' "$SELFTEST_FAILURES"
        printf 'RESULT: FAIL\n'
        exit 1
    fi
    printf 'SELF-TEST: PASS — every planted defect went red for the expected reason\n'
    printf 'RESULT: PASS\n'
    exit 0
}

# ---------------------------------------------------------------------------

if [ "$SELF_TEST" -eq 1 ]; then
    if [ -n "$TRANSCRIPT" ]; then
        mkdir -p "$(dirname "$TRANSCRIPT")" || fail_closed "transcript" "cannot create $TRANSCRIPT"
        # shellcheck disable=SC2317
        selftest 2>&1 | tee "$TRANSCRIPT"
        exit "${PIPESTATUS[0]}"
    fi
    selftest
    exit $?
fi

if [ -n "$TRANSCRIPT" ]; then
    mkdir -p "$(dirname "$TRANSCRIPT")" || fail_closed "transcript" "cannot create $TRANSCRIPT"
    gate 2>&1 | tee "$TRANSCRIPT"
    exit "${PIPESTATUS[0]}"
fi

gate
exit $?
