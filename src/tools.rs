use crate::artifacts;
use crate::config;
use crate::conflict;
use crate::diff;
use crate::edit;
use crate::jobs;
use crate::kernel;
use crate::mcp;
use crate::profiles;
use crate::skills;
use crate::url;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::{
    fs,
    io::Read,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub(crate) const MAX_OUTPUT_BYTES: usize = 30_000;

/// Largest file `read_file`'s `:conflicts` selector scans whole, so listing blocks cannot exhaust memory.
const MAX_SCAN_BYTES: u64 = 8 << 20;

/// A command that outlives this is killed: one stuck worker must not stall the fleet forever.
pub(crate) const BASH_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the pipes may stay open after the command itself exited: a child it backgrounded
/// inherits them, and joining its drain would wait for that child.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Process groups running right now, so quitting overseer does not leave a `cargo build` behind.
///
/// A group is deregistered the moment its command is reaped: that is the first moment its id may be
/// handed to another process, and `kill_running` must never signal one that is not ours.
static GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

fn groups() -> MutexGuard<'static, Vec<i32>> {
    GROUPS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Spawns `command` and registers its process group while holding the registry lock, so a sweep
/// cannot drain the list between the start and the registration and leave the command behind.
pub(crate) fn spawn_grouped(command: &mut Command) -> Result<(Child, i32), String> {
    let mut live = groups();
    let child = command.spawn().map_err(|e| e.to_string())?;
    let group = i32::try_from(child.id()).map_err(|e| e.to_string())?;
    live.push(group);
    Ok((child, group))
}

pub(crate) fn deregister(group: i32) {
    groups().retain(|live| *live != group);
}

/// Kills every command still running: the session calls this on its way out, under any frontend.
pub(crate) fn kill_running() {
    let running: Vec<i32> = groups().drain(..).collect();
    for group in running {
        let _ = Command::new("kill").arg("-9").arg(format!("-{group}")).status();
    }
    jobs::kill_all();
    crate::kernel::kill_all();
}

/// Names of the worker's file and shell tools. The schema and the dispatch both use these.
pub(crate) const BASH: &str = "bash";
pub(crate) const READ_FILE: &str = "read_file";
pub(crate) const WRITE_FILE: &str = "write_file";
pub(crate) const EDIT: &str = "edit";
/// The structured search tools, so a policy without `bash` can still find files and lines.
pub(crate) const GLOB: &str = "glob";
pub(crate) const GREP: &str = "grep";
/// The persistent runtimes: one kernel per (working directory, runtime), shared between calls.
pub(crate) const PYTHON: &str = "python";
/// The progress-note tool an agent writes to its owner with.
pub(crate) const REPLY: &str = "reply";
/// The tool an agent ends its turn and reports with.
pub(crate) const CLOSE: &str = "close";
/// Tools every agent keeps whatever its profile grants: they reach only the owner's inbox and the
/// agent's own lifecycle, so an empty allow list can still report and end.
pub(crate) const ALWAYS_GRANTED: &[&str] = &[REPLY, CLOSE];
/// Programs a read-only worker may still run.
pub(crate) const READ_ONLY_PROGRAMS: &[&str] =
    &["ls", "cat", "head", "tail", "wc", "stat", "file", "tree", "pwd", "rg", "grep", "diff", "jq", "git"];

/// `git` subcommands a read-only worker may run; everything else writes to the repository.
pub(crate) const READ_ONLY_GIT: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "branch",
    "remote",
    "ls-files",
    "rev-parse",
    "cat-file",
    "grep",
    "describe",
    "shortlog",
    "tag",
];

/// `git` subcommands that list on flags alone; a bare word after them names a branch, a tag or a
/// remote to write (`git branch topic`, `git tag v1`, `git remote add`).
const GIT_LISTING: &[&str] = &["branch", "remote", "tag"];

/// Arguments that make another listed program write a file or run a program of its own.
const WRITING_ARGS: &[(&str, &[&str])] = &[
    ("git", &["--output"]),
    ("rg", &["--pre", "--hostname-bin"]),
    ("file", &["-C", "--compile"]),
    ("tree", &["-o", "--output"]),
    ("diff", &["-o", "--output"]),
];

/// Shell syntax that writes, feeds a program or hides what runs.
const WRITING_SHELL: &[&str] = &[">", "<", ";", "&&", "||", "&", "`", "$(", "\n"];

/// Shell syntax a word-based command rule cannot see through, so a policy with rules refuses it:
/// `ls; rm -rf x` names its program `ls` to a rule and runs two.
const SHELL_SYNTAX: &[&str] = &[";", "|", "&", ">", "<", "`", "$(", "\n"];

/// Programs that hand the command to the program named after them, for a command rule.
const WRAPPERS: &[&str] = &["env", "command", "exec", "nohup", "nice", "time", "timeout", "sudo"];

/// What one worker may do, as Overseer set it.
///
/// A guard, not a sandbox: a `bash` command is matched word by word, so the lists are only as good
/// as they are tight. Workers already run unsandboxed, with the user's own rights.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Policy {
    /// Tools this agent may call. An exact tool name, or `mcp__<server>` for that server's tools.
    /// Empty grants nothing. A profile that declares no list at all is refused at spawn.
    pub(crate) allow_tools: Vec<String>,
    /// `bash` runs only `READ_ONLY_PROGRAMS`, and `write_file` and MCP tools are refused.
    pub(crate) read_only: bool,
    /// When non-empty, a `bash` command's program word must match one of these.
    pub(crate) allow_commands: Vec<String>,
    /// A `bash` command whose program word matches one of these never runs. Deny beats allow.
    pub(crate) deny_commands: Vec<String>,
    /// Every `bash` command is refused. Two allow lists that share nothing allow nothing, and an
    /// empty `allow_commands` already means "no restriction", so the refusal needs its own flag.
    pub(crate) deny_all_commands: bool,
}

/// Whether `allow_tools` grants `name`: an exact name, or `mcp__<server>` for every tool of that
/// server. The trailing `_` keeps `mcp__slack` from granting `mcp__slackbot_x`.
pub(crate) fn grants(allow_tools: &[String], name: &str) -> bool {
    allow_tools.iter().any(|entry| {
        entry == name
            || (name.starts_with(mcp::PREFIX)
                && name.strip_prefix(entry.as_str()).is_some_and(|rest| rest.starts_with('_')))
    })
}

impl Policy {
    /// This policy with the fields that are given replaced; an omitted field keeps its value.
    pub(crate) fn set(
        &self,
        read_only: Option<bool>,
        allow_tools: Option<Vec<String>>,
        allow_commands: Option<Vec<String>>,
        deny_commands: Option<Vec<String>>,
    ) -> Self {
        Self {
            allow_tools: allow_tools.unwrap_or_else(|| self.allow_tools.clone()),
            read_only: read_only.unwrap_or(self.read_only),
            allow_commands: allow_commands.unwrap_or_else(|| self.allow_commands.clone()),
            deny_commands: deny_commands.unwrap_or_else(|| self.deny_commands.clone()),
            deny_all_commands: self.deny_all_commands,
        }
    }

    /// The policy a spawned agent is born with: this one, tightened by `given` and never loosened.
    ///
    /// `read_only` sticks, deny lists are unioned, and the two allow lists intersect — a child of a
    /// restricted worker cannot be handed back a capability its owner lacks. An intersection that
    /// leaves nothing allows nothing, rather than reading as an absent allow list. Called once, on
    /// the spawn path: an agent's policy never changes after that.
    pub(crate) fn inherit(&self, given: &Self) -> Self {
        // A child keeps only the entries its owner's list already covers, so `mcp__slack` in the
        // parent still covers a narrower `mcp__slack_feedback` in the child.
        let allow_tools: Vec<String> =
            given.allow_tools.iter().filter(|tool| grants(&self.allow_tools, tool.as_str())).cloned().collect();
        let allow_commands = if self.deny_all_commands || given.deny_all_commands {
            Vec::new()
        } else if self.allow_commands.is_empty() {
            given.allow_commands.clone()
        } else if given.allow_commands.is_empty() {
            self.allow_commands.clone()
        } else {
            self.allow_commands.iter().filter(|allowed| given.allow_commands.contains(allowed)).cloned().collect()
        };
        let deny_all_commands = self.deny_all_commands
            || given.deny_all_commands
            || (!self.allow_commands.is_empty() && !given.allow_commands.is_empty() && allow_commands.is_empty());
        let mut deny_commands = self.deny_commands.clone();
        for command in &given.deny_commands {
            if !deny_commands.contains(command) {
                deny_commands.push(command.clone());
            }
        }
        Self {
            allow_tools,
            read_only: self.read_only || given.read_only,
            allow_commands,
            deny_commands,
            deny_all_commands,
        }
    }

    /// The rights paragraph an agent reads in its prompt.
    pub(crate) fn describe(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        // A grant has no schema and no call, so it is not part of the list a model can invoke.
        let callable: Vec<&str> =
            self.allow_tools.iter().map(String::as_str).filter(|tool| *tool != crate::prompt::FREE_SPAWN).collect();
        if callable.is_empty() {
            lines.push("- tools you may call: none — you can only think and answer.".into());
        } else {
            lines.push(format!("- tools you may call: {}.", callable.join(" ")));
        }
        if grants(&self.allow_tools, crate::prompt::FREE_SPAWN) {
            lines.push(
                "- free_spawn: a child you start with spawn_agent(free: true) gets its profile's rights, not the intersection with yours."
                    .into(),
            );
        }
        if self.read_only {
            lines.push(format!(
                "- read-only: write_file and edit are refused, and bash runs only {} (git only: {}).",
                READ_ONLY_PROGRAMS.join(" "),
                READ_ONLY_GIT.join(" ")
            ));
        }
        if self.deny_all_commands {
            lines.push("- bash runs nothing: your allow list and your owner's share no command.".into());
        }
        if !self.allow_commands.is_empty() {
            lines.push(format!("- bash commands must match one of: {}.", self.allow_commands.join(" · ")));
        }
        if !self.deny_commands.is_empty() {
            lines.push(format!("- banned bash commands: {}.", self.deny_commands.join(" · ")));
        }
        format!(
            "Rights set by your profile; no agent, yourself included, can change them while you run:\n{}\n",
            lines.join("\n")
        )
    }

    /// Why this call may not run, or `None` when it may.
    pub(crate) fn refuse(&self, name: &str, arguments: &str) -> Option<String> {
        if !ALWAYS_GRANTED.contains(&name) && !grants(&self.allow_tools, name) {
            return Some(if self.allow_tools.is_empty() {
                "this agent may call no tool: it can only think and answer".to_string()
            } else {
                format!("tool {name} is not granted to this agent; it may call {}", self.allow_tools.join(" "))
            });
        }
        if (name == WRITE_FILE || name == EDIT || name == PYTHON) && self.read_only {
            return Some(format!("this agent is read-only: {name} is refused"));
        }
        // The tool list already hides these from a read-only worker, but a policy is re-read on
        // every call and a model may still call a tool it was shown in an earlier turn.
        if self.read_only && name.starts_with(mcp::PREFIX) {
            return Some(format!("this agent is read-only: {name} is an MCP tool with unknown side effects"));
        }
        if name != BASH {
            return None;
        }
        if self.deny_all_commands {
            return Some("this agent runs no bash command: its allow list and its owner's share none".into());
        }
        // Bad arguments are the dispatcher's error to report, not a policy decision.
        let Ok(args) = serde_json::from_str::<BashArgs>(arguments) else { return None };
        let command = args.command.trim();
        // A rule names words, and only a shell can see past them: `ls; rm -rf x` starts with `ls`.
        if (!self.allow_commands.is_empty() || !self.deny_commands.is_empty())
            && let Some(token) = SHELL_SYNTAX.iter().find(|token| command.contains(*token))
        {
            return Some(format!("this agent's command rules do not cover shell syntax `{token}`"));
        }
        let words = rule_words(command);
        if let Some(rule) = self.deny_commands.iter().find(|rule| rule_matches(&words, rule)) {
            return Some(format!("this agent may not run `{rule}`"));
        }
        if !self.allow_commands.is_empty() && !self.allow_commands.iter().any(|rule| rule_matches(&words, rule)) {
            return Some(format!("this agent runs only: {}", self.allow_commands.join(" · ")));
        }
        self.read_only.then(|| read_only_refusal(command)).flatten()
    }
}

/// What this policy says about one tool: allowed, denied with the reason, or not offered at all.
pub(crate) fn verdict(policy: &Policy, tool: &str, known: &[String]) -> String {
    if !known.iter().any(|name| name == tool) {
        return format!("unknown tool {tool}; tools are {}", known.join(", "));
    }
    if let Some(refusal) = policy.refuse(tool, "{}") {
        return format!("denied: {refusal}");
    }
    if tool == BASH {
        let constrained = policy.read_only
            || policy.deny_all_commands
            || !policy.allow_commands.is_empty()
            || !policy.deny_commands.is_empty();
        return if constrained { policy.describe() } else { "bash: no command rules".to_string() };
    }
    "allowed".to_string()
}

/// `path`'s last component: a rule never tells `/bin/rm` and `rm` apart.
fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// The words a command rule matches: the program reduced to its file name, with leading
/// `VAR=value` assignments, wrapper programs and their flags skipped, then its arguments. So
/// `/bin/rm`, `env rm` and `X=1 rm` all read as `rm`.
fn rule_words(command: &str) -> Vec<&str> {
    let mut words = command.split_whitespace();
    let program = words.by_ref().find_map(|word| {
        let name = file_name(word);
        let assignment = word.contains('=') && !word.starts_with('-');
        (!assignment && !WRAPPERS.contains(&name) && !word.starts_with('-')).then_some(name)
    });
    program.into_iter().chain(words).collect()
}

/// Whether the command's program and arguments start with the rule's words.
fn rule_matches(words: &[&str], rule: &str) -> bool {
    let mut expected = rule.split_whitespace();
    let Some(program) = expected.next() else { return false };
    let mut command = words.iter();
    if command.next().copied() != Some(file_name(program)) {
        return false;
    }
    expected.all(|word| command.next().copied() == Some(word))
}

/// Whether `word` is `flags` entry `flag`, bare or carrying a value (`--output=x`).
fn names_flag(flags: &[&str], word: &str) -> bool {
    flags.iter().any(|flag| *flag == word || word.strip_prefix(flag).is_some_and(|rest| rest.starts_with('=')))
}

/// Why this command is not read-only, or `None` when every pipeline segment is.
fn read_only_refusal(command: &str) -> Option<String> {
    if let Some(token) = WRITING_SHELL.iter().find(|token| command.contains(*token)) {
        return Some(format!("this agent is read-only: shell syntax `{token}` is not allowed"));
    }
    for segment in command.split('|') {
        let words: Vec<&str> = segment.split_whitespace().collect();
        let Some(program) = words.first() else { continue };
        if !READ_ONLY_PROGRAMS.contains(program) {
            return Some(format!(
                "this agent is read-only: `{program}` is not on its list ({})",
                READ_ONLY_PROGRAMS.join(" ")
            ));
        }
        if *program == "git" {
            let sub = words.get(1).copied().unwrap_or_default();
            if !READ_ONLY_GIT.contains(&sub) {
                return Some(format!("this agent is read-only: `git {sub}` writes ({})", READ_ONLY_GIT.join(" ")));
            }
            // These three list on flags alone: a bare word names a branch, a tag or a remote to write.
            if GIT_LISTING.contains(&sub)
                && let Some(arg) = words.iter().skip(2).copied().find(|arg| !arg.starts_with('-'))
            {
                return Some(format!("this agent is read-only: `git {sub} {arg}` names what to write"));
            }
        }
        if let Some(writing) = WRITING_ARGS.iter().find(|entry| entry.0 == *program).map(|entry| entry.1)
            && let Some(arg) = words.iter().skip(1).copied().find(|arg| names_flag(writing, arg))
        {
            return Some(format!("this agent is read-only: `{program} {arg}` writes"));
        }
    }
    None
}

/// Output lines a tool result shows before the rest is only counted.
pub(crate) const PREVIEW_LINES: usize = 4;

/// Longest column of one previewed output line.
const PREVIEW_WIDTH: usize = 160;

/// What a tool result shows at a glance: a verdict, the head of its output, and what it hid.
///
/// The tool result itself is what the model reads, and stays the whole text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Preview {
    /// A one-line verdict such as `exit 3`; empty when the output speaks for itself.
    pub(crate) status: String,
    /// The first lines of output, newline separated.
    pub(crate) head: String,
    /// Output lines past `head`.
    pub(crate) more: usize,
    pub(crate) failed: bool,
}

/// Distils a tool result for display, so a chatty command does not flood the transcript.
pub(crate) fn preview(name: &str, output: &str) -> Preview {
    if name == EDIT {
        return edit_preview(output);
    }
    let mut failed = output.starts_with("error:");
    let mut status = String::new();
    let mut rows: Vec<&str> = Vec::new();
    for line in output.trim_end().lines() {
        if let Some(code) = line.strip_prefix("exit code: ") {
            failed |= code != "0";
            if failed {
                status = format!("exit {code}");
            }
            continue;
        }
        // The two stream labels describe the result's shape, not what the command did.
        if line == "stdout:" || line == "stderr:" {
            continue;
        }
        if line.starts_with("killed after ") {
            status = line.to_string();
            failed = true;
            continue;
        }
        rows.push(line);
    }
    let mut rows: Vec<&str> = rows.into_iter().skip_while(|row| row.trim().is_empty()).collect();
    while rows.last().is_some_and(|row| row.trim().is_empty()) {
        rows.pop();
    }
    if status.is_empty() && name == READ_FILE && !rows.is_empty() {
        status = format!("{} lines", rows.len());
    }
    let head: Vec<String> = rows.iter().take(PREVIEW_LINES).map(|row| shorten(row, PREVIEW_WIDTH)).collect();
    Preview { status, head: head.join("\n"), more: rows.len().saturating_sub(PREVIEW_LINES), failed }
}

/// An edit result: the verdict row, then whole diff rows, counted past what the card previews.
fn edit_preview(output: &str) -> Preview {
    let mut rows: Vec<&str> = output.trim_end().lines().collect();
    let status = rows.first().and_then(|row| row.strip_prefix("edit: ")).map(str::to_string);
    if status.is_some() {
        rows.remove(0);
    }
    let head: Vec<String> = rows.iter().take(edit::DIFF_PREVIEW_LINES).map(|row| (*row).to_string()).collect();
    Preview {
        status: status.unwrap_or_default(),
        head: head.join("\n"),
        more: rows.len().saturating_sub(edit::DIFF_PREVIEW_LINES),
        failed: output.starts_with("error:"),
    }
}

/// `text` in at most `width` characters, the cut marked with an ellipsis.
pub(crate) fn shorten(text: &str, width: usize) -> String {
    let mut short: String = text.chars().take(width).collect();
    if text.chars().count() > width {
        short.push('…');
    }
    short
}

/// Schemas of the file and shell tools every worker gets.
///
/// The URI paragraph comes from the scheme table, so a scheme that gains handlers appears here
/// without a second edit, and a reserved name never reaches a model.
pub(crate) fn definitions() -> Value {
    let uris = url::summary()
        .map_or_else(String::new, |summary| format!("\nInternal URIs are accepted as `path`:\n{summary}"));
    json!([
        {"type": "function", "function": {
            "name": BASH,
            "description": format!("Run a shell command with `sh -c` in the working directory. Returns exit code, stdout and stderr. Stdin is closed; the command is killed after {} seconds. With `background`, it keeps running and its output is read through `proc://`. Use it for programs, tests and git, never to change a file's contents: `edit` and `write_file` are the audited ways, and the only ones that record what changed.", BASH_TIMEOUT.as_secs()),
            "parameters": {"type": "object", "properties": {
                "command": {"type": "string"},
                "background": {"type": "boolean", "description": "Start the command as a background job and return its proc:// id instead of waiting."}
            }, "required": ["command"]}
        }},
        {"type": "function", "function": {
            "name": READ_FILE,
            "description": format!("Read a UTF-8 text file. Relative paths resolve against the working directory. Lines are numbered `N:text`; append `:N`, `:N-M`, `:N-` or `:-N` to read a range and `:raw` to drop the numbers. A file holding merge-conflict markers gets every block an id, listed after the text. `<path>:conflicts` registers and lists every block of a file.{uris}"),
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
        }},
        {"type": "function", "function": {
            "name": WRITE_FILE,
            "description": format!("Create or overwrite a file with the given content. Creates missing parent directories. Relative paths resolve against the working directory.{uris}"),
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}
        }},
        {"type": "function", "function": {
            "name": EDIT,
            "description": "Change an existing file by line ranges taken from a numbered `read_file`. This agent must have read the file and the file must be unchanged since. Patch rows: `PUT N.=M:` replaces lines N..M with the `+TEXT` rows that follow, `PUT <N:` / `PUT >N:` / `PUT >$:` inserts, `CUT N.=M` deletes, `REM` deletes the file, `MV DEST` renames. Anchors are the original line numbers. Returns a compact diff. Use `write_file` to create a file.",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "patch": {"type": "string"}}, "required": ["path", "patch"]}
        }},
        {"type": "function", "function": {
            "name": GLOB,
            "description": "List files matching a glob pattern, relative to the working directory. Respects .gitignore, skips dotfiles and binary files. A pattern without `/` matches at any depth. One path per line, sorted.",
            "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}}, "required": ["pattern"]}
        }},
        {"type": "function", "function": {
            "name": GREP,
            "description": "Search file contents with a regular expression, returning `path:line: text` rows. Respects .gitignore, skips dotfiles and binary files. `path` optionally scopes the search to a directory or a file.",
            "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}, "path": {"type": "string"}}, "required": ["pattern"]}
        }},
        {"type": "function", "function": {
            "name": PYTHON,
            "description": format!("Run Python in a persistent kernel for this working directory: variables survive between calls; printed output is the result. Call an overseer tool from code with `tool('write_file', {{'path': …, 'content': …}})`; it returns that tool's result text. A read-only agent cannot call this. The {budget}-second budget covers the whole call; a kernel that outlives it is killed with its state.", budget = BASH_TIMEOUT.as_secs()),
            "parameters": {"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}
        }}
    ])
}

/// Arguments of each worker tool. `deny_unknown_fields` turns a wrong key into an error naming the right ones.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    /// Start the command as a background job and return its `proc://` id instead of waiting.
    #[serde(default)]
    background: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    patch: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    pattern: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    /// Scope the search to a directory or a file, relative to the working directory.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KernelArgs {
    code: String,
}

/// Parses tool arguments, turning a serde failure into the text the model reads.
pub(crate) fn parse_args<'a, T: Deserialize<'a>>(arguments: &'a str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

/// Renders a tool outcome as the text the model reads: `error: ` for a failure, truncated otherwise.
pub(crate) fn outcome(result: Result<String, String>) -> String {
    truncate(result.unwrap_or_else(|e| format!("error: {e}")))
}

/// What a worker's tool call may reach beyond its working directory: its skills, its profiles, the
/// servers, and the caller itself when it has one.
pub(crate) struct Context<'a> {
    pub(crate) skills: &'a skills::Catalogue,
    pub(crate) profiles: &'a profiles::Catalogue,
    pub(crate) mcp: &'a mcp::Registry,
    pub(crate) scope: Option<&'a url::Scope<'a>>,
}

/// Runs one bridge tool call, bounded after the fact by `budget` and `cancel`.
///
/// The kernel's whole-call deadline needs a hung tool to fail the call. `on_tool` borrows the
/// caller's per-agent state (`conflicts`, the edit store), which cannot move to a watchdog thread;
/// the real bound is therefore the tools themselves: file tools and the searches always finish,
/// `bash` and background jobs kill themselves at their own timeout, MCP servers answer or error.
/// What this check buys: a tool that nonetheless blocked until `budget` elapses makes the kernel
/// call fail loudly on the next loop iteration instead of hiding behind a successful reply.
pub(crate) fn bounded_tool_call(
    on_tool: &mut dyn FnMut(&str, &str) -> Result<String, String>,
    name: &str,
    arguments: &str,
    budget: Duration,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Result<String, String>, ()> {
    let started = Instant::now();
    if cancel.is_some_and(|cancel| cancel.load(std::sync::atomic::Ordering::Relaxed)) {
        return Err(());
    }
    let result = on_tool(name, arguments);
    if started.elapsed() > budget {
        return Err(());
    }
    Ok(result)
}

/// Runs a worker's tool call under its policy, killing a running command once `cancel` is set.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_guarded(
    working_dir: &Path,
    name: &str,
    arguments: &str,
    policy: &Policy,
    cancel: Option<&AtomicBool>,
    conflicts: &mut conflict::Registry,
    seen: &mut edit::Store,
    context: &Context<'_>,
) -> String {
    match policy.refuse(name, arguments) {
        Some(refusal) => outcome(Err(refusal)),
        None => outcome(run(working_dir, name, arguments, policy, conflicts, seen, BASH_TIMEOUT, cancel, context)),
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    working_dir: &Path,
    name: &str,
    arguments: &str,
    policy: &Policy,
    conflicts: &mut conflict::Registry,
    seen: &mut edit::Store,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
    context: &Context<'_>,
) -> Result<String, String> {
    match name {
        BASH => parse_args::<BashArgs>(arguments).and_then(|args| bash(working_dir, &args, timeout, cancel)),
        READ_FILE => {
            parse_args::<ReadArgs>(arguments).and_then(|args| read_file(working_dir, &args, conflicts, seen, context))
        }
        WRITE_FILE => {
            parse_args::<WriteArgs>(arguments).and_then(|args| write_file(working_dir, &args, conflicts, seen, context))
        }
        EDIT => {
            parse_args::<EditArgs>(arguments).and_then(|args| edit::apply(working_dir, &args.path, &args.patch, seen))
        }
        GLOB => parse_args::<GlobArgs>(arguments).and_then(|args| glob_files(working_dir, &args)),
        GREP => parse_args::<GrepArgs>(arguments).and_then(|args| grep_files(working_dir, &args)),
        PYTHON => parse_args::<KernelArgs>(arguments).and_then(|args| {
            kernel::call(
                working_dir,
                kernel::Runtime::Python,
                &args.code,
                timeout,
                cancel,
                &mut |tool_name, tool_arguments| {
                    // A kernel calling itself would recurse forever; refuse before dispatching.
                    if tool_name == PYTHON {
                        return Err("a kernel cannot call itself".to_string());
                    }
                    Ok(execute_guarded(
                        working_dir,
                        tool_name,
                        tool_arguments,
                        policy,
                        cancel,
                        conflicts,
                        seen,
                        context,
                    ))
                },
            )
        }),
        // An MCP server reports its own failures as text, which is what the model reads.
        _ if name.starts_with(mcp::PREFIX) => Ok(context.mcp.call(name, arguments)),
        _ => Err(format!("unknown tool {name}")),
    }
}

/// Rows a search tool shows before the `… N more` note.
const MAX_SEARCH_ROWS: usize = 200;

/// Longest one `grep` row shows of a matched line.
const GREP_LINE_WIDTH: usize = 200;

/// Lists files matching `pattern` under the working directory, relative and sorted.
fn glob_files(working_dir: &Path, args: &GlobArgs) -> Result<String, String> {
    let glob = globset::Glob::new(&args.pattern).map_err(|e| format!("invalid pattern: {e}"))?;
    let matcher = glob.compile_matcher();
    let mut found: Vec<String> = ignore::WalkBuilder::new(working_dir)
        // .gitignore applies in a plain directory too: that is the walk the prompt warns about.
        .require_git(false)
        .build()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                return None;
            }
            let relative = entry.path().strip_prefix(working_dir).unwrap_or(entry.path());
            if relative.as_os_str().is_empty() {
                return None;
            }
            let display = relative.to_string_lossy().replace('\\', "/");
            matcher.is_match(&display).then_some(display)
        })
        .collect();
    found.sort();
    if found.is_empty() {
        return Ok(format!("no matches for {}", args.pattern));
    }
    Ok(paginate(found, "paths"))
}

/// Searches file contents with a regex, returning `path:line: text` rows, ordered by path then
/// line number: a lexical sort would put `f:10:` before `f:2:`.
fn grep_files(working_dir: &Path, args: &GrepArgs) -> Result<String, String> {
    let pattern = regex::Regex::new(&args.pattern).map_err(|e| format!("invalid pattern: {e}"))?;
    let root = args.path.as_deref().map_or_else(|| working_dir.to_path_buf(), |path| working_dir.join(path));
    let label = args.path.clone().unwrap_or_else(|| ".".to_string());
    let root = root.canonicalize().map_err(|e| config::io_error(label, &e))?;
    let mut rows: Vec<(String, usize, String)> = Vec::new();
    for entry in ignore::WalkBuilder::new(&root).require_git(false).build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else { continue };
        if metadata.len() > MAX_SCAN_BYTES {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else { continue };
        if bytes.iter().take(512).any(|b| *b == 0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else { continue };
        // Strip the walked root, not the raw working dir: a symlinked path (macOS /tmp) would
        // otherwise fail the strip and leak absolute paths.
        let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/");
        for (index, line) in text.lines().enumerate() {
            if pattern.is_match(line) {
                rows.push((relative.clone(), index + 1, shorten(line, GREP_LINE_WIDTH)));
            }
        }
    }
    if rows.is_empty() {
        return Ok(format!("no matches for {}", args.pattern));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    Ok(paginate(rows.into_iter().map(|(path, line, text)| format!("{path}:{line}: {text}")).collect(), "matches"))
}

/// Joins the first rows and, past the cap, appends the count of what is hidden.
fn paginate(rows: Vec<String>, what: &str) -> String {
    if rows.len() <= MAX_SEARCH_ROWS {
        return rows.join("\n");
    }
    let hidden = rows.len() - MAX_SEARCH_ROWS;
    let mut shown: Vec<String> = rows.into_iter().take(MAX_SEARCH_ROWS).collect();
    shown.push(format!("… {hidden} more {what}"));
    shown.join("\n")
}

/// How a `bash` command ended.
#[derive(Clone, Copy)]
enum Stopped {
    /// The command exited by itself.
    Exited,
    /// The command outlived its timeout.
    TimedOut,
    /// Overseer interrupted the turn: kill the command rather than let it run to its timeout.
    Interrupted,
}

fn bash(working_dir: &Path, args: &BashArgs, timeout: Duration, cancel: Option<&AtomicBool>) -> Result<String, String> {
    if args.background {
        let id = jobs::start(&args.command, working_dir)?;
        return Ok(format!("started job {id}; read proc://{id} for its output"));
    }
    // Its own process group, so a timeout kills the command's children too: killing `sh` alone leaves a
    // grandchild holding the pipes open, and reading their output then blocks until it exits by itself.
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&args.command)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let (mut child, group) = spawn_grouped(&mut command)?;
    // Both pipes are drained by their own thread, so a chatty command cannot fill a pipe and stall.
    let mut stdout = drain(child.stdout.take(), MAX_OUTPUT_BYTES);
    let mut stderr = drain(child.stderr.take(), MAX_OUTPUT_BYTES);
    let (status, stopped) = match wait_with_deadline(&mut child, group, timeout, cancel) {
        Ok(done) => done,
        Err(e) => {
            // The command may still be running: nothing else would kill it once it is deregistered.
            let _ = Command::new("kill").arg("-9").arg(format!("-{group}")).status();
            deregister(group);
            return Err(e);
        }
    };
    // Still registered: a `sleep 300 &` the command left behind holds these pipes, and only the
    // group kill below makes their drains finish.
    let drain_by = Instant::now() + DRAIN_GRACE;
    let (out, out_cut) = stdout.take_output(drain_by, group);
    let (err, err_cut) = stderr.take_output(drain_by, group);
    deregister(group);
    Ok(format_output(status, stopped, timeout, (&out, out_cut), (&err, err_cut)))
}

/// Runs a program to completion and returns its stdout, for a scheme that shells out to a client.
pub(crate) fn capture(program: &str, args: &[String], cwd: &Path, timeout: Duration) -> Result<String, String> {
    capture_limited(program, args, cwd, timeout, MAX_OUTPUT_BYTES)
}

/// As [`capture`], keeping only the first `max_bytes` of stdout: `ssh://` reads a bound file, not a whole one.
pub(crate) fn capture_limited(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    max_bytes: usize,
) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let (mut child, group) = spawn_grouped(&mut command).map_err(|e| format!("{program}: {e}"))?;
    let mut stdout = drain(child.stdout.take(), max_bytes);
    let mut stderr = drain(child.stderr.take(), MAX_OUTPUT_BYTES);
    let (status, stopped) = match wait_with_deadline(&mut child, group, timeout, None) {
        Ok(done) => done,
        Err(e) => {
            let _ = Command::new("kill").arg("-9").arg(format!("-{group}")).status();
            deregister(group);
            return Err(e);
        }
    };
    let drain_by = Instant::now() + DRAIN_GRACE;
    let (out, _) = stdout.take_output(drain_by, group);
    let (err, _) = stderr.take_output(drain_by, group);
    deregister(group);
    if let Stopped::TimedOut = stopped {
        return Err(format!("{program} killed after {}s", timeout.as_secs()));
    }
    if status.success() {
        return Ok(String::from_utf8_lossy(&out).into_owned());
    }
    let code = status.code().map_or_else(|| "killed by signal".to_string(), |code| code.to_string());
    Err(format!("{program} exited {code}: {}", String::from_utf8_lossy(&err).trim()))
}

/// Waits for `child` to exit, killing its process group once `timeout` elapses or `cancel` is set.
fn wait_with_deadline(
    child: &mut Child,
    group: i32,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<(ExitStatus, Stopped), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok((status, Stopped::Exited));
        }
        let stopped = if Instant::now() >= deadline {
            Some(Stopped::TimedOut)
        } else if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            Some(Stopped::Interrupted)
        } else {
            None
        };
        let Some(stopped) = stopped else {
            thread::sleep(Duration::from_millis(20));
            continue;
        };
        let _ = Command::new("kill").arg("-9").arg(format!("-{group}")).status();
        let _ = child.kill();
        let status = child.wait().map_err(|e| e.to_string())?;
        return Ok((status, stopped));
    }
}

fn format_output(
    status: ExitStatus,
    stopped: Stopped,
    timeout: Duration,
    (out, out_cut): (&[u8], bool),
    (err, err_cut): (&[u8], bool),
) -> String {
    let code = status.code().map_or_else(|| "killed by signal".to_string(), |c| c.to_string());
    let killed = match stopped {
        Stopped::Exited => String::new(),
        Stopped::TimedOut => format!("killed after {}s\n", timeout.as_secs()),
        Stopped::Interrupted => "interrupted by overseer\n".into(),
    };
    let cut = if out_cut || err_cut {
        format!("\n[output truncated: only the first {MAX_OUTPUT_BYTES} bytes of each stream were kept]")
    } else {
        String::new()
    };
    format!(
        "{killed}exit code: {code}\nstdout:\n{}\nstderr:\n{}{cut}",
        String::from_utf8_lossy(out),
        String::from_utf8_lossy(err)
    )
}

/// A pipe reader that keeps draining in the background until the pipe closes, keeping a bounded head.
struct Drain(Option<thread::JoinHandle<(Vec<u8>, bool)>>);

impl Drain {
    /// The bytes kept, and whether the stream held more than were kept.
    ///
    /// Waits for the pipe to close until `deadline`, then kills the leftover process group: a child
    /// the command backgrounded inherited the pipe, and joining it would never return.
    fn take_output(&mut self, deadline: Instant, group: i32) -> (Vec<u8>, bool) {
        let Some(handle) = self.0.take() else { return (Vec::new(), false) };
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if !handle.is_finished() {
            let _ = Command::new("kill").arg("-9").arg(format!("-{group}")).status();
        }
        handle.join().unwrap_or_default()
    }
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>, cap: usize) -> Drain {
    Drain(pipe.map(|mut pipe| {
        thread::spawn(move || {
            // Storage stops at the cap; reading never does, because an unread pipe blocks its writer.
            let mut kept = Vec::with_capacity(8 * 1024);
            let mut scratch = [0_u8; 8 * 1024];
            let mut truncated = false;
            loop {
                match pipe.read(&mut scratch) {
                    Ok(0) | Err(_) => return (kept, truncated),
                    Ok(read) => {
                        let room = cap.saturating_sub(kept.len());
                        let keep = read.min(room);
                        if let Some(head) = scratch.get(..keep) {
                            kept.extend_from_slice(head);
                        }
                        truncated |= keep < read;
                    }
                }
            }
        })
    }))
}

/// What a `read_file` path's line selector asks for.
struct Selector {
    /// First line, 1-based; `None` for the head.
    from: Option<usize>,
    /// Last line, 1-based; `None` for the tail.
    to: Option<usize>,
    /// Drop the line numbers, returning the rows as they are.
    raw: bool,
}

/// Reads a UTF-8 file, numbered by line, and records what this agent saw.
///
/// Every conflict block the read shows is registered, so a later write can address it by id.
fn read_file(
    working_dir: &Path,
    args: &ReadArgs,
    conflicts: &mut conflict::Registry,
    seen: &mut edit::Store,
    context: &Context<'_>,
) -> Result<String, String> {
    if url::is_url(&args.path) {
        return url::read(
            &mut url::Ctx {
                conflicts: Some(conflicts),
                skills: Some(context.skills),
                profiles: Some(context.profiles),
                scope: context.scope,
            },
            &args.path,
        );
    }
    if let Some(display) = args.path.strip_suffix(conflict::LIST_SUFFIX) {
        return list_conflicts(working_dir, display, conflicts);
    }
    let (path, selector) = split_selector(working_dir, &args.path);
    let full = working_dir.join(path);
    let (text, read_cut) = read_limited(&full, path)?;
    let absolute = fs::canonicalize(&full).map_err(|e| config::io_error(path, &e))?;
    conflicts.reconcile(&absolute, path, &text);
    let rows = diff::split_lines(&text);
    let total = rows.len();
    let (from, _to, selected) = select(&rows, selector.as_ref(), path, total)?;
    let raw = selector.as_ref().is_some_and(|selector| selector.raw);
    let (mut body, number_cut, emitted) = numbered(selected, from, raw);
    if selector.is_none() && text.ends_with('\n') && !body.is_empty() && body.len() < edit::MAX_READ_BYTES {
        body.push('\n');
    }
    let cut = read_cut || number_cut;
    let mut out = with_note(body, cut);
    let summary = conflicts.summary(&absolute);
    if !summary.is_empty() {
        out.push('\n');
        out.push_str(&summary);
        if cut {
            let _ = writeln!(
                out,
                "(only the blocks above the truncation point are registered; `read_file` on `{}:conflicts` registers them all)",
                args.path
            );
        }
    }
    if total == 0 {
        seen.record(absolute, edit::hash_file(&full)?, 0, 0, 0);
    } else if emitted > 0 {
        let first = if selector.is_none() && !cut { 1 } else { from };
        let last = first.saturating_add(emitted).saturating_sub(1);
        seen.record(absolute, edit::hash_file(&full)?, total, first, last);
    }
    Ok(out)
}

/// Splits a selector off a path, unless a file of that exact name exists.
fn split_selector<'a>(working_dir: &Path, path: &'a str) -> (&'a str, Option<Selector>) {
    if working_dir.join(path).exists() {
        return (path, None);
    }
    let mut base = path;
    let mut raw = false;
    let mut range = None;
    if let Some(stripped) = base.strip_suffix(":raw") {
        raw = true;
        base = stripped;
    }
    if let Some((head, tail)) = base.rsplit_once(':')
        && let Some(parsed) = parse_range(tail)
    {
        range = Some(parsed);
        base = head;
        if let Some(stripped) = base.strip_suffix(":raw") {
            raw = true;
            base = stripped;
        }
    }
    if !raw && range.is_none() {
        return (path, None);
    }
    let (from, to) = range.unwrap_or((None, None));
    (base, Some(Selector { from, to, raw }))
}

/// `N`, `N-M`, `N-`, `-N` as `(from, to)`, each side optional.
fn parse_range(text: &str) -> Option<(Option<usize>, Option<usize>)> {
    if let Some((from, to)) = text.split_once('-') {
        if from.is_empty() {
            return Some((None, Some(to.parse().ok()?)));
        }
        let from = from.parse().ok()?;
        if to.is_empty() {
            return Some((Some(from), None));
        }
        return Some((Some(from), Some(to.parse().ok()?)));
    }
    Some((Some(text.parse().ok()?), Some(text.parse().ok()?)))
}

/// The rows a selector names, and the 1-based range it names.
fn select<'a>(
    rows: &'a [&'a str],
    selector: Option<&Selector>,
    path: &str,
    total: usize,
) -> Result<(usize, usize, &'a [&'a str]), String> {
    let Some(selector) = selector else {
        return Ok((1, total, rows));
    };
    let from = selector.from.unwrap_or(1);
    let to = selector.to.unwrap_or(total.max(from));
    if from == 0 || to == 0 {
        return Err("line 0 is not a line: ranges start at 1".into());
    }
    if from > to {
        return Err(format!("range {from}-{to} is reversed"));
    }
    if from > total || (selector.to.is_some() && to > total) {
        return Err(format!("range {from}-{to} is past {path} ({total} lines)"));
    }
    let to = to.min(total);
    let selected = rows.get(from.saturating_sub(1)..to).unwrap_or_default();
    Ok((from, to, selected))
}

/// Numbers `rows` from `first`, spending the read budget; returns what it showed and how many rows.
fn numbered(rows: &[&str], first: usize, raw: bool) -> (String, bool, usize) {
    let mut out = String::new();
    let mut cut = false;
    let mut emitted: usize = 0;
    for (offset, row) in rows.iter().enumerate() {
        let number = first.saturating_add(offset);
        let prefix = if raw { String::new() } else { format!("{number}:") };
        let separator = usize::from(!out.is_empty());
        let needed = prefix.len().saturating_add(row.len()).saturating_add(separator);
        let remaining = edit::MAX_READ_BYTES.saturating_sub(out.len());
        if needed <= remaining {
            if separator == 1 {
                out.push('\n');
            }
            out.push_str(&prefix);
            out.push_str(row);
            emitted = emitted.saturating_add(1);
            continue;
        }
        cut = true;
        let room = remaining.saturating_sub(prefix.len().saturating_add(separator));
        if room > 0 {
            if separator == 1 {
                out.push('\n');
            }
            out.push_str(&prefix);
            out.push_str(shorten_bytes(row, room));
            emitted = emitted.saturating_add(1);
        }
        break;
    }
    (out, cut, emitted)
}

/// The longest valid UTF-8 prefix of `text` that fits in `room` bytes.
fn shorten_bytes(text: &str, room: usize) -> &str {
    if text.len() <= room {
        return text;
    }
    let mut end = room;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

/// Reads a UTF-8 file, keeping the head of a file larger than the read budget, and says whether it
/// stopped early.
///
/// `label` is what the model sees in an error: a skill path is not the working directory's path,
/// and naming it as one would send the model to the wrong place.
pub(crate) fn read_limited(path: &Path, label: &str) -> Result<(String, bool), String> {
    let (text, cut) = read_limited_bounded(path, label, true)?;
    Ok((text?, cut))
}

/// The bounded read with the binary question split out: `read_file` refuses a binary file with a
/// bash hint, but `history://` and `rule://` are written raw by the session itself — a NUL there
/// would make an agent's log unreadable, so those reads keep a lossy decode and stay silent.
pub(crate) fn read_limited_bounded(
    path: &Path,
    label: &str,
    refuse_binary: bool,
) -> Result<(Result<String, String>, bool), String> {
    let file = fs::File::open(path).map_err(|e| config::io_error(label, &e))?;
    let limit = u64::try_from(edit::MAX_READ_BYTES).map_or(u64::MAX, |cap| cap.saturating_add(1));
    let mut bytes = Vec::with_capacity(8 * 1024);
    file.take(limit).read_to_end(&mut bytes).map_err(|e| config::io_error(label, &e))?;
    let cut = bytes.len() > edit::MAX_READ_BYTES;
    let binary = bytes.iter().take(512).any(|b| *b == 0);
    if binary && refuse_binary {
        let size = fs::metadata(path).map_or(0, |m| m.len());
        return Ok((
            Err(format!(
                "{label}: binary file ({size} bytes); use bash `strings {label}` or `file {label}` to inspect it"
            )),
            cut,
        ));
    }
    if cut {
        // The budget can land inside a multi-byte character: keep the longest valid prefix below it.
        let end = bytes
            .get(..edit::MAX_READ_BYTES)
            .map_or(0, |head| std::str::from_utf8(head).map_or_else(|e| e.valid_up_to(), |_| head.len()));
        bytes.truncate(end);
    }
    // A binary file is decoded lossily when the caller accepts it: losing bytes beats losing the log.
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok((Ok(text), cut))
}

/// The text as the model reads it, with the truncation note when the read stopped early.
pub(crate) fn with_note(text: String, cut: bool) -> String {
    if !cut {
        return text;
    }
    let shown = text.len();
    let budget = edit::MAX_READ_BYTES;
    format!("{text}\n[truncated: first {shown} bytes shown; {budget} is the read budget, not the file size]")
}

/// Registers every block of a file, past the read budget, and lists them.
fn list_conflicts(working_dir: &Path, display: &str, conflicts: &mut conflict::Registry) -> Result<String, String> {
    let full = working_dir.join(display);
    let size = fs::metadata(&full).map_err(|e| config::io_error(display, &e))?.len();
    if size > MAX_SCAN_BYTES {
        return Err(format!("{display}: {size} bytes is past the {MAX_SCAN_BYTES}-byte conflict-scan budget"));
    }
    let text = fs::read_to_string(&full).map_err(|e| config::io_error(display, &e))?;
    let absolute = fs::canonicalize(&full).map_err(|e| config::io_error(display, &e))?;
    conflicts.reconcile(&absolute, display, &text);
    let summary = conflicts.summary(&absolute);
    if summary.is_empty() {
        return Ok(format!("no conflict markers in {display}"));
    }
    Ok(summary)
}

fn write_file(
    working_dir: &Path,
    args: &WriteArgs,
    conflicts: &mut conflict::Registry,
    seen: &mut edit::Store,
    context: &Context<'_>,
) -> Result<String, String> {
    if url::is_url(&args.path) {
        return url::write(
            &mut url::Ctx { conflicts: Some(conflicts), skills: None, profiles: None, scope: context.scope },
            &args.path,
            &args.content,
        );
    }
    let full = working_dir.join(&args.path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).map_err(|e| config::io_error(parent.display(), &e))?;
    }
    fs::write(&full, &args.content).map_err(|e| config::io_error(&args.path, &e))?;
    let absolute = fs::canonicalize(&full).map_err(|e| config::io_error(&args.path, &e))?;
    let lines = diff::line_count(&args.content);
    let (from, to) = if lines == 0 { (0, 0) } else { (1, lines) };
    seen.record(absolute, edit::hash(args.content.as_bytes()), lines, from, to);
    Ok(format!("wrote {} bytes to {}", args.content.len(), args.path))
}

/// Keeps the head and the tail of `s`: the head names what ran, the tail is where a command says why it failed.
fn truncate(s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let total = s.len();
    let id = artifacts::spill(s.clone());
    let half = MAX_OUTPUT_BYTES / 2;
    let head_end = floor_boundary(&s, half);
    let tail_start = ceil_boundary(&s, total - half);
    let elided = tail_start.saturating_sub(head_end);
    let head = s.get(..head_end).unwrap_or_default();
    let tail = s.get(tail_start..).unwrap_or_default();
    format!("{head}\n[truncated: {elided} of {total} bytes elided; full output at artifact://{id}]\n{tail}")
}

/// Largest char boundary of `s` at or below `at`.
fn floor_boundary(s: &str, at: usize) -> usize {
    let mut end = at.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Smallest char boundary of `s` at or above `at`.
fn ceil_boundary(s: &str, at: usize) -> usize {
    let mut start = at.min(s.len());
    while !s.is_char_boundary(start) {
        start += 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One tool call's per-agent state: conflicts to resolve and files seen.
    #[derive(Default)]
    struct Sandbox {
        conflicts: conflict::Registry,
        seen: edit::Store,
    }
    /// A policy granting every file tool, so a test about one of them is not about the allow list.
    fn file_tools() -> Policy {
        Policy { allow_tools: vec![BASH.into(), READ_FILE.into(), WRITE_FILE.into(), EDIT.into()], ..Policy::default() }
    }

    fn sandbox() -> Sandbox {
        Sandbox::default()
    }

    /// A tool call with no policy, no cancellation and a fresh sandbox: each test sees its own ids.
    fn execute(dir: &Path, name: &str, arguments: &str) -> String {
        guarded(dir, name, arguments, &file_tools(), None, &mut sandbox())
    }

    /// One tool call against a sandbox the caller keeps, for a read a later edit builds on.
    fn with_sandbox(dir: &Path, name: &str, arguments: &str, state: &mut Sandbox) -> String {
        guarded(dir, name, arguments, &file_tools(), None, state)
    }

    /// The configuration a scoped call runs under: one root, one model, one base URL.
    fn test_config(dir: &Path) -> config::Config {
        config::Config {
            roots: vec![dir.to_path_buf()],
            base_url: "https://user:pass@example.test/v1".into(),
            model: "m-1".into(),
            theme: config::Theme::Auto,
            max_depth: 3,
            dir: dir.to_path_buf(),
            home: dir.to_path_buf(),
        }
    }

    /// A `read_file` as a real agent makes it: with a scope over `cfg` and `policy`.
    fn scoped(dir: &Path, cfg: &config::Config, policy: &Policy, path: &str) -> String {
        let home = tempfile::tempdir().unwrap();
        let skills = skills::Catalogue::user(home.path());
        let profiles = profiles::Catalogue::default();
        let mcp = mcp::Registry::load(home.path());
        let scope =
            url::Scope { caller: 1, swarm: None, config: cfg, working_dir: dir, policy, mcp: &mcp, remaining_depth: 3 };
        let context = Context { skills: &skills, profiles: &profiles, mcp: &mcp, scope: Some(&scope) };
        let args = json!({"path": path}).to_string();
        let mut state = Sandbox::default();
        execute_guarded(dir, READ_FILE, &args, policy, None, &mut state.conflicts, &mut state.seen, &context)
    }

    #[test]
    fn the_settings_are_readable_one_key_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let policy = file_tools();
        assert_eq!(scoped(dir.path(), &cfg, &policy, "cfg://model"), "m-1");
        assert_eq!(
            scoped(dir.path(), &cfg, &policy, "cfg://base_url"),
            "https://***@example.test/v1",
            "the userinfo of a base URL never reaches a model"
        );
        assert_eq!(scoped(dir.path(), &cfg, &policy, "cfg://max_depth"), "3");
        let unknown = scoped(dir.path(), &cfg, &policy, "cfg://nobody");
        assert!(unknown.starts_with("error: unknown setting nobody; settings are roots, base_url"), "{unknown}");
        let listing = scoped(dir.path(), &cfg, &policy, "cfg://");
        assert!(listing.starts_with("roots = "), "{listing}");
        assert!(listing.contains("\nmodel = m-1\n"), "{listing}");
    }

    #[test]
    fn the_rights_and_the_schemas_are_readable() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config(dir.path());
        let open = file_tools();
        assert!(
            scoped(dir.path(), &cfg, &open, "security://").contains("tools you may call: bash read_file write_file"),
            "{}",
            scoped(dir.path(), &cfg, &open, "security://")
        );
        assert_eq!(scoped(dir.path(), &cfg, &open, "security://bash"), "bash: no command rules");
        assert_eq!(scoped(dir.path(), &cfg, &open, "security://read_file"), "allowed");
        let readonly = Policy { read_only: true, ..file_tools() };
        assert_eq!(
            scoped(dir.path(), &cfg, &readonly, "security://write_file"),
            "denied: this agent is read-only: write_file is refused"
        );
        assert!(
            scoped(dir.path(), &cfg, &open, "security://nobody").starts_with("unknown tool nobody; tools are "),
            "an unknown tool names the ones that exist"
        );

        let schema = scoped(dir.path(), &cfg, &open, "xd://read_file");
        let parsed: Value = serde_json::from_str(&schema).expect("the schema is JSON");
        assert_eq!(parsed.get("name").and_then(Value::as_str), Some("read_file"));
        let names = scoped(dir.path(), &cfg, &open, "xd://");
        assert!(names.lines().any(|line| line == "write_file"), "{names}");
        assert!(scoped(dir.path(), &cfg, &open, "xd://nobody").starts_with("error: unknown tool nobody"));

        assert_eq!(scoped(dir.path(), &cfg, &open, "overseer://"), "topics: schemes, config, tools, agents");
        assert!(scoped(dir.path(), &cfg, &open, "overseer://schemes").contains("internal URIs:"));
        assert!(scoped(dir.path(), &cfg, &open, "overseer://config").starts_with("roots = "));
        assert!(scoped(dir.path(), &cfg, &open, "overseer://tools").lines().any(|line| line == "bash: Run a shell command with `sh -c` in the working directory. Returns exit code, stdout and stderr. Stdin is closed; the command is killed after 300 seconds. With `background`, it keeps running and its output is read through `proc://`. Use it for programs, tests and git, never to change a file's contents: `edit` and `write_file` are the audited ways, and the only ones that record what changed."));
        assert!(scoped(dir.path(), &cfg, &open, "overseer://x").starts_with("error: unknown topic x; topics are "));
    }

    /// The same call, with the empty skills and servers a plain tool test has.
    fn guarded(
        dir: &Path,
        name: &str,
        arguments: &str,
        policy: &Policy,
        cancel: Option<&AtomicBool>,
        state: &mut Sandbox,
    ) -> String {
        let home = tempfile::tempdir().unwrap();
        let skills = skills::Catalogue::user(home.path());
        let mcp = mcp::Registry::load(home.path());
        let profiles = profiles::Catalogue::default();
        execute_guarded(
            dir,
            name,
            arguments,
            policy,
            cancel,
            &mut state.conflicts,
            &mut state.seen,
            &Context { skills: &skills, profiles: &profiles, mcp: &mcp, scope: None },
        )
    }

    #[test]
    fn a_child_cannot_be_looser_than_its_owner() {
        let owner = Policy { allow_tools: vec![BASH.into()], read_only: true, ..Policy::default() };
        let asked = Policy { allow_tools: vec![BASH.into(), WRITE_FILE.into()], ..Policy::default() };
        let inherited = owner.inherit(&asked);
        assert!(inherited.read_only, "a read-only owner born a writing agent");
        assert_eq!(inherited.allow_tools, ["bash"], "the owner's list bounds its child's");
    }

    #[test]
    fn a_child_may_be_tighter_than_its_owner() {
        let owner = Policy { allow_tools: vec![BASH.into(), READ_FILE.into()], ..Policy::default() };
        let asked = Policy { allow_tools: vec![READ_FILE.into()], read_only: true, ..Policy::default() };
        assert_eq!(owner.inherit(&asked), asked);
    }

    #[test]
    fn a_server_grant_covers_its_own_tools_only() {
        let owner = Policy { allow_tools: vec!["mcp__slack".into(), BASH.into()], ..Policy::default() };
        let asked = Policy {
            allow_tools: vec!["mcp__slack_feedback".into(), "mcp__slackbot_x".into(), BASH.into()],
            ..Policy::default()
        };
        let inherited = owner.inherit(&asked);
        assert_eq!(inherited.allow_tools, ["mcp__slack_feedback", "bash"]);
        assert!(grants(&owner.allow_tools, "mcp__slack_msg"));
        assert!(!grants(&owner.allow_tools, "mcp__slackbot_x"), "a prefix is not a grant");
        assert!(grants(&owner.allow_tools, "mcp__slack"), "the server entry grants the server's tools");
    }

    #[test]
    fn a_grant_is_described_but_never_listed_as_callable() {
        let alone = Policy { allow_tools: vec![crate::prompt::FREE_SPAWN.into()], ..Policy::default() };
        let text = alone.describe();
        assert!(text.contains("- tools you may call: none — you can only think and answer."), "{text}");
        assert!(text.contains("- free_spawn: a child you start with spawn_agent(free: true)"), "{text}");
        let mixed = Policy { allow_tools: vec![BASH.into(), crate::prompt::FREE_SPAWN.into()], ..Policy::default() };
        let text = mixed.describe();
        assert!(text.contains("- tools you may call: bash."), "{text}");
        assert!(!text.contains("tools you may call: bash free_spawn"), "{text}");
        assert!(text.contains("- free_spawn:"), "{text}");
    }

    #[test]
    fn deny_lists_are_unioned_and_allow_lists_intersected() {
        let owner = Policy {
            deny_commands: vec!["rm".into()],
            allow_commands: vec!["git".into(), "cargo".into()],
            ..file_tools()
        };
        let asked = Policy {
            deny_commands: vec!["curl".into(), "rm".into()],
            allow_commands: vec!["cargo".into()],
            ..file_tools()
        };
        let inherited = owner.inherit(&asked);
        assert_eq!(inherited.deny_commands, ["rm", "curl"]);
        assert_eq!(inherited.allow_commands, ["cargo"]);
        assert_eq!(inherited.allow_tools, file_tools().allow_tools);
    }

    #[test]
    fn allow_lists_that_share_nothing_refuse_every_command() {
        let owner = Policy { allow_commands: vec!["git".into()], ..file_tools() };
        let asked = Policy { allow_commands: vec!["cargo".into()], ..file_tools() };
        let inherited = owner.inherit(&asked);
        assert!(inherited.deny_all_commands, "an empty intersection read as an absent allow list");
        assert_eq!(inherited.allow_commands, Vec::<String>::new());
        let dir = tempfile::tempdir().unwrap();
        let command = json!({"command": "git status"}).to_string();
        let out = guarded(dir.path(), BASH, &command, &inherited, None, &mut sandbox());
        assert!(out.starts_with("error: this agent runs no bash command"), "{out}");
        // A child of a deny-all agent stays deny-all, whatever allow list it asks for.
        let looser = inherited.inherit(&Policy { allow_commands: vec!["ls".into()], ..file_tools() });
        assert!(looser.deny_all_commands);
    }

    #[test]
    fn an_empty_allow_list_can_call_nothing() {
        let policy = Policy::default();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            guarded(dir.path(), BASH, r#"{"command":"ls"}"#, &policy, None, &mut sandbox()),
            "error: this agent may call no tool: it can only think and answer"
        );
        assert!(policy.refuse(REPLY, "{}").is_none(), "reporting home is never a right to ask for");
        assert!(policy.refuse(CLOSE, &json!({"report": "x"}).to_string()).is_none());
        let refused = Policy { allow_tools: vec![READ_FILE.into()], ..Policy::default() };
        assert_eq!(
            refused.refuse(BASH, r#"{"command":"ls"}"#),
            Some("tool bash is not granted to this agent; it may call read_file".to_string())
        );
    }

    #[test]
    fn bash_runs_in_cwd_and_reports_exit_code_and_streams() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("marker"), "").unwrap();
        let out = execute(dir.path(), BASH, r#"{"command":"ls; echo oops >&2; exit 3"}"#);
        assert!(out.contains("exit code: 3"), "{out}");
        assert!(out.contains("stdout:\nmarker\n"), "{out}");
        assert!(out.contains("stderr:\noops\n"), "{out}");
    }

    #[test]
    fn bash_is_killed_past_its_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let command = r#"{"command":"echo before; sleep 30"}"#;
        let mut state = sandbox();
        let home = tempfile::tempdir().unwrap();
        let skills = skills::Catalogue::user(home.path());
        let mcp = mcp::Registry::load(home.path());
        let profiles = profiles::Catalogue::default();
        let context = Context { skills: &skills, profiles: &profiles, mcp: &mcp, scope: None };
        let out = outcome(run(
            dir.path(),
            BASH,
            command,
            &file_tools(),
            &mut state.conflicts,
            &mut state.seen,
            Duration::from_millis(200),
            None,
            &context,
        ));
        assert!(out.starts_with("killed after 0s"), "{out}");
        assert!(out.contains("stdout:\nbefore\n"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn write_then_read_relative_path_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let write = json!({"path": "a/b/note.txt", "content": "héllo"}).to_string();
        assert_eq!(execute(dir.path(), WRITE_FILE, &write), "wrote 6 bytes to a/b/note.txt");
        assert_eq!(fs::read_to_string(dir.path().join("a/b/note.txt")).unwrap(), "héllo");
        assert_eq!(execute(dir.path(), READ_FILE, r#"{"path":"a/b/note.txt"}"#), "1:héllo");
    }

    #[test]
    fn errors_are_returned_not_raised() {
        let root = Path::new("/");
        assert!(execute(root, READ_FILE, r#"{"path":"/nonexistent/x"}"#).starts_with("error: /nonexistent/x:"));
        assert!(execute(root, BASH, "not json").starts_with("error: invalid arguments:"));
        assert!(execute(root, BASH, "{}").starts_with("error: invalid arguments: missing field `command`"));
        assert!(
            execute(root, BASH, r#"{"command":"ls","path":"x"}"#)
                .starts_with("error: invalid arguments: unknown field `path`, expected `command`")
        );
        assert_eq!(
            execute(root, "nope", "{}"),
            "error: tool nope is not granted to this agent; it may call bash read_file write_file edit"
        );
    }

    #[test]
    fn every_defined_tool_is_dispatched() {
        for tool in definitions().as_array().unwrap() {
            let name = tool.get("function").and_then(|f| f.get("name")).and_then(Value::as_str).unwrap();
            assert!(
                !execute(Path::new("/"), name, "{}").starts_with("error: unknown tool"),
                "{name} is defined but not dispatched"
            );
        }
    }

    #[test]
    fn a_read_numbers_conflicts_and_a_write_resolves_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src.rs");
        fs::write(&file, "<<<<<<< HEAD\nlet x = 1;\n=======\nlet x = 2;\n>>>>>>> other\n").unwrap();
        let mut state = sandbox();
        let read = with_sandbox(dir.path(), READ_FILE, r#"{"path":"src.rs"}"#, &mut state);
        assert!(read.contains("⚠ 1 unresolved conflict in src.rs"), "{read}");
        assert!(read.contains("- #1: lines 1–5 (ours \"HEAD\", theirs \"other\")"), "{read}");
        let write = json!({"path": "conflict://1", "content": "@theirs"}).to_string();
        let out = with_sandbox(dir.path(), WRITE_FILE, &write, &mut state);
        assert!(out.starts_with("resolved conflict #1 at lines 1–5 in src.rs"), "{out}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "let x = 2;\n");
        assert!(!out.contains("error"), "{out}");
    }

    #[test]
    fn tool_descriptions_advertise_only_the_uris_that_resolve() {
        let described = |name: &str| -> String {
            definitions().as_array().unwrap().iter().find(|tool| tool["function"]["name"] == name).unwrap()["function"]
                ["description"]
                .as_str()
                .unwrap()
                .to_string()
        };
        for name in [READ_FILE, WRITE_FILE] {
            let text = described(name);
            assert!(text.contains("Internal URIs are accepted as `path`"), "{text}");
            // Every scheme the build resolves is named, and nothing else looks like one.
            for scheme in url::SCHEMES.iter().map(|scheme| scheme.name) {
                assert!(text.contains(&format!("`{scheme}://`")), "{name} does not advertise {scheme}: {text}");
            }
            assert!(!text.contains("`nobody://`"), "{name} advertises a scheme this build lacks: {text}");
        }
    }

    #[test]
    fn a_plain_file_read_carries_no_conflict_notice() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("plain.rs"), "fn main() {}\n").unwrap();
        assert_eq!(execute(dir.path(), READ_FILE, r#"{"path":"plain.rs"}"#), "1:fn main() {}\n");
        assert_eq!(
            execute(dir.path(), READ_FILE, r#"{"path":"plain.rs:conflicts"}"#),
            "no conflict markers in plain.rs"
        );
    }

    #[test]
    fn preview_separates_the_verdict_from_the_output() {
        let ok = preview(BASH, "exit code: 0\nstdout:\na\nb\nstderr:\n");
        assert_eq!(ok.status, "", "a success needs no chip");
        assert_eq!(ok.head, "a\nb");
        assert_eq!(ok.more, 0);
        assert!(!ok.failed);
        let failed = preview(BASH, "exit code: 2\nstdout:\nstderr:\nmissing\n");
        assert_eq!(failed.status, "exit 2");
        assert!(failed.failed);
        assert_eq!(failed.head, "missing");
        let file = (1..=9).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
        let read = preview(READ_FILE, &file);
        assert_eq!(read.status, "9 lines");
        assert_eq!(read.head.lines().count(), PREVIEW_LINES);
        assert_eq!(read.more, 9 - PREVIEW_LINES);
        let edit = preview(EDIT, "edit: +1/-1\n-2|b\n+2|B\n");
        assert_eq!(edit.status, "+1/-1");
        assert_eq!(edit.head, "-2|b\n+2|B");
        assert_eq!(edit.more, 0);
        assert!(!edit.failed);
    }

    #[test]
    fn truncation_keeps_the_head_and_the_tail() {
        let out = truncate("é".repeat(20_000)); // 40 000 bytes, 2 per char
        assert!(out.contains("[truncated: 10000 of 40000 bytes elided; full output at artifact://"), "{out}");
        assert!(out.starts_with("éé"), "{out}");
        assert!(out.ends_with("éé"), "{out}");
        assert!(out.len() < MAX_OUTPUT_BYTES + 120, "{}", out.len());
    }

    #[test]
    fn an_elided_result_is_read_back_whole_through_its_artifact() {
        let text = format!("HEAD\n{}\nTAIL", "middle ".repeat(6_000));
        let out = truncate(text.clone());
        let marker = out.split("full output at artifact://").nth(1).expect("the marker names an artifact");
        let id: String = marker.chars().take_while(char::is_ascii_digit).collect();
        let dir = tempfile::tempdir().unwrap();
        let mut read = String::new();
        let mut offset = 0;
        loop {
            let page =
                execute(dir.path(), READ_FILE, &json!({"path": format!("artifact://{id}/{offset}")}).to_string());
            let Some((body, rest)) = page.split_once("\n[artifact ") else {
                read.push_str(&page);
                break;
            };
            read.push_str(body);
            let link = rest.split_once("read artifact://").expect("a page marker links to the next page").1;
            let next = link.split_once('/').expect("the link carries the next offset").1;
            offset = next.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap();
        }
        assert_eq!(read, text);
    }

    #[test]
    fn a_chatty_command_is_capped_per_stream() {
        let dir = tempfile::tempdir().unwrap();
        let out = execute(dir.path(), BASH, r#"{"command":"yes | head -c 200000 >&2"}"#);
        assert!(out.contains("[output truncated: only the first 30000 bytes of each stream were kept]"), "{out}");
        assert!(out.len() < MAX_OUTPUT_BYTES + 200, "{}", out.len());
    }

    #[test]
    fn a_backgrounded_child_does_not_hold_the_call_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = sandbox();
        let home = tempfile::tempdir().unwrap();
        let skills = skills::Catalogue::user(home.path());
        let mcp = mcp::Registry::load(home.path());
        let profiles = profiles::Catalogue::default();
        let context = Context { skills: &skills, profiles: &profiles, mcp: &mcp, scope: None };
        let started = Instant::now();
        // `sh` exits at once; the child it left behind inherits both pipes, so only the group kill
        // DRAIN_GRACE allows can close them.
        let out = outcome(run(
            dir.path(),
            BASH,
            r#"{"command":"sleep 300 &"}"#,
            &file_tools(),
            &mut state.conflicts,
            &mut state.seen,
            Duration::from_millis(200),
            None,
            &context,
        ));
        assert!(out.contains("exit code: 0"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(20), "the call waited on the backgrounded child");
    }

    #[test]
    fn kill_running_stops_a_command_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pid");
        let command = format!(r#"{{"command":"echo $$ > {}; sleep 30"}}"#, marker.display());
        let handle = thread::spawn(move || execute(dir.path(), BASH, &command));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(Instant::now() < deadline, "the command never started");
            thread::sleep(Duration::from_millis(10));
        }
        let killed = Instant::now();
        kill_running();
        let out = handle.join().unwrap();
        assert!(out.contains("killed by signal"), "{out}");
        assert!(killed.elapsed() < Duration::from_secs(10), "the command outlived the kill");
    }

    #[test]
    fn a_large_file_is_read_only_up_to_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big"), "a".repeat(100_000)).unwrap();
        let out = execute(dir.path(), READ_FILE, r#"{"path":"big"}"#);
        assert!(out.ends_with("28000 is the read budget, not the file size]"), "{}", &out[out.len() - 90..]);
        assert!(out.len() < MAX_OUTPUT_BYTES, "{}", out.len());
    }
    /// A read-only policy and the calls it must refuse.
    #[test]
    fn read_only_refuses_writes_and_writing_commands() {
        let dir = tempfile::tempdir().unwrap();
        let policy = Policy { read_only: true, ..file_tools() };
        let write = json!({"path": "x", "content": "y"}).to_string();
        assert!(
            guarded(dir.path(), WRITE_FILE, &write, &policy, None, &mut sandbox())
                .starts_with("error: this agent is read-only"),
            "write_file ran in a read-only agent"
        );
        let patch = json!({"path": "x", "patch": "PUT 1.=1:\n+y\n"}).to_string();
        assert!(
            guarded(dir.path(), EDIT, &patch, &policy, None, &mut sandbox())
                .starts_with("error: this agent is read-only"),
            "edit ran in a read-only worker"
        );
        for command in ["rm x", "ls > x", "echo $(rm x)", "git commit -m x", "sed -i s/a/b/ x"] {
            let args = json!({"command": command}).to_string();
            let out = guarded(dir.path(), BASH, &args, &policy, None, &mut sandbox());
            assert!(out.starts_with("error: this agent is read-only"), "`{command}` ran: {out}");
        }
        let read = json!({"command": "ls | head -5"}).to_string();
        let out = guarded(dir.path(), BASH, &read, &policy, None, &mut sandbox());
        assert!(!out.starts_with("error:"), "a read-only pipeline was refused: {out}");
    }

    /// A subcommand or a flag that writes must not pass as read-only: listing `git` subcommands and
    /// the programs with a writing option are the ways around a list of program names.
    #[test]
    fn read_only_refuses_writing_subcommands_and_flags() {
        let dir = tempfile::tempdir().unwrap();
        let policy = Policy { read_only: true, ..file_tools() };
        for command in [
            "git branch -D feature",
            "git branch topic",
            "git remote add origin url",
            "git tag -d v1",
            "git tag v1",
            "git diff --output=x",
            "rg --pre 'sh -c id' needle",
            "file -C -m out src",
            "tree -o out.md",
        ] {
            let args = json!({"command": command}).to_string();
            let out = guarded(dir.path(), BASH, &args, &policy, None, &mut sandbox());
            assert!(out.starts_with("error: this agent is read-only"), "`{command}` ran: {out}");
        }
        for command in ["git branch --merged", "git remote -v", "git tag -l", "git log --oneline -5"] {
            let args = json!({"command": command}).to_string();
            let out = guarded(dir.path(), BASH, &args, &policy, None, &mut sandbox());
            assert!(!out.starts_with("error: this agent is read-only"), "`{command}` was refused: {out}");
        }
    }

    #[test]
    fn denied_tools_and_commands_never_run() {
        let dir = tempfile::tempdir().unwrap();
        let policy = Policy {
            allow_tools: vec![BASH.into(), WRITE_FILE.into()],
            allow_commands: vec!["ls".into(), "echo".into()],
            deny_commands: vec!["echo secret".into()],
            ..Policy::default()
        };
        let read = json!({"path": "f"}).to_string();
        assert_eq!(
            guarded(dir.path(), READ_FILE, &read, &policy, None, &mut sandbox()),
            "error: tool read_file is not granted to this agent; it may call bash write_file"
        );
        let denied = json!({"command": "echo secret"}).to_string();
        assert_eq!(
            guarded(dir.path(), BASH, &denied, &policy, None, &mut sandbox()),
            "error: this agent may not run `echo secret`"
        );
        let outside = json!({"command": "pwd"}).to_string();
        assert_eq!(
            guarded(dir.path(), BASH, &outside, &policy, None, &mut sandbox()),
            "error: this agent runs only: ls · echo"
        );
        let allowed = json!({"command": "ls"}).to_string();
        assert!(!guarded(dir.path(), BASH, &allowed, &policy, None, &mut sandbox()).starts_with("error:"));
    }

    /// A rule names words: what a text prefix would let through must be refused, and what hides the
    /// program behind a path or a wrapper must not.
    #[test]
    fn a_command_rule_matches_words_not_text() {
        let dir = tempfile::tempdir().unwrap();
        let only_ls = Policy { allow_commands: vec!["ls".into()], ..file_tools() };
        for command in ["lsof", "ls; rm -rf x", "ls | sh", "ls && id"] {
            let args = json!({"command": command}).to_string();
            let out = guarded(dir.path(), BASH, &args, &only_ls, None, &mut sandbox());
            assert!(out.starts_with("error:"), "`{command}` passed an `ls` rule: {out}");
        }
        let wrapped = json!({"command": "env ls"}).to_string();
        assert!(!guarded(dir.path(), BASH, &wrapped, &only_ls, None, &mut sandbox()).starts_with("error:"));

        let no_rm = Policy { deny_commands: vec!["rm".into()], ..file_tools() };
        for command in ["env rm -rf x", "/bin/rm -rf x", "X=1 rm -rf x"] {
            let args = json!({"command": command}).to_string();
            let out = guarded(dir.path(), BASH, &args, &no_rm, None, &mut sandbox());
            assert!(out.starts_with("error: this agent may not run `rm`"), "`{command}` ran: {out}");
        }
    }

    #[test]
    fn a_skill_uri_reads_through_the_catalogue() {
        let home = tempfile::tempdir().unwrap();
        let skill = home.path().join(".agents/skills/caveman");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "---\ndescription: be brief\n---\nbody\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let skills = skills::Catalogue::user(home.path());
        let mcp = mcp::Registry::load(home.path());
        let profiles = profiles::Catalogue::default();
        let context = Context { skills: &skills, profiles: &profiles, mcp: &mcp, scope: None };
        let mut state = sandbox();
        let read = json!({"path": "skill://caveman"}).to_string();
        let out = execute_guarded(
            dir.path(),
            READ_FILE,
            &read,
            &file_tools(),
            None,
            &mut state.conflicts,
            &mut state.seen,
            &context,
        );
        assert!(out.contains("body"), "{out}");
        let missing = json!({"path": "skill://nope"}).to_string();
        let out = execute_guarded(
            dir.path(),
            READ_FILE,
            &missing,
            &file_tools(),
            None,
            &mut state.conflicts,
            &mut state.seen,
            &context,
        );
        assert!(out.starts_with("error: unknown skill nope"), "{out}");
    }

    #[test]
    fn a_worker_reads_a_profile_through_read_file() {
        let home = tempfile::tempdir().unwrap();
        let skills = skills::Catalogue::in_dir(home.path(), home.path());
        let profiles = profiles::Catalogue::default();
        let mcp = mcp::Registry::load(home.path());
        let context = Context { skills: &skills, profiles: &profiles, mcp: &mcp, scope: None };
        let mut conflicts = conflict::Registry::default();
        let mut seen = edit::Store::default();
        let args = ReadArgs { path: "profile://orchestrator".into() };
        let text = read_file(home.path(), &args, &mut conflicts, &mut seen, &context).unwrap();
        assert!(text.contains("You own one task from the brief"), "{text}");
        let missing = ReadArgs { path: "profile://ghost".into() };
        let text = read_file(home.path(), &missing, &mut conflicts, &mut seen, &context).unwrap_err();
        assert!(text.contains("unknown profile ghost"), "{text}");
    }

    #[test]
    fn a_read_selector_numbers_only_what_it_names() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("t.rs"), "a\nb\nc\nd\ne\n").unwrap();
        let read = |path: &str| execute(dir.path(), READ_FILE, &json!({"path": path}).to_string());
        assert_eq!(read("t.rs:2-4"), "2:b\n3:c\n4:d");
        assert_eq!(read("t.rs:2-"), "2:b\n3:c\n4:d\n5:e");
        assert_eq!(read("t.rs:-2"), "1:a\n2:b");
        assert_eq!(read("t.rs:3"), "3:c");
        assert_eq!(read("t.rs:2-3:raw"), "b\nc");
        assert_eq!(read("t.rs:raw:2-3"), "b\nc");
        assert_eq!(read("t.rs:0"), "error: line 0 is not a line: ranges start at 1");
        assert_eq!(read("t.rs:4-2"), "error: range 4-2 is reversed");
        assert_eq!(read("t.rs:4-9"), "error: range 4-9 is past t.rs (5 lines)");
    }

    #[test]
    fn a_literal_file_named_like_a_selector_wins() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes:12"), "literal\n").unwrap();
        assert_eq!(execute(dir.path(), READ_FILE, r#"{"path":"notes:12"}"#), "1:literal\n");
    }

    #[test]
    fn a_read_numbers_and_an_edit_anchors_on_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = sandbox();
        let write = json!({"path": "t.rs", "content": "a\nb\nc\n"}).to_string();
        assert_eq!(with_sandbox(dir.path(), WRITE_FILE, &write, &mut state), "wrote 6 bytes to t.rs");
        assert_eq!(with_sandbox(dir.path(), READ_FILE, r#"{"path":"t.rs"}"#, &mut state), "1:a\n2:b\n3:c\n");
        let patch = json!({"path": "t.rs", "patch": "PUT 2.=2:\n+B\n"}).to_string();
        assert_eq!(with_sandbox(dir.path(), EDIT, &patch, &mut state), "edit: +1/-1\n-2|b\n+2|B");
        assert_eq!(fs::read_to_string(dir.path().join("t.rs")).unwrap(), "a\nB\nc\n");
        // A partial read licenses only the range it showed.
        assert_eq!(with_sandbox(dir.path(), READ_FILE, r#"{"path":"t.rs:2-3"}"#, &mut state), "2:B\n3:c");
        let outside = json!({"path": "t.rs", "patch": "PUT 1.=1:\n+Z\n"}).to_string();
        assert_eq!(
            with_sandbox(dir.path(), EDIT, &outside, &mut state),
            "error: line 1 is outside what you read (you saw 2-3); read the range first"
        );
        let inside = json!({"path": "t.rs", "patch": "PUT 3.=3:\n+C\n"}).to_string();
        assert_eq!(with_sandbox(dir.path(), EDIT, &inside, &mut state), "edit: +1/-1\n-3|c\n+3|C");
        // An edit leaves the whole file seen.
        assert_eq!(with_sandbox(dir.path(), READ_FILE, r#"{"path":"t.rs"}"#, &mut state), "1:a\n2:B\n3:C\n");
    }

    #[test]
    fn an_edit_after_a_concurrent_change_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = sandbox();
        let write = json!({"path": "t.rs", "content": "a\n"}).to_string();
        with_sandbox(dir.path(), WRITE_FILE, &write, &mut state);
        with_sandbox(dir.path(), READ_FILE, r#"{"path":"t.rs"}"#, &mut state);
        fs::write(dir.path().join("t.rs"), "a\nb\n").unwrap();
        let patch = json!({"path": "t.rs", "patch": "PUT 1.=1:\n+z\n"}).to_string();
        assert_eq!(
            with_sandbox(dir.path(), EDIT, &patch, &mut state),
            "error: t.rs changed since you read it; read it again"
        );
        assert_eq!(fs::read_to_string(dir.path().join("t.rs")).unwrap(), "a\nb\n");
    }

    #[test]
    fn an_edit_of_an_unread_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("t.rs"), "a\n").unwrap();
        let patch = json!({"path": "t.rs", "patch": "PUT 1.=1:\n+z\n"}).to_string();
        assert_eq!(with_sandbox(dir.path(), EDIT, &patch, &mut sandbox()), "error: read t.rs before editing it");
    }

    #[test]
    fn a_no_op_edit_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = sandbox();
        let write = json!({"path": "t.rs", "content": "a\n"}).to_string();
        with_sandbox(dir.path(), WRITE_FILE, &write, &mut state);
        let patch = json!({"path": "t.rs", "patch": "PUT 1.=1:\n+a\n"}).to_string();
        assert_eq!(
            with_sandbox(dir.path(), EDIT, &patch, &mut state),
            "error: no change: the patch leaves t.rs as it is"
        );
        assert_eq!(fs::read_to_string(dir.path().join("t.rs")).unwrap(), "a\n");
    }

    #[test]
    fn a_cancelled_command_is_killed_without_waiting_for_its_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        let out = guarded(dir.path(), BASH, r#"{"command":"sleep 30"}"#, &file_tools(), Some(&cancel), &mut sandbox());
        assert!(out.contains("interrupted by overseer"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }
}
