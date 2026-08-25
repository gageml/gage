//! Cargo test target for the Rune tests in the scanner bundle. A
//! libtest-mimic harness exposes each Rune `#[test]` function as its
//! own trial, so cargo-native filtering and reporting apply:
//!
//! ```shell
//! cargo test -p gage-scan --test rune -- test_stats_mean
//! ```

use libtest_mimic::{Arguments, Failed, Trial};

use gage_scan::test_runner::{self, TestCase, TestOutcome};

fn main() {
    let args = Arguments::from_args();
    let cases = match test_runner::collect_tests() {
        Ok(cases) => cases,
        Err(e) => {
            eprintln!("error: failed to collect Rune tests: {e}");
            std::process::exit(1);
        }
    };
    let trials = cases.into_iter().map(trial).collect();
    libtest_mimic::run(&args, trials).exit();
}

fn trial(case: TestCase) -> Trial {
    Trial::test(case.name.clone(), move || run(case))
}

fn run(case: TestCase) -> Result<(), Failed> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    match rt.block_on(test_runner::run_test(case)) {
        TestOutcome::Pass => Ok(()),
        TestOutcome::Fail(report) => Err(report.into()),
    }
}
