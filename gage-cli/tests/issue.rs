//! `gage issue list` and `gage issue show` read their rows with SQL
//! that the compiler does not check against the schema. These tests
//! run each statement against a store that holds an issue.

use std::path::Path;
use std::process::{Command, Stdio};

fn run(home: &Path, args: &[&str]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_gage"))
        .args(args)
        .env("GAGE_HOME", home)
        .env("USER", "tester")
        .env_remove("GAGE_STORE")
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let mut text = String::from_utf8(output.stdout).unwrap();
    text.push_str(&String::from_utf8(output.stderr).unwrap());
    (output.status.code().unwrap(), text)
}

fn ok(home: &Path, args: &[&str]) -> String {
    let (code, out) = run(home, args);
    assert_eq!(code, 0, "{args:?}: {out}");
    out
}

/// The cells of each table row in `out`, without the border rows.
fn rows(out: &str) -> Vec<Vec<String>> {
    out.lines()
        .filter(|line| line.starts_with('│'))
        .map(|line| {
            line.trim_matches('│')
                .split('│')
                .map(|cell| cell.trim().to_string())
                .collect()
        })
        .collect()
}

/// A store under `home` with one open issue, and the issue's ID.
fn store_with_issue(home: &Path) -> String {
    ok(home, &["store", "init"]);
    ok(
        home,
        &[
            "issue",
            "add",
            "--title",
            "First issue",
            "--description",
            "Body",
            "-y",
        ],
    );
    let listed = ok(home, &["issue", "list", "--full-id"]);
    let id = rows(&listed)
        .into_iter()
        .nth(1)
        .and_then(|row| row.into_iter().next());
    id.unwrap_or_else(|| panic!("no issue row in {listed}"))
}

#[test]
fn list_shows_open_issues_and_closed_issues_separately() {
    let home = tempfile::tempdir().unwrap();
    let id = store_with_issue(home.path());
    assert_eq!(id.len(), 26, "{id}");

    let open = ok(home.path(), &["issue", "list", "--full-id"]);
    let listed = rows(&open);
    assert_eq!(
        listed[0],
        ["Id", "Name", "Title", "Scan", "Status", "Created"]
    );
    assert_eq!(
        listed[1][..5],
        [&id, "user-issue", "First issue", "", "open"]
    );
    assert_eq!(listed.len(), 2, "{open}");
    assert_eq!(
        ok(home.path(), &["issue", "list", "-c"]).trim_end(),
        "No closed issues"
    );
    assert_eq!(
        ok(home.path(), &["issue", "list", "--name", "other"]).trim_end(),
        "No issues found"
    );

    ok(
        home.path(),
        &["issue", "close", &id, "--wontfix", "-m", "Not needed", "-y"],
    );
    assert_eq!(
        ok(home.path(), &["issue", "list"]).trim_end(),
        "No open issues (use -c to show closed issues)"
    );
    let closed = rows(&ok(home.path(), &["issue", "list", "-c", "--full-id"]));
    assert_eq!(
        closed[1][..5],
        [&id, "user-issue", "First issue", "", "wontfix"]
    );
}

#[test]
fn show_reports_the_attributes_and_events_of_an_issue() {
    let home = tempfile::tempdir().unwrap();
    let id = store_with_issue(home.path());

    let shown = rows(&ok(home.path(), &["issue", "show", &id[..8]]));
    let value = |label: &str| {
        let row = shown
            .iter()
            .find(|row| row[0] == label)
            .unwrap_or_else(|| panic!("no {label} row in {shown:?}"));
        row[1].clone()
    };
    assert_eq!(value("id"), id);
    assert_eq!(value("name"), "user-issue");
    assert_eq!(value("title"), "First issue");
    assert_eq!(value("status"), "open");
    assert_eq!(value("description"), "Body");
    assert_eq!(value("sessions"), "");
    assert_eq!(value("author"), "user:tester");
    assert_eq!(value("scan"), "");
    assert_eq!(value("created"), value("modified"));
    let events = value("events");
    assert!(
        events.starts_with("created open · user:tester · "),
        "{events}"
    );
}
