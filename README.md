# overseer

A coding-agent harness written in Rust. One overseer agent coordinates a fleet of agents and never does the work itself. It has no file or shell tools: it spawns, messages, lists and administers agents, and reads the list of git repositories already sitting under the directories you configure, which starts it with its fleet inventory and refills it through `list_repos`. Each agent starts in an existing directory you name — the overseer picks one of the repositories listed under those roots — with `bash`, `read_file`, `write_file`, and `reply` (a progress note to the overseer). There is no clone tool. Agents are not sandboxed: their commands run without confirmation and can reach any path your user can.

The overseer administers its agents at any moment. An urgent message cuts the running turn short, a `bash` command included, and runs ahead of anything queued. `pause` lets the current instruction finish, then holds the queue; `resume` releases it; `stop` kills the turn and ends the agent; `restart` kills the turn, drops the history and starts the given task. It also sets what each agent may do — at spawn or later, without a restart: `read_only` refuses `write_file` and every `bash` command that is not on a fixed read-only list (`ls cat head tail wc stat file tree pwd rg grep diff jq`, with `git` limited to `status log diff show blame branch remote ls-files rev-parse cat-file grep describe shortlog tag`), `deny_tools` bans a tool by name, and `allow_commands`/`deny_commands` match command prefixes, deny first. That is a guard, not a sandbox: commands are matched as text, so keep the lists tight.

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

On a terminal it opens a full-screen UI: the transcript, an agent panel on the right, and the prompt at the bottom. Every model call reaches its view one way: entries appended in the order the call produced them, so a thought, the answer and the tool call it asks for read top to bottom, and an agent's stream opens with the instruction that started it. Model prose renders as Markdown: headings, emphasis, lists, quotes, links and tables; tool calls and their output stay verbatim. The transcript is the lead's own stream: an agent's tool calls, progress notes and messages never enter it, only one lifecycle line per event (`started`, `says`, `finished`, `failed`, `interrupted`). Every agent has a stream of its own, with its own scrollback and scroll position, opened by clicking its row or with F4; the agent panel counts the lines a closed view missed and shows each agent as working, paused, stopped, idle or failed, with `read-only` when restricted. Each agent is labelled with a codename drawn from its id: the panel shows `brave-heron (2)`, and the lines that already carry the id show the name alone, `brave-heron`. The id stays what `/resume` and `message_agent` take. `ctrl+r` renames the agent picked in the panel, else the newest one, and the name you type replaces the codename for the rest of the run, everywhere that agent is named. Names live in memory only: a fresh run starts from the codenames again. `/resume <id>` re-tasks an agent without spending a model call; `exit` quits at once.
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
|F2|hide or show the agent panel|
|ctrl+0|expand every tool card, or collapse it back: a card starts as one row, and expands to the whole body plus what the call returned|
|F4|open the picked agent's view, or close it|
|F5|hand the mouse back to the terminal, so its own selection and copy work; F5 captures it again, in either view|
|click an agent row|open that agent's view; click its `esc back to fleet` line to come back|
|drag the rule left of the panel|resize the agent panel: the transcript keeps at least 40 columns, the panel never drops below 20, and the width it is left at opens the next run|
|↑ / ↓|inside an agent view: the previous or next agent|
|PageUp / Home|inside an agent view: scroll its own stream|
|esc|in an agent's view: stop that agent and everything below it now; with nothing running, back to the lead's stream. It never stops the overseer and never quits the app|
|ctrl+p / ctrl+n|recall the previous or next prompt|
|ctrl+r|rename the agent picked in the panel, else the newest one; enter accepts, esc cancels|
|ctrl+d|quit once no agent is busy|
|ctrl+c|quit now|

With stdin or stdout redirected, or with `OVERSEER_PLAIN=1`, it runs in line mode instead: one prompt per stdin line, answers on stdout, agent activity and tool calls on stderr. Either way, agent reports wake the overseer on their own. An agent's `bash` command is killed after 300 seconds. Each agent logs its instructions, tool calls and results to `~/.local/state/overseer/sessions/<start time>/agent-<id>.log`; `tail -f` one to watch it.

## Release

1. Bump `version` in `Cargo.toml` and merge it.
2. Push the tag `v<version>` on `main`.

CI checks that the tag matches the crate, builds the three binaries, publishes the GitHub release, then updates `Formula/overseer.rb` in `Ephasme/homebrew-tap`. That last step uses the secret `HOMEBREW_TAP_TOKEN`, a fine-grained token with Contents read-write on the tap only; when it expires, releases stop reaching Homebrew until it is replaced.
