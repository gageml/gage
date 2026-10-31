//! Each bench runs at its smallest size, so a bench whose queries or
//! store calls no longer match the code fails the test suite instead
//! of the next person to run it. The timings are not read.

use std::process::Command;

fn run(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_gage-bench"))
        .args(args)
        .arg("--no-save")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "gage-bench {args:?}: {}\n{stdout}\n{stderr}",
        output.status
    );
    assert!(!stdout.contains("Results:"), "{stdout}");
    stdout
}

#[test]
fn store_bench_runs_at_its_smallest_size() {
    let stdout = run(&[
        "store",
        "--notes",
        "2",
        "--sessions",
        "2",
        "--large-sessions",
        "1",
        "--large-kb",
        "64",
        "--datasets",
        "1",
        "--dataset-size",
        "1",
        "--iterations",
        "1",
        "--random-reads",
        "2",
    ]);
    assert!(stdout.contains("session add"), "{stdout}");
}

#[test]
fn stored_session_list_bench_runs_at_its_smallest_size() {
    let stdout = run(&[
        "stored-session-list",
        "--sessions",
        "2",
        "--iterations",
        "1",
    ]);
    assert!(stdout.contains("warm.list_full"), "{stdout}");
}
