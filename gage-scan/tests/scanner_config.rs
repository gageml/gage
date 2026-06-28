use gage_registry::scanner::{Scanner, ScannerRegistry};

fn registry() -> ScannerRegistry {
    ScannerRegistry::load()
}

#[test]
fn from_spec_unknown_scanner() {
    let reg = registry();
    let err = Scanner::from_spec("nonexistent#{x: 1}", &reg).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("Unknown scanner"), "got: {msg}");
}
