//! The three TUI commands refuse with a `gage query` hint when
//! standard output is not a terminal, instead of panicking inside the
//! terminal setup.

use std::process::{Command, Stdio};

fn run(args: &[&str]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_gage"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let code = output.status.code().unwrap();
    (code, String::from_utf8(output.stderr).unwrap())
}

#[test]
fn scan_view_refuses_without_a_terminal() {
    let (code, stderr) = run(&["scan", "view"]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stderr.trim_end(),
        "gage scan view: standard output is not a terminal; read scans with gage query \
         (tables scan, scan_session, scan_issue, scan_note)"
    );
}

#[test]
fn session_view_refuses_without_a_terminal() {
    let (code, stderr) = run(&["session", "view"]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stderr.trim_end(),
        "gage session view: standard output is not a terminal; read sessions with gage \
         query (tables session, message, tool_use)"
    );
}

#[test]
fn store_view_refuses_without_a_terminal() {
    let (code, stderr) = run(&["store", "view"]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stderr.trim_end(),
        "gage store view: standard output is not a terminal; read the store with gage query"
    );
}
