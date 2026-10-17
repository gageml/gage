use rune::runtime::{Vm, VmError};
use rune::sync::Arc as RuneArc;
use rune::{Diagnostics, Source, Sources};

fn call_main(script: &str) -> Result<rune::Value, VmError> {
    let mut context = gage_runtime::context().unwrap();
    context.install(test_helpers_module()).unwrap();
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
    vm.call(["main"], ())
}

fn run_rune(script: &str) -> rune::Value {
    call_main(script).unwrap()
}

/// The VM fault a script's `main` raises
fn run_rune_fault(script: &str) -> VmError {
    call_main(script).err().unwrap()
}

#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the test holds the only live handle"
)]
fn run_template(script: &str) -> String {
    let val = run_rune(script);
    rune::from_value::<String>(val).unwrap()
}

#[test]
fn simple_variable_substitution() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        pub fn main() {
            render_template("Hello {{ name }}!", #{ name: "world" })
        }
        "#,
    );
    assert_eq!(result, "Hello world!");
}

#[test]
fn integer_value() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        pub fn main() {
            render_template("count: {{ n }}", #{ n: 42 })
        }
        "#,
    );
    assert_eq!(result, "count: 42");
}

#[test]
fn float_value() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        pub fn main() {
            render_template("score: {{ s }}", #{ s: 3.5 })
        }
        "#,
    );
    assert_eq!(result, "score: 3.5");
}

#[test]
fn bool_value() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        pub fn main() {
            render_template("flag: {{ f }}", #{ f: true })
        }
        "#,
    );
    assert_eq!(result, "flag: true");
}

#[test]
fn for_loop_over_list() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        pub fn main() {
            render_template(
                "{% for x in items %}{{ x }} {% endfor %}",
                #{ items: [1, 2, 3] },
            )
        }
        "#,
    );
    assert_eq!(result, "1 2 3 ");
}

#[test]
fn nested_object_access() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        pub fn main() {
            render_template(
                "{{ msg.line }}: {{ msg.text }}",
                #{ msg: #{ line: 5, text: "hello" } },
            )
        }
        "#,
    );
    assert_eq!(result, "5: hello");
}

#[test]
fn list_of_objects() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        pub fn main() {
            let msgs = [
                #{ line: 1, role: "user" },
                #{ line: 2, role: "assistant" },
            ];
            render_template(
                "{% for m in msgs %}{{ m.line }}-{{ m.role }} {% endfor %}",
                #{ msgs },
            )
        }
        "#,
    );
    assert_eq!(result, "1-user 2-assistant ");
}

#[test]
fn conditional() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        pub fn main() {
            render_template(
                "{% if show %}visible{% else %}hidden{% endif %}",
                #{ show: true },
            )
        }
        "#,
    );
    assert_eq!(result, "visible");
}

#[test]
fn missing_variable_renders_empty() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        pub fn main() {
            render_template("hi {{ name }}!", #{})
        }
        "#,
    );
    assert_eq!(result, "hi !");
}

#[test]
fn invalid_template_returns_error() {
    let val = run_rune(
        r#"
        use gage::Error;
        pub fn main() {
            match gage::Template::new("{% if %}") {
                Ok(_) => "ok",
                Err(Error::Template(_)) => "template",
                Err(_) => "other",
            }
        }
        "#,
    );
    #[expect(
        clippy::disallowed_methods,
        reason = "takes the VM execution's return value; the test holds the only live handle"
    )]
    let result = rune::from_value::<String>(val).unwrap();
    assert_eq!(result, "template");
}

#[test]
fn unknown_filter_is_a_vm_fault() {
    let err = run_rune_fault(
        r#"
        pub fn main() {
            gage::Template::new("{{ x | nosuchfilter }}").unwrap().render(#{ x: 1 })
        }
        "#,
    );
    assert!(err.to_string().contains("nosuchfilter"), "{err}");
}

#[test]
fn message_field_access() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        use test::make_message;
        pub fn main() {
            let msg = make_message(#{
                line: 3,
                type: "user",
                subtype: "text",
                text: "Hello there",
            });
            render_template(
                "Line {{ msg.line }} - {{ msg.type }}: {{ msg.text }}",
                #{ msg },
            )
        }
        "#,
    );
    assert_eq!(result, "Line 3 - user: Hello there");
}

#[test]
fn message_list_iteration() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        use test::make_message;
        pub fn main() {
            let msgs = [
                make_message(#{ line: 1, type: "user", text: "Hi" }),
                make_message(#{ line: 2, type: "assistant", text: "Hello" }),
                make_message(#{ line: 3, type: "user", text: "Bye" }),
            ];
            let tmpl = "{% for m in messages %}{{ m.line }}-{{ m.type }} {% endfor %}";
            render_template(tmpl, #{ messages: msgs })
        }
        "#,
    );
    assert_eq!(result, "1-user 2-assistant 3-user ");
}

#[test]
fn entry_field_access() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        use test::make_entry;
        pub fn main() {
            let e = make_entry(#{
                line: 7,
                type: "assistant",
                raw: "test-raw",
            });
            render_template(
                "{{ entry.line }}: {{ entry.type }}",
                #{ entry: e },
            )
        }
        "#,
    );
    assert_eq!(result, "7: assistant");
}

#[test]
fn message_missing_field_is_empty() {
    let result = run_template(
        r#"
        fn render_template(t, ctx) { gage::Template::new(t).unwrap().render(ctx) }
        use test::make_message;
        pub fn main() {
            let msg = make_message(#{ line: 1, type: "user", text: "Hi" });
            render_template(
                "{% if msg.subtype %}sub={{ msg.subtype }}{% else %}no-sub{% endif %}",
                #{ msg },
            )
        }
        "#,
    );
    assert_eq!(result, "no-sub");
}

#[test]
fn template_struct_simple() {
    let result = run_template(
        r#"
        use gage::Template;
        pub fn main() {
            let t = Template::new("Hello {{ name }}!").unwrap();
            t.render(#{ name: "world" })
        }
        "#,
    );
    assert_eq!(result, "Hello world!");
}

#[test]
fn template_struct_reused() {
    let result = run_template(
        r#"
        use gage::Template;
        pub fn main() {
            let t = Template::new("n={{ n }}").unwrap();
            let a = t.render(#{ n: 1 });
            let b = t.render(#{ n: 2 });
            `${a};${b}`
        }
        "#,
    );
    assert_eq!(result, "n=1;n=2");
}

#[test]
fn template_struct_parse_error_at_new() {
    let val = run_rune(
        r#"
        use gage::{Template, Error};
        pub fn main() {
            match Template::new("{% if %}") {
                Ok(_) => "ok",
                Err(Error::Template(_)) => "template",
                Err(_) => "other",
            }
        }
        "#,
    );
    #[expect(
        clippy::disallowed_methods,
        reason = "takes the VM execution's return value; the test holds the only live handle"
    )]
    let result = rune::from_value::<String>(val).unwrap();
    assert_eq!(result, "template");
}

#[test]
fn template_struct_nested_context() {
    let result = run_template(
        r#"
        use gage::Template;
        pub fn main() {
            let t = Template::new("{{ msg.line }}: {{ msg.text }}").unwrap();
            t.render(#{ msg: #{ line: 5, text: "hello" } })
        }
        "#,
    );
    assert_eq!(result, "5: hello");
}

/// `test::make_message` and `test::make_entry`: row values from an
/// object, for templates over rows without a store
fn test_helpers_module() -> rune::Module {
    use gage_runtime::row::{Entry, Message};
    use rune::runtime::{Object, Value};

    let mut m = rune::Module::with_crate("test").unwrap();
    m.function("make_message", |obj: Object| -> Value {
        rune::to_value(Message {
            inner: obj,
            object: std::sync::OnceLock::new(),
        })
        .unwrap()
    })
    .build()
    .unwrap();
    m.function("make_entry", |obj: Object| -> Value {
        rune::to_value(Entry {
            inner: obj,
            object: std::sync::OnceLock::new(),
        })
        .unwrap()
    })
    .build()
    .unwrap();
    m
}
