# A2A Rust SDK — build toolchain
# See https://just.systems for installation

# Default recipe: build + test
default: fmt-check lint build test

# Install cargo hack for workspace-aware builds
cargo-hack:
    @command -v cargo-hack >/dev/null 2>&1 || cargo install cargo-hack --locked

check-compile: cargo-hack
    cargo hack check --workspace --exclude itk-rust-current-agent --all-targets --each-feature

# Build all workspace crates
build:
    cargo build --workspace --exclude itk-rust-current-agent

# Build in release mode
build-release:
    cargo build --workspace --exclude itk-rust-current-agent --release

# Run all tests
test:
    cargo test --workspace --exclude itk-rust-current-agent

# Run tests with output shown
test-verbose:
    cargo test --workspace --exclude itk-rust-current-agent -- --nocapture

# Run clippy lints
lint: cargo-hack
    cargo hack clippy --workspace --exclude itk-rust-current-agent --all-targets --each-feature -- -D warnings

# Check formatting
fmt-check:
    cargo fmt --all -- --check

# Apply formatting
fmt:
    cargo fmt --all

# Run all checks (format, lint, test)
check: fmt-check lint test

# Generate code coverage report (requires cargo-llvm-cov)
coverage:
    rustup component add llvm-tools-preview --toolchain stable
    rustup run stable cargo llvm-cov --workspace --exclude itk-rust-current-agent --html --ignore-filename-regex 'gen/'
    @echo "Coverage report: target/llvm-cov/html/index.html"

# Generate LCOV coverage output for CI uploads
coverage-lcov:
    mkdir -p coverage
    rustup component add llvm-tools-preview --toolchain stable
    rustup run stable cargo llvm-cov --workspace --exclude itk-rust-current-agent --no-report --ignore-filename-regex 'gen/'
    rustup run stable cargo llvm-cov report --lcov --output-path coverage/lcov.info --ignore-filename-regex 'gen/'
    rm -rf target/llvm-cov/html
    rustup run stable cargo llvm-cov report --html --output-dir target/llvm-cov --ignore-filename-regex 'gen/'
    @echo "LCOV report: coverage/lcov.info"
    @echo "Coverage report: target/llvm-cov/html/index.html"

# Run the helloworld example
example:
    cargo run -p helloworld

# Clean build artifacts
clean:
    cargo clean

# Verify copyright headers on all Rust source files
check-headers:
    #!/usr/bin/env bash
    set -euo pipefail
    # Each required line just needs to appear somewhere in the file's first 5
    # lines -- not on a fixed line number, and independent of the others' order
    # or presence -- so adding, reordering or dropping a copyright holder here
    # is the only edit a future change needs.
    required=(
        "Copyright AGNTCY Contributors (https://github.com/agntcy)"
        "Copyright A2A Contributors (https://github.com/a2aproject)"
    )
    missing=0
    for f in $(find . -name '*.rs' -not -path '*/gen/*' -not -path '*/target/*'); do
        header=$(head -5 "$f")
        for line in "${required[@]}"; do
            if ! grep -qF "// $line" <<< "$header"; then
                echo "Missing '$line': $f"
                missing=$((missing + 1))
            fi
        done
    done
    if [ "$missing" -gt 0 ]; then
        echo "ERROR: $missing missing copyright line(s)"
        exit 1
    fi
    echo "All source files have copyright headers"

# Run all CI checks
ci: check-compile check-headers fmt-check lint test
