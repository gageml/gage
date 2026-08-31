//! Runner for the Rune tests in the scanner bundle: `#[test]`
//! functions in `.rn` files under `scanners/`. The cargo test target
//! `tests/rune.rs` (a libtest-mimic harness) collects the tests with
//! [`collect_tests`] and runs each with [`run_test`].

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gage_claude::session::SessionInfo;
use rune::alloc::prelude::TryToOwned;
use rune::runtime::Vm;
use rune::sync::Arc as RuneArc;
use rune::termcolor::NoColor;
use rune::{Diagnostics, Source, Sources};

use gage_query::ScanSessionContext;

use gage_registry::scanner::{extract_scanners, scanners_dir};
use gage_runtime as runtime;
use gage_runtime::state::{RunContext, SCAN_CTX, ScanContext};

pub enum TestOutcome {
    Pass,
    Fail(String),
}

/// One runnable case: a Rune `#[test]` function, or a file's build
/// failure surfaced as a case that fails with the diagnostics.
pub struct TestCase {
    pub name: String,
    kind: CaseKind,
}

enum CaseKind {
    BuildError { report: String },
    Test(Box<RuneTest>),
}

struct RuneTest {
    rt: RuneArc<rune::runtime::RuntimeContext>,
    unit: RuneArc<rune::Unit>,
    hash: rune::Hash,
    module_name: String,
    item: rune::ItemBuf,
    file: Arc<FileSource>,
}

/// Source text for one `.rn` file, kept for on-demand error
/// rendering. `Sources` is rebuilt from this as needed; a rebuilt
/// single-file `Sources` yields the same source id the unit's debug
/// info references.
struct FileSource {
    rel: String,
    code: String,
    path: PathBuf,
}

/// Extract the scanner bundle and compile every `.rn` file, returning
/// a case per `#[test]` function. A file that fails to compile yields
/// a single `<module>::build` case carrying its diagnostics; warnings
/// on a successful build go to stderr.
pub fn collect_tests() -> io::Result<Vec<TestCase>> {
    extract_scanners().unwrap();
    let dir = scanners_dir();

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

    let mut cases = Vec::new();
    for path in walk_rn_files(&dir) {
        let rel = path
            .strip_prefix(&dir)
            .expect("path under dir via walk_rn_files")
            .to_string_lossy()
            .to_string();
        let code = std::fs::read_to_string(&path)?;

        let mut sources = Sources::new();
        sources
            .insert(Source::with_path(&rel, &code, &path).unwrap())
            .unwrap();

        let mut test_visitor = TestVisitor::default();
        let mut diagnostics = Diagnostics::new();
        let result = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .with_visitor(&mut test_visitor)
            .unwrap()
            .build();

        let module_name = module_name(&path);
        let unit = match result {
            Ok(unit) => RuneArc::try_new(unit).unwrap(),
            Err(_) => {
                cases.push(TestCase {
                    name: format!("{module_name}::build"),
                    kind: CaseKind::BuildError {
                        report: render_diagnostics(&diagnostics, &sources),
                    },
                });
                continue;
            }
        };
        if !diagnostics.is_empty() {
            let mut writer =
                rune::termcolor::StandardStream::stderr(rune::termcolor::ColorChoice::Auto);
            diagnostics.emit(&mut writer, &sources).unwrap();
        }

        let file = Arc::new(FileSource { rel, code, path });
        for (hash, item) in test_visitor.into_functions() {
            cases.push(TestCase {
                name: format!("{module_name}::{item}"),
                kind: CaseKind::Test(Box::new(RuneTest {
                    rt: rt.clone(),
                    unit: unit.clone(),
                    hash,
                    module_name: module_name.clone(),
                    item,
                    file: file.clone(),
                })),
            });
        }
    }
    Ok(cases)
}

/// A `scanner.rn` test is named for its scanner directory; any other
/// file is named for its stem.
fn module_name(path: &std::path::Path) -> String {
    let stem = path.file_stem().unwrap().to_string_lossy();
    if stem == "scanner" {
        path.parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| stem.to_string())
    } else {
        stem.to_string()
    }
}

fn render_diagnostics(diagnostics: &Diagnostics, sources: &Sources) -> String {
    let mut writer = NoColor::new(Vec::new());
    diagnostics.emit(&mut writer, sources).unwrap();
    String::from_utf8(writer.into_inner()).unwrap()
}

/// Run one collected case under a stub scan context.
pub async fn run_test(case: TestCase) -> TestOutcome {
    let name = case.name;
    let test = match case.kind {
        CaseKind::BuildError { report } => return TestOutcome::Fail(report),
        CaseKind::Test(test) => test,
    };

    let stub_selected: Arc<[SessionInfo]> = Arc::from(Vec::<SessionInfo>::new().into_boxed_slice());
    let stub_run = Arc::new(RunContext {
        scan_id: "test".to_string(),
        selected: stub_selected.clone(),
        projects: HashMap::new(),
        scan_ctx: Arc::new(ScanSessionContext::new(&stub_selected)),
        mcp_host: None,
        dispatcher: std::sync::OnceLock::new(),
        agent_pool: Arc::new(tokio::sync::Semaphore::new(1)),
        model_map: Default::default(),
        invalidate: false,
        agent_fault: std::sync::OnceLock::new(),
    });
    let stub_db = Arc::new(Mutex::new(gage_db::db::open_db_in_memory().unwrap()));
    // Seed the stub scan row so note/issue writes that link to the
    // current scan satisfy the scan_note/scan_issue FKs.
    stub_db
        .lock()
        .unwrap()
        .execute("INSERT INTO scan (id, created) VALUES ('test', 0)", [])
        .unwrap();
    let (stub_tx, _stub_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut vm = Vm::new(test.rt.clone(), test.unit.clone());
    // Tests don't exercise call_agent custom-tool dispatch, so a stub
    // empty Sources is fine.
    let stub_sources = Arc::new(Sources::new());
    let ctx = Arc::new(ScanContext {
        scanner_name: test.module_name.clone(),
        task_name: test.item.to_string(),
        params: None,
        run: stub_run,
        db: stub_db,
        runtime_tx: stub_tx,
        rt: test.rt.clone(),
        unit: test.unit.clone(),
        sources: stub_sources,
        task_fault: Mutex::new(None),
    });
    let hash = test.hash;
    let result = SCAN_CTX
        .scope(ctx, async move {
            vm.execute(hash, ()).unwrap().async_complete().await
        })
        .await;
    match result {
        Err(e) => {
            let raw = e.to_string();
            let detail = raw.strip_prefix("Panicked: ").unwrap_or(&raw);
            let sources = file_sources(&test.file);
            TestOutcome::Fail(format_error(&name, detail, &e, &sources))
        }
        Ok(value) => {
            #[expect(
                clippy::disallowed_methods,
                reason = "takes the VM execution's return value; the test \
                          runner holds the only live handle"
            )]
            if let Ok(Err(err)) =
                rune::from_value::<Result<rune::runtime::Value, rune::runtime::Value>>(value)
            {
                let rendered = crate::error::render_task_error(err);
                TestOutcome::Fail(format!(
                    "{name} failed: returned Err({rendered})\n  at {}\n",
                    test.file.rel
                ))
            } else {
                TestOutcome::Pass
            }
        }
    }
}

fn file_sources(file: &FileSource) -> Sources {
    let mut sources = Sources::new();
    sources
        .insert(Source::with_path(&file.rel, &file.code, &file.path).unwrap())
        .unwrap();
    sources
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
