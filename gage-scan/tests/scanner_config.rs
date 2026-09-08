use gage_registry::scanner::ScannerRegistry;

fn registry() -> ScannerRegistry {
    ScannerRegistry::load()
}

#[test]
fn unknown_scanner_name_does_not_resolve() {
    let reg = registry();
    assert!(reg.get_def("nonexistent").is_none());
    assert!(!reg.is_known("nonexistent"));
}
