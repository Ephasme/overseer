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
    groups: Mutex<Vec<i32>>,
    /// For discovery, registration and refresh; each server has its own client for its own calls.
    http: reqwest::blocking::Client,
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
        let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(60)).build().unwrap_or_default();
        Self {
            state: Mutex::new(State { servers, index: HashMap::new() }),
            store: Store::load(home),
            path,
            groups: Mutex::new(Vec::new()),
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
        let token = config.url().and_then(|url| self.token(url));
        match client::connect(&config, token) {
            Ok(connected) => {
                self.record_group(&connected.client);
                self.set_connected(index, connected);
            }
            Err(ConnectError::Unauthorized { challenge }) => {
                self.set_status(index, Status::OauthRequired { challenge });
            }
            Err(ConnectError::Failed(why)) => self.set_status(index, Status::Failed(why)),
        }
    }

    /// The stored access token for a server URL, refreshed first when it has expired.
    fn token(&self, url: &str) -> Option<String> {
        let credential = self.store.get(url)?;
        if !credential.expired() {
            return Some(credential.access_token);
        }
        match oauth::refresh(&self.http, &credential) {
            Ok(renewed) => {
                let _ = self.store.put(renewed.clone());
                Some(renewed.access_token)
            }
            // A refresh that fails leaves the old token: the server decides whether it is still good.
            Err(_) => Some(credential.access_token),
        }
    }

    fn record_group(&self, client: &Client) {
        if let Some(group) = client.group() {
            self.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(group);
        }
    }

    fn set_status(&self, index: usize, status: Status) {
        let mut state = self.guard();
        if let Some(server) = state.servers.get_mut(index) {
            server.status = status;
        }
    }

    /// Records a live connection: its tools under their runtime names, its notes, its status.
    fn set_connected(&self, index: usize, connected: client::Connected) {
        let client::Connected { client, tools, instructions } = connected;
        {
            let state = self.guard();
            let Some(server) = state.servers.get(index) else { return };
            let slot = Arc::clone(&server.client);
            drop(state);
            *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(client);
        }
        self.record_tools(index, tools, instructions);
    }

    /// Indexes a server's tools under their runtime names; the client itself is already stored.
    fn record_tools(&self, index: usize, tools: Vec<ToolDef>, instructions: Option<String>) {
        let mut state = self.guard();
        let Some(server) = state.servers.get_mut(index) else { return };
        let notes = server.config.as_ref().filter(|config| config.instructions).and(instructions);
        let mut named: Vec<(String, ToolDef)> = Vec::with_capacity(tools.len());
        let mut additions: Vec<(String, (usize, String))> = Vec::with_capacity(tools.len());
        for tool in tools {
            let runtime = runtime_name(&server.name, &tool.name);
            additions.push((runtime.clone(), (index, tool.name.clone())));
            named.push((runtime, tool));
        }
        server.tools = named;
        server.notes = notes;
        server.status = Status::Connected;
        state.index.retain(|_, (owner, _)| *owner != index);
        for (runtime, target) in additions {
            state.index.insert(runtime, target);
        }
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
        let mut guard = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(client) = guard.as_mut() else {
            return format!("error: mcp server {name} is not connected");
        };
        let first = client.call(&tool, arguments);
        match first {
            Ok(text) => text,
            Err(CallError::Failed(why)) => format!("error: mcp server {name}: {why}"),
            Err(CallError::Unauthorized) => {
                self.retry_after_refresh(index, &name, config.as_ref(), &mut guard, &tool, arguments)
            }
        }
    }

    /// One refresh and one retry: a token the server just rejected may simply have expired.
    fn retry_after_refresh(
        &self,
        index: usize,
        name: &str,
        config: Option<&ServerConfig>,
        slot: &mut Option<Client>,
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
        let Some(credential) = self.store.get(url) else { return needs_login() };
        let Ok(renewed) = oauth::refresh(&self.http, &credential) else { return needs_login() };
        let _ = self.store.put(renewed.clone());
        let Ok(connected) = client::connect(config, Some(renewed.access_token)) else { return needs_login() };
        self.record_group(&connected.client);
        if let Some(old) = slot.as_mut() {
            old.close();
        }
        *slot = Some(connected.client);
        let retry = match slot.as_mut() {
            Some(client) => client.call(tool, arguments),
            None => return needs_login(),
        };
        match retry {
            Ok(text) => {
                self.record_tools(index, connected.tools, connected.instructions);
                text
            }
            Err(CallError::Unauthorized) => needs_login(),
            Err(CallError::Failed(why)) => format!("error: mcp server {name}: {why}"),
        }
    }

    /// `N servers, M tools, K unavailable, /mcp to list`, as the startup notice reads it.
    fn notice(&self) -> String {
        let (servers, tools, unavailable) = self.counts();
        format!("mcp: {servers} servers, {tools} tools, {unavailable} unavailable, /mcp to list")
    }

    fn counts(&self) -> (usize, usize, usize) {
        let state = self.guard();
        let tools = state.servers.iter().map(|server| server.tools.len()).sum();
        let unavailable = state
            .servers
            .iter()
            .filter(|server| matches!(server.status, Status::Failed(_) | Status::OauthRequired { .. }))
            .count();
        (state.servers.len(), tools, unavailable)
    }

    /// `/mcp`: the counts, then one line per server.
    pub(crate) fn summary(&self) -> String {
        let state = self.guard();
        if state.servers.is_empty() {
            return format!("no MCP servers in {}", self.path.display());
        }
        let tools: usize = state.servers.iter().map(|server| server.tools.len()).sum();
        let unavailable = state
            .servers
            .iter()
            .filter(|server| matches!(server.status, Status::Failed(_) | Status::OauthRequired { .. }))
            .count();
        let mut lines = vec![format!("mcp: {} servers, {tools} tools, {unavailable} unavailable", state.servers.len())];
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
        let credential = match oauth::login(&self.http, &config, challenge.as_deref(), notify) {
            Ok(credential) => credential,
            Err(why) => return format!("error: {why}"),
        };
        if let Err(why) = self.store.put(credential.clone()) {
            return format!("error: {why}");
        }
        // A server that was already connected is re-authorized: its old client is not reused.
        let slot = {
            let state = self.guard();
            state.servers.get(index).map(|server| Arc::clone(&server.client))
        };
        if let Some(slot) = &slot
            && let Some(old) = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_mut()
        {
            old.close();
        }
        match client::connect(&config, Some(credential.access_token)) {
            Ok(connected) => {
                let count = connected.tools.len();
                self.record_group(&connected.client);
                self.set_connected(index, connected);
                format!("logged in to {name}: {count} tools")
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
        let groups: Vec<i32> =
            self.groups.lock().unwrap_or_else(std::sync::PoisonError::into_inner).drain(..).collect();
        for group in groups {
            let _ = std::process::Command::new("kill").arg("-9").arg(format!("-{group}")).status();
        }
    }
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
}
