set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Whole gate, as run by CI.
check: check-rust

check-rust: fmt-check clippy test-rust shear deny

fmt-check:
    cargo fmt --all --check

clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings

test-rust:
    cargo nextest run --workspace --locked --no-tests=pass

shear:
    cargo shear

deny:
    cargo deny check
