use rune::runtime::Vm;
use rune::sync::Arc as RuneArc;
use rune::{Diagnostics, Source, Sources};

fn run_rune(script: &str) -> rune::Value {
    let rt = gage_scan::runner::TestRuntime::new();
    let mut context = rune_modules::default_context().unwrap();
    context.install(rt.types_module().unwrap()).unwrap();
    context.install(rt.gage_module().unwrap()).unwrap();
    let runtime = RuneArc::try_new(context.runtime().unwrap()).unwrap();

    let mut sources = Sources::new();
    sources
        .insert(Source::new("test", script).unwrap())
        .unwrap();

    let mut diagnostics = Diagnostics::new();
    let result = rune::prepare(&mut sources)
        .with_context(&context)
        .with_diagnostics(&mut diagnostics)
        .build();

    if !diagnostics.is_empty() {
        let mut writer =
            rune::termcolor::StandardStream::stderr(rune::termcolor::ColorChoice::Auto);
        diagnostics.emit(&mut writer, &sources).unwrap();
    }

    let unit = RuneArc::try_new(result.unwrap()).unwrap();
    let mut vm = Vm::new(runtime, unit);
    vm.call(["main"], ()).unwrap()
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the test holds the only live handle"
)]
fn args_variant_round_trips() {
    let val = run_rune(
        r#"
        use gage::Error;
        pub fn main() {
            let e = Error::Args("bad input");
            match e {
                Error::Args(msg) => msg,
                _ => "wrong variant",
            }
        }
        "#,
    );
    assert_eq!(rune::from_value::<String>(val).unwrap(), "bad input");
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the test holds the only live handle"
)]
fn db_variant_round_trips() {
    let val = run_rune(
        r#"
        use gage::Error;
        pub fn main() {
            let e = Error::Db("locked");
            match e {
                Error::Db(msg) => msg,
                _ => "wrong variant",
            }
        }
        "#,
    );
    assert_eq!(rune::from_value::<String>(val).unwrap(), "locked");
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the test holds the only live handle"
)]
fn http_variant_carries_status_and_body() {
    let val = run_rune(
        r#"
        use gage::Error;
        pub fn main() {
            let e = Error::Http { status: 503, body: "overloaded" };
            match e {
                Error::Http { status, body } => format!("{}-{}", status, body),
                _ => "wrong variant".to_string(),
            }
        }
        "#,
    );
    assert_eq!(rune::from_value::<String>(val).unwrap(), "503-overloaded");
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the test holds the only live handle"
)]
fn http_getters_accessible_without_destructure() {
    let val = run_rune(
        r#"
        use gage::Error;
        pub fn main() {
            let e = Error::Http { status: 418, body: "teapot" };
            format!("{}: {}", e.status, e.body)
        }
        "#,
    );
    assert_eq!(rune::from_value::<String>(val).unwrap(), "418: teapot");
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the test holds the only live handle"
)]
fn duplicate_variant_carries_prev_and_new() {
    let val = run_rune(
        r#"
        use gage::Error;
        pub fn main() {
            let e = Error::Duplicate { prev: "p", new: "n" };
            match e {
                Error::Duplicate { prev, new } => format!("{}-{}", prev, new),
                _ => "wrong variant".to_string(),
            }
        }
        "#,
    );
    assert_eq!(rune::from_value::<String>(val).unwrap(), "p-n");
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the test holds the only live handle"
)]
fn each_variant_constructs() {
    let val = run_rune(
        r#"
        use gage::Error;
        pub fn main() {
            let kinds = [
                Error::Args("a"),
                Error::Db("d"),
                Error::Config("c"),
                Error::Network("n"),
                Error::Http { status: 0, body: "" },
                Error::Decode("e"),
                Error::Template("t"),
            ];
            kinds.len()
        }
        "#,
    );
    assert_eq!(rune::from_value::<i64>(val).unwrap(), 7);
}
