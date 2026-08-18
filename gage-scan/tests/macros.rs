use std::path::PathBuf;

use rune::runtime::Vm;
use rune::sync::Arc as RuneArc;
use rune::{Diagnostics, Source, Sources};

fn run_rune(embed_key: &str, script: &str) -> rune::Value {
    let base: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests"].iter().collect();
    let source_path = base.join(embed_key);
    let rt = gage_scan::runner::TestRuntime::new();
    let mut context = rune_modules::default_context().unwrap();
    context.install(rt.types_module().unwrap()).unwrap();
    context.install(rt.macros_module().unwrap()).unwrap();
    context.install(rt.gage_module().unwrap()).unwrap();
    let runtime = RuneArc::try_new(context.runtime().unwrap()).unwrap();

    let mut sources = Sources::new();
    sources
        .insert(Source::with_path("test", script, &source_path).unwrap())
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
fn include_str_loads_file() {
    let val = run_rune(
        "test-fixture.rn",
        r#"
        pub fn main() {
            let s = include_str!("test-fixture.jsonc");
            s.len() > 0
        }
        "#,
    );
    assert!(rune::from_value::<bool>(val).unwrap());
}

#[test]
fn include_json_loads_array() {
    let val = run_rune(
        "test-fixture.rn",
        r#"
        pub fn main() {
            let items = include_json!("test-fixture.jsonc");
            items.len()
        }
        "#,
    );
    #[expect(
        clippy::disallowed_methods,
        reason = "takes the VM execution's return value; the test holds the only live handle"
    )]
    let len = rune::from_value::<i64>(val).unwrap();
    assert_eq!(len, 3);
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the test holds the only live handle"
)]
fn include_json_elements_are_strings() {
    let val = run_rune(
        "test-fixture.rn",
        r#"
        pub fn main() {
            let items = include_json!("test-fixture.jsonc");
            items[0] is String
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
fn include_json_contains_known_element() {
    let val = run_rune(
        "test-fixture.rn",
        r#"
        pub fn main() {
            let items = include_json!("test-fixture.jsonc");
            items.iter().any(|w| w == "beta")
        }
        "#,
    );
    assert!(rune::from_value::<bool>(val).unwrap());
}
