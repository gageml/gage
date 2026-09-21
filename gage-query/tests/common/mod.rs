use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use datafusion::arrow::array::StringArray;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_claude::index::cache_dir_for;
use gage_query::create_context;
use gage_registry::driver::DriverRegistry;

pub fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

/// Point `GAGE_HOME` at a per-binary temp dir so tests never touch the
/// product location (`~/.gage`), and `CLAUDE_PROJECTS_DIR` at the test
/// corpus so the default source is the testdata. Set once, before any
/// reader.
pub fn isolate_env() {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    static SET: OnceLock<()> = OnceLock::new();
    let dir = HOME.get_or_init(|| tempfile::tempdir().expect("create gage home tempdir"));
    SET.get_or_init(|| {
        // SAFETY: set_var is unsafe in edition 2024; this runs once via
        // OnceLock before any test reads these through this fixture.
        unsafe {
            std::env::set_var("GAGE_HOME", dir.path());
            std::env::set_var("CLAUDE_PROJECTS_DIR", testdata());
        }
    });
}

/// Warm the test corpus's derived-artifact cache once per binary with
/// a blocking reconcile so concurrent tests never race an empty cache
/// (a query-time try-lock reconcile skips on contention by design).
fn warm_cache() {
    static WARM: OnceLock<()> = OnceLock::new();
    WARM.get_or_init(|| {
        gage_claude::index::IndexStore::new(testdata(), cache_dir_for(&testdata()))
            .reconcile(gage_claude::index::LockMode::Wait)
            .expect("warm reconcile");
    });
}

pub async fn test_ctx() -> SessionContext {
    isolate_env();
    warm_cache();
    let source = DriverRegistry::builtin()
        .open_source("")
        .expect("open testdata source");
    create_context(source.as_ref())
        .await
        .expect("build test context")
}

/// A context over the testdata corpus for tests that manage their own
/// `GAGE_HOME`. Sets `CLAUDE_PROJECTS_DIR` to the corpus. Each test
/// binary compiles this module on its own, so binaries that do not
/// call it see it as unused.
#[allow(dead_code)]
pub async fn testdata_ctx() -> SessionContext {
    // SAFETY: set_var is unsafe in edition 2024; callers are serial
    // tests that set GAGE_HOME the same way before any reader.
    unsafe { std::env::set_var("CLAUDE_PROJECTS_DIR", testdata()) };
    let source = DriverRegistry::builtin()
        .open_source("")
        .expect("open testdata source");
    create_context(source.as_ref())
        .await
        .expect("build test context")
}

#[allow(clippy::indexing_slicing)]
pub fn col_strings(batch: &RecordBatch, idx: usize) -> Vec<String> {
    batch
        .column(idx)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap()
        .iter()
        .map(|v| v.unwrap().to_string())
        .collect()
}
