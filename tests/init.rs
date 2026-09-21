#[cfg(unix)]
#[test]
fn init_owns_hooks_and_migrates_a_legacy_beads_store() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("demo");
    let bin = temp.path().join("bin");
    let home = temp.path().join("home");
    std::fs::create_dir_all(repo.join(".beads")).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let bd = bin.join("bd");
    std::fs::write(
        &bd,
        "#!/bin/sh\nprintf '%s\\n' '{\"_type\":\"issue\",\"id\":\"demo-1\",\"title\":\"legacy\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"2026-09-20T00:00:00Z\",\"updated_at\":\"2026-09-20T00:00:00Z\"}'\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&bd).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&bd, permissions).unwrap();

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let status = Command::new(env!("CARGO_BIN_EXE_marbles"))
        .args(["-C", repo.to_str().unwrap(), "init", "--quiet"])
        .env("MARBLES_HOME", &home)
        .env("PATH", path)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(repo.join(".marbles/project.toml").is_file());
    assert!(repo.join(".marbles/beads-migrated").is_file());
    let agents = std::fs::read_to_string(repo.join("AGENTS.md")).unwrap();
    assert!(agents.contains("BEGIN MARBLES"));

    let output = Command::new(env!("CARGO_BIN_EXE_marbles"))
        .args(["-C", repo.to_str().unwrap(), "show", "demo-1"])
        .env("MARBLES_HOME", &home)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().contains("legacy"));
}
