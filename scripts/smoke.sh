#!/usr/bin/env bash
# Runs the CLI against the scripted fake model, end to end, with no network and no key.
set -euo pipefail

port=${1:-8799}
cargo build -q -p overseer-fake-llm -p overseer-cli

target/debug/overseer-fake-llm --port "$port" --fixtures crates/core/tests/fixtures/llm &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT

for _ in $(seq 1 50); do
    if nc -z 127.0.0.1 "$port" 2>/dev/null; then break; fi
    sleep 0.1
done

tmp=$(mktemp -d)
trap 'kill "$server" 2>/dev/null || true; rm -rf "$tmp"' EXIT
printf 'base_url = "http://127.0.0.1:%s/v1"\nmodel = "fake"\n' "$port" >"$tmp/config.toml"

output=$(printf 'hello\n' | OVERSEER_CONFIG="$tmp/config.toml" target/debug/overseer-cli)
printf '%s\n' "$output"
grep -q "Hello, world" <<<"$output"
grep -q "turn end: Done" <<<"$output"
