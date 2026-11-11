//! Running the filters of a publish. Each filter the definition names
//! is a Rune filter from the registry: it is compiled against the
//! runtime context and each of its tasks is run under a publish
//! context, where `publish()` hands the task its own contribution.
//! The contributions of a filter's tasks are merged into one
//! [`FilterOutput`] per filter, and the derivation applies them. A
//! filter runs at every rebuild, so what it contributes is derived
//! from its source and the params the definition records, never
//! stored.

use gage_registry::filter::{Filter, FilterDef, FilterRegistry, FilterTaskDef};
use gage_runtime::error::render_task_error;
use gage_runtime::{
    Contribution, Fail, Level, OUTPUT_SINK, Output, OutputSink, PUBLISH_CTX, PublishContext,
    TaskOutput, is_ignore, render_vm_error,
};
use rune::runtime::{RuntimeContext, Unit, Value};
use rune::sync::Arc as RuneArc;
use rune::{Diagnostics, Source, Sources, Vm};
use tokio::sync::mpsc;

use crate::PublishError;
use crate::definition::FilterEntry;

/// What one filter of the definition contributed: the merged
/// contributions of its tasks, in task order.
#[derive(Debug, Clone)]
pub struct FilterOutput {
    pub name: String,
    pub contribution: Contribution,
}

impl FilterOutput {
    /// The author the filter's redactions carry, `filter:<name>`.
    pub fn author(&self) -> String {
        format!("filter:{}", self.name)
    }
}

/// Run every filter of `entries` from `registry`, in list order. A
/// name the registry does not know, a filter that does not compile,
/// and a task that fails are errors naming the filter.
pub(crate) async fn run_filters(
    registry: &FilterRegistry,
    entries: &[FilterEntry],
) -> Result<Vec<FilterOutput>, PublishError> {
    let mut outputs = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(def) = registry.get_def(&entry.name) else {
            return Err(PublishError::Input(format!(
                "unknown filter {:?}; filters are {}",
                entry.name,
                registry.names().join(", ")
            )));
        };
        let compiled = compile(def)?;
        let filter = Filter::with_params(def, entry.params.as_ref());
        let mut contribution = Contribution::default();
        for task in def.tasks.values() {
            merge(&mut contribution, run_task(&compiled, &filter, task).await?);
        }
        outputs.push(FilterOutput {
            name: def.name.clone(),
            contribution,
        });
    }
    Ok(outputs)
}

/// The redaction instructions of every filter as markdown: per filter
/// with instructions a level-three heading with its name in code
/// font, then each instruction text, separated by blank lines. A
/// filter that contributed no instructions has no section.
pub fn instructions_markdown(outputs: &[FilterOutput]) -> String {
    let mut out = String::new();
    for output in outputs {
        if output.contribution.instructions.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("### `{}`\n", output.name));
        for instructions in &output.contribution.instructions {
            out.push('\n');
            out.push_str(instructions.trim_end());
            out.push('\n');
        }
    }
    out
}

fn merge(into: &mut Contribution, from: Contribution) {
    into.entries.extend(from.entries);
    into.values.extend(from.values);
    into.literals.extend(from.literals);
    into.spans.extend(from.spans);
    into.patterns.extend(from.patterns);
    into.instructions.extend(from.instructions);
}

/// A filter compiled and ready to run; a fresh VM is built per task.
struct Compiled {
    rt: RuneArc<RuntimeContext>,
    unit: RuneArc<Unit>,
    sources: Sources,
}

/// Compile `def` and verify that every declared task names a function
/// it defines. A declaration with problems does not compile.
fn compile(def: &FilterDef) -> Result<Compiled, PublishError> {
    let filter = |m: String| PublishError::Filter(format!("filter {}: {m}", def.name));
    if !def.problems.is_empty() {
        return Err(filter(format!(
            "invalid FILTER declaration\n{}",
            def.render_problems()
        )));
    }
    let context = gage_runtime::context().unwrap();
    let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();
    let mut sources = Sources::new();
    sources
        .insert(Source::with_path(&def.name, def.source(), &def.path).unwrap())
        .unwrap();
    let mut diagnostics = Diagnostics::new();
    let result = rune::prepare(&mut sources)
        .with_context(&context)
        .with_diagnostics(&mut diagnostics)
        .build();
    let rendered = if diagnostics.is_empty() {
        String::new()
    } else {
        let mut buf = rune::termcolor::Buffer::no_color();
        diagnostics.emit(&mut buf, &sources).unwrap();
        String::from_utf8(buf.into_inner()).unwrap()
    };
    let unit = match result {
        Ok(unit) => {
            if !rendered.is_empty() {
                tracing::warn!(filter = %def.name, "{rendered}");
            }
            RuneArc::try_new(unit).unwrap()
        }
        Err(_) => return Err(filter(format!("failed to compile\n{rendered}"))),
    };
    let vm = Vm::new(rt.clone(), unit.clone());
    for task in def.tasks.values() {
        if vm.lookup_function([task.call.as_str()]).is_err() {
            return Err(filter(format!(
                "declares task {} but defines no function {}",
                task.name, task.call
            )));
        }
    }
    Ok(Compiled { rt, unit, sources })
}

/// Run one task under its publish context and output sink, and
/// return what it contributed. The task's output goes to the log
/// under the filter and task names.
async fn run_task(
    compiled: &Compiled,
    filter: &Filter<'_>,
    task: &FilterTaskDef,
) -> Result<Contribution, PublishError> {
    let name = &filter.def.name;
    let ctx = PublishContext::new(name, &task.name, filter.params.clone());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink = OutputSink {
        scanner: name.clone(),
        task: task.name.clone(),
        tx,
    };
    let result = PUBLISH_CTX
        .scope(
            ctx.clone(),
            OUTPUT_SINK.scope(sink, execute(compiled, &task.call)),
        )
        .await;
    while let Ok(output) = rx.try_recv() {
        log_output(output);
    }
    result.map_err(|e| PublishError::Filter(format!("filter {name} task {}: {e}", task.name)))?;
    Ok(ctx.take_contribution())
}

/// Run `function` on a fresh VM and interpret its return value.
async fn execute(compiled: &Compiled, function: &str) -> Result<(), String> {
    let vm = Vm::new(compiled.rt.clone(), compiled.unit.clone());
    let render = |e| render_vm_error(&e, Some(&compiled.sources));
    let execution = vm.send_execute([function], ()).map_err(render)?;
    let value = execution.complete().await.map_err(render)?;
    task_result(value)
}

/// Interpret a task's return value the way a scan task's is: unit,
/// `Ok`, and `Err(Ignore)` succeeded; `Err(Fail(msg))` fails with
/// `msg`; any other `Err` is a filter defect and fails with the value.
#[expect(
    clippy::disallowed_methods,
    reason = "takes the VM execution's return value; the runtime holds the only live handle"
)]
fn task_result(value: Value) -> Result<(), String> {
    match rune::from_value::<Result<Value, Value>>(value) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) if is_ignore(&err) => Ok(()),
        Ok(Err(err)) => {
            if let Ok(fail) = err.borrow_ref::<Fail>() {
                return Err(fail.message().to_string());
            }
            Err(format!("task returned Err: {}", render_task_error(err)))
        }
        Err(_) => Ok(()),
    }
}

/// A task's print or log record, under the filter and task names.
fn log_output(output: TaskOutput) {
    let TaskOutput {
        scanner: filter,
        task,
        output,
    } = output;
    match output {
        Output::Print(text) | Output::Println(text) => {
            tracing::info!(%filter, %task, "{text}");
        }
        Output::Log { level, message } => match level {
            Level::Trace => tracing::trace!(%filter, %task, "{message}"),
            Level::Debug => tracing::debug!(%filter, %task, "{message}"),
            Level::Info => tracing::info!(%filter, %task, "{message}"),
            Level::Warn => tracing::warn!(%filter, %task, "{message}"),
            Level::Error => tracing::error!(%filter, %task, "{message}"),
        },
        Output::Progress { .. } | Output::AgentPool(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use gage_runtime::Reason;
    use gage_session::TypedSet;

    use super::*;

    /// The registry over the filters this repository ships.
    pub(crate) fn builtin_registry() -> FilterRegistry {
        let builtin = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../filters"));
        FilterRegistry::load_from_roots(&builtin, vec![builtin.clone()])
    }

    fn entry(name: &str) -> FilterEntry {
        FilterEntry {
            name: name.into(),
            params: None,
        }
    }

    /// Every shipped filter compiles and runs, and each contributes
    /// what its source declares.
    #[tokio::test]
    async fn the_shipped_filters_run_and_contribute_what_they_declare() {
        let registry = builtin_registry();
        let entries: Vec<FilterEntry> =
            ["gitleaks", "harness", "language", "pii", "secret", "system"]
                .into_iter()
                .map(entry)
                .collect();
        let outputs = run_filters(&registry, &entries).await.unwrap();
        let by_name = |name: &str| {
            &outputs
                .iter()
                .find(|o| o.name == name)
                .unwrap_or_else(|| panic!("no output for {name}"))
                .contribution
        };
        let harness = by_name("harness");
        assert_eq!(
            harness.entries,
            [
                (TypedSet::HarnessContext, Reason::Scope),
                (TypedSet::HarnessRecord, Reason::Scope)
            ]
        );
        assert_eq!(harness.spans, [(TypedSet::HarnessRecord, Reason::Scope)]);
        assert!(harness.instructions.is_empty());
        let language = by_name("language");
        assert!(language.instructions[0].starts_with("Identify language"));
        let pii = by_name("pii");
        assert_eq!(pii.values, [(TypedSet::Pii, Reason::Pii)]);
        assert_eq!(pii.literals, [(TypedSet::Pii, Reason::Pii)]);
        assert_eq!(pii.patterns[0].1, Reason::Pii);
        assert_eq!(
            pii.patterns[0].0.matches("mail a@b.co"),
            [("email", "a@b.co")]
        );
        let gitleaks = by_name("gitleaks");
        assert!(gitleaks.patterns[0].0.len() > 200);
        assert!(gitleaks.instructions.is_empty());
        let secret = by_name("secret");
        assert!(secret.patterns.is_empty());
        assert!(secret.instructions[0].starts_with("Remove every credential"));
        let system = by_name("system");
        assert_eq!(system.values, [(TypedSet::System, Reason::System)]);
        assert_eq!(system.literals, [(TypedSet::System, Reason::System)]);
        let markdown = instructions_markdown(&outputs);
        assert!(
            markdown.starts_with("### `language`\n\nIdentify language"),
            "{markdown}"
        );
        assert!(!markdown.contains("harness"), "{markdown}");
        assert!(markdown.contains("\n\n### `pii`\n\n"), "{markdown}");
    }

    /// A param the definition records reaches the task through
    /// `params()` and lands in the rendered instructions.
    #[tokio::test]
    async fn recorded_params_reach_the_instructions() {
        let registry = builtin_registry();
        let mut params = serde_json::Map::new();
        params.insert("prompt".into(), "Watch for street addresses.".into());
        let entries = [FilterEntry {
            name: "pii".into(),
            params: Some(params),
        }];
        let outputs = run_filters(&registry, &entries).await.unwrap();
        let instructions = &outputs[0].contribution.instructions[0];
        assert!(
            instructions.ends_with("\n\nWatch for street addresses."),
            "{instructions}"
        );
        let plain = run_filters(&registry, &[entry("pii")]).await.unwrap();
        assert!(
            !plain[0].contribution.instructions[0].ends_with("\n"),
            "an empty prompt leaves no trailing blank"
        );
    }

    fn write_filter(dir: &Path, source: &str) -> FilterRegistry {
        std::fs::create_dir_all(dir.join("t")).unwrap();
        std::fs::write(dir.join("t").join("filter.rn"), source).unwrap();
        FilterRegistry::load_from_roots(dir, vec![dir.to_path_buf()])
    }

    /// An unknown name, a declaration with problems, a task without
    /// its function, a compile error, and a failing task are each an
    /// error naming the filter.
    #[tokio::test]
    async fn filter_failures_name_the_filter() {
        let registry = builtin_registry();
        let err = run_filters(&registry, &[entry("nope")]).await.unwrap_err();
        assert!(err.to_string().contains("unknown filter \"nope\""), "{err}");

        let tmp = tempfile::tempdir().unwrap();
        let cases = [
            (
                "pub const FILTER = #{ name: \"t\", tasks: #{ a: 1 } };",
                "invalid FILTER declaration",
            ),
            (
                "pub const FILTER = #{ name: \"t\", tasks: #{ a: #{} } };",
                "declares task a but defines no function a",
            ),
            (
                "pub const FILTER = #{ name: \"t\", tasks: #{ a: #{} } };\npub fn a() { nope() }",
                "failed to compile",
            ),
            (
                "use gage::Fail;\npub const FILTER = #{ name: \"t\", tasks: #{ a: #{} } };\npub fn a() { Err(Fail(\"no ducks\")) }",
                "filter t task a: no ducks",
            ),
            (
                "pub const FILTER = #{ name: \"t\", tasks: #{ a: #{} } };\npub fn a() { Err(\"boom\") }",
                "task returned Err: boom",
            ),
            (
                "pub const FILTER = #{ name: \"t\", tasks: #{ a: #{} } };\npub fn a() { gage::publish().pattern_redact(#{ x: \"(\" }, gage::filter::Reason::Pii) }",
                "pattern x: ",
            ),
        ];
        for (source, message) in cases {
            let registry = write_filter(tmp.path(), source);
            let err = run_filters(&registry, &[entry("t")]).await.unwrap_err();
            let text = err.to_string();
            assert!(text.contains(message), "{source}\n{text}");
            assert!(text.starts_with("filter t"), "{text}");
        }
    }

    /// A task that returns unit, `Ok`, or `Err(Ignore)` succeeds, and
    /// the contributions of two tasks merge in task order.
    #[tokio::test]
    async fn task_outcomes_and_merging() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = write_filter(
            tmp.path(),
            r#"
use gage::{Ignore, publish, filter::Reason};
pub const FILTER = #{ name: "t", tasks: #{ a: #{}, b: #{ call: "run_b" }, c: #{} } };
pub fn a() { publish().agent_redact("A"); }
pub fn run_b() { publish().agent_redact("B"); Ok(()) }
pub fn c() { log::info!("nothing to do"); Err(Ignore) }
"#,
        );
        let outputs = run_filters(&registry, &[entry("t")]).await.unwrap();
        assert_eq!(outputs[0].contribution.instructions, ["A", "B"]);
        assert_eq!(outputs[0].author(), "filter:t");
    }
}
