scanners := env("GAGE_SCANNER_TESTS", justfile_directory() / ".." / "gage-scanners")

# Show this help and exit
help:
    @just -l

# Build Gage
build:
    cargo build

# Run Clippy (lints)
clippy:
    cargo clippy

# Full verification gate: clippy, fmt, tests. The tests include the
# published scanners, read from the checkout that GAGE_SCANNER_TESTS
# names or from ../gage-scanners.
check:
    @test -d "{{ scanners }}" || { echo "check needs the published scanners: clone gage-scanners to {{ scanners }} or set GAGE_SCANNER_TESTS"; exit 1; }
    cargo clippy --all-targets -- -D warnings
    cargo fmt
    GAGE_SCANNER_TESTS="{{ scanners }}" cargo test

# Run tests
test:
    cargo test

# Validate session drivers against upstream sessions (downloads them)
validate *args:
    cargo run -p gage-validate -- run {{args}}

# Regenerate the Pi reference sessions with the installed Pi (run by hand)
pi-fixtures *args:
    node gage-pi/fixtures/generate.mjs {{args}}

# Install Gage
install:
    cargo install --path gage-cli

# Install a dev wrapper for gage to ~/.local/bin/gage
install-dev:
    #!/bin/bash
    set -e
    cat > ~/.local/bin/gage <<EOF
    #!/bin/bash
    set -e
    PROJECT={{ justfile_directory() }}
    (cd \$PROJECT && cargo build)
    exec \$PROJECT/target/debug/gage "\$@"
    EOF
    chmod +x ~/.local/bin/gage
    echo "Installed gage wrapper to ~/.local/bin/gage (PROJECT={{ justfile_directory() }})"

# Install release build via cargo
install-cargo-bin:
   cargo install --path gage-cli

# Generate distribution
dist: _require_dist
    dist build

# Refresh dist ga workflows
dist-init: _require_dist
    dist init --yes

_require_dist:
    @command -v dist >/dev/null 2>&1 || { echo "This recipe requires dist - run 'cargo install cargo-dist --locked' to install it"; exit 1; }

# List unmerged branches
unmerged:
    @jj log -r 'remote_bookmarks() ~ ::@'
