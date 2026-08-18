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
fn random_f64_in_unit_range() {
    let val = run_rune(
        r#"
        pub fn main() {
            let rng = rand::rng();
            let x = rng.random::<f64>();
            x >= 0.0 && x < 1.0
        }
        "#,
    );
    assert!(rune::from_value::<bool>(val).unwrap());
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the test holds the only live handle"
)]
fn random_range_f64() {
    let val = run_rune(
        r#"
        pub fn main() {
            let rng = rand::rng();
            let x = rng.random_range::<f64>(5.0..10.0);
            x >= 5.0 && x < 10.0
        }
        "#,
    );
    assert!(rune::from_value::<bool>(val).unwrap());
}
