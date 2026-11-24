set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Whole gate, as run by CI.
check: check-rust check-web

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

check-web: web-lint web-typecheck web-test web-build web-routes-drift web-e2e

web-install:
    cd web && bun install --frozen-lockfile

web-lint:
    cd web && bun run lint

web-typecheck:
    cd web && bun run typecheck

web-test:
    cd web && bun run test

web-build:
    cd web && bun run build

# The build regenerates the route tree; a diff means it was not committed.
web-routes-drift:
    git diff --exit-code -- web/src/routeTree.gen.ts

# Browser binaries are not in bun.lock; this installs the build matching @playwright/test.
web-e2e-install:
    cd web && bunx playwright install chromium

web-e2e:
    cd web && bun run e2e
