//! `gage native` adds, lists, edits, renames, and deletes the names
//! that the user config and a project config give to session sources.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

fn run(home: &Path, cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_gage"))
        .args(args)
        .env("GAGE_HOME", home)
        .env("NO_COLOR", "1")
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

/// Run a command that should succeed and return what it printed
fn ok(home: &Path, cwd: &Path, args: &[&str]) -> String {
    let (code, stdout, stderr) = run(home, cwd, args);
    assert_eq!(code, 0, "{args:?}: {stdout}{stderr}");
    format!("{stdout}{stderr}")
}

/// Run a command that should fail and return what it printed
fn refused(home: &Path, cwd: &Path, args: &[&str]) -> String {
    let (code, stdout, stderr) = run(home, cwd, args);
    assert_eq!(code, 1, "{args:?}: {stdout}{stderr}");
    format!("{stdout}{stderr}")
}

/// The cells of each row of `gage native list`
fn listed(home: &Path, cwd: &Path) -> Vec<Vec<String>> {
    ok(home, cwd, &["native", "list"])
        .lines()
        .filter(|line| line.starts_with('│'))
        .skip(1)
        .map(|line| {
            line.trim_matches('│')
                .split('│')
                .map(|cell| cell.trim().to_string())
                .collect()
        })
        .collect()
}

#[test]
fn a_source_is_added_edited_renamed_and_deleted_in_the_user_config() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (home, cwd) = (home.path(), cwd.path());
    let config = home.join("config.toml");
    fs::write(&config, "# kept\n[backup]\nnative = [\"work\"]\n").unwrap();
    let config_text = || fs::read_to_string(&config).unwrap();

    let out = ok(home, cwd, &["native", "add", "work", "pi:~/w", "-y"]);
    assert!(out.contains("Added native source work (pi:~/w)"), "{out}");
    assert_eq!(
        config_text(),
        "# kept\n[backup]\nnative = [\"work\"]\n\n[native]\nwork = \"pi:~/w\"\n"
    );
    let out = refused(home, cwd, &["native", "add", "work", "codex", "-y"]);
    assert!(out.contains("work is already set in"), "{out}");

    ok(home, cwd, &["native", "edit", "work", "codex:/c", "-y"]);
    assert_eq!(
        ok(home, cwd, &["config", "get", "native.work"]).trim_end(),
        "codex:/c"
    );
    let out = refused(home, cwd, &["native", "edit", "work", "zzz:/c", "-y"]);
    assert!(out.contains("no driver serves scheme \"zzz\""), "{out}");
    let out = refused(home, cwd, &["native", "edit", "none", "pi", "-y"]);
    assert_eq!(
        out.trim_end(),
        "gage native edit: none is not set in a config file"
    );

    // The backup list names the source, so a delete is refused and a
    // rename carries the list along
    let out = refused(home, cwd, &["native", "delete", "work", "-y"]);
    assert!(out.contains("work is listed in backup.native"), "{out}");
    let out = ok(home, cwd, &["native", "rename", "work", "job", "-y"]);
    assert!(out.contains("Renamed native source work to job"), "{out}");
    assert_eq!(
        config_text(),
        "# kept\n[backup]\nnative = [\"job\"]\n\n[native]\njob = \"codex:/c\"\n"
    );

    ok(home, cwd, &["native", "add", "tmp", "claude", "-y"]);
    let out = ok(home, cwd, &["native", "delete", "tmp", "-y"]);
    assert!(out.contains("Deleted 1 native source"), "{out}");
    assert_eq!(
        config_text(),
        "# kept\n[backup]\nnative = [\"job\"]\n\n[native]\njob = \"codex:/c\"\n"
    );
}

#[test]
fn a_project_source_is_written_to_the_project_config_and_listed_with_its_file() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = root.path().canonicalize().unwrap().join("project");
    fs::create_dir_all(project.join("sub")).unwrap();
    let sub = project.join("sub");
    fs::create_dir_all(project.join(".gage")).unwrap();
    let project_config = project.join(".gage/config.toml");

    ok(home, &sub, &["native", "add", "work", "pi:~/w", "-y"]);
    let out = refused(home, &sub, &["native", "add", "rel", "pi:sessions", "-y"]);
    assert!(out.contains("has a relative path"), "{out}");
    ok(
        home,
        &sub,
        &["native", "add", "default", "pi:sessions", "--project", "-y"],
    );
    ok(
        home,
        &sub,
        &["native", "add", "work", "codex", "--project", "-y"],
    );
    assert_eq!(
        fs::read_to_string(&project_config).unwrap(),
        "[native]\ndefault = \"pi:sessions\"\nwork = \"codex\"\n"
    );

    let shown = project_config.display().to_string();
    let row = |cells: [&str; 3]| cells.map(String::from).to_vec();
    assert_eq!(
        listed(home, &sub),
        [
            row([
                "default",
                &format!("pi:{}", project.join("sessions").display()),
                &shown
            ]),
            row(["claude", "claude:", "built in"]),
            row(["codex", "codex:", "built in"]),
            row(["pi", "pi:", "built in"]),
            row(["work", "codex", &shown]),
        ]
    );

    // The project's name is the one in effect, so it is the one a
    // delete removes, and the user's is in effect after it
    ok(home, &sub, &["native", "delete", "work", "default", "-y"]);
    assert_eq!(fs::read_to_string(&project_config).unwrap(), "[native]\n");
    let user = home.join("config.toml").display().to_string();
    assert_eq!(
        listed(home, &sub),
        [
            row(["default", "claude:", "drivers.default"]),
            row(["claude", "claude:", "built in"]),
            row(["codex", "codex:", "built in"]),
            row(["pi", "pi:", "built in"]),
            row(["work", "pi:~/w", &user]),
        ]
    );
}

#[test]
fn a_source_that_is_not_one_can_be_deleted() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (home, cwd) = (home.path(), cwd.path());
    fs::write(home.join("config.toml"), "[native]\nbad = \"zzz:/x\"\n").unwrap();
    refused(home, cwd, &["scan", "--list-scanners"]);
    ok(home, cwd, &["native", "delete", "bad", "-y"]);
    ok(home, cwd, &["scan", "--list-scanners"]);
}
