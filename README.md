# overseer

A coding-agent harness written in Rust. One overseer agent runs the tree and never does the work itself: it spawns, messages, lists and administers agents, reads files, runs read-only commands, and reads the list of git repositories already sitting under the directories you configure, which starts it with its fleet inventory and refills it through `list_repos`. Every agent runs a profile from `~/.overseer/agent/profiles`: the profile fixes its model, its reasoning effort, its role and the exact tools it may call, and `spawn_agent` cannot add or remove one. Each agent starts in an existing directory you name — the overseer picks one of the repositories listed under those roots. There is no clone tool. `read_file` numbers its lines and `edit` anchors on those numbers, refusing a file the agent has not read or one that changed since; `write_file` creates a file. Agents are not sandboxed: their commands run without confirmation and can reach any path your user can.

The overseer administers its agents at any moment. An urgent message cuts the running turn short, a `bash` command included, and runs ahead of anything queued. `pause` lets the current instruction finish, then holds the queue; `resume` releases it; `stop` kills the turn and ends the agent; `archive` takes a settled one out of the roster while its entry and log stay. An agent ends its own turn by calling `close` with its report, and its owner re-opens it by sending another instruction. What an agent may do comes from its profile: `allow_tools` grants tools one by one (`mcp__<server>` grants a whole server), `read_only` refuses `write_file`, `edit` and every `bash` command that is not on a fixed read-only list (`ls cat head tail wc stat file tree pwd rg grep diff jq`, with `git` limited to `status log diff show blame branch remote ls-files rev-parse cat-file grep describe shortlog tag`), and `allow_commands`/`deny_commands` match the command's words, deny first. That is a guard, not a sandbox: commands are matched as text, so keep the lists tight.

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
approvals = "ask"                     # ask | cached-ask | write | yolo; omitted means ask
```

The first three keys are required; `theme` and `approvals` are optional. `approvals` gates `bash`, `write_file` and `edit` in the TUI: `ask` prompts on every gated call, `cached-ask` prompts the first time each distinct call appears and replays the answer for the rest of the session, `write` lets the two file tools through and applies cached-ask to bash, and `yolo` never prompts. At a prompt, `y` allows that call once, `n` refuses it and `esc` refuses too; `a` allows the call for the rest of the session whatever the tier, so a repeated call stops asking. Line mode has no terminal to answer in, so there a gated call is refused unless the tier is `yolo`. `auto` follows a non-empty `NO_COLOR` (every slot then takes the terminal's own foreground, and selection is reverse video) and is otherwise the dark ramp; an explicit `theme` outranks `NO_COLOR`. The API key comes from `OVERSEER_API_KEY`, or from `~/.config/overseer/.env` when the environment does not set it:

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
|ctrl+o / ctrl+0|expand every tool card, or collapse it back: a shell command always shows whole, and every other card starts as one row and expands to the whole body plus what the call returned|
|F4|open the picked agent's view, or close it|
|F5|hand the mouse back to the terminal, so its own selection and copy work; F5 captures it again, in either view|
|F8|cycle the approval tier (ask → cached-ask → write → yolo); the tier shows in the status line, and a pending prompt answers with `y` allow once, `a` always allow, `n` deny, `esc` deny|
|click an agent row|open that agent's view; click its `esc back to fleet` line to come back|
|drag the rule left of the panel|resize the agent panel: the transcript keeps at least 40 columns, the panel never drops below 20, and the width it is left at opens the next run|
|↑ / ↓|inside an agent view: the previous or next agent|
|PageUp / Home|inside an agent view: scroll its own stream|
|esc|in an agent's view: stop that agent and everything below it now; with nothing running, back to the lead's stream. It never stops the overseer and never quits the app|
|ctrl+p / ctrl+n|recall the previous or next prompt|
|ctrl+r|rename the agent picked in the panel, else the newest one; enter accepts, esc cancels|
|ctrl+d|quit once no agent is busy|
|ctrl+c|quit now|

With stdin or stdout redirected, or with `OVERSEER_PLAIN=1`, it runs in line mode instead: one prompt per stdin line, answers on stdout, agent activity and tool calls on stderr. Either way, agent reports wake the overseer on their own. An agent's `bash` command is killed after 300 seconds. Each agent writes machine-readable JSONL session records — every instruction, committed assistant reply (tool-call arguments included, in full), tool result and turn outcome — to `~/.local/state/overseer/sessions/<start time>/agent-<id>.jsonl`, the substrate resume, search and export build on; `history://<id>` renders those records back into the readable shape, so `tail -f` has an equivalent to watch.

Headless: `overseer -p "prompt"` runs one turn — the answer to stdout, everything else on stderr, exit 1 when the root never answered — and `overseer --mode json` writes each shown event as one compact JSON line on stdout instead, so a consumer filters by `type` (`jq -r 'select(.type=="answer") | .text'`). The two combine; `--mode text` forces line mode like the other flags, with plain text output. Shown event types: `answer` (the root's final answer), `notice`, `error`, `debug` (each with `text`), `agent` (`id`, `name`, `kind` = says/finished/failed/interrupted, `text`), `out` (`id`, `name`, `text`), and `tool` (`id`, `call`, `phase` = start/done; on `done` also `name`, `secs`, `status`, `head`, `more`, `failed`). Usage errors exit 2.

## Release

1. Bump `version` in `Cargo.toml` and merge it.
2. Push the tag `v<version>` on `main`.

CI checks that the tag matches the crate, builds the three binaries, publishes the GitHub release, then updates `Formula/overseer.rb` in `Ephasme/homebrew-tap`. That last step uses the secret `HOMEBREW_TAP_TOKEN`, a fine-grained token with Contents read-write on the tap only; when it expires, releases stop reaching Homebrew until it is replaced.
