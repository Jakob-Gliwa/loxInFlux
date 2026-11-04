use std::process::Command;

#[test]
fn check_config_warns_on_ignored_keys_without_logging_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[telegraf]\nprotocol='execd'\nround_floats=true\nround_decimals='private-sentinel'\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_loxtotelegraf"))
        .env_remove("RUST_LOG")
        .args([
            "--config-file",
            path.to_str().unwrap(),
            "--check-config",
            "--log-level",
            "warn",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("W!"));
    assert!(stderr.contains("telegraf.round_floats"));
    assert!(stderr.contains("telegraf.round_decimals"));
    assert!(stderr.contains("has no effect"));
    assert!(stderr.contains("future version"));
    assert!(!stderr.contains("private-sentinel"));
}
