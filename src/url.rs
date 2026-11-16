//! Internal URIs: a tool path of the form `scheme://…` names something other than a file.
//!
//! One table decides what exists. A scheme with handlers is advertised everywhere a model looks —
//! tool descriptions, `/schemes` — and a reserved name is refused with the same words as a name
//! nobody ever wrote, so a model cannot learn about a capability this build does not have.

use crate::artifacts;
use crate::clipboard;
use crate::config;
use crate::conflict;
use crate::forge;
use crate::jobs;
use crate::mcp;
use crate::memory;
use crate::profiles;
use crate::prompt;
use crate::rules;
use crate::skills;
use crate::swarm::Swarm;
use crate::tools;
use crate::vault;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Mutex, Weak};

/// The caller a handler answers for: who asked, and the state of the process it asked from.
///
/// `swarm` is optional only so a unit test can build a scope without a tree; every path the product
/// takes sets it, and a handler that needs the tree says so when it is absent.
pub(crate) struct Scope<'a> {
    pub(crate) caller: usize,
    pub(crate) swarm: Option<&'a Weak<Mutex<Swarm>>>,
    pub(crate) config: &'a config::Config,
    pub(crate) working_dir: &'a Path,
    pub(crate) policy: &'a tools::Policy,
    pub(crate) mcp: &'a mcp::Registry,
    /// Delegations the caller has left, so `xd://` lists what this agent may actually call.
    pub(crate) remaining_depth: usize,
}

/// What a handler is given besides its own path: the state of the caller that asked.
pub(crate) struct Ctx<'a> {
    /// The caller's conflict registry; `None` for the orchestrator, which owns no files.
    pub(crate) conflicts: Option<&'a mut conflict::Registry>,
    /// The skills the caller may read; `None` for a caller that has no catalogue, such as a write.
    pub(crate) skills: Option<&'a skills::Catalogue>,
    /// The profiles the caller may read; `None` for a caller that has none, such as a write.
    pub(crate) profiles: Option<&'a profiles::Catalogue>,
    pub(crate) scope: Option<&'a Scope<'a>>,
}

/// A handler answers one `scheme://…` path: `(caller, rest of the URI)` or `(caller, rest, content)`.
type Read = fn(&mut Ctx<'_>, &str) -> Result<String, String>;
type Write = fn(&mut Ctx<'_>, &str, &str) -> Result<String, String>;

/// One scheme: what it is called, the one line models and the user read, and what it answers to.
pub(crate) struct Scheme {
    pub(crate) name: &'static str,
    summary: &'static str,
    read: Option<Read>,
    write: Option<Write>,
}

/// Every scheme this build resolves, in its order.
pub(crate) const SCHEMES: &[Scheme] = &[
    Scheme {
        name: "agent",
        summary: "an agent under you: read `agent://` for the roster, `agent://<id>` for one record, `agent://<id>/task` for what it was told; write `agent://<id>` (or `agent://all`) to instruct it",
        read: Some(agent_read),
        write: Some(agent_write),
    },
    Scheme {
        name: "artifact",
        summary: "output a tool result elided: read `artifact://` to list the ids, `artifact://<N>` for the whole text",
        read: Some(artifact_read),
        write: None,
    },
    Scheme {
        name: "attachment",
        summary: "an image pasted into the composer: read `attachment://` to list what this session holds, `attachment://<name>` for one file",
        read: Some(attachment_read),
        write: None,
    },
    Scheme {
        name: "cfg",
        summary: "the settings this session runs with, read-only: `cfg://` for every key, `cfg://<key>` for one",
        read: Some(cfg_read),
        write: None,
    },
    Scheme {
        name: "conflict",
        summary: "a merge-conflict block by id: read `conflict://<N>` (`/ours`, `/theirs`, `/base` for one side), write it to resolve that block, `conflict://*` to resolve several at once",
        read: Some(conflict_read),
        write: Some(conflict_write),
    },
    Scheme {
        name: "history",
        summary: "an agent's log: read `history://` to list the logs you may read, `history://<id>` for one",
        read: Some(history_read),
        write: None,
    },
    Scheme {
        name: "issue",
        summary: "a GitHub issue by number: read `issue://` for the open list (`?state=…` filters it), `issue://<N>` for one",
        read: Some(issue_read),
        write: None,
    },
    Scheme {
        name: "local",
        summary: "a note every agent of this session may read: write `local://<name>` to store one, read `local://` to list them or `local://<name>` for one",
        read: Some(local_read),
        write: Some(local_write),
    },
    Scheme {
        name: "mcp",
        summary: "a resource an MCP server offers: read `mcp://` to list them, `mcp://<uri>` for one",
        read: Some(mcp_read),
        write: None,
    },
    Scheme {
        name: "memory",
        summary: "this project's notes across sessions: read `memory://`, write it to replace them; `memory://<name>` is one note beside the project's",
        read: Some(memory_read),
        write: Some(memory_write),
    },
    Scheme {
        name: "mr",
        summary: "a GitLab merge request, through `glab`: read `mr://` for the list (`?state=…` filters it), `mr://<N>` for one, `mr://<N>/diff` for its patch",
        read: Some(mr_read),
        write: None,
    },
    Scheme {
        name: "out",
        summary: "a text shown to the user as you write it: write `out://` with the text, read `out://` to list what was written, `out://<n>` for one",
        read: Some(out_read),
        write: Some(out_write),
    },
    Scheme {
        name: "overseer",
        summary: "this harness's own manual: read `overseer://` for the topics, `overseer://<topic>` for one of schemes, config, tools, agents",
        read: Some(overseer_read),
        write: None,
    },
    Scheme {
        name: "pr",
        summary: "a GitHub pull request: read `pr://` for the list (`?state=…` filters it), `pr://<N>` for one, `pr://<N>/diff` for its patch",
        read: Some(pr_read),
        write: None,
    },
    Scheme {
        name: "proc",
        summary: "a background job `bash` started: read `proc://` to list them, `proc://<id>` for its tail, `proc://<id>/streams` for its output; write `proc://<id>` to feed its stdin, `proc://<id>/kill` to end it",
        read: Some(proc_read),
        write: Some(proc_write),
    },
    Scheme {
        name: "profile",
        summary: "an agent profile by name: read `profile://<name>` for its file, frontmatter included",
        read: Some(profile_read),
        write: None,
    },
    Scheme {
        name: "rule",
        summary: "the instruction files that govern your directory: read `rule://` to list them, `rule://<path>` for one",
        read: Some(rule_read),
        write: None,
    },
    Scheme {
        name: "security",
        summary: "the rights this agent runs with: read `security://` for them, `security://<tool>` to know whether one call would be allowed",
        read: Some(security_read),
        write: None,
    },
    Scheme {
        name: "skill",
        summary: "a skill by name: read `skill://<name>` for its `SKILL.md`, `skill://<name>/<path>` for another file of it",
        read: Some(skill_read),
        write: None,
    },
    Scheme {
        name: "ssh",
        summary: "a file on another host, read over BatchMode ssh: `ssh://<host>/<path>`, with `~/` for the remote home",
        read: Some(ssh_read),
        write: None,
    },
    Scheme {
        name: "vault",
        summary: "the `.env` credential file: read `vault://` to list its keys, `vault://<NAME>` for one value; write it to set a key",
        read: Some(vault_read),
        write: Some(vault_write),
    },
    Scheme {
        name: "xd",
        summary: "a tool's schema: read `xd://` for the tools this agent may call, `xd://<name>` for one schema as JSON",
        read: Some(xd_read),
        write: None,
    },
];

/// True when `path` is an internal URI rather than a file: `<scheme>://…`.
///
/// The scheme is read the way RFC 3986 reads it, so `http://…` is reported as an unavailable URI
/// instead of being mistaken for a file that happens to contain a colon.
pub(crate) fn is_url(path: &str) -> bool {
    let Some((scheme, _)) = path.split_once("://") else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Reads `href` through the scheme that owns it.
pub(crate) fn read(ctx: &mut Ctx, href: &str) -> Result<String, String> {
    let scheme = resolve(href)?;
    let handler = scheme.read.ok_or_else(|| format!("`{}://` cannot be read", scheme.name))?;
    handler(ctx, rest_of(href))
}

/// Writes `content` to `href` through the scheme that owns it.
pub(crate) fn write(ctx: &mut Ctx, href: &str, content: &str) -> Result<String, String> {
    let scheme = resolve(href)?;
    let handler = scheme.write.ok_or_else(|| format!("`{}://` is read-only", scheme.name))?;
    handler(ctx, rest_of(href), content)
}

/// The schemes, as tool descriptions state them; `None` when this build resolves nothing.
pub(crate) fn summary() -> Option<String> {
    let lines: Vec<String> =
        SCHEMES.iter().map(|scheme| format!("- `{}://` — {}", scheme.name, scheme.summary)).collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Every scheme: what `/schemes` shows a human.
pub(crate) fn report() -> String {
    let lines: Vec<String> = SCHEMES.iter().map(|scheme| format!("  {}://  {}", scheme.name, scheme.summary)).collect();
    let mut out = format!("internal URIs: {} active\n", lines.len());
    out.push_str(&lines.join("\n"));
    out.push('\n');
    out
}

/// Splits `href` into its scheme and the remainder, or says why no scheme answers to it.
fn resolve(href: &str) -> Result<&'static Scheme, String> {
    let Some((name, _)) = href.split_once("://") else {
        return Err(format!("`{}` is not an internal URI: it has no `scheme://` prefix", shown(href)));
    };
    SCHEMES.iter().find(|scheme| scheme.name.eq_ignore_ascii_case(name)).ok_or_else(|| unavailable(name))
}

/// The remainder of `href` past its `scheme://`, which `resolve` has already checked.
fn rest_of(href: &str) -> &str {
    href.split_once("://").map_or("", |(_, rest)| rest)
}

/// `text` with control characters escaped, so a path cannot forge lines in a diagnostic.
fn shown(text: &str) -> String {
    text.chars().flat_map(char::escape_debug).collect()
}

fn unavailable(name: &str) -> String {
    let known: Vec<String> = SCHEMES.iter().map(|scheme| format!("`{}://`", scheme.name)).collect();
    if known.is_empty() {
        return format!("`{}://` is not an available scheme: this build resolves none", shown(name));
    }
    format!("`{}://` is not an available scheme: this build resolves {}", shown(name), known.join(", "))
}

fn conflict_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(conflicts) = ctx.conflicts.as_mut() else {
        return Err("conflict ids belong to the agent that read the file".to_string());
    };
    conflicts.read(rest)
}

fn conflict_write(ctx: &mut Ctx, rest: &str, content: &str) -> Result<String, String> {
    let Some(conflicts) = ctx.conflicts.as_mut() else {
        return Err("conflict ids belong to the agent that read the file".to_string());
    };
    conflicts.write(rest, content)
}

fn skill_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(skills) = ctx.skills else {
        return Err("skills are not available to this caller: it has no skill catalogue".to_string());
    };
    skills.read(rest)
}

/// Longest slice one `artifact://` read returns: a tool result is capped anyway, so a bigger one
/// would only be cut again, and the page marker is how the model asks for the rest.
const ARTIFACT_PAGE: usize = 28_000;

fn artifact_read(_ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    if rest.is_empty() {
        let listing = artifacts::list();
        if listing.is_empty() {
            return Ok("no artifacts in this session".to_string());
        }
        let lines: Vec<String> =
            listing.iter().map(|(id, bytes, first)| format!("#{id} {bytes} bytes {first}")).collect();
        return Ok(lines.join("\n"));
    }
    let known = || {
        let ids = artifacts::ids();
        if ids.is_empty() {
            "none".to_string()
        } else {
            ids.iter().map(ToString::to_string).collect::<Vec<String>>().join(", ")
        }
    };
    let (number, offset) = match rest.split_once('/') {
        Some((number, offset)) => {
            (number, offset.parse::<usize>().map_err(|_| "usage: artifact://<id>/<offset>".to_string())?)
        }
        None => (rest, 0),
    };
    let id: usize = number.parse().map_err(|_| format!("unknown artifact {number}; artifacts are {}", known()))?;
    let text = artifacts::read(id).ok_or_else(|| format!("unknown artifact {id}; artifacts are {}", known()))?;
    let start = floor_boundary(&text, offset);
    let end = floor_boundary(&text, start.saturating_add(ARTIFACT_PAGE));
    let mut page = text.get(start..end).unwrap_or_default().to_string();
    if end < text.len() {
        let _ = write!(
            page,
            "\n[artifact {id}: bytes {start}–{end} of {}; read artifact://{id}/{end} for the rest]",
            text.len()
        );
    }
    Ok(page)
}

/// Largest char boundary of `text` at or below `at`.
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut end = at.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn profile_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(profiles) = ctx.profiles else {
        return Err("profiles are not available to this caller: it has no profile catalogue".to_string());
    };
    profiles.read(rest)
}

fn attachment_read(_ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    if rest.is_empty() {
        return Ok(clipboard::listing());
    }
    clipboard::read(rest)
}

fn cfg_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(scope) = ctx.scope else {
        return Err("`cfg://` needs an agent session: this caller has none".to_string());
    };
    if rest.is_empty() {
        return Ok(config::describe(scope.config));
    }
    config::value(scope.config, rest)
}

fn security_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(scope) = ctx.scope else {
        return Err("`security://` needs an agent session: this caller has none".to_string());
    };
    if rest.is_empty() {
        return Ok(scope.policy.describe());
    }
    let known = prompt::tool_names(scope.mcp, scope.policy, scope.remaining_depth);
    Ok(tools::verdict(scope.policy, rest, &known))
}

fn xd_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(scope) = ctx.scope else {
        return Err("`xd://` needs an agent session: this caller has none".to_string());
    };
    if rest.is_empty() {
        return Ok(prompt::tool_names(scope.mcp, scope.policy, scope.remaining_depth).join("\n"));
    }
    prompt::schema(rest, scope.mcp, scope.policy, scope.remaining_depth)
}

/// The tree behind a handler that needs it, or the one sentence a caller without a session reads.
fn swarm_of<'a>(scope: Option<&'a Scope<'a>>, scheme: &str) -> Result<&'a Weak<Mutex<Swarm>>, String> {
    scope
        .and_then(|scope| scope.swarm)
        .ok_or_else(|| format!("`{scheme}://` needs an agent session: this caller has none"))
}

fn agent_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let weak = swarm_of(ctx.scope, "agent")?;
    let caller = ctx.scope.map_or(0, |scope| scope.caller);
    let Some(swarm) = weak.upgrade() else {
        return Err("`agent://` needs an agent session: this caller has none".to_string());
    };
    crate::swarm::lock(&swarm).agent_read(caller, rest)
}

fn agent_write(ctx: &mut Ctx, rest: &str, content: &str) -> Result<String, String> {
    let weak = swarm_of(ctx.scope, "agent")?;
    let caller = ctx.scope.map_or(0, |scope| scope.caller);
    let Some(swarm) = weak.upgrade() else {
        return Err("`agent://` needs an agent session: this caller has none".to_string());
    };
    crate::swarm::lock(&swarm).agent_write(caller, rest, content)
}

fn history_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let weak = swarm_of(ctx.scope, "history")?;
    let caller = ctx.scope.map_or(0, |scope| scope.caller);
    let Some(swarm) = weak.upgrade() else {
        return Err("`history://` needs an agent session: this caller has none".to_string());
    };
    crate::swarm::lock(&swarm).history_read(caller, rest)
}

fn local_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let weak = swarm_of(ctx.scope, "local")?;
    let Some(swarm) = weak.upgrade() else {
        return Err("`local://` needs an agent session: this caller has none".to_string());
    };
    crate::swarm::lock(&swarm).local_read(rest)
}

fn local_write(ctx: &mut Ctx, rest: &str, content: &str) -> Result<String, String> {
    let weak = swarm_of(ctx.scope, "local")?;
    let Some(swarm) = weak.upgrade() else {
        return Err("`local://` needs an agent session: this caller has none".to_string());
    };
    crate::swarm::lock(&swarm).local_write(rest, content)
}

fn out_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let weak = swarm_of(ctx.scope, "out")?;
    let Some(swarm) = weak.upgrade() else {
        return Err("`out://` needs an agent session: this caller has none".to_string());
    };
    crate::swarm::lock(&swarm).out_read(rest)
}

/// How long `ssh://` may take, and how much of the file it keeps: a tool result is capped anyway.
const SSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const SSH_BYTES: usize = 1 << 20;

/// The `ssh` invocation one `ssh://<host>/<path>` names.
///
/// The remote command is a single argv element: `ssh` joins its arguments with spaces and the
/// remote login shell parses the result, so the path is single-quoted and cannot become syntax.
pub(crate) fn ssh_args(rest: &str) -> Result<Vec<String>, String> {
    let usage = || "usage: ssh://<host>/<path>".to_string();
    let (host, path) = rest.split_once('/').ok_or_else(usage)?;
    let host_ok = !host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !host_ok || path.is_empty() || path.chars().any(char::is_control) {
        return Err(usage());
    }
    let command = match path.strip_prefix("~/") {
        Some(home) => format!("sh -c 'cat -- \"$HOME/$1\"' sh {}", quote(home)),
        None => format!("sh -c 'cat -- \"$1\"' sh {}", quote(path)),
    };
    Ok(vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "--".to_string(),
        host.to_string(),
        command,
    ])
}

/// `text` as one POSIX single-quoted word: the only character with meaning is `'` itself.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn ssh_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let args = ssh_args(rest)?;
    let Some(scope) = ctx.scope else {
        return Err("`ssh://` needs an agent session: this caller has none".to_string());
    };
    tools::capture_limited("ssh", &args, scope.working_dir, SSH_TIMEOUT, SSH_BYTES)
}

/// The directory a forge command runs in: the one the caller works in, so `gh` and `glab` pick the
/// repository the agent is already editing.
fn forge_dir(ctx: &Ctx<'_>) -> Result<std::path::PathBuf, String> {
    match ctx.scope {
        Some(scope) => Ok(scope.working_dir.to_path_buf()),
        None => Err("this scheme needs an agent session: this caller has none".to_string()),
    }
}

fn issue_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let dir = forge_dir(ctx)?;
    forge::issue(&dir, rest)
}

fn pr_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let dir = forge_dir(ctx)?;
    forge::pr(&dir, rest)
}

fn mr_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let dir = forge_dir(ctx)?;
    forge::mr(&dir, rest)
}

fn overseer_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    match rest {
        "" => Ok("topics: schemes, config, tools, agents".to_string()),
        "schemes" => Ok(report()),
        "config" => {
            let Some(scope) = ctx.scope else {
                return Err("`overseer://config` needs an agent session: this caller has none".to_string());
            };
            Ok(config::describe(scope.config))
        }
        "tools" => {
            let Some(scope) = ctx.scope else {
                return Err("`overseer://tools` needs an agent session: this caller has none".to_string());
            };
            Ok(prompt::tool_summaries(scope.mcp, scope.policy, scope.remaining_depth).join("\n"))
        }
        "agents" => {
            let weak = swarm_of(ctx.scope, "overseer")?;
            let caller = ctx.scope.map_or(0, |scope| scope.caller);
            let Some(swarm) = weak.upgrade() else {
                return Err("`overseer://agents` needs an agent session: this caller has none".to_string());
            };
            let roster = crate::swarm::lock(&swarm).list(caller);
            Ok(format!("{roster}\naddress one with agent://<id>; write it to send an instruction"))
        }
        other => Err(format!("unknown topic {other}; topics are schemes, config, tools, agents")),
    }
}

fn mcp_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(scope) = ctx.scope else {
        return Err("`mcp://` needs an agent session: this caller has none".to_string());
    };
    if rest.is_empty() {
        return Ok(scope.mcp.resource_listing());
    }
    scope.mcp.read_resource(rest)
}

fn proc_read(_ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    if rest.is_empty() {
        return Ok(jobs::list());
    }
    jobs::read(rest)
}

fn proc_write(_ctx: &mut Ctx, rest: &str, content: &str) -> Result<String, String> {
    jobs::write(rest, content)
}

/// The directory the configuration lives in, which `rule://` searches after the repository's own.
fn config_dir(scope: &Scope<'_>) -> std::path::PathBuf {
    config::path(&scope.config.home).parent().map_or_else(|| scope.config.home.clone(), Path::to_path_buf)
}

fn rule_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(scope) = ctx.scope else {
        return Err("`rule://` needs an agent session: this caller has none".to_string());
    };
    if rest.is_empty() {
        return Ok(rules::list(&scope.config.rule_dir, scope.working_dir));
    }
    let (text, cut) = rules::read(&scope.config.rule_dir, scope.working_dir, rest)?;
    Ok(tools::with_note(text, cut))
}

/// The file `vault://` reads and writes: the `.env` beside `config.toml`.
fn vault_path(scope: &Scope<'_>) -> std::path::PathBuf {
    config::path(&scope.config.home).with_file_name(".env")
}

fn vault_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(scope) = ctx.scope else {
        return Err("`vault://` needs an agent session: this caller has none".to_string());
    };
    if rest.is_empty() {
        return Ok(vault::list(&vault_path(scope)));
    }
    vault::read(&vault_path(scope), rest)
}

fn vault_write(ctx: &mut Ctx, rest: &str, content: &str) -> Result<String, String> {
    let Some(scope) = ctx.scope else {
        return Err("`vault://` needs an agent session: this caller has none".to_string());
    };
    vault::write(&vault_path(scope), rest, content)
}

/// Where the project memories live: one directory under the state root.
fn memory_root(scope: &Scope<'_>) -> std::path::PathBuf {
    config::state_dir(&scope.config.home).join("memory")
}

fn memory_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(scope) = ctx.scope else {
        return Err("`memory://` needs an agent session: this caller has none".to_string());
    };
    memory::read(&memory_root(scope), scope.working_dir, rest)
}

fn memory_write(ctx: &mut Ctx, rest: &str, content: &str) -> Result<String, String> {
    let Some(scope) = ctx.scope else {
        return Err("`memory://` needs an agent session: this caller has none".to_string());
    };
    memory::write(&memory_root(scope), scope.working_dir, rest, content)
}

fn out_write(ctx: &mut Ctx, _rest: &str, content: &str) -> Result<String, String> {
    let weak = swarm_of(ctx.scope, "out")?;
    let caller = ctx.scope.map_or(0, |scope| scope.caller);
    let Some(swarm) = weak.upgrade() else {
        return Err("`out://` needs an agent session: this caller has none".to_string());
    };
    let seq = crate::swarm::lock(&swarm).out_write(caller, content);
    Ok(format!("shown to the user as out://{seq}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The toolkit this port follows. A rename here is a decision, not a typo fix.
    const PARITY: &[&str] = &[
        "agent",
        "artifact",
        "attachment",
        "cfg",
        "conflict",
        "history",
        "issue",
        "local",
        "mcp",
        "memory",
        "mr",
        "out",
        "overseer",
        "pr",
        "proc",
        "profile",
        "rule",
        "security",
        "skill",
        "ssh",
        "vault",
        "xd",
    ];

    fn context(registry: &mut conflict::Registry) -> Ctx<'_> {
        Ctx { conflicts: Some(registry), skills: None, profiles: None, scope: None }
    }

    #[test]
    fn the_table_holds_every_scheme_of_the_toolkit() {
        let names: Vec<&str> = SCHEMES.iter().map(|scheme| scheme.name).collect();
        assert_eq!(names, PARITY);
        assert_eq!(format!("{}://", SCHEMES[4].name), conflict::SCHEME, "the registry name and the scheme must agree");
    }

    #[test]
    fn every_scheme_is_advertised() {
        let summary = summary().unwrap();
        for name in PARITY {
            assert!(summary.contains(&format!("`{name}://`")), "{name} is missing from the summary: {summary}");
        }
    }

    #[test]
    fn the_report_says_every_scheme_is_active() {
        let report = report();
        let first = format!("internal URIs: {} active", PARITY.len());
        assert_eq!(report.lines().next(), Some(first.as_str()), "{report}");
        assert!(!report.contains("reserved"), "{report}");
        for name in PARITY {
            assert!(report.contains(&format!("  {name}://  ")), "{name} is missing from /schemes: {report}");
        }
    }

    #[test]
    fn an_unknown_name_is_refused_with_the_full_list() {
        let known: Vec<String> = PARITY.iter().map(|name| format!("`{name}://`")).collect();
        let expected = format!("`nobody://` is not an available scheme: this build resolves {}", known.join(", "));
        let mut registry = conflict::Registry::default();
        assert_eq!(read(&mut context(&mut registry), "nobody://x").unwrap_err(), expected);
        assert_eq!(read(&mut context(&mut registry), "nobody://").unwrap_err(), expected);
    }

    #[test]
    fn an_ssh_target_becomes_one_quoted_remote_command() {
        assert_eq!(
            ssh_args("host/~/x").unwrap(),
            ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "--", "host", "sh -c 'cat -- \"$HOME/$1\"' sh 'x'"]
        );
        assert_eq!(
            ssh_args("host/tmp/a b").unwrap().last().map(String::as_str),
            Some("sh -c 'cat -- \"$1\"' sh 'tmp/a b'"),
            "an absolute path keeps its own form"
        );
        let quoted = ssh_args("host/it's here").unwrap();
        assert_eq!(quoted.last().map(String::as_str), Some("sh -c 'cat -- \"$1\"' sh 'it'\\''s here'"));
        for refused in ["host", "/x", "ho st/x", "host/a\nb"] {
            assert_eq!(ssh_args(refused).unwrap_err(), "usage: ssh://<host>/<path>", "{refused} was accepted");
        }
    }

    #[test]
    fn a_path_without_a_scheme_is_not_a_uri() {
        assert!(is_url("conflict://1"));
        assert!(is_url("Conflict://1"), "RFC 3986 schemes are case-insensitive");
        assert!(is_url("http://example.com"));
        assert!(!is_url("src/main.rs"));
        assert!(!is_url("/tmp/a://b"));
        assert!(!is_url("C:/x"));
    }

    #[test]
    fn a_scheme_name_is_matched_without_regard_to_case() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src.rs");
        let text = "<<<<<<< HEAD\nlet x = 1;\n=======\nlet x = 2;\n>>>>>>> other\n";
        fs::write(&file, text).unwrap();
        let mut registry = conflict::Registry::default();
        registry.reconcile(&file, "src.rs", text);
        let mut ctx = context(&mut registry);
        assert!(read(&mut ctx, "Conflict://1").unwrap().contains("let x = 1;"));
    }

    #[test]
    fn conflict_uris_go_through_the_layer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src.rs");
        let text = "<<<<<<< HEAD\nlet x = 1;\n=======\nlet x = 2;\n>>>>>>> other\n";
        fs::write(&file, text).unwrap();
        let mut registry = conflict::Registry::default();
        registry.reconcile(&file, "src.rs", text);
        let mut ctx = context(&mut registry);
        assert!(read(&mut ctx, "conflict://1").unwrap().contains("let x = 1;"));
        assert_eq!(write(&mut ctx, "conflict://1", "@theirs").unwrap().split(' ').next(), Some("resolved"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "let x = 2;\n");
    }

    #[test]
    fn a_caller_without_a_registry_cannot_use_conflict_ids() {
        let mut ctx = Ctx { conflicts: None, skills: None, profiles: None, scope: None };
        assert_eq!(read(&mut ctx, "conflict://1").unwrap_err(), "conflict ids belong to the agent that read the file");
        assert!(write(&mut ctx, "conflict://1", "@ours").is_err());
    }

    #[test]
    fn a_profile_reads_through_its_scheme_and_nothing_writes_one() {
        let catalogue = profiles::Catalogue::default();
        let mut ctx = Ctx { conflicts: None, skills: None, profiles: Some(&catalogue), scope: None };
        assert!(read(&mut ctx, "profile://orchestrator").unwrap().contains("You own one task from the brief"));
        let error = read(&mut ctx, "profile://ghost").unwrap_err();
        assert!(error.contains("unknown profile ghost"), "{error}");
        let mut blind = Ctx { conflicts: None, skills: None, profiles: None, scope: None };
        assert!(read(&mut blind, "profile://orchestrator").is_err());
        assert!(write(&mut blind, "profile://orchestrator", "x").is_err(), "profiles are read-only");
    }
}
