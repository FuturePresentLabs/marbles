# The repository gate

`tools/check.sh` is the repository's single verification command. Local runs and CI
invoke the same script; CI does not keep its own list of steps.

```bash
tools/check.sh --transcript target/check-transcript.txt
tools/check.sh --self-test
```

## It is fail-closed

A green run means the checks actually ran against a known commit. Anything the gate
cannot establish is a failure, never a skip.

- **Transcript binding.** The first lines are `repo: <path>`, `head: <full sha>`, and
  `tree: clean` or `tree: dirty (N files)`. If `git rev-parse HEAD` or
  `git status --porcelain` fails, the gate fails with `FAIL transcript binding: ...`
  and does not run. It never prints a transcript without a commit binding.
- **Dirty trees fail.** A dirty tree is `FAIL tree: dirty (N files)` unless
  `--allow-dirty` is passed for a local diagnostic; the transcript still records
  `tree: dirty (N files)` so the dirt is never hidden.
- **Missing dependencies fail by name.** `git`, `cargo`, `rustfmt`, `cargo-clippy`,
  and `df` must all be present. A missing one prints
  `FAIL missing dependency: <name>` and exits non-zero. There is no skip path.
- **Disk floor.** The gate reads free space on the filesystem that holds the repo and
  prints the exact measurement with its threshold, e.g.
  `disk free: 25.0 GiB (floor 15.0 GiB)`. Below **15 GiB** the gate refuses to run the
  build; override only for a deliberate diagnostic with `--disk-floor-gib N`.
- **No external service is required today.** If a future check needs one, an
  unreachable service is a failure, not a skip.
- **`SKIP` is a failure.** If a required step exits 0 while printing `SKIP` or
  `SKIPPED`, the gate converts that to `FAIL step: <name> reported SKIP/SKIPPED`.
  A check that did nothing must not look green.

## Required steps

1. Transcript binding (repo, full `HEAD`, clean/dirty file count).
2. Named dependency check.
3. Disk floor with the measured value.
4. `cargo fmt --all -- --check`
5. `cargo clippy --all-targets --all-features` (same form as before: no `-D warnings`;
   the repo has a known warning baseline)
6. `cargo test --all-targets --all-features`
7. `bash -n` over `scripts/*.sh`, `deploy/*.sh`, and `.github/scripts/*.sh`.

Every step prints its exact command and exit code, followed by a final
`RESULT: PASS` or `RESULT: FAIL`. `--transcript PATH` writes that same transcript to a
file.

## Mutation tests: prove the gate can go red

`tools/check.sh --self-test` plants defects in throwaway fixture repos and requires the
gate to fail **for the expected reason**, naming the step or tool that found it:

| mutation | expected red |
| --- | --- |
| bad formatting | `FAIL step: cargo fmt exited <n>` |
| failing test | `FAIL step: cargo test exited <n>` |
| missing dependency | `FAIL missing dependency: cargo` |
| impossible disk floor | `FAIL disk floor: disk free: X.X GiB (floor 999999.0 GiB)` |
| broken transcript binding (failing `git`) | `FAIL transcript binding: git rev-parse HEAD failed` |
| required step prints `SKIP` | `FAIL step: cargo fmt reported SKIP/SKIPPED` |
| dirty tree | `FAIL tree: dirty (N files)` |

The self-test exits 0 only when every planted defect went red for the expected reason;
if any mutation passes, it exits non-zero. `tests/gate.rs` runs it from `cargo test`, so
the gate's own red/green behaviour is part of the suite. That test also asserts
`.github/workflows/ci.yml` invokes `tools/check.sh`, so deleting the CI wiring fails the
build. The fixtures have no dependencies, so the self-test needs no network.

## CI

`.github/workflows/ci.yml` replaces the separate `cargo fmt`/`cargo clippy`/`cargo test`
steps with one gate run against the pull-request merge-result checkout:

```yaml
- run: |
    if ! tools/check.sh --transcript target/check-transcript.txt; then
      cat target/check-transcript.txt || true
      exit 1
    fi
    cat target/check-transcript.txt
```

The registry certificate and image-build steps are unchanged and remain
credential-gated. A PR without a green gate transcript bound to the merge-result `HEAD`
and a clean tree is unlandable.
