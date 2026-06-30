use rune::runtime::Vm;
use rune::sync::Arc as RuneArc;
use rune::{Diagnostics, Source, Sources};

fn run_rune(script: &str) -> rune::Value {
    let rt = gage_scan::runner::TestRuntime::new();
    let mut context = rune_modules::default_context().unwrap();
    context.install(rt.types_module().unwrap()).unwrap();
    context.install(rt.macros_module().unwrap()).unwrap();
    context.install(rt.gage_module().unwrap()).unwrap();
    context.install(rt.test_helpers_module().unwrap()).unwrap();
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

fn run_template(script: &str) -> String {
    let val = run_rune(script);
    rune::from_value::<String>(val).unwrap()
}

#[test]
fn simple_variable_substitution() {
    let result = run_template(
        r#"
        use gage::render_template;
        pub fn main() {
            render_template("Hello {{ name }}!", #{ name: "world" }).unwrap()
        }
        "#,
    );
    assert_eq!(result, "Hello world!");
}

#[test]
fn integer_value() {
    let result = run_template(
        r#"
        use gage::render_template;
        pub fn main() {
            render_template("count: {{ n }}", #{ n: 42 }).unwrap()
        }
        "#,
    );
    assert_eq!(result, "count: 42");
}

#[test]
fn float_value() {
    let result = run_template(
        r#"
        use gage::render_template;
        pub fn main() {
            render_template("score: {{ s }}", #{ s: 3.5 }).unwrap()
        }
        "#,
    );
    assert_eq!(result, "score: 3.5");
}

#[test]
fn bool_value() {
    let result = run_template(
        r#"
        use gage::render_template;
        pub fn main() {
            render_template("flag: {{ f }}", #{ f: true }).unwrap()
        }
        "#,
    );
    assert_eq!(result, "flag: true");
}

#[test]
fn for_loop_over_list() {
    let result = run_template(
        r#"
        use gage::render_template;
        pub fn main() {
            render_template(
                "{% for x in items %}{{ x }} {% endfor %}",
                #{ items: [1, 2, 3] },
            ).unwrap()
        }
        "#,
    );
    assert_eq!(result, "1 2 3 ");
}

#[test]
fn nested_object_access() {
    let result = run_template(
        r#"
        use gage::render_template;
        pub fn main() {
            render_template(
                "{{ msg.line }}: {{ msg.text }}",
                #{ msg: #{ line: 5, text: "hello" } },
            ).unwrap()
        }
        "#,
    );
    assert_eq!(result, "5: hello");
}

#[test]
fn list_of_objects() {
    let result = run_template(
        r#"
        use gage::render_template;
        pub fn main() {
            let msgs = [
                #{ line: 1, role: "user" },
                #{ line: 2, role: "assistant" },
            ];
            render_template(
                "{% for m in msgs %}{{ m.line }}-{{ m.role }} {% endfor %}",
                #{ msgs },
            ).unwrap()
        }
        "#,
    );
    assert_eq!(result, "1-user 2-assistant ");
}

#[test]
fn conditional() {
    let result = run_template(
        r#"
        use gage::render_template;
        pub fn main() {
            render_template(
                "{% if show %}visible{% else %}hidden{% endif %}",
                #{ show: true },
            ).unwrap()
        }
        "#,
    );
    assert_eq!(result, "visible");
}

#[test]
fn missing_variable_renders_empty() {
    let result = run_template(
        r#"
        use gage::render_template;
        pub fn main() {
            render_template("hi {{ name }}!", #{}).unwrap()
        }
        "#,
    );
    assert_eq!(result, "hi !");
}

#[test]
fn invalid_template_returns_error() {
    let val = run_rune(
        r#"
        use gage::{render_template, Error};
        pub fn main() {
            match render_template("{% if %}", #{}) {
                Ok(_) => "ok",
                Err(Error::Template(_)) => "template",
                Err(_) => "other",
            }
        }
        "#,
    );
    let result = rune::from_value::<String>(val).unwrap();
    assert_eq!(result, "template");
}

#[test]
fn message_field_access() {
    let result = run_template(
        r#"
        use gage::render_template;
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
            ).unwrap()
        }
        "#,
    );
    assert_eq!(result, "Line 3 - user: Hello there");
}

#[test]
fn message_list_iteration() {
    let result = run_template(
        r#"
        use gage::render_template;
        use test::make_message;
        pub fn main() {
            let msgs = [
                make_message(#{ line: 1, type: "user", text: "Hi" }),
                make_message(#{ line: 2, type: "assistant", text: "Hello" }),
                make_message(#{ line: 3, type: "user", text: "Bye" }),
            ];
            let tmpl = "{% for m in messages %}{{ m.line }}-{{ m.type }} {% endfor %}";
            render_template(tmpl, #{ messages: msgs }).unwrap()
        }
        "#,
    );
    assert_eq!(result, "1-user 2-assistant 3-user ");
}

#[test]
fn entry_field_access() {
    let result = run_template(
        r#"
        use gage::render_template;
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
            ).unwrap()
        }
        "#,
    );
    assert_eq!(result, "7: assistant");
}

#[test]
fn message_missing_field_is_empty() {
    let result = run_template(
        r#"
        use gage::render_template;
        use test::make_message;
        pub fn main() {
            let msg = make_message(#{ line: 1, type: "user", text: "Hi" });
            render_template(
                "{% if msg.subtype %}sub={{ msg.subtype }}{% else %}no-sub{% endif %}",
                #{ msg },
            ).unwrap()
        }
        "#,
    );
    assert_eq!(result, "no-sub");
}

fn run_rune_async(script: &str) -> rune::Value {
    let rt = gage_scan::runner::TestRuntime::new();
    let mut context = rune_modules::default_context().unwrap();
    context.install(rt.types_module().unwrap()).unwrap();
    context.install(rt.macros_module().unwrap()).unwrap();
    context.install(rt.gage_module().unwrap()).unwrap();
    context.install(rt.test_helpers_module().unwrap()).unwrap();
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
    let rt_tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt_tokio.block_on(async move {
        vm.execute(["main"], ())
            .unwrap()
            .async_complete()
            .await
            .unwrap()
    })
}

fn run_template_async(script: &str) -> String {
    let val = run_rune_async(script);
    rune::from_value::<Result<String, rune::Value>>(val)
        .unwrap()
        .unwrap()
}

#[test]
fn template_struct_simple() {
    let result = run_template_async(
        r#"
        use gage::Template;
        pub async fn main() {
            let t = Template::new("Hello {{ name }}!").await?;
            t.render(#{ name: "world" }).await
        }
        "#,
    );
    assert_eq!(result, "Hello world!");
}

#[test]
fn template_struct_reused() {
    let result = run_template_async(
        r#"
        use gage::Template;
        pub async fn main() {
            let t = Template::new("n={{ n }}").await?;
            let a = t.render(#{ n: 1 }).await?;
            let b = t.render(#{ n: 2 }).await?;
            Ok(`${a};${b}`)
        }
        "#,
    );
    assert_eq!(result, "n=1;n=2");
}

#[test]
fn template_struct_parse_error_at_new() {
    let val = run_rune_async(
        r#"
        use gage::{Template, Error};
        pub async fn main() {
            match Template::new("{% if %}").await {
                Ok(_) => "ok",
                Err(Error::Template(_)) => "template",
                Err(_) => "other",
            }
        }
        "#,
    );
    let result = rune::from_value::<String>(val).unwrap();
    assert_eq!(result, "template");
}

#[test]
fn template_struct_nested_context() {
    let result = run_template_async(
        r#"
        use gage::Template;
        pub async fn main() {
            let t = Template::new("{{ msg.line }}: {{ msg.text }}").await?;
            t.render(#{ msg: #{ line: 5, text: "hello" } }).await
        }
        "#,
    );
    assert_eq!(result, "5: hello");
}
