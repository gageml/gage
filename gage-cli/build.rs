use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    write_version();
}

// Compute the version string baked into `gage --version`. Release builds set
// RELEASE to take the version from Cargo.toml, the single source of truth
// (dist verifies the release tag against it). Other builds use the current git
// commit (mirroring `rune --version`), falling back to the crate version when
// git is unavailable (e.g. a published crate).
fn write_version() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));

    let version = if env::var_os("RELEASE").is_some() {
        cargo_version()
    } else {
        git_commit_version().unwrap_or_else(cargo_version)
    };

    fs::write(out_dir.join("version.txt"), version).expect("writing version.txt");

    println!("cargo::rerun-if-env-changed=RELEASE");
    println!("cargo::rerun-if-changed=../.git/HEAD");
}

fn cargo_version() -> String {
    env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION set by cargo")
}

fn git_commit_version() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let rev = String::from_utf8(output.stdout).ok()?;
    let rev = rev.trim();
    (!rev.is_empty()).then(|| format!("git-{rev}"))
}
