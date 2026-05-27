# A2A Rust SDK — build toolchain
# See https://just.systems for installation

# Default recipe: build + test
default: fmt-check lint build test

# Install cargo hack for workspace-aware builds
cargo-hack:
    @command -v cargo-hack >/dev/null 2>&1 || cargo install cargo-hack --locked

check-compile: cargo-hack
    cargo hack check --workspace --all-targets --each-feature

# Build all workspace crates
build:
    cargo build --workspace

# Build in release mode
build-release:
    cargo build --workspace --release

# Run all tests
test:
    cargo test --workspace

# Run tests with output shown
test-verbose:
    cargo test --workspace -- --nocapture

# Run clippy lints
lint: cargo-hack
    cargo hack clippy --workspace --all-targets --each-feature -- -D warnings

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
    rustup run stable cargo llvm-cov --workspace --html --ignore-filename-regex 'gen/'
    @echo "Coverage report: target/llvm-cov/html/index.html"

# Generate LCOV coverage output for CI uploads
coverage-lcov:
    mkdir -p coverage
    rustup component add llvm-tools-preview --toolchain stable
    rustup run stable cargo llvm-cov --workspace --no-report --ignore-filename-regex 'gen/'
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
    missing=0
    for f in $(find . -name '*.rs' -not -path '*/gen/*' -not -path '*/target/*'); do
        if ! head -1 "$f" | grep -q '^// Copyright AGNTCY'; then
            echo "Missing header: $f"
            missing=$((missing + 1))
        fi
    done
    if [ "$missing" -gt 0 ]; then
        echo "ERROR: $missing file(s) missing copyright header"
        exit 1
    fi
    echo "All source files have copyright headers"

# Run all CI checks
ci: check-compile check-headers fmt-check lint test
