use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use datafusion::arrow::array::StringArray;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::prelude::SessionContext;
use gage_query::create_context;

pub fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
}

/// One derived-artifact cache per test binary, warmed once with a
/// blocking reconcile so concurrent tests never race an empty cache
/// (a query-time try-lock reconcile skips on contention by design).
fn cache_dir() -> &'static Path {
    static CACHE: OnceLock<tempfile::TempDir> = OnceLock::new();
    static WARM: OnceLock<()> = OnceLock::new();
    let dir = CACHE
        .get_or_init(|| tempfile::tempdir().expect("create cache tempdir"))
        .path();
    WARM.get_or_init(|| {
        gage_index::IndexStore::new(testdata(), dir)
            .reconcile(gage_index::LockMode::Wait)
            .expect("warm reconcile");
    });
    dir
}

pub async fn test_ctx() -> SessionContext {
    let cache = cache_dir().to_path_buf();
    create_context(&testdata(), &cache).await
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
