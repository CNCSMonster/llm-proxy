# llm-proxy Rust v2 developer tasks
# Usage: just [recipe]

set windows-shell := ["cmd.exe", "/C"]

default: lint

# ── Lint ───────────────────────────────────────────────────────────────────

# Check formatting, clippy lint (deny warnings), and tests.
lint:
    cargo fmt -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test

# Apply Rust formatting.
lint-fix:
    cargo fmt

# ── Build ──────────────────────────────────────────────────────────────────

# Build the current platform debug binary.
build:
    cargo build

# Build the current platform release binary (formal: no dirty hash).
build-release:
    cargo build --release --features formal

# ── Test ───────────────────────────────────────────────────────────────────

# Run unit and integration tests.
test:
    cargo test

# Run test coverage check (core modules ≥ 80%).
coverage:
    bash scripts/check-coverage.sh 80

# ── Release Gate ───────────────────────────────────────────────────────────

# Full release gate: fmt + clippy + test + coverage + release build.
# Options: --skip-e2e (skip real-client E2E), --quick (skip release build).
release-gate args="":
    bash scripts/release-gate.sh {{args}}

# ── Install ────────────────────────────────────────────────────────────────

# Build release binary and install it. Default destination is ~/.cargo/bin.
install dest="~/.cargo/bin":
    #!/usr/bin/env bash
    set -e
    dest="{{dest}}"
    dest="${dest/#\~/$HOME}"
    cargo build --release --features formal
    mkdir -p "$dest"
    cp target/release/llm-proxy "$dest/llm-proxy"
    echo "installed: $dest/llm-proxy"

# ── Clean ──────────────────────────────────────────────────────────────────

# Remove Cargo build artifacts.
clean:
    cargo clean
