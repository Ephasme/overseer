//! The servers one overseer process knows about: connecting, naming their tools, calling them.

use super::PREFIX;
use super::auth::Store;
use super::client::{self, CallError, Client, ConnectError, ToolDef};
use super::config::{self, ServerConfig};
use super::oauth;
use crate::event::UiEvent;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

/// Longest runtime tool name a model sees, before it is truncated and hashed.
const MAX_NAME: usize = 64;

/// What one server is doing, as `/mcp` shows it.
enum Status {
    Connecting,
    Connected,
    OauthRequired { challenge: Option<String> },
    Failed(String),
}

struct Server {
    name: String,
    /// `None` when the file itself could not be read, or the declaration is unusable.
    config: Option<ServerConfig>,
    status: Status,
    /// The live connection, in its own lock so a slow call does not hold the server list.
    client: Arc<Mutex<Option<Client>>>,
    /// Runtime name and definition of each tool, as `tools/list` returned them.
    tools: Vec<(String, ToolDef)>,
    /// What the server said about itself on `initialize`.
    notes: Option<String>,
}

/// The server list and the tool index, under one lock so a name never points at missing tools.
struct State {
    servers: Vec<Server>,
    index: HashMap<String, (usize, String)>,
}

pub(crate) struct Registry {
    state: Mutex<State>,
    store: Store,
    path: PathBuf,
    /// One process group per server, replaced when that server reconnects, so a group whose client
    /// is gone is never signalled and a recycled pgid is never hit.
    groups: Mutex<HashMap<usize, i32>>,
    /// Refreshes that could not be persisted; the next start would refresh again with the same
    /// stale token, so the failure has to be visible.
    warnings: Mutex<Vec<String>>,
    /// For discovery, registration and refresh; each server has its own client for its own calls.
    /// The build error is kept instead of a default client, which would drop the 60s timeout.
    http: Result<reqwest::blocking::Client, String>,
}

impl Registry {
    /// The registry a home directory describes, without connecting anything; tests use it as the
    /// empty case, and `start` builds on it.
    pub(crate) fn load(home: &Path) -> Self {
        let path = config::path(home);
        let servers: Vec<Server> = config::discover(home)
            .into_iter()
            .map(|entry| match entry {
                Ok(config) => Server {
                    name: config.name.clone(),
                    config: Some(config),
                    status: Status::Connecting,
                    client: Arc::new(Mutex::new(None)),
                    tools: Vec::new(),
                    notes: None,
                },
                Err((name, why)) => Server {
                    name,
                    config: None,
                    status: Status::Failed(why),
                    client: Arc::new(Mutex::new(None)),
                    tools: Vec::new(),
                    notes: None,
                },
            })
            .collect();
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("http client: {e}"));
        Self {
            state: Mutex::new(State { servers, index: HashMap::new() }),
            store: Store::load(home),
            path,
            groups: Mutex::new(HashMap::new()),
            warnings: Mutex::new(Vec::new()),
            http,
        }
    }

    /// Reads the server list and connects each server on its own thread: startup never waits.
    pub(crate) fn start(home: &Path, ui: &Sender<UiEvent>) -> Arc<Self> {
        let registry = Arc::new(Self::load(home));
        let total = registry.servers().len();
        if total == 0 {
            let _ = ui.send(UiEvent::Notice(registry.notice()));
            return registry;
        }
        let remaining = Arc::new(AtomicUsize::new(total));
        for index in 0..total {
            let weak = Arc::downgrade(&registry);
            let ui = ui.clone();
            let remaining = Arc::clone(&remaining);
            // A `Weak` on purpose: a server still connecting while overseer quits must not keep it alive.
            thread::spawn(move || {
                if let Some(registry) = weak.upgrade() {
                    registry.connect(index);
                }
                if remaining.fetch_sub(1, Ordering::SeqCst) == 1
                    && let Some(registry) = weak.upgrade()
                {
                    let _ = ui.send(UiEvent::Notice(registry.notice()));
                }
            });
        }
        registry
    }

    fn guard(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn servers(&self) -> Vec<String> {
        self.guard().servers.iter().map(|server| server.name.clone()).collect()
    }

    /// Connects one server and records what came back; runs on that server's own thread.
    fn connect(&self, index: usize) {
        let Some(config) = self.guard().servers.get(index).and_then(|server| server.config.clone()) else { return };
        let token = config.url().and_then(|url| self.token(index, url));
        match client::connect(&config, token) {
            Ok(connected) => self.set_connected(index, connected),
            Err(ConnectError::Unauthorized { challenge }) => {
                self.set_status(index, Status::OauthRequired { challenge });
            }
            Err(ConnectError::Failed(why)) => self.set_status(index, Status::Failed(why)),
        }
    }

    /// The stored access token for a server URL, refreshed first when it has expired.
    fn token(&self, index: usize, url: &str) -> Option<String> {
        let credential = self.store.get(url)?;
        if !credential.expired() {
            return Some(credential.access_token);
        }
        let http = match &self.http {
            Ok(http) => http,
            Err(why) => {
                self.warn(index, why);
                return Some(credential.access_token);
            }
        };
        match oauth::refresh(http, &credential) {
            Ok(renewed) => {
                // The store keeps the new credential in memory even when the write fails, so the
                // token works now; the warning is what says the next start will refresh again.
                if let Err(why) = self.store.put(renewed.clone()) {
                    self.warn(index, &why);
                }
                Some(renewed.access_token)
            }
            // A refresh that fails leaves the old token: the server decides whether it is still good.
            Err(_) => Some(credential.access_token),
        }
    }

    /// Records a problem that did not fail the call, for `/mcp` and the startup notice.
    fn warn(&self, index: usize, why: &str) {
        let Some(name) = self.guard().servers.get(index).map(|server| server.name.clone()) else { return };
        let warning = format!("{name}: {why}");
        let mut warnings = self.warnings.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !warnings.contains(&warning) {
            warnings.push(warning);
        }
    }

    /// Keeps at most one process group per server: the previous one is freed when its client goes.
    fn set_group(&self, index: usize, group: Option<i32>) {
        let mut groups = self.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match group {
            Some(group) => {
                groups.insert(index, group);
            }
            None => {
                groups.remove(&index);
            }
        }
    }

    fn set_status(&self, index: usize, status: Status) {
        let mut state = self.guard();
        if let Some(server) = state.servers.get_mut(index) {
            server.status = status;
        }
    }

    /// Publishes a live connection: its client slot, its tools, its notes and the tool index move
    /// together under the state lock, so no reader sees a client whose tools are not indexed yet.
    ///
    /// Lock order is the state lock, then that server's client, then `groups`; nothing takes the
    /// state lock while holding a client.
    fn set_connected(&self, index: usize, connected: client::Connected) {
        let client::Connected { client, tools, instructions } = connected;
        let group = client.group();
        let mut state = self.guard();
        let Some(server) = state.servers.get(index) else { return };
        let slot = Arc::clone(&server.client);
        let notes = server.config.as_ref().filter(|config| config.instructions).and(instructions);
        let server_name = server.name.clone();
        *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(client);
        // Names another server already holds stay claimed: a collision must not hide one of the two.
        let mut entries: HashMap<String, (usize, String)> = std::mem::take(&mut state.index);
        entries.retain(|_, (owner, _)| *owner != index);
        let mut named: Vec<(String, ToolDef)> = Vec::with_capacity(tools.len());
        for tool in tools {
            let original = tool.name.clone();
            let wanted = runtime_name(&server_name, &original);
            let runtime = if entries.contains_key(&wanted) {
                free_runtime_name(&wanted, &server_name, &original, &entries)
            } else {
                wanted
            };
            entries.insert(runtime.clone(), (index, original));
            named.push((runtime, tool));
        }
        if let Some(server) = state.servers.get_mut(index) {
            server.tools = named;
            server.notes = notes;
            server.status = Status::Connected;
        }
        state.index = entries;
        drop(state);
        self.set_group(index, group);
    }

    /// Every tool a worker may call, in the chat-completions shape.
    pub(crate) fn tool_defs(&self) -> Vec<Value> {
        let state = self.guard();
        let mut defs = Vec::new();
        for server in &state.servers {
            for (runtime, tool) in &server.tools {
                defs.push(json!({"type": "function", "function": {
                    "name": runtime,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                }}));
            }
        }
        defs
    }

    /// Every resource the connected servers offer, one line each; a server without resources is
    /// simply absent, and one whose `resources/list` fails contributes nothing to the listing.
    pub(crate) fn resource_listing(&self) -> String {
        let slots = self.client_slots();
        let mut lines: Vec<String> = Vec::new();
        for (name, slot) in slots {
            let mut guard = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(client) = guard.as_mut() else { continue };
            let Ok(resources) = client.resources() else { continue };
            for resource in resources {
                let mime = resource.mime.as_deref().unwrap_or("no mime");
                lines.push(format!("mcp://{} — {} ({name}, {mime})", resource.uri, resource.name));
            }
        }
        if lines.is_empty() { "no MCP resources".to_string() } else { lines.join("\n") }
    }

    /// One resource's text, or why no server serves it: each server's error is kept, so a missing
    /// resource and a broken server do not read the same.
    pub(crate) fn read_resource(&self, uri: &str) -> Result<String, String> {
        let slots = self.client_slots();
        let mut errors: Vec<String> = Vec::new();
        for (name, slot) in slots {
            let outcome = {
                let mut guard = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                match guard.as_mut() {
                    Some(client) => client
                        .read_resource(uri)
                        .map_err(|e| match e {
                            CallError::Unauthorized => format!("{name}: needs a login (/login {name})"),
                            CallError::Failed(why) => format!("{name}: {why}"),
                        })
                        .map(Some),
                    None => Ok(None),
                }
            };
            match outcome {
                Ok(Some(text)) => return Ok(text),
                Ok(None) => {}
                Err(why) => errors.push(why),
            }
        }
        if errors.is_empty() {
            Err(format!("no MCP server serves {uri}: none is connected"))
        } else {
            Err(format!("no MCP server serves {uri}: {}", errors.join("; ")))
        }
    }

    /// Each server's name and client slot, so a reader locks one connection at a time.
    fn client_slots(&self) -> Vec<(String, Arc<Mutex<Option<Client>>>)> {
        let state = self.guard();
        state.servers.iter().map(|server| (server.name.clone(), Arc::clone(&server.client))).collect()
    }

    /// One entry per server that has tools, for the line the prompt shows instead of the schemas.
    pub(crate) fn tool_counts(&self) -> Vec<(String, usize)> {
        self.guard()
            .servers
            .iter()
            .filter(|server| !server.tools.is_empty())
            .map(|server| (server.name.clone(), server.tools.len()))
            .collect()
    }

    /// What each server said about itself on `initialize`, when its config asked for it.
    pub(crate) fn instructions(&self) -> Vec<(String, String)> {
        self.guard()
            .servers
            .iter()
            .filter_map(|server| server.notes.clone().map(|notes| (server.name.clone(), notes)))
            .collect()
    }

    /// Calls one tool by its runtime name and returns the text the model reads.
    pub(crate) fn call(&self, runtime_name: &str, arguments: &str) -> String {
        let Some((index, tool)) = self.guard().index.get(runtime_name).cloned() else {
            return format!("error: unknown tool {runtime_name}");
        };
        let (name, config, slot) = {
            let state = self.guard();
            let Some(server) = state.servers.get(index) else {
                return format!("error: unknown tool {runtime_name}");
            };
            (server.name.clone(), server.config.clone(), Arc::clone(&server.client))
        };
        let outcome = {
            let mut guard = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match guard.as_mut() {
                Some(client) => client.call(&tool, arguments),
                None => return format!("error: mcp server {name} is not connected"),
            }
        };
        match outcome {
            Ok(text) => text,
            Err(CallError::Failed(why)) => format!("error: mcp server {name}: {why}"),
            // A refresh and a reconnect take seconds: the client lock is already released, so this
            // server's other calls are not queued behind them.
            Err(CallError::Unauthorized) => {
                self.retry_after_refresh(index, &name, config.as_ref(), &slot, &tool, arguments)
            }
        }
    }

    /// One refresh and one retry: a token the server just rejected may simply have expired.
    fn retry_after_refresh(
        &self,
        index: usize,
        name: &str,
        config: Option<&ServerConfig>,
        slot: &Arc<Mutex<Option<Client>>>,
        tool: &str,
        arguments: &str,
    ) -> String {
        let needs_login = || {
            self.set_status(index, Status::OauthRequired { challenge: None });
            format!("error: mcp server {name} needs a login: /login {name}")
        };
        let (Some(config), Some(url)) = (config, config.and_then(ServerConfig::url)) else {
            return needs_login();
        };
        let http = match &self.http {
            Ok(http) => http,
            Err(why) => return format!("error: {why}"),
        };
        let Some(credential) = self.store.get(url) else { return needs_login() };
        let Ok(renewed) = oauth::refresh(http, &credential) else { return needs_login() };
        if let Err(why) = self.store.put(renewed.clone()) {
            self.warn(index, &why);
        }
        let Ok(connected) = client::connect(config, Some(renewed.access_token)) else { return needs_login() };
        // The old client is reaped before the new one takes its slot, so its process is not left
        // as a zombie and its group is not signalled twice.
        {
            let mut guard = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(old) = guard.as_mut() {
                old.close();
            }
            *guard = None;
        }
        // Published before the retry, so a retry that fails does not leave the old connection's
        // tools advertised.
        self.set_connected(index, connected);
        let retry = {
            let mut guard = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.as_mut().map(|client| client.call(tool, arguments))
        };
        match retry {
            Some(Ok(text)) => text,
            Some(Err(CallError::Failed(why))) => format!("error: mcp server {name}: {why}"),
            Some(Err(CallError::Unauthorized)) | None => needs_login(),
        }
    }

    /// `N servers, M tools, K unavailable, /mcp to list`, as the startup notice reads it.
    fn notice(&self) -> String {
        let (servers, tools, unavailable) = count(&self.guard());
        let mut text = format!("mcp: {servers} servers, {tools} tools, {unavailable} unavailable, /mcp to list");
        for warning in self.warnings.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter() {
            let _ = write!(text, "\n{warning}");
        }
        text
    }

    /// `/mcp`: the counts, then one line per server, then the warnings.
    pub(crate) fn summary(&self) -> String {
        let state = self.guard();
        if state.servers.is_empty() {
            return format!("no MCP servers in {}", self.path.display());
        }
        let (servers, tools, unavailable) = count(&state);
        let mut lines = vec![format!("mcp: {servers} servers, {tools} tools, {unavailable} unavailable")];
        for server in &state.servers {
            let kind = server.config.as_ref().map_or("unknown", ServerConfig::kind);
            let status = match &server.status {
                Status::Connecting => "connecting".to_string(),
                Status::Connected => format!("{} tools", server.tools.len()),
                Status::OauthRequired { .. } => format!("oauth required (/login {})", server.name),
                Status::Failed(why) => why.lines().next().unwrap_or_default().to_string(),
            };
            lines.push(format!("- {} {kind} {status}", server.name));
        }
        drop(state);
        for warning in self.warnings.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter() {
            lines.push(format!("! {warning}"));
        }
        lines.join("\n")
    }

    /// `/login <server>`: the browser flow, then a reconnect. Runs on the core thread by design, so
    /// the wait for the callback is visible in the transcript rather than silent.
    pub(crate) fn login(&self, name: &str, notify: &dyn Fn(&str)) -> String {
        let found = {
            let state = self.guard();
            state.servers.iter().enumerate().find(|(_, server)| server.name == name).map(|(index, server)| {
                let challenge = match &server.status {
                    Status::OauthRequired { challenge } => challenge.clone(),
                    _ => None,
                };
                (index, server.config.clone(), challenge)
            })
        };
        let Some((index, config, challenge)) = found else { return format!("unknown MCP server {name}") };
        let Some(config) = config else { return format!("error: mcp server {name} has no usable configuration") };
        let http = match &self.http {
            Ok(http) => http,
            Err(why) => return format!("error: {why}"),
        };
        let credential = match oauth::login(http, &config, challenge.as_deref(), notify) {
            Ok(credential) => credential,
            Err(why) => return format!("error: {why}"),
        };
        if let Err(why) = self.store.put(credential.clone()) {
            return format!("error: {why}");
        }
        // A server that was already connected is re-authorized: its old client is closed and the
        // slot emptied, so a reconnect that fails answers "not connected" instead of a dead pipe.
        let slot = {
            let state = self.guard();
            state.servers.get(index).map(|server| Arc::clone(&server.client))
        };
        if let Some(slot) = &slot {
            let mut guard = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(old) = guard.as_mut() {
                old.close();
            }
            *guard = None;
        }
        self.set_group(index, None);
        match client::connect(&config, Some(credential.access_token)) {
            Ok(connected) => {
                let tools = connected.tools.len();
                self.set_connected(index, connected);
                format!("logged in to {name}: {tools} tools")
            }
            Err(ConnectError::Unauthorized { challenge }) => {
                self.set_status(index, Status::OauthRequired { challenge });
                format!("error: {name} refused the new login")
            }
            Err(ConnectError::Failed(why)) => {
                self.set_status(index, Status::Failed(why.clone()));
                format!("error: {why}")
            }
        }
    }

    /// Kills every stdio server still running; quitting must not leave one behind.
    pub(crate) fn kill_all(&self) {
        let groups: Vec<i32> = self
            .groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .map(|(_, group)| group)
            .collect();
        for group in groups {
            let _ = std::process::Command::new("kill").arg("-9").arg(format!("-{group}")).status();
        }
    }
}

/// Servers, tools and unavailable servers; the startup notice and `/mcp` share one count.
fn count(state: &State) -> (usize, usize, usize) {
    let tools = state.servers.iter().map(|server| server.tools.len()).sum();
    let unavailable = state
        .servers
        .iter()
        .filter(|server| matches!(server.status, Status::Failed(_) | Status::OauthRequired { .. }))
        .count();
    (state.servers.len(), tools, unavailable)
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// The runtime name of one tool: `mcp__<server>_<tool>`, lowercased and sanitised.
///
/// Sanitisation is ASCII-only, so the truncation below can never split a character.
pub(crate) fn runtime_name(server: &str, tool: &str) -> String {
    let server_part = sanitise(server, "server");
    let mut tool_part = sanitise(tool, "tool");
    let prefix = format!("{server_part}_");
    if let Some(stripped) = tool_part.strip_prefix(&prefix) {
        tool_part = stripped.to_string();
    }
    let name = format!("{PREFIX}{server_part}_{tool_part}");
    if name.len() <= MAX_NAME {
        return name;
    }
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let suffix = format!("_{:08x}", hasher.finish() & 0xffff_ffff);
    let head = name.get(..MAX_NAME.saturating_sub(suffix.len())).unwrap_or_default();
    format!("{head}{suffix}")
}

/// A runtime name another tool already holds, made unique with a hash of this tool's own name.
///
/// Truncated to `MAX_NAME` like `runtime_name`, so both stay inside the same limit.
fn free_runtime_name(wanted: &str, server: &str, tool: &str, taken: &HashMap<String, (usize, String)>) -> String {
    let mut hasher = DefaultHasher::new();
    (server, tool).hash(&mut hasher);
    let mut attempt = hasher.finish() & 0xffff_ffff;
    loop {
        let suffix = format!("_{attempt:08x}");
        let budget = MAX_NAME.saturating_sub(suffix.len());
        let head = wanted.get(..wanted.len().min(budget)).unwrap_or_default();
        let candidate = format!("{head}{suffix}");
        if !taken.contains_key(&candidate) {
            return candidate;
        }
        attempt = attempt.wrapping_add(1);
    }
}

/// Lowercase, every character outside `[a-z0-9_]` replaced, runs collapsed, ends trimmed.
fn sanitise(part: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(part.len());
    for ch in part.to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() { fallback.to_string() } else { trimmed.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registry_without_servers_offers_no_resource_and_names_the_uri_it_could_not_read() {
        let home = tempfile::tempdir().unwrap();
        let registry = Registry::load(home.path());
        assert_eq!(registry.resource_listing(), "no MCP resources");
        let refused = registry.read_resource("x").unwrap_err();
        assert_eq!(refused, "no MCP server serves x: none is connected");
    }

    #[test]
    fn a_runtime_name_is_the_server_and_tool_sanitised() {
        assert_eq!(runtime_name("agent-mail-perso", "search_messages"), "mcp__agent_mail_perso_search_messages");
        assert_eq!(runtime_name("fs", "fs_read"), "mcp__fs_read", "a tool already naming its server keeps one prefix");
        assert_eq!(runtime_name("slack", "chat.postMessage"), "mcp__slack_chat_postmessage");
        assert_eq!(runtime_name("__weird__", ""), "mcp__weird_tool");
        let long = runtime_name(&"s".repeat(60), &"t".repeat(60));
        assert_eq!(long.len(), MAX_NAME, "{long}");
        assert!(long.starts_with("mcp__"), "{long}");
        assert_eq!(long, runtime_name(&"s".repeat(60), &"t".repeat(60)), "the hash is stable");
    }

    #[test]
    fn a_taken_runtime_name_gets_a_stable_suffix() {
        let mut taken = HashMap::new();
        taken.insert("mcp__fs_read".to_string(), (0, "read".to_string()));
        let name = free_runtime_name("mcp__fs_read", "fs", "fs_read", &taken);
        assert!(name.starts_with("mcp__fs_read_"), "{name}");
        assert!(name.len() <= MAX_NAME, "{name}");
        assert!(!taken.contains_key(&name), "{name}");
        assert_eq!(name, free_runtime_name("mcp__fs_read", "fs", "fs_read", &taken), "the hash is stable");
    }
}
