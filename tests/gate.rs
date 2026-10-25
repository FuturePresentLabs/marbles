//! Contract tests for the repository gate.
//!
//! The gate is only worth trusting if it can go red on a planted defect and if CI
//! runs that same command instead of a reimplementation. Both are asserted here.

use std::path::PathBuf;
use std::process::Command;

/// Contract tests need a repository checkout. The Docker builder deliberately copies
/// only `Cargo.toml`, `src/` and `tests/` (no `.git`, no `tools/`, no `.github/`), so
/// there is nothing to assert there — the repository gate is what enforces these
/// contracts, not the image build. A checkout without `.git` is treated as a bare
/// package build; a checkout *with* `.git` is held to the contract, so deleting the
/// gate script or the CI wiring still fails.
fn checkout_root() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if root.join(".git").exists() {
        Some(root)
    } else {
        eprintln!(
            "skipping gate contract test: {} is a bare package build, not a checkout",
            root.display()
        );
        None
    }
}

/// `tools/check.sh --self-test` must exit 0 and name every mutation it caught.
#[test]
fn self_test_proves_the_gate_can_go_red() {
    let Some(root) = checkout_root() else {
        return;
    };
    let gate = root.join("tools/check.sh");
    assert!(gate.is_file(), "missing gate script: {}", gate.display());

    let output = Command::new("bash")
        .arg(&gate)
        .arg("--self-test")
        .current_dir(&root)
        .output()
        .expect("failed to run tools/check.sh --self-test");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "tools/check.sh --self-test failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    // A self-test that merely prints PASS is not evidence: it must say which planted
    // defect went red and why.
    for case in [
        "bad formatting",
        "failing test",
        "missing dependency: cargo",
        "disk floor",
        "transcript binding",
        "skipped check",
        "dirty tree",
    ] {
        assert!(
            stdout.contains(case),
            "self-test report never named the {case:?} mutation:\n{stdout}"
        );
    }
}

/// CI must invoke the gate itself, not a hand-maintained copy of its steps.
#[test]
fn ci_runs_the_gate_not_a_reimplementation() {
    let Some(root) = checkout_root() else {
        return;
    };
    let ci = root.join(".github/workflows/ci.yml");
    let text = std::fs::read_to_string(&ci).expect("read .github/workflows/ci.yml");

    assert!(
        text.contains("tools/check.sh"),
        "CI does not invoke tools/check.sh; deleting the wiring must fail a test"
    );
    assert!(
        text.contains("--transcript target/check-transcript.txt"),
        "CI must keep the gate transcript as evidence"
    );

    // The gate owns these steps. If they reappear in the workflow, CI has drifted back
    // to a reimplementation and can pass while the gate is red.
    for duplicated in [
        "cargo fmt --all -- --check",
        "cargo clippy --all-targets --all-features",
        "cargo test --all-targets --all-features",
    ] {
        assert!(
            !text.contains(duplicated),
            "CI reimplements a gate step directly: {duplicated}"
        );
    }
}
