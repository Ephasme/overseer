# 2. Toolchain, quality gate and frontend stack

- Status: Accepted
- Date: 2026-11-24
- Build status: everything marked *(planned)* is a decision not yet in the tree, because the store, the server and the client UI it needs are still phases 4–7 of the [v0 plan](../plans/2026-11-24-overseer-v0.md). Unmarked statements are what the repo does today.

## Context

[ADR 0001](0001-v0-architecture.md) fixes the architecture: a Rust server and a React + TypeScript client. This ADR fixes the tools that build, check and test both, and the client's libraries. Exact versions live in `rust-toolchain.toml`, `Cargo.lock` and `bun.lock`; only major versions that change behaviour are named here.

## Decision

### Rust

- Toolchain pinned in `rust-toolchain.toml` (1.100), edition 2024, with `rustfmt` and `clippy` components.
- Lints in `[workspace.lints]`, inherited by every crate: clippy `pedantic` at warn, with the noisiest lints allowed explicitly there and nowhere else; `unsafe_code = "forbid"`. The gate runs clippy with `-D warnings`.
- Tests: `cargo nextest`.
- Unused dependencies: `cargo shear`.
- Supply chain: `cargo deny` checks advisories, licenses, banned and duplicate crates (`deny.toml`).
- Libraries in use: tokio, reqwest, `thiserror` in library crates, `anyhow` in binaries only. *(planned)* axum 0.8, SeaORM 2, `tracing`.

### API contract: OpenAPI, generated from Rust *(planned)*

- The server describes its routes and every protocol type with `utoipa` 6 and `utoipa-axum`, so a route cannot exist without its spec entry.
- The spec is written to `web/openapi.json` by a `just openapi` recipe and committed. CI regenerates it and fails on any diff.
- The SSE event union is published as a named schema component; the `text/event-stream` route references it, so event types are generated like any other.
- The client is generated from that spec by `@hey-api/openapi-ts` with three plugins: the fetch SDK, Zod schemas (Zod 4), and TanStack Query hooks. The SDK validates every request and response against its Zod schema. No hand-written request or response type.

### Validation: Zod at every boundary *(only `import.meta.env` today; the other boundaries arrive with the client)*

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

- Package manager and script launcher: bun, version pinned by `packageManager`; `bun.lock` committed. The tools themselves (Vite, Vitest, `tsc`, Playwright) are Node programs; `web/.node-version` pins Node for CI.
- Build: Vite 8, single-page app; no SSR, the server serves the static bundle.
- React 19 with the React Compiler; no hand-written `useMemo`/`useCallback` for performance.
- TypeScript 7 (native `tsc`), `strict` plus `noUncheckedIndexedAccess` and `exactOptionalPropertyTypes`. Type-checking is a gate step of its own; Vite does not type-check. Browser code and Node-side files (configs, Playwright specs) are separate programs, so Node globals never type-check in browser code.
- Lint and format: Biome 2, one `biome.json`, recommended rules; the gate fails on warnings as well as errors.
- Routing: TanStack Router, file-based, with typed search params.
- Server state *(planned)*: TanStack Query, via the generated query hooks, for REST. SSE goes through one in-house `EventSource` hook that parses and folds events into session state; SSE data never goes through the Query cache.
- Design system: shadcn/ui on Base UI, styled with Tailwind 4. `components.json` is committed with the `base-nova` style; components are copied into `web/src/components/ui` by the shadcn CLI and owned by the repo, starting with the `Button` the home route renders.
- Markdown rendering for assistant output: `react-markdown` *(planned)*.

### Tests

- Rust: unit and integration tests under nextest. *(planned)* store tests on in-memory SQLite and, when `TEST_DATABASE_URL` is set, on Postgres.
- Web unit tests: Vitest; Testing Library *(planned, with the first component that carries behaviour)*. The SSE reducer and the tool-call cards are tested directly *(planned)*.
- End-to-end: Playwright, no network and no real model. It drives the Vite dev server today; against the real server binary and the fake model below *(planned)*.

### Test harness for the client

- **Fake model.** A scripted OpenAI-compatible `/chat/completions` endpoint, a binary of the workspace, answering from the SSE fixtures in `crates/core/tests/fixtures/llm`: `tool_calls.sse` while the raw request body carries the `@tool` sentinel and no `"role":"tool"` yet, `text.sse` otherwise. Fixtures for reasoning, a mid-stream provider error, a slow tool and truncated or `[DONE]`-less streams are committed and replayed by nextest; `just record` captures a new one from a real streamed completion. *(planned)* the real server, session actors, SSE and database running unchanged against it — only the model is fake, so the client speaks the real protocol with no second API implementation to drift — plus scenario files instead of the sentinel, a long-bash-output fixture, and Playwright using those scenarios.
- **`just dev-mock`** *(planned)* starts the server on the fake model, plus Vite, for manual work on the UI. Playwright uses the same scenarios.
- **Storybook** *(planned)* shows each UI state with no backend: streaming text, tool card running, failed or truncated, reconnecting, interrupted session. A story is fed a fixture event log typed by the generated schemas, and every story also runs as a Vitest component test.

### Task runner and gate

- `just` is the single entry point: `just check` runs the whole gate locally, CI runs the same recipes. `just hooks` points `core.hooksPath` at `.githooks`, whose pre-commit runs `just check`.
- Rust gate: `cargo fmt --check`, clippy with `-D warnings`, rustdoc with `-D warnings`, nextest, `cargo shear`, `cargo deny check`.
- Web gate: Biome `ci --error-on-warnings`, `tsc --noEmit` on the browser and Node programs separately, Vitest, `vite build`, a diff check on the generated `web/src/routeTree.gen.ts`, Playwright.
- *(planned)* OpenAPI drift check, once the spec exists.

### CI

GitHub Actions. Non-GitHub actions are pinned to commit SHAs; GitHub-owned `actions/*` use major-version tags. Every job has a timeout, and the gate token is read-only (`permissions: contents: read`).

- `ci.yml`, on every push to `main` and every pull request, with superseded runs cancelled:
  - **rust**: `just check-rust`. *(planned)* store tests against Postgres in a service container.
  - **web**: `just check-web`, after installing the Playwright browser and its system dependencies.
  - **build**: `just release`, uploading the two binaries as a workflow artifact. Not a deploy: it proves the optimized build links.
- `release-please.yml` and `release.yml` are the release path, in [ADR 0003](0003-release-pipeline.md).

## Consequences

- Clippy `pedantic` produces false positives; each allowed lint is listed once in `Cargo.toml` with nothing scattered as `#[allow]`.
- OpenAPI costs a `#[utoipa::path]` annotation per route and `ToSchema` derives on every protocol type. In return the REST API is documented and the client types and Zod schemas cannot drift from the server.
- OpenAPI describes an SSE stream only loosely: nothing in the spec enforces that the stream emits the event schema. The client's Zod parse catches a mismatch at runtime; the Playwright suite catches it before release.
- Validating every payload costs a parse per response and per SSE event. Negligible at this scale; a streaming delta is a few bytes.
- TypeScript 7 is new: a tool that loads the TypeScript compiler API in-process may not support it yet. `@hey-api/openapi-ts` is the first suspect: its peer range stops at 6, and support for 7 is untested. A tool that fails gets TypeScript 5 pinned for itself; the app still type-checks with 7.
- bun is the package manager, but Vitest and Playwright run their own runners, not `bun test`; both are invoked through `bun run`.
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
