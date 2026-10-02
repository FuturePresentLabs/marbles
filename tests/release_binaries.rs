use std::process::Command;

#[test]
fn migration_binary_is_a_runnable_release_artifact() {
    let status = Command::new(env!("CARGO_BIN_EXE_marbles-pg-migrate"))
        .arg("--help")
        .status()
        .expect("run marbles-pg-migrate --help");
    assert!(status.success());
}
