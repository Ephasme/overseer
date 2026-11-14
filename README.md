# overseer

A coding-agent harness written in Rust. One overseer agent coordinates a fleet of worker agents and never does the work itself. It has no file or shell tools: it spawns, messages, lists and administers workers, and reads the list of git repositories already sitting under the directories you configure, which starts it with its fleet inventory and refills it through `list_repos`. Each worker starts in an existing directory you name — the overseer picks one of the repositories listed under those roots — with `bash`, `read_file`, `write_file`, and `reply` (a progress note to the overseer). There is no clone tool. Workers are not sandboxed: their commands run without confirmation and can reach any path your user can.

The overseer administers its workers at any moment. An urgent message cuts the running turn short, a `bash` command included, and runs ahead of anything queued. `pause` lets the current instruction finish, then holds the queue; `resume` releases it; `stop` kills the turn and ends the worker; `restart` kills the turn, drops the history and starts the given task. It also sets what each worker may do — at spawn or later, without a restart: `read_only` refuses `write_file` and every `bash` command that is not on a fixed read-only list (`ls cat head tail wc stat file tree pwd rg grep diff jq`, with `git` limited to `status log diff show blame branch remote ls-files rev-parse cat-file grep describe shortlog tag`), `deny_tools` bans a tool by name, and `allow_commands`/`deny_commands` match command prefixes, deny first. That is a guard, not a sandbox: commands are matched as text, so keep the lists tight.

## Install

```sh
brew install ephasme/tap/overseer
```

Prebuilt for macOS (Apple silicon and Intel) and Linux (x86_64). From source: `cargo install --git https://github.com/Ephasme/overseer`, or `just install` in a clone, which builds the release binary into `~/.local/bin`.

## Configure

`~/.config/overseer/config.toml`:

```toml
roots = ["~/code"]                    # repositories under these are listed for the overseer; not a sandbox
base_url = "https://api.openai.com/v1" # any OpenAI-compatible /chat/completions endpoint
model = "gpt-4.1"
theme = "auto"                        # auto | dark | light | mono; omitted means auto
```

The first three keys are required; `theme` is optional. `auto` follows a non-empty `NO_COLOR` (every slot then takes the terminal's own foreground, and selection is reverse video) and is otherwise the dark ramp; an explicit `theme` outranks `NO_COLOR`. The API key comes from `OVERSEER_API_KEY`, or from `~/.config/overseer/.env` when the environment does not set it:

```sh
OVERSEER_API_KEY=...
```

## Run

```sh
overseer
```

On a terminal it opens a full-screen UI: the transcript, a worker panel on the right, and the prompt at the bottom. Every model call reaches its view one way: entries appended in the order the call produced them, so a thought, the answer and the tool call it asks for read top to bottom, and a worker's stream opens with the instruction that started it. The transcript is the lead's own stream: a worker's tool calls, progress notes and messages never enter it, only one lifecycle line per event (`started`, `says`, `finished`, `failed`, `interrupted`). Every worker has a stream of its own, with its own scrollback and scroll position, opened by clicking its row or with F4; the worker panel counts the lines a closed view missed and shows each worker as working, paused, stopped, idle or failed, with `read-only` when restricted. `/resume <id>` re-tasks a worker without spending a model call; `exit` quits at once.
Typing `/` at the start of the prompt opens a completion popup of the commands overseer knows, narrowed by what you type; `/resume ` follows with the agent ids that exist right now, each with its state and its directory, so an id that is gone is never offered.

|Key|Effect|
|---|---|
|enter|send the prompt; with the completion popup open, take the highlighted command instead|
|tab|with the completion popup open: take the highlighted command|
|↑ / ↓|with the completion popup open: the previous or next completion, leaving the caret alone|
|esc|with the completion popup open: close it and keep what you typed|
|ctrl+j|new line in the prompt|
|ctrl+v|attach the image on the clipboard to the prompt; the message carries its path|
|ctrl+x|drop the attached image|
|PageUp / PageDown|scroll the view you are in; End follows new output again|
|F2|hide or show the worker panel|
|F3|expand every tool card, or collapse it back: what the call returned, and the whole body of a long call, which starts at two rows plus a `… N more lines` row|
|ctrl+o|the same toggle as F3, for a keyboard without function keys: press it to open a card|
|F4|open the picked worker's view, or close it|
|F5|hand the mouse back to the terminal, so its own selection and copy work; F5 captures it again, in either view|
|click a worker row|open that worker's view; click its `esc back to fleet` line to come back|
|↑ / ↓|inside a worker view: the previous or next worker|
|PageUp / Home|inside a worker view: scroll its own stream|
|esc|inside a worker view: back to the lead's stream; it never quits the app|
|ctrl+p / ctrl+n|recall the previous or next prompt|
|ctrl+d|quit once no worker is busy|
|ctrl+c|quit now|

With stdin or stdout redirected, or with `OVERSEER_PLAIN=1`, it runs in line mode instead: one prompt per stdin line, answers on stdout, worker activity and tool calls on stderr. Either way, worker reports wake the overseer on their own. A worker's `bash` command is killed after 300 seconds. Each worker logs its instructions, tool calls and results to `~/.local/state/overseer/sessions/<start time>/worker-<id>.log`; `tail -f` one to watch it.

## Release

1. Bump `version` in `Cargo.toml` and merge it.
2. Push the tag `v<version>` on `main`.

CI checks that the tag matches the crate, builds the three binaries, publishes the GitHub release, then updates `Formula/overseer.rb` in `Ephasme/homebrew-tap`. That last step uses the secret `HOMEBREW_TAP_TOKEN`, a fine-grained token with Contents read-write on the tap only; when it expires, releases stop reaching Homebrew until it is replaced.
