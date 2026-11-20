//! A session Pi migrated, added to a store that holds the file it
//! replaced.

use std::fs;
use std::path::Path;

use gage_pi::driver::PiDriver;
use gage_session::{Driver, SourceOptions};
use gage_store::{SessionOutcome, SessionStore, Store};

const ID: &str = "11111111-1111-4111-8111-111111111111";
const FILE_NAME: &str = "2025-06-01T10-00-00-000Z_11111111-1111-4111-8111-111111111111.jsonl";

/// A hand-built file in format version 1 with a blank line and a torn
/// line
const BEFORE: &str = include_str!("../testdata/v1-extended.jsonl");

/// The same file as Pi 1.5.2 rewrote it on opening it
const AFTER: &str = include_str!(
    "../fixtures/pi-1.5.2/migrated/2025-06-01T10-00-00-000Z_11111111-1111-4111-8111-111111111111.jsonl"
);

fn add(work: &Path, store: &Store) -> SessionOutcome {
    let driver = PiDriver::new();
    let url = format!("pi:{}", work.join("sessions").display());
    let source = driver
        .open_source(SourceOptions::new(&url).cache_root(work.join("cache")))
        .unwrap();
    let mut native = source.open_native(ID).unwrap();
    let sessions = SessionStore::from(store);
    sessions.add(&driver, native.as_mut()).unwrap().outcome
}

/// Pi migrates a file by rewriting it without its blank and
/// unparsable lines, so the later state of the session is the shorter
/// file. The store takes it as an update, and the file it replaced as
/// an older copy.
#[test]
fn a_shorter_migration_updates_the_stored_session() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().join("sessions/--w-legacy--");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(FILE_NAME);
    let store_path = work.path().join("store.git");
    gage_store::init_with_cache_root(&store_path, &work.path().join("cache")).unwrap();
    let store = Store::open_with_cache_root(&store_path, &work.path().join("cache")).unwrap();
    assert!(AFTER.lines().count() < BEFORE.lines().count());

    fs::write(&file, BEFORE).unwrap();
    assert_eq!(add(work.path(), &store), SessionOutcome::Added);
    assert_eq!(add(work.path(), &store), SessionOutcome::Unchanged);

    fs::write(&file, AFTER).unwrap();
    assert_eq!(add(work.path(), &store), SessionOutcome::Updated);

    fs::write(&file, BEFORE).unwrap();
    assert_eq!(add(work.path(), &store), SessionOutcome::Older);
}
