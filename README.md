<p align="center"><img src="docs/brand/logo.png" alt="" width="120" /></p>

# overseer

A coding agent harness: a Rust server that runs the agent loop, a React client that drives it.

Phases 0–3 of [the v0 plan](docs/plans/2026-11-24-overseer-v0.md) are in: a streaming model client, four tools, the turn loop, and a CLI. The server and the web client are next.

```bash
just check   # the gate CI runs
just smoke   # the CLI end to end against a scripted model, no key needed
just mock    # the scripted model on its own
```

Architecture: [ADR 0001](docs/adr/0001-v0-architecture.md), [ADR 0002](docs/adr/0002-toolchain-and-frontend-stack.md).
