//! The `native.default` setting is the session source a command reads
//! when it is given none. A project config, `<project>/.gage/config.toml`,
//! sets it for commands run in the project directory and below, in
//! place of the user config's.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn run(home: &Path, cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_gage"))
        .args(args)
        .env("GAGE_HOME", home)
        .current_dir(cwd)
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

/// A project directory with a `sub` directory and the given project
/// config, canonical so that it compares equal to the directory the
/// command reports.
fn project(root: &Path, config: &str) -> PathBuf {
    let project = root.canonicalize().unwrap().join("project");
    fs::create_dir_all(project.join("sub")).unwrap();
    fs::create_dir_all(project.join(".gage")).unwrap();
    fs::write(project.join(".gage/config.toml"), config).unwrap();
    project
}

#[test]
fn no_default_native_source_is_set_until_a_config_sets_one() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (code, _stdout, stderr) = run(
        home.path(),
        cwd.path(),
        &["config", "get", "native.default"],
    );
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stderr.trim_end(),
        "gage config get: no setting named native.default"
    );
}

#[test]
fn a_project_config_replaces_the_default_native_source_of_the_user_config() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("config.toml"),
        "[native]\ndefault = \"codex\"\nwork = \"pi:~/work\"\n",
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let project = project(root.path(), "[native]\ndefault = \"claude:sessions\"\n");

    let get = |cwd: &Path, name: &str| {
        let (code, stdout, stderr) = run(home.path(), cwd, &["config", "get", name]);
        assert_eq!(code, 0, "{stderr}");
        stdout.trim_end().to_string()
    };
    let sessions = format!("claude:{}", project.join("sessions").display());
    assert_eq!(get(&project, "native.default"), sessions);
    assert_eq!(get(&project.join("sub"), "native.default"), sessions);
    assert_eq!(get(&project.join("sub"), "native.work"), "pi:~/work");
    assert_eq!(get(root.path(), "native.default"), "codex");

    let (code, _stdout, stderr) = run(home.path(), &project.join("sub"), &["scanner", "list"]);
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn a_project_config_that_sets_anything_but_native_stops_commands() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let project = project(root.path(), "[drivers]\ndefault = \"pi\"\n");
    let (code, _stdout, stderr) = run(home.path(), &project, &["scanner", "list"]);
    assert_eq!(code, 1, "{stderr}");
    let config = project.join(".gage/config.toml");
    assert!(
        stderr.starts_with(&format!("reading {}: ", config.display())),
        "{stderr}"
    );
    assert!(stderr.contains("unknown field `drivers`"), "{stderr}");
}

#[test]
fn a_default_native_source_that_is_not_a_source_stops_commands() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let project = project(root.path(), "[native]\ndefault = \"work\"\n");
    let (code, _stdout, stderr) = run(home.path(), &project, &["scanner", "list"]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stderr.trim_end(),
        format!(
            "{}: native.default: \"work\" is not a source; write it as SCHEME:PATH or SCHEME, \
             where SCHEME is one of claude, codex, pi",
            project.join(".gage/config.toml").display()
        )
    );
}

#[test]
fn a_relative_path_in_the_user_config_stops_commands() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    fs::write(&config, "[native]\ndefault = \"pi:sessions\"\n").unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (code, _stdout, stderr) = run(home.path(), cwd.path(), &["scanner", "list"]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stderr.trim_end(),
        format!(
            "{}: native.default: \"pi:sessions\" has a relative path; write an absolute path \
             or a path under ~",
            config.display()
        )
    );
}
