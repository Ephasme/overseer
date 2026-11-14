use crate::config;
use crate::conflict;
use crate::mcp;
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

const MAX_OUTPUT_BYTES: usize = 30_000;

/// Longest file body `read_file` returns, leaving room for its truncation note.
const MAX_READ_BYTES: usize = 28_000;

/// Largest file `read_file`'s `:conflicts` selector scans whole, so listing blocks cannot exhaust memory.
const MAX_SCAN_BYTES: u64 = 8 << 20;

/// A command that outlives this is killed: one stuck worker must not stall the fleet forever.
const BASH_TIMEOUT: Duration = Duration::from_secs(300);

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
fn spawn_grouped(command: &mut Command) -> Result<(Child, i32), String> {
    let mut live = groups();
    let child = command.spawn().map_err(|e| e.to_string())?;
    let group = i32::try_from(child.id()).map_err(|e| e.to_string())?;
    live.push(group);
    Ok((child, group))
}

fn deregister(group: i32) {
    groups().retain(|live| *live != group);
}

/// Kills every command still running: the session calls this on its way out, under any frontend.
pub(crate) fn kill_running() {
    let running: Vec<i32> = groups().drain(..).collect();
    for group in running {
        let _ = Command::new("kill").arg("-9").arg(format!("-{group}")).status();
    }
}

/// Names of the worker's file and shell tools. The schema and the dispatch both use these.
pub(crate) const BASH: &str = "bash";
pub(crate) const READ_FILE: &str = "read_file";
pub(crate) const WRITE_FILE: &str = "write_file";
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
    /// `write_file` is refused and `bash` runs only `READ_ONLY_PROGRAMS`.
    pub(crate) read_only: bool,
    /// Tool names this worker may never call.
    pub(crate) deny_tools: Vec<String>,
    /// When non-empty, a `bash` command's program word must match one of these.
    pub(crate) allow_commands: Vec<String>,
    /// A `bash` command whose program word matches one of these never runs. Deny beats allow.
    pub(crate) deny_commands: Vec<String>,
    /// Every `bash` command is refused. Two allow lists that share nothing allow nothing, and an
    /// empty `allow_commands` already means "no restriction", so the refusal needs its own flag.
    pub(crate) deny_all_commands: bool,
}

impl Policy {
    /// This policy with the fields that are given replaced; an omitted field keeps its value.
    pub(crate) fn set(
        &self,
        read_only: Option<bool>,
        deny_tools: Option<Vec<String>>,
        allow_commands: Option<Vec<String>>,
        deny_commands: Option<Vec<String>>,
    ) -> Self {
        Self {
            read_only: read_only.unwrap_or(self.read_only),
            deny_tools: deny_tools.unwrap_or_else(|| self.deny_tools.clone()),
            allow_commands: allow_commands.unwrap_or_else(|| self.allow_commands.clone()),
            deny_commands: deny_commands.unwrap_or_else(|| self.deny_commands.clone()),
            deny_all_commands: self.deny_all_commands,
        }
    }

    /// The policy a spawned agent is born with: this one, tightened by `given` and never loosened.
    ///
    /// `read_only` sticks, deny lists are unioned, and two allow lists intersect — a child of a
    /// restricted agent cannot be handed back a capability the owner refused. An intersection that
    /// leaves nothing allows nothing, rather than reading as an absent allow list. Called once, on
    /// the spawn path: an agent's policy never changes after that.
    pub(crate) fn inherit(&self, given: &Self) -> Self {
        let mut deny_tools = self.deny_tools.clone();
        for tool in &given.deny_tools {
            if !deny_tools.contains(tool) {
                deny_tools.push(tool.clone());
            }
        }
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
            read_only: self.read_only || given.read_only,
            deny_tools,
            allow_commands,
            deny_commands,
            deny_all_commands,
        }
    }

    /// The restriction lines a worker reads in its prompt; empty when nothing is restricted.
    pub(crate) fn describe(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        if self.read_only {
            lines.push(format!(
                "- read-only: write_file is refused, and bash runs only {} (git only: {}).",
                READ_ONLY_PROGRAMS.join(" "),
                READ_ONLY_GIT.join(" ")
            ));
        }
        if !self.deny_tools.is_empty() {
            lines.push(format!("- banned tools: {}.", self.deny_tools.join(" ")));
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
        if lines.is_empty() {
            return String::new();
        }
        format!(
            "Rights your owner gave you when it started you; they are fixed for your lifetime and a call that breaks one comes back as an error:\n{}\n",
            lines.join("\n")
        )
    }

    /// Why this call may not run, or `None` when it may.
    pub(crate) fn refuse(&self, name: &str, arguments: &str) -> Option<String> {
        if self.deny_tools.iter().any(|tool| tool == name) {
            return Some(format!("tool {name} is banned for this agent"));
        }
        if name == WRITE_FILE && self.read_only {
            return Some("this agent is read-only: write_file is refused".into());
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
            "description": format!("Run a shell command with `sh -c` in the working directory. Returns exit code, stdout and stderr. Stdin is closed; the command is killed after {} seconds.", BASH_TIMEOUT.as_secs()),
            "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}
        }},
        {"type": "function", "function": {
            "name": READ_FILE,
            "description": format!("Read a UTF-8 text file. Relative paths resolve against the working directory. A file holding merge-conflict markers gets every block an id, listed after the text. `<path>:conflicts` registers and lists every block of a file.{uris}"),
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}
        }},
        {"type": "function", "function": {
            "name": WRITE_FILE,
            "description": format!("Create or overwrite a file with the given content. Creates missing parent directories. Relative paths resolve against the working directory.{uris}"),
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}
        }}
    ])
}

/// Arguments of each worker tool. `deny_unknown_fields` turns a wrong key into an error naming the right ones.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
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

/// Parses tool arguments, turning a serde failure into the text the model reads.
pub(crate) fn parse_args<'a, T: Deserialize<'a>>(arguments: &'a str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

/// Renders a tool outcome as the text the model reads: `error: ` for a failure, truncated otherwise.
pub(crate) fn outcome(result: Result<String, String>) -> String {
    truncate(result.unwrap_or_else(|e| format!("error: {e}")))
}

/// What a worker's tool call may reach beyond its working directory: its skills and the servers.
pub(crate) struct Context<'a> {
    pub(crate) skills: &'a skills::Catalogue,
    pub(crate) mcp: &'a mcp::Registry,
}

/// Runs a worker's tool call under its policy, killing a running command once `cancel` is set.
pub(crate) fn execute_guarded(
    working_dir: &Path,
    name: &str,
    arguments: &str,
    policy: &Policy,
    cancel: Option<&AtomicBool>,
    conflicts: &mut conflict::Registry,
    context: &Context<'_>,
) -> String {
    match policy.refuse(name, arguments) {
        Some(refusal) => outcome(Err(refusal)),
        None => outcome(run(working_dir, name, arguments, conflicts, BASH_TIMEOUT, cancel, context)),
    }
}

fn run(
    working_dir: &Path,
    name: &str,
    arguments: &str,
    conflicts: &mut conflict::Registry,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
    context: &Context<'_>,
) -> Result<String, String> {
    match name {
        BASH => parse_args::<BashArgs>(arguments).and_then(|args| bash(working_dir, &args, timeout, cancel)),
        READ_FILE => {
            parse_args::<ReadArgs>(arguments).and_then(|args| read_file(working_dir, &args, conflicts, context))
        }
        WRITE_FILE => parse_args::<WriteArgs>(arguments).and_then(|args| write_file(working_dir, &args, conflicts)),
        // An MCP server reports its own failures as text, which is what the model reads.
        _ if name.starts_with(mcp::PREFIX) => Ok(context.mcp.call(name, arguments)),
        _ => Err(format!("unknown tool {name}")),
    }
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
    let mut stdout = drain(child.stdout.take());
    let mut stderr = drain(child.stderr.take());
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

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Drain {
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
                        let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
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

/// Reads a UTF-8 file, keeping the head of a file larger than the read budget.
///
/// Every conflict block the read shows is registered, so a later write can address it by id.
fn read_file(
    working_dir: &Path,
    args: &ReadArgs,
    conflicts: &mut conflict::Registry,
    context: &Context<'_>,
) -> Result<String, String> {
    if url::is_url(&args.path) {
        return url::read(&mut url::Ctx { conflicts: Some(conflicts), skills: Some(context.skills) }, &args.path);
    }
    if let Some(display) = args.path.strip_suffix(conflict::LIST_SUFFIX) {
        return list_conflicts(working_dir, display, conflicts);
    }
    let full = working_dir.join(&args.path);
    let (text, cut) = read_limited(&full, &args.path)?;
    let absolute = fs::canonicalize(&full).map_err(|e| config::io_error(&args.path, &e))?;
    conflicts.reconcile(&absolute, &args.path, &text);
    let mut out = with_note(text, cut);
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
    Ok(out)
}

/// Reads a UTF-8 file, keeping the head of a file larger than the read budget, and says whether it
/// stopped early.
///
/// `label` is what the model sees in an error: a skill path is not the working directory's path,
/// and naming it as one would send the model to the wrong place.
pub(crate) fn read_limited(path: &Path, label: &str) -> Result<(String, bool), String> {
    let file = fs::File::open(path).map_err(|e| config::io_error(label, &e))?;
    let limit = u64::try_from(MAX_READ_BYTES).map_or(u64::MAX, |cap| cap.saturating_add(1));
    let mut bytes = Vec::with_capacity(8 * 1024);
    file.take(limit).read_to_end(&mut bytes).map_err(|e| config::io_error(label, &e))?;
    let cut = bytes.len() > MAX_READ_BYTES;
    if cut {
        // The budget can land inside a multi-byte character: keep the longest valid prefix below it.
        let end = bytes
            .get(..MAX_READ_BYTES)
            .map_or(0, |head| std::str::from_utf8(head).map_or_else(|e| e.valid_up_to(), |_| head.len()));
        bytes.truncate(end);
    }
    let text = String::from_utf8(bytes).map_err(|_| format!("{label}: not valid UTF-8"))?;
    Ok((text, cut))
}

/// The text as the model reads it, with the truncation note when the read stopped early.
pub(crate) fn with_note(text: String, cut: bool) -> String {
    if !cut {
        return text;
    }
    let shown = text.len();
    format!("{text}\n[truncated: first {shown} bytes shown; {MAX_READ_BYTES} is the read budget, not the file size]")
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

fn write_file(working_dir: &Path, args: &WriteArgs, conflicts: &mut conflict::Registry) -> Result<String, String> {
    if url::is_url(&args.path) {
        return url::write(&mut url::Ctx { conflicts: Some(conflicts), skills: None }, &args.path, &args.content);
    }
    let full = working_dir.join(&args.path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).map_err(|e| config::io_error(parent.display(), &e))?;
    }
    fs::write(&full, &args.content).map_err(|e| config::io_error(&args.path, &e))?;
    Ok(format!("wrote {} bytes to {}", args.content.len(), args.path))
}

/// Keeps the head and the tail of `s`: the head names what ran, the tail is where a command says why it failed.
fn truncate(s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let total = s.len();
    let half = MAX_OUTPUT_BYTES / 2;
    let head_end = floor_boundary(&s, half);
    let tail_start = ceil_boundary(&s, total - half);
    let elided = tail_start.saturating_sub(head_end);
    let head = s.get(..head_end).unwrap_or_default();
    let tail = s.get(tail_start..).unwrap_or_default();
    format!("{head}\n[truncated: {elided} of {total} bytes elided]\n{tail}")
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

    /// A registry per call: a policy test never resolves a conflict.
    fn registry() -> conflict::Registry {
        conflict::Registry::default()
    }

    /// A tool call with no policy, no cancellation and a fresh registry: each test sees its own ids.
    fn execute(dir: &Path, name: &str, arguments: &str) -> String {
        guarded(dir, name, arguments, &Policy::default(), None, &mut registry())
    }

    /// One tool call against a registry the caller keeps, for a read that a later write resolves.
    fn with_registry(dir: &Path, name: &str, arguments: &str, conflicts: &mut conflict::Registry) -> String {
        guarded(dir, name, arguments, &Policy::default(), None, conflicts)
    }

    /// The same call, with the empty skills and servers a plain tool test has.
    fn guarded(
        dir: &Path,
        name: &str,
        arguments: &str,
        policy: &Policy,
        cancel: Option<&AtomicBool>,
        conflicts: &mut conflict::Registry,
    ) -> String {
        let home = tempfile::tempdir().unwrap();
        let skills = skills::Catalogue::user(home.path());
        let mcp = mcp::Registry::load(home.path());
        execute_guarded(dir, name, arguments, policy, cancel, conflicts, &Context { skills: &skills, mcp: &mcp })
    }

    #[test]
    fn a_child_cannot_be_looser_than_its_owner() {
        let owner = Policy { read_only: true, deny_tools: vec!["bash".into()], ..Policy::default() };
        let asked = Policy::default();
        let inherited = owner.inherit(&asked);
        assert!(inherited.read_only, "a read-only owner born a writing agent");
        assert_eq!(inherited.deny_tools, ["bash"]);
    }

    #[test]
    fn a_child_may_be_tighter_than_its_owner() {
        let owner = Policy::default();
        let asked = Policy { read_only: true, deny_tools: vec!["bash".into()], ..Policy::default() };
        assert_eq!(owner.inherit(&asked), asked);
    }

    #[test]
    fn deny_lists_are_unioned_and_allow_lists_intersected() {
        let owner = Policy {
            deny_commands: vec!["rm".into()],
            allow_commands: vec!["git".into(), "cargo".into()],
            ..Policy::default()
        };
        let asked = Policy {
            deny_commands: vec!["curl".into(), "rm".into()],
            allow_commands: vec!["cargo".into()],
            ..Policy::default()
        };
        let inherited = owner.inherit(&asked);
        assert_eq!(inherited.deny_commands, ["rm", "curl"]);
        assert_eq!(inherited.allow_commands, ["cargo"]);
    }

    #[test]
    fn allow_lists_that_share_nothing_refuse_every_command() {
        let owner = Policy { allow_commands: vec!["git".into()], ..Policy::default() };
        let asked = Policy { allow_commands: vec!["cargo".into()], ..Policy::default() };
        let inherited = owner.inherit(&asked);
        assert!(inherited.deny_all_commands, "an empty intersection read as an absent allow list");
        assert_eq!(inherited.allow_commands, Vec::<String>::new());
        let dir = tempfile::tempdir().unwrap();
        let command = json!({"command": "git status"}).to_string();
        let out = guarded(dir.path(), BASH, &command, &inherited, None, &mut registry());
        assert!(out.starts_with("error: this agent runs no bash command"), "{out}");
        // A child of a deny-all agent stays deny-all, whatever allow list it asks for.
        let looser = inherited.inherit(&Policy { allow_commands: vec!["ls".into()], ..Policy::default() });
        assert!(looser.deny_all_commands);
    }

    #[test]
    fn a_denied_spawn_agent_survives_inheritance() {
        let owner = Policy { deny_tools: vec!["spawn_agent".into()], ..Policy::default() };
        let inherited = owner.inherit(&Policy::default());
        assert_eq!(inherited.deny_tools, ["spawn_agent"]);
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
        let mut conflicts = conflict::Registry::default();
        let home = tempfile::tempdir().unwrap();
        let skills = skills::Catalogue::user(home.path());
        let mcp = mcp::Registry::load(home.path());
        let context = Context { skills: &skills, mcp: &mcp };
        let out = outcome(run(dir.path(), BASH, command, &mut conflicts, Duration::from_millis(200), None, &context));
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
        assert_eq!(execute(dir.path(), READ_FILE, r#"{"path":"a/b/note.txt"}"#), "héllo");
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
        assert_eq!(execute(root, "nope", "{}"), "error: unknown tool nope");
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
        let mut conflicts = conflict::Registry::default();
        let read = with_registry(dir.path(), READ_FILE, r#"{"path":"src.rs"}"#, &mut conflicts);
        assert!(read.contains("⚠ 1 unresolved conflict in src.rs"), "{read}");
        assert!(read.contains("- #1: lines 1–5 (ours \"HEAD\", theirs \"other\")"), "{read}");
        let write = json!({"path": "conflict://1", "content": "@theirs"}).to_string();
        let out = with_registry(dir.path(), WRITE_FILE, &write, &mut conflicts);
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
            assert!(text.contains("`conflict://`"), "{text}");
            for reserved in ["mcp://", "ssh://", "pr://"] {
                assert!(!text.contains(reserved), "{name} advertises {reserved}: {text}");
            }
        }
    }

    #[test]
    fn a_plain_file_read_carries_no_conflict_notice() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("plain.rs"), "fn main() {}\n").unwrap();
        assert_eq!(execute(dir.path(), READ_FILE, r#"{"path":"plain.rs"}"#), "fn main() {}\n");
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
    }

    #[test]
    fn truncation_keeps_the_head_and_the_tail() {
        let out = truncate("é".repeat(20_000)); // 40 000 bytes, 2 per char
        assert!(out.contains("[truncated: 10000 of 40000 bytes elided]"), "{out}");
        assert!(out.starts_with("éé"), "{out}");
        assert!(out.ends_with("éé"), "{out}");
        assert!(out.len() < MAX_OUTPUT_BYTES + 80, "{}", out.len());
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
        let mut conflicts = conflict::Registry::default();
        let home = tempfile::tempdir().unwrap();
        let skills = skills::Catalogue::user(home.path());
        let mcp = mcp::Registry::load(home.path());
        let context = Context { skills: &skills, mcp: &mcp };
        let started = Instant::now();
        // `sh` exits at once; the child it left behind inherits both pipes, so only the group kill
        // DRAIN_GRACE allows can close them.
        let out = outcome(run(
            dir.path(),
            BASH,
            r#"{"command":"sleep 300 &"}"#,
            &mut conflicts,
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
        let policy = Policy { read_only: true, ..Policy::default() };
        let write = json!({"path": "x", "content": "y"}).to_string();
        assert!(
            guarded(dir.path(), WRITE_FILE, &write, &policy, None, &mut registry())
                .starts_with("error: this agent is read-only"),
            "write_file ran in a read-only worker"
        );
        for command in ["rm x", "ls > x", "echo $(rm x)", "git commit -m x", "sed -i s/a/b/ x"] {
            let args = json!({"command": command}).to_string();
            let out = guarded(dir.path(), BASH, &args, &policy, None, &mut registry());
            assert!(out.starts_with("error: this agent is read-only"), "`{command}` ran: {out}");
        }
        let read = json!({"command": "ls | head -5"}).to_string();
        let out = guarded(dir.path(), BASH, &read, &policy, None, &mut registry());
        assert!(!out.starts_with("error:"), "a read-only pipeline was refused: {out}");
    }

    /// A subcommand or a flag that writes must not pass as read-only: listing `git` subcommands and
    /// the programs with a writing option are the ways around a list of program names.
    #[test]
    fn read_only_refuses_writing_subcommands_and_flags() {
        let dir = tempfile::tempdir().unwrap();
        let policy = Policy { read_only: true, ..Policy::default() };
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
            let out = guarded(dir.path(), BASH, &args, &policy, None, &mut registry());
            assert!(out.starts_with("error: this agent is read-only"), "`{command}` ran: {out}");
        }
        for command in ["git branch --merged", "git remote -v", "git tag -l", "git log --oneline -5"] {
            let args = json!({"command": command}).to_string();
            let out = guarded(dir.path(), BASH, &args, &policy, None, &mut registry());
            assert!(!out.starts_with("error: this agent is read-only"), "`{command}` was refused: {out}");
        }
    }

    #[test]
    fn denied_tools_and_commands_never_run() {
        let dir = tempfile::tempdir().unwrap();
        let policy = Policy {
            read_only: false,
            deny_tools: vec![READ_FILE.into()],
            allow_commands: vec!["ls".into(), "echo".into()],
            deny_commands: vec!["echo secret".into()],
            deny_all_commands: false,
        };
        let read = json!({"path": "f"}).to_string();
        assert_eq!(
            guarded(dir.path(), READ_FILE, &read, &policy, None, &mut registry()),
            "error: tool read_file is banned for this agent"
        );
        let denied = json!({"command": "echo secret"}).to_string();
        assert_eq!(
            guarded(dir.path(), BASH, &denied, &policy, None, &mut registry()),
            "error: this agent may not run `echo secret`"
        );
        let outside = json!({"command": "pwd"}).to_string();
        assert_eq!(
            guarded(dir.path(), BASH, &outside, &policy, None, &mut registry()),
            "error: this agent runs only: ls · echo"
        );
        let allowed = json!({"command": "ls"}).to_string();
        assert!(!guarded(dir.path(), BASH, &allowed, &policy, None, &mut registry()).starts_with("error:"));
    }

    /// A rule names words: what a text prefix would let through must be refused, and what hides the
    /// program behind a path or a wrapper must not.
    #[test]
    fn a_command_rule_matches_words_not_text() {
        let dir = tempfile::tempdir().unwrap();
        let only_ls = Policy { allow_commands: vec!["ls".into()], ..Policy::default() };
        for command in ["lsof", "ls; rm -rf x", "ls | sh", "ls && id"] {
            let args = json!({"command": command}).to_string();
            let out = guarded(dir.path(), BASH, &args, &only_ls, None, &mut registry());
            assert!(out.starts_with("error:"), "`{command}` passed an `ls` rule: {out}");
        }
        let wrapped = json!({"command": "env ls"}).to_string();
        assert!(!guarded(dir.path(), BASH, &wrapped, &only_ls, None, &mut registry()).starts_with("error:"));

        let no_rm = Policy { deny_commands: vec!["rm".into()], ..Policy::default() };
        for command in ["env rm -rf x", "/bin/rm -rf x", "X=1 rm -rf x"] {
            let args = json!({"command": command}).to_string();
            let out = guarded(dir.path(), BASH, &args, &no_rm, None, &mut registry());
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
        let context = Context { skills: &skills, mcp: &mcp };
        let read = json!({"path": "skill://caveman"}).to_string();
        let out = execute_guarded(dir.path(), READ_FILE, &read, &Policy::default(), None, &mut registry(), &context);
        assert!(out.contains("body"), "{out}");
        let missing = json!({"path": "skill://nope"}).to_string();
        let out = execute_guarded(dir.path(), READ_FILE, &missing, &Policy::default(), None, &mut registry(), &context);
        assert!(out.starts_with("error: unknown skill nope"), "{out}");
    }

    #[test]
    fn a_cancelled_command_is_killed_without_waiting_for_its_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(true);
        let started = Instant::now();
        let out =
            guarded(dir.path(), BASH, r#"{"command":"sleep 30"}"#, &Policy::default(), Some(&cancel), &mut registry());
        assert!(out.contains("interrupted by overseer"), "{out}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }
}
