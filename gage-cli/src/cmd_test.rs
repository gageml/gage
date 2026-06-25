use clap::Args;
use console::style;

use gage_scan::test_runner::{TestOutcome, TestResult};

#[derive(Args)]
pub struct TestArgs {
    /// Filter tests by name (substring match, repeatable)
    #[arg(value_name = "FILTER")]
    filters: Vec<String>,

    /// Display one character per test instead of one line
    #[arg(short, long)]
    quiet: bool,

    /// Stop after the first failure
    #[arg(short, long)]
    fail_fast: bool,
}

pub async fn run(args: TestArgs) {
    let quiet = args.quiet;

    let summary = match gage_scan::test_runner::run_tests(&args.filters, args.fail_fast, |result| {
        if quiet {
            print_quiet(result);
        } else {
            print_result(result);
        }
    })
    .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    if quiet && summary.total > 0 {
        eprintln!();
    }

    if summary.total == 0 {
        eprintln!("Nothing to test");
        return;
    }

    let status = if summary.failed > 0 || summary.build_errors > 0 {
        style("FAILED").red().bold()
    } else {
        style("ok").green().bold()
    };

    let mut parts = vec![
        format!("{} passed", summary.passed),
        format!("{} failed", summary.failed),
    ];
    if summary.build_errors > 0 {
        parts.push(format!("{} build errors", summary.build_errors));
    }
    if summary.filtered > 0 {
        parts.push(format!("{} filtered out", summary.filtered));
    }
    eprintln!("test result: {status}. {}", parts.join("; "));

    if summary.failed > 0 || summary.build_errors > 0 {
        std::process::exit(1);
    }
}

fn print_result(result: &TestResult) {
    match &result.outcome {
        TestOutcome::Pass => {
            eprintln!("{}: {}", style("pass").green().bright(), result.name);
        }
        TestOutcome::Fail(report) => {
            eprint!("{report}");
        }
    }
}

fn print_quiet(result: &TestResult) {
    match &result.outcome {
        TestOutcome::Pass => eprint!("{}", style(".").green().bright()),
        TestOutcome::Fail(_) => eprint!("{}", style("F").red().bright()),
    }
}
