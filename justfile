# llm-proxy Rust v2 developer tasks
# Usage: just [recipe]

set windows-shell := ["cmd.exe", "/C"]

default: lint

# ── Lint ───────────────────────────────────────────────────────────────────

# Check formatting and clippy lint (deny warnings).
lint:
    cargo fmt -- --check
    cargo clippy --all-targets -- -D warnings

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

# Run unit and integration tests with cargo-nextest (required dependency).
test args="":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v cargo-nextest >/dev/null 2>&1; then
        echo "❌ cargo-nextest 未安装（开发环境必需依赖）。请执行: cargo install --locked cargo-nextest" >&2
        exit 1
    fi
    cargo nextest run --all-targets {{args}}

# ── Release Gate ───────────────────────────────────────────────────────────

# Full release gate: fmt + clippy + tests.
release-gate args="":
    cargo fmt -- --check
    cargo clippy --all-targets -- -D warnings
    cargo nextest run --all-targets {{args}}

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
