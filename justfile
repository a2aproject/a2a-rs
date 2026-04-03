# A2A Rust SDK — build toolchain
# See https://just.systems for installation

# Default recipe: build + test
default: build test

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
lint:
    cargo clippy --workspace --all-targets -- -D warnings

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
    rustup run stable cargo llvm-cov --workspace --lcov --output-path coverage/lcov.info --ignore-filename-regex 'gen/'
    @echo "LCOV report: coverage/lcov.info"

# Run the helloworld example
example:
    cargo run -p helloworld

# Clean build artifacts
clean:
    cargo clean

# Check that the project compiles without producing binaries
check-compile:
    cargo check --workspace

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
