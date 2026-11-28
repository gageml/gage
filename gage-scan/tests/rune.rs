//! Cargo test target for the Rune tests of scanners. A libtest-mimic
//! harness exposes each Rune `#[test]` function as its own trial, so
//! cargo-native filtering and reporting apply. `GAGE_SCANNER_TESTS`
//! names the directory of scanners to test:
//!
//! ```shell
//! GAGE_SCANNER_TESTS=../gage-scanners cargo test -p gage-scan --test rune
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
