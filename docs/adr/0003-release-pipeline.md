# 3. Release pipeline: release-please, tag-built binaries

- Status: Accepted
- Date: 2026-11-24

## Context

Nothing is deployed: overseer ships as two binaries, `overseer-cli` and `overseer-fake-llm`. [ADR 0002](0002-toolchain-and-frontend-stack.md) fixes the gate but says nothing about how a version is chosen, tagged or published, nor about the CI job that builds the shippable binaries.

## Decision

### Version and changelog: release-please

- `release-please.yml` runs on every push to `main`. `release-please-config.json` names one package, `.`, with release type `rust`; `.release-please-manifest.json` holds its current version.
- `bump-minor-pre-major` is on: while the major version is 0, a breaking change moves the minor, not the major. `include-component-in-tag` is off, so tags are `vX.Y.Z`.
- Merging the release PR is the only version bump path: it writes `Cargo.toml`'s workspace version, updates the changelog, creates the tag and the GitHub release. Commit messages carry the changelog through Conventional Commits.

### Artifacts: built from the tag

- `release.yml` runs on `v*` tags. It runs `just release` (`cargo build --release --locked -p overseer-cli -p overseer-fake-llm`) and uploads both binaries to the release the tag names.
- The release may not exist yet when the tag build starts, so the upload waits: six `gh release view` attempts five seconds apart, then `gh release upload … --clobber`. `--clobber` makes a re-run of the same tag safe.
- `just release` is the same recipe locally, so the CI path is reproducible without the workflow.
- The CI `build` job runs `just release` on every push and keeps the binaries as a workflow artifact for 14 days. It exists to fail before a tag when the optimized build stops linking.

### Permissions

The gate token is read-only. `release-please.yml` takes `contents: write` and `pull-requests: write`; `release.yml` takes `contents: write`, because it attaches assets to a release. Non-GitHub actions are pinned to commit SHAs; GitHub-owned `actions/*` use major-version tags.

## Consequences

- A manual `git tag` bypasses the changelog and the version file: the release is created empty and the asset build races it.
- If the release is not created within about 30 seconds of the tag build, the upload fails; the workflow is re-run, not the tag.
- Only Linux x86_64 is built. macOS and arm64 users build from source until a target matrix is added to `release.yml`.
- Artifacts are unsigned and come with no checksums, so a downloader cannot verify them beyond TLS and the GitHub release page.
- The `build` job spends a runner on every push, including documentation-only changes.
- Every release is a merge on `main`, so the release commit is a merge commit and not a hand-edited one.

## Alternatives considered

- **Manual tags and a hand-written changelog.** No dependency, but the version and the changelog drift from the commits, and nothing enforces that a tag was meant to be released.
- **`cargo-dist`.** Produces installers, shell completions and a homebrew tap. More machinery than two binaries need, and it owns the release flow.
- **`goreleaser`.** Same idea, Go-first; a second ecosystem to learn for a Rust-only repository.
- **Container image.** Nothing is deployed and there is no registry to publish to.
- **Building artifacts only on the tag.** Cheaper, but an optimized build that stops linking is discovered during a release instead of on the push that broke it.
- **macOS and arm64 targets now.** Each target is a runner cost and a separate binary to test; deferred until someone needs a prebuilt one.
