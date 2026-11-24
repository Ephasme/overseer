# 2. Toolchain, quality gate and frontend stack

- Status: Accepted
- Date: 2026-11-24

## Context

[ADR 0001](0001-v0-architecture.md) fixes the architecture: a Rust server and a React + TypeScript client. This ADR fixes the tools that build, check and test both, and the client's libraries. Exact versions live in `rust-toolchain.toml`, `Cargo.lock` and `bun.lock`; only major versions that change behaviour are named here.

## Decision

### Rust

- Toolchain pinned in `rust-toolchain.toml` (1.100), edition 2024, with `rustfmt` and `clippy` components.
- Lints in `[workspace.lints]`, inherited by every crate: clippy `pedantic` at warn, with the noisiest lints allowed explicitly there and nowhere else; `unsafe_code = "forbid"`. The gate runs clippy with `-D warnings`.
- Tests: `cargo nextest`.
- Unused dependencies: `cargo shear`.
- Supply chain: `cargo deny` checks advisories, licenses, banned and duplicate crates (`deny.toml`).
- Libraries: tokio, axum 0.8, reqwest, SeaORM 2, `tracing`; `thiserror` in library crates, `anyhow` in binaries only.

### API contract: OpenAPI, generated from Rust

- The server describes its routes and every protocol type with `utoipa` 6 and `utoipa-axum`, so a route cannot exist without its spec entry.
- The spec is written to `web/openapi.json` by a `just openapi` recipe and committed. CI regenerates it and fails on any diff.
- The SSE event union is published as a named schema component; the `text/event-stream` route references it, so event types are generated like any other.
- The client is generated from that spec by `@hey-api/openapi-ts` with three plugins: the fetch SDK, Zod schemas (Zod 4), and TanStack Query hooks. The SDK validates every request and response against its Zod schema. No hand-written request or response type.

### Validation: Zod at every boundary

Data crossing into or out of the client is parsed with a Zod 4 schema before use; a TypeScript type alone never vouches for external data. TypeScript types are inferred from the schemas (`z.infer`), never declared beside them.

| Boundary | Schema source |
|---|---|
| REST requests and responses | generated from OpenAPI, enforced by the SDK |
| SSE events: each `data:` payload, parsed before the reducer sees it | generated event-union schema |
| URL search params | TanStack Router `validateSearch` with a Zod schema |
| Form input | Zod schema, shared with the request schema where one exists |
| `localStorage` / `sessionStorage` | hand-written schema; a payload that fails to parse is discarded |
| `import.meta.env` | hand-written schema, parsed once at startup |

A parse failure is an error surfaced in the UI and logged, never coerced or ignored.

### Web client

- Package manager and script runner: bun. `bun.lock` committed.
- Build: Vite 8, single-page app; no SSR, the server serves the static bundle.
- React 19 with the React Compiler; no hand-written `useMemo`/`useCallback` for performance.
- TypeScript 7 (native `tsc`), `strict` plus `noUncheckedIndexedAccess` and `exactOptionalPropertyTypes`. Type-checking is a gate step of its own; Vite does not type-check.
- Lint and format: Biome 2, one `biome.json`, recommended rules as errors.
- Routing: TanStack Router, file-based, with typed search params.
- Server state: TanStack Query, via the generated query hooks, for REST. SSE goes through one in-house `EventSource` hook that parses and folds events into session state; SSE data never goes through the Query cache.
- Design system: shadcn/ui on Base UI, styled with Tailwind 4. Components are copied into `web/src/components/ui` by the shadcn CLI and owned by the repo.
- Markdown rendering for assistant output: `react-markdown`.

### Tests

- Rust: unit and integration tests under nextest. Store tests run on in-memory SQLite and, when `TEST_DATABASE_URL` is set, on Postgres.
- Web unit tests: Vitest + Testing Library. The SSE reducer and the tool-call cards are tested directly.
- End-to-end: Playwright against the real server binary, with a mock LLM endpoint serving recorded SSE streams; no network, no real model.

### Task runner and gate

- `just` is the single entry point: `just check` runs the whole gate locally, CI runs the same recipes.
- Gate: `cargo fmt --check`, clippy, nextest, `cargo shear`, `cargo deny check`, OpenAPI drift check, `biome ci`, `tsc --noEmit`, Vitest, Playwright.

### CI

GitHub Actions, one workflow, two jobs:

- **rust**: the Rust gate plus store tests against Postgres in a service container.
- **web**: OpenAPI drift check, Biome, `tsc`, Vitest, Playwright (needs the server binary, so it builds it).

## Consequences

- Clippy `pedantic` produces false positives; each allowed lint is listed once in `Cargo.toml` with nothing scattered as `#[allow]`.
- OpenAPI costs a `#[utoipa::path]` annotation per route and `ToSchema` derives on every protocol type. In return the REST API is documented and the client types and Zod schemas cannot drift from the server.
- OpenAPI describes an SSE stream only loosely: nothing in the spec enforces that the stream emits the event schema. The client's Zod parse catches a mismatch at runtime; the Playwright suite catches it before release.
- Validating every payload costs a parse per response and per SSE event. Negligible at this scale; a streaming delta is a few bytes.
- TypeScript 7 is new: a tool that loads the TypeScript compiler API in-process may not support it yet. `@hey-api/openapi-ts` is the first suspect: its peer range stops at 6, and support for 7 is untested. A tool that fails gets TypeScript 5 pinned for itself; the app still type-checks with 7.
- bun is the package manager, but Vitest and Playwright run their own runners, not `bun test`. Playwright's support for running under bun is to be confirmed at scaffold time; if it fails, Playwright runs under Node.
- Biome has fewer type-aware rules than typescript-eslint; `tsc` strict settings carry that load.
- shadcn components are source code in the repo: upgrades are manual re-runs of the CLI and a diff review.

## Alternatives considered

- **ts-rs** (Rust types → TS directly). Less machinery, but documents types only, not routes, and leaves request/response wiring and Zod schemas hand-written.
- **`openapi-typescript` + `openapi-fetch`.** Types only, no runtime validation: Zod schemas would be written by hand beside the generated types and drift.
- **ESLint + typescript-eslint + Prettier.** Most type-aware rules, slowest, two tools to configure. **oxlint + Prettier**: fast, still two tools.
- **pnpm on Node.** Most compatible; bun chosen for install speed and a single tool.
- **React Router.** Equivalent; TanStack Router chosen for typed routes and search params, and to share TanStack conventions with Query.
- **shadcn/ui on Radix.** Mature; Base UI is the current shadcn default for new projects.
- **Hand-written components on plain Tailwind.** No dependency, but accessibility (focus traps, keyboard navigation, ARIA) must be rebuilt.
