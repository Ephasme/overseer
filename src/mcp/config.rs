//! `~/.overseer/agent/mcp.json`, in the shape Oh my Pi writes, and what it takes to launch a server.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Longest a `!command` header or environment value may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a failed `!command` is remembered before it is tried again.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// Default client timeout for one request; omp's own default.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the server definitions live, beside omp's own file rather than in XDG's config directory.
pub(crate) fn path(home: &Path) -> PathBuf {
    home.join(".overseer/agent/mcp.json")
}

/// One server's OAuth settings, as the file declares them.
#[derive(Debug, Clone, Default)]
pub(crate) struct OAuthConfig {
    pub(crate) client_id: Option<String>,
    pub(crate) client_secret: Option<String>,
    pub(crate) scope: Option<String>,
    pub(crate) redirect_uri: Option<String>,
    pub(crate) callback_port: Option<u16>,
    pub(crate) callback_path: Option<String>,
    pub(crate) prompt: Option<String>,
}

/// One server's static token settings; `credentialId` is accepted and ignored, because credentials
/// are keyed by the server URL instead.
#[derive(Debug, Clone, Default)]
pub(crate) struct AuthConfig {
    pub(crate) token_url: Option<String>,
    pub(crate) client_id: Option<String>,
    pub(crate) client_secret: Option<String>,
    pub(crate) resource: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        /// Declared as `sse`: a transport this client does not speak.
        legacy_sse: bool,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ServerConfig {
    pub(crate) name: String,
    pub(crate) transport: Transport,
    /// `None` means wait forever, which is what a configured `timeout: 0` asks for.
    pub(crate) timeout: Option<Duration>,
    /// Whether the server's `initialize` instructions reach the worker's prompt.
    pub(crate) instructions: bool,
    pub(crate) oauth: Option<OAuthConfig>,
    pub(crate) auth: Option<AuthConfig>,
}

impl ServerConfig {
    /// `stdio` or `http`, as the status list names it.
    pub(crate) fn kind(&self) -> &'static str {
        match self.transport {
            Transport::Stdio { .. } => "stdio",
            Transport::Http { .. } => "http",
        }
    }

    /// The server's own URL, which keys its stored credential.
    pub(crate) fn url(&self) -> Option<&str> {
        match &self.transport {
            Transport::Http { url, .. } => Some(url),
            Transport::Stdio { .. } => None,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawFile {
    #[serde(rename = "mcpServers")]
    servers: BTreeMap<String, RawServer>,
    #[serde(rename = "disabledServers")]
    disabled: Vec<String>,
    #[serde(rename = "enabledServers")]
    force_enabled: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawServer {
    enabled: Option<bool>,
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    headers: BTreeMap<String, String>,
    timeout: Option<u64>,
    instructions: Option<bool>,
    oauth: Option<RawOAuth>,
    auth: Option<RawAuth>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawOAuth {
    client_id: Option<String>,
    client_secret: Option<String>,
    scope: Option<String>,
    redirect_uri: Option<String>,
    callback_port: Option<u16>,
    callback_path: Option<String>,
    prompt: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawAuth {
    // `type` and `credentialId` are accepted and ignored: credentials are keyed by server URL.
    credential_id: Option<String>,
    token_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    resource: Option<String>,
}

/// Every server the file declares: `Ok` for one to connect, `Err` for a name and why it cannot be.
pub(crate) type Discovery = Vec<Result<ServerConfig, (String, String)>>;

/// Reads the server list; a missing file is an empty list, an unreadable one is one failure.
pub(crate) fn discover(home: &Path) -> Discovery {
    let file = path(home);
    let Ok(text) = fs::read_to_string(&file) else { return Vec::new() };
    let raw: RawFile = match serde_json::from_str(&text) {
        Ok(raw) => raw,
        Err(e) => return vec![Err(("mcp.json".to_string(), format!("{}: {e}", file.display())))],
    };
    raw.servers.into_iter().map(|(name, server)| build(name, server, &raw.disabled, &raw.force_enabled)).collect()
}

fn build(
    name: String,
    raw: RawServer,
    disabled: &[String],
    forced: &[String],
) -> Result<ServerConfig, (String, String)> {
    let fail = |why: String| Err((name.clone(), why));
    if disabled.contains(&name) {
        return fail("listed in disabledServers".into());
    }
    let enabled = forced.contains(&name) || raw.enabled.unwrap_or(true);
    if !enabled {
        return fail("enabled is false".into());
    }
    let transport = match transport(&raw) {
        Ok(transport) => transport,
        Err(why) => return fail(why),
    };
    let timeout = match raw.timeout {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(DEFAULT_TIMEOUT),
    };
    Ok(ServerConfig {
        name,
        transport,
        timeout,
        instructions: raw.instructions.unwrap_or(true),
        oauth: raw.oauth.map(|oauth| OAuthConfig {
            client_id: oauth.client_id.as_deref().map(expand),
            client_secret: oauth.client_secret.as_deref().map(expand),
            scope: oauth.scope.as_deref().map(expand),
            redirect_uri: oauth.redirect_uri.as_deref().map(expand),
            callback_port: oauth.callback_port,
            callback_path: oauth.callback_path.as_deref().map(expand),
            prompt: oauth.prompt.as_deref().map(expand),
        }),
        auth: raw.auth.map(|auth| AuthConfig {
            token_url: auth.token_url.as_deref().map(expand),
            client_id: auth.client_id.as_deref().map(expand),
            client_secret: auth.client_secret.as_deref().map(expand),
            resource: auth.resource.as_deref().map(expand),
        }),
    })
}

/// The transport a server declares, or why its declaration is unusable.
fn transport(raw: &RawServer) -> Result<Transport, String> {
    let stdio = || Transport::Stdio {
        command: raw.command.as_deref().map_or_default(expand),
        args: raw.args.iter().map(|arg| expand(arg)).collect(),
        env: raw.env.iter().map(|(key, value)| (key.clone(), expand(value))).collect(),
        cwd: raw.cwd.as_deref().map(expand).map(PathBuf::from),
    };
    let http = |legacy_sse: bool| Transport::Http {
        url: raw.url.as_deref().map_or_default(expand),
        headers: raw.headers.iter().map(|(key, value)| (key.clone(), expand(value))).collect(),
        legacy_sse,
    };
    match raw.kind.as_deref() {
        Some("stdio") => {
            if raw.command.as_deref().is_none_or(|command| command.trim().is_empty()) {
                return Err("type is \"stdio\" but no command is set".into());
            }
            Ok(stdio())
        }
        Some("http" | "sse") => {
            if raw.url.as_deref().is_none_or(|url| url.trim().is_empty()) {
                return Err(format!("type is \"{}\" but no url is set", raw.kind.as_deref().unwrap_or_default()));
            }
            Ok(http(raw.kind.as_deref() == Some("sse")))
        }
        Some(other) => Err(format!("unknown type \"{other}\"; use \"stdio\", \"http\" or \"sse\"")),
        None => {
            if raw.url.as_deref().is_some_and(|url| !url.trim().is_empty()) {
                return Ok(http(false));
            }
            if raw.command.as_deref().is_some_and(|command| !command.trim().is_empty()) {
                return Ok(stdio());
            }
            Err("neither command nor url is set".into())
        }
    }
}

/// `${VAR}` and `${VAR:-default}`, over every string a server declares.
///
/// An unset variable with no default stays literal, so a typo shows up in the request rather than
/// becoming an empty header nobody can explain.
pub(crate) fn expand(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let Some(head) = rest.get(..start) else { break };
        out.push_str(head);
        let after = rest.get(start + 2..).unwrap_or_default();
        let Some(end) = after.find('}') else { break };
        let inner = after.get(..end).unwrap_or_default();
        let (name, default) = inner.split_once(":-").map_or((inner, None), |(name, value)| (name, Some(value)));
        match env_value(name) {
            Some(value) => out.push_str(&value),
            None => {
                if let Some(default) = default {
                    out.push_str(default);
                } else {
                    out.push_str("${");
                    out.push_str(inner);
                    out.push('}');
                }
            }
        }
        rest = after.get(end + 1..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

/// A set, non-empty environment variable: an empty one is indistinguishable from an unset one.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[derive(Clone)]
struct Cached {
    at: Instant,
    value: Option<String>,
}

/// `!command` results, so a secret is fetched once per process rather than per launch.
static CACHE: LazyLock<Mutex<HashMap<String, Cached>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// What one `env` entry or header is worth at launch, or `None` when it has no value.
///
/// A leading `!` runs the rest as a shell command, which is how a secret reaches a server without
/// being written in the file: the command's stdout is the value.
pub(crate) fn resolve_value(value: &str) -> Option<String> {
    let Some(command) = value.strip_prefix('!') else {
        return Some(env_value(value).unwrap_or_else(|| value.to_string()));
    };
    let mut known = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = known.get(command) {
        // A value that worked is kept for the process's lifetime; a failure is retried, but not
        // on every launch, or a broken secret would stall each server start.
        if entry.value.is_some() || entry.at.elapsed() < RETRY_AFTER {
            return entry.value.clone();
        }
    }
    let value = run_command(command);
    known.insert(command.to_string(), Cached { at: Instant::now(), value: value.clone() });
    value
}

/// Runs one `!command`, bounded, and keeps its trimmed stdout when it succeeded.
fn run_command(command: &str) -> Option<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let group = i32::try_from(child.id()).ok()?;
    let stdout = child.stdout.take();
    let reader = thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(mut pipe) = stdout {
            let _ = pipe.read_to_end(&mut out);
        }
        out
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => {
                kill_group(group);
                let _ = child.wait();
                return None;
            }
        }
        if Instant::now() >= deadline {
            kill_group(group);
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let out = reader.join().ok()?;
    if !status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Kills a command's whole process group, so a child of `sh` cannot hold the pipe open.
fn kill_group(group: i32) {
    let _ = Command::new("kill").arg("-9").arg(format!("-{group}")).status();
}

/// The environment a stdio server starts with: overseer's own, plus every entry that resolved.
pub(crate) fn launch_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.iter().filter_map(|(key, value)| resolve_value(value).map(|value| (key.clone(), value))).collect()
}

/// The headers an http server sends: those that resolved; an entry with no value is left out.
pub(crate) fn launch_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    launch_env(headers)
}

/// Path a stdio server starts in: a relative `cwd` resolves against overseer's own.
pub(crate) fn launch_cwd(cwd: Option<&Path>) -> Option<PathBuf> {
    let cwd = cwd?;
    if cwd.is_absolute() {
        return Some(cwd.to_path_buf());
    }
    let here = std::env::current_dir().ok()?;
    Some(here.join(cwd))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(json: &str) -> Discovery {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".overseer/agent")).unwrap();
        fs::write(path(dir.path()), json).unwrap();
        discover(dir.path())
    }

    fn only(json: &str) -> ServerConfig {
        let found = server(json);
        let mut found = found.into_iter();
        found.next().and_then(Result::ok).expect("one usable server")
    }

    #[test]
    fn variables_expand_with_a_default_and_stay_literal_when_unset() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand("${HOME}/x"), format!("{home}/x"));
        assert_eq!(expand("${OVERSEER_TEST_UNSET:-fallback}"), "fallback");
        assert_eq!(expand("${OVERSEER_TEST_UNSET}"), "${OVERSEER_TEST_UNSET}");
        assert_eq!(expand("no variables"), "no variables");
    }

    #[test]
    fn enabled_false_and_disabled_servers_are_skipped_but_enabled_servers_forces_one_back() {
        let found = server(
            r#"{"mcpServers":{
                "off":{"command":"true","enabled":false},
                "forced":{"command":"true","enabled":false},
                "banned":{"command":"true"},
                "live":{"command":"true"}
            },"disabledServers":["banned"],"enabledServers":["forced"]}"#,
        );
        let mut live: Vec<&str> =
            found.iter().filter_map(|entry| entry.as_ref().ok()).map(|c| c.name.as_str()).collect();
        live.sort_unstable();
        assert_eq!(live, ["forced", "live"]);
        let mut skipped: Vec<&str> =
            found.iter().filter_map(|entry| entry.as_ref().err()).map(|(name, _)| name.as_str()).collect();
        skipped.sort_unstable();
        assert_eq!(skipped, ["banned", "off"]);
    }

    #[test]
    fn disabled_servers_beats_enabled_servers() {
        let found = server(
            r#"{"mcpServers":{"both":{"command":"true"}},"disabledServers":["both"],"enabledServers":["both"]}"#,
        );
        assert!(found.first().is_some_and(Result::is_err));
    }

    #[test]
    fn the_transport_is_inferred_from_what_the_server_sets() {
        assert!(matches!(only(r#"{"mcpServers":{"s":{"command":"npx"}}}"#).transport, Transport::Stdio { .. }));
        assert!(matches!(only(r#"{"mcpServers":{"s":{"url":"https://x/mcp"}}}"#).transport, Transport::Http { .. }));
        assert!(matches!(
            only(r#"{"mcpServers":{"s":{"type":"sse","url":"https://x/sse"}}}"#).transport,
            Transport::Http { legacy_sse: true, .. }
        ));
        let neither = server(r#"{"mcpServers":{"s":{"type":"stdio"}}}"#);
        assert!(neither.first().is_some_and(Result::is_err), "a stdio server needs a command");
        let unknown = server(r#"{"mcpServers":{"s":{"type":"carrier-pigeon","url":"x"}}}"#);
        let (_, why) = unknown.first().and_then(|entry| entry.as_ref().err()).expect("an error");
        assert!(why.contains("unknown type"), "{why}");
        let empty = server(r#"{"mcpServers":{"s":{}}}"#);
        assert!(empty.first().is_some_and(Result::is_err), "a server with neither field is not silently dropped");
    }

    #[test]
    fn an_unparsable_file_is_one_failure_not_a_crash() {
        let found = server("{not json");
        let (name, why) = found.first().and_then(|entry| entry.as_ref().err()).expect("an error");
        assert_eq!(name, "mcp.json");
        assert!(why.contains("mcp.json"), "{why}");
        let missing = tempfile::tempdir().unwrap();
        assert!(discover(missing.path()).is_empty(), "a missing file is no server at all");
    }

    #[test]
    fn a_bang_command_supplies_the_value_and_a_failing_one_supplies_nothing() {
        assert_eq!(resolve_value("!printf 'Bearer test'").as_deref(), Some("Bearer test"));
        assert_eq!(resolve_value("!exit 1"), None);
        assert_eq!(resolve_value("!printf '   '"), None);
        assert_eq!(resolve_value("literal-value").as_deref(), Some("literal-value"));
        assert_eq!(resolve_value("PATH"), env_value("PATH"), "a variable name takes that variable's value");
    }

    #[test]
    fn the_timeout_defaults_to_thirty_seconds_and_zero_waits_forever() {
        assert_eq!(only(r#"{"mcpServers":{"s":{"command":"true"}}}"#).timeout, Some(DEFAULT_TIMEOUT));
        assert_eq!(only(r#"{"mcpServers":{"s":{"command":"true","timeout":0}}}"#).timeout, None);
        assert_eq!(
            only(r#"{"mcpServers":{"s":{"command":"true","timeout":1500}}}"#).timeout,
            Some(Duration::from_millis(1500))
        );
    }

    #[test]
    fn a_server_that_resolves_to_nothing_is_left_out_of_the_launch() {
        let mut env = BTreeMap::new();
        env.insert("GOOD".to_string(), "!printf ok".to_string());
        env.insert("BAD".to_string(), "!exit 1".to_string());
        let launched = launch_env(&env);
        assert_eq!(launched.get("GOOD").map(String::as_str), Some("ok"));
        assert!(!launched.contains_key("BAD"), "an entry with no value is omitted, never sent empty");
    }
}
