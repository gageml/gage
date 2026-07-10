use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use gage_claude::session::SessionInfo;
use rune::alloc::prelude::TryToOwned;
use rune::runtime::Vm;
use rune::sync::Arc as RuneArc;
use rune::{Diagnostics, Source, Sources};

use gage_query::ScanSessionContext;

use gage_registry::scanner::{extract_scanners, scanners_dir};
use gage_runtime as runtime;
use gage_runtime::state::{RunContext, SCAN_CTX, ScanContext};

pub enum TestOutcome {
    Pass,
    Fail(String),
}

pub struct TestResult {
    pub name: String,
    pub outcome: TestOutcome,
}

pub struct TestSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub filtered: usize,
    pub build_errors: usize,
}

pub async fn run_tests(
    filters: &[String],
    fail_fast: bool,
    mut on_result: impl FnMut(&TestResult),
) -> io::Result<TestSummary> {
    extract_scanners().unwrap();
    let dir = scanners_dir();
    let rn_files = walk_rn_files(&dir);

    let mut summary = TestSummary {
        total: 0,
        passed: 0,
        failed: 0,
        filtered: 0,
        build_errors: 0,
    };

    let mut context = rune_modules::with_config(false).unwrap();
    context.install(runtime::io_module().unwrap()).unwrap();
    context.install(runtime::types_module().unwrap()).unwrap();
    context.install(runtime::macros_module().unwrap()).unwrap();
    context.install(runtime::gage_module().unwrap()).unwrap();
    context.install(runtime::tools_module().unwrap()).unwrap();
    context.install(runtime::log_module().unwrap()).unwrap();
    context.install(runtime::stats_module().unwrap()).unwrap();
    context.install(runtime::json_module().unwrap()).unwrap();
    let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();

    for path in &rn_files {
        let rel = path
            .strip_prefix(&dir)
            .expect("path under dir via walk_rn_files")
            .to_string_lossy()
            .to_string();

        let code = std::fs::read_to_string(path)?;

        let mut sources = Sources::new();
        sources
            .insert(Source::with_path(&rel, &code, path).unwrap())
            .unwrap();

        let mut test_visitor = TestVisitor::default();
        let mut diagnostics = Diagnostics::new();
        let result = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .with_visitor(&mut test_visitor)
            .unwrap()
            .build();

        if !diagnostics.is_empty() {
            let mut writer =
                rune::termcolor::StandardStream::stderr(rune::termcolor::ColorChoice::Auto);
            diagnostics.emit(&mut writer, &sources).unwrap();
        }

        let unit = match result {
            Ok(unit) => RuneArc::try_new(unit).unwrap(),
            Err(_) => {
                summary.build_errors += 1;
                continue;
            }
        };

        let tests = test_visitor.into_functions();
        if tests.is_empty() {
            continue;
        }

        let stem = path.file_stem().unwrap().to_string_lossy();
        let module_name = if stem == "scanner" {
            path.parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| stem.to_string())
        } else {
            stem.to_string()
        };

        let stub_selected: std::sync::Arc<[SessionInfo]> =
            std::sync::Arc::from(Vec::<SessionInfo>::new().into_boxed_slice());
        let stub_run = std::sync::Arc::new(RunContext {
            scan_id: "test".to_string(),
            selected: stub_selected.clone(),
            projects: HashMap::new(),
            scan_ctx: std::sync::Arc::new(ScanSessionContext::new(&stub_selected)),
            mcp_host: None,
            dispatcher: std::sync::OnceLock::new(),
            agent_pool: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        });
        let stub_db = std::sync::Arc::new(Mutex::new(gage_db::db::open_db_in_memory().unwrap()));
        // Seed the stub scan row so note/issue writes that link to the
        // current scan satisfy the scan_note/scan_issue FKs.
        stub_db
            .lock()
            .unwrap()
            .execute("INSERT INTO scan (id, created) VALUES ('test', 0)", [])
            .unwrap();
        let (stub_tx, _stub_rx) = tokio::sync::mpsc::unbounded_channel();

        for (hash, item) in &tests {
            let test_name = format!("{module_name}::{item}");

            if !matches_filter(&test_name, filters) {
                summary.filtered += 1;
                continue;
            }

            summary.total += 1;

            let mut vm = Vm::new(rt.clone(), unit.clone());
            // Tests don't exercise call_agent custom-tool dispatch, so a
            // stub empty Sources is fine. Cloning the real `sources`
            // is impossible (rune::Sources isn't Clone) and wrapping
            // the existing value in Arc would force every test loop
            // iteration through a single heap allocation.
            let stub_sources = std::sync::Arc::new(rune::Sources::new());
            let ctx = std::sync::Arc::new(ScanContext {
                scanner_name: module_name.clone(),
                params: None,
                run: stub_run.clone(),
                db: stub_db.clone(),
                runtime_tx: stub_tx.clone(),
                rt: rt.clone(),
                unit: unit.clone(),
                sources: stub_sources,
            });
            let hash = *hash;
            let result = SCAN_CTX
                .scope(ctx, async move {
                    vm.execute(hash, ()).unwrap().async_complete().await
                })
                .await;
            let outcome = match result {
                Err(e) => {
                    let raw = e.to_string();
                    let detail = raw.strip_prefix("Panicked: ").unwrap_or(&raw);
                    let report = format_error(&test_name, detail, &e, &sources);
                    TestOutcome::Fail(report)
                }
                Ok(value) => {
                    if let Ok(Err(err)) = rune::from_value::<
                        Result<rune::runtime::Value, rune::runtime::Value>,
                    >(value)
                    {
                        let rendered = crate::error::render_task_error(err);
                        TestOutcome::Fail(format!(
                            "{test_name} failed: returned Err({rendered})\n  at {rel}\n",
                        ))
                    } else {
                        TestOutcome::Pass
                    }
                }
            };

            match &outcome {
                TestOutcome::Pass => summary.passed += 1,
                TestOutcome::Fail(_) => summary.failed += 1,
            }

            let is_fail = matches!(&outcome, TestOutcome::Fail(_));
            on_result(&TestResult {
                name: test_name,
                outcome,
            });

            if fail_fast && is_fail {
                return Ok(summary);
            }
        }
    }

    Ok(summary)
}

fn matches_filter(name: &str, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    filters.iter().any(|f| name.contains(f.as_str()))
}

fn format_error(
    name: &str,
    detail: &str,
    error: &rune::runtime::VmError,
    sources: &Sources,
) -> String {
    use codespan_reporting::diagnostic::{Diagnostic, Label};
    use codespan_reporting::term;

    if let Some(loc) = error.first_location()
        && let Some(debug_info) = loc.unit.debug_info()
        && let Some(inst) = debug_info.instruction_at(loc.ip)
    {
        let msg = format!("{name} failed: {detail}");
        let diagnostic = Diagnostic::error().with_message(&msg).with_labels(vec![
            Label::primary(inst.source_id, inst.span.range()).with_message(&msg),
        ]);
        let mut buf = Vec::new();
        let config = term::Config::default();
        term::emit_to_io_write(&mut buf, &config, sources, &diagnostic).unwrap();
        String::from_utf8(buf).unwrap()
    } else {
        format!("error: {name} failed: {detail}\n")
    }
}

fn walk_rn_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    walk_rn_files_rec(dir, &mut result);
    result.sort();
    result
}

fn walk_rn_files_rec(dir: &std::path::Path, result: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_rn_files_rec(&path, result);
        } else if path.extension().is_some_and(|ext| ext == "rn") {
            result.push(path);
        }
    }
}

#[derive(Default)]
struct TestVisitor {
    functions: Vec<(rune::Hash, rune::ItemBuf)>,
}

impl TestVisitor {
    fn into_functions(self) -> Vec<(rune::Hash, rune::ItemBuf)> {
        self.functions
    }
}

impl rune::compile::CompileVisitor for TestVisitor {
    fn register_meta(
        &mut self,
        meta: rune::compile::MetaRef<'_>,
    ) -> Result<(), rune::compile::MetaError> {
        if let rune::compile::meta::Kind::Function { is_test: true, .. } = meta.kind {
            self.functions
                .push((meta.hash, meta.item.try_to_owned().unwrap()));
        }
        Ok(())
    }
}
