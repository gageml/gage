//! A project config, `<project>/.gage/config.toml`, names a store of
//! the project's own under its `.gage` directory. Commands run in the
//! project directory and below use that store in place of the user's.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn run_with(home: &Path, cwd: &Path, gage_store: Option<&Path>, args: &[&str]) -> (i32, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gage"));
    cmd.args(args)
        .env("GAGE_HOME", home)
        .env_remove("GAGE_STORE")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(store) = gage_store {
        cmd.env("GAGE_STORE", store);
    }
    let output = cmd.output().unwrap();
    let mut text = String::from_utf8(output.stdout).unwrap();
    text.push_str(&String::from_utf8(output.stderr).unwrap());
    (output.status.code().unwrap(), text)
}

fn run(home: &Path, cwd: &Path, args: &[&str]) -> (i32, String) {
    run_with(home, cwd, None, args)
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

fn is_store(path: &Path) -> bool {
    path.join("HEAD").is_file()
}

#[test]
fn a_command_in_a_project_uses_the_store_the_project_config_names() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let project = project(root.path(), "[store]\npath = \".gage/store.git\"\n");
    let store = project.join(".gage/store.git");

    let (code, out) = run(home.path(), &project.join("sub"), &["store", "init"]);
    assert_eq!(code, 0, "{out}");
    assert!(is_store(&store), "{out}");
    assert!(!is_store(&home.path().join("store.git")));

    let (code, out) = run(home.path(), &project, &["config", "get", "store.path"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.trim_end(), store.display().to_string());

    // Outside the project the store is the one under the home
    let (code, out) = run(home.path(), root.path(), &["store", "init"]);
    assert_eq!(code, 0, "{out}");
    assert!(is_store(&home.path().join("store.git")), "{out}");
    let (code, out) = run(home.path(), root.path(), &["config", "get", "store.path"]);
    assert_eq!(code, 1, "{out}");
}

#[test]
fn gage_store_takes_precedence_over_the_project_store() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let project = project(root.path(), "[store]\npath = \".gage/store.git\"\n");
    let named = root.path().join("named.git");

    let (code, out) = run_with(home.path(), &project, Some(&named), &["store", "init"]);
    assert_eq!(code, 0, "{out}");
    assert!(is_store(&named), "{out}");
    assert!(!is_store(&project.join(".gage/store.git")));
}

#[test]
fn a_project_store_outside_the_project_stops_the_command() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let project = project(root.path(), "[store]\npath = \"../elsewhere/store.git\"\n");

    let (code, out) = run(home.path(), &project, &["store", "init"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("store.path"), "{out}");
    assert!(out.contains(".gage/config.toml"), "{out}");
    assert!(!is_store(&home.path().join("store.git")));
    assert!(!root.path().join("elsewhere").exists());

    // The config commands still run, so the file can be corrected
    let (code, out) = run(home.path(), &project, &["native", "list"]);
    assert_eq!(code, 0, "{out}");
}
