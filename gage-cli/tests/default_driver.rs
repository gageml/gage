//! The `drivers.default` setting names the default driver. A config
//! that names a driver the build does not have stops every command
//! but `gage config`, which is how the setting is corrected.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

fn run(home: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_gage"))
        .args(args)
        .env("GAGE_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    (
        output.status.code().unwrap(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn default_driver_is_claude_when_config_names_none() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = run(home.path(), &["config", "get", "drivers.default"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim_end(), "claude");
}

#[test]
fn unknown_default_driver_stops_commands_and_leaves_config_usable() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    fs::write(&config, "[drivers]\ndefault = \"nope\"\n").unwrap();

    let (code, _stdout, stderr) = run(home.path(), &["scan", "--list-scanners"]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stderr.trim_end(),
        format!(
            "{}: drivers.default: \"nope\" is not a driver in this build; drivers: claude",
            config.display()
        )
    );

    let (code, stdout, stderr) = run(home.path(), &["config", "get", "drivers.default"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim_end(), "nope");
}

#[test]
fn named_default_driver_lets_commands_run() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("config.toml"),
        "[drivers]\ndefault = \"claude\"\n",
    )
    .unwrap();
    let (code, _stdout, stderr) = run(home.path(), &["scan", "--list-scanners"]);
    assert_eq!(code, 0, "{stderr}");
}
