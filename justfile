# Show this help and exit
help:
    @just -l

# Build Gage
build:
    cargo build

# Run Clippy (lints)
clippy:
    cargo clippy

# Run tests
test:
    cargo test

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

# Generate distribution
dist: _require_dist
    dist build

# Refresh dist ga workflows
dist-init: _require_dist
    dist init --yes

_require_dist:
    @command -v dist >/dev/null 2>&1 || { echo "This recipe requires dist - run 'cargo install cargo-dist --locked' to install it"; exit 1; }

# Propose jj commit messages
commit:
    claude -p /commit-jj \
      --model sonnet \
      --allowedTools "Bash(jj status:*)" "Bash(jj diff:*)" "Bash(jj log:*)"
