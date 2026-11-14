# overseer

A coding-agent harness written in Rust. One overseer agent coordinates a fleet of worker agents and never does the work itself. It has no file or shell tools: it spawns, messages and lists workers, and reads the list of git repositories already sitting under the directories you configure, which starts it with its fleet inventory and refills it through `list_repos`. Each worker starts in one existing repository under those directories, with `bash`, `read_file`, `write_file`, and `reply` (a progress note to the overseer). There is no clone tool. Workers are not sandboxed: their commands run without confirmation and can reach any path your user can.

## Install

```sh
brew install ephasme/tap/overseer
```

Prebuilt for macOS (Apple silicon and Intel) and Linux (x86_64). From source: `cargo install --git https://github.com/Ephasme/overseer`, or `just install` in a clone, which builds the release binary into `~/.local/bin`.

## Configure

`~/.config/overseer/config.toml`:

```toml
roots = ["~/code"]                    # each worker starts in an existing repository under one of these
base_url = "https://api.openai.com/v1" # any OpenAI-compatible /chat/completions endpoint
model = "gpt-4.1"
```

All three keys are required. The API key comes from `OVERSEER_API_KEY`, or from `~/.config/overseer/.env` when the environment does not set it:

```sh
OVERSEER_API_KEY=...
```

## Run

```sh
overseer
```

Talk to the overseer on stdin. Worker reports wake it on their own. Each worker logs its instructions, tool calls and results to `~/.local/state/overseer/sessions/<start time>/worker-<id>.log`; `tail -f` one to watch it. `exit` quits at once; Ctrl-D quits once no worker is busy.

## Release

1. Bump `version` in `Cargo.toml` and merge it.
2. Push the tag `v<version>` on `main`.

CI checks that the tag matches the crate, builds the three binaries, publishes the GitHub release, then updates `Formula/overseer.rb` in `Ephasme/homebrew-tap`. That last step uses the secret `HOMEBREW_TAP_TOKEN`, a fine-grained token with Contents read-write on the tap only; when it expires, releases stop reaching Homebrew until it is replaced.
