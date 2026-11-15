//! One MCP server: the JSON-RPC handshake, and the two transports this client speaks.

use super::config::{self, ServerConfig, Transport};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{self, Child, ChildStdin, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Protocol this client announces; the same one omp announces.
const PROTOCOL_VERSION: &str = "2025-11-25";

/// Longest error body echoed into a tool result.
const MAX_ERROR_BODY: usize = 500;

/// Most `tools/list` pages followed before the server is called broken.
const MAX_TOOL_PAGES: usize = 64;

/// Most `resources/list` pages followed before the server is called broken.
const MAX_RESOURCE_PAGES: usize = 64;

/// One resource a server offers, as `mcp://` lists it.
pub(crate) struct Resource {
    pub(crate) uri: String,
    pub(crate) name: String,
    pub(crate) mime: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolDef {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
}

/// A server that completed the handshake.
pub(crate) struct Connected {
    pub(crate) client: Client,
    pub(crate) tools: Vec<ToolDef>,
    pub(crate) instructions: Option<String>,
}

#[derive(Debug)]
pub(crate) enum CallError {
    /// The server wants a credential; the registry turns this into a `/login` hint.
    Unauthorized,
    Failed(String),
}

pub(crate) enum ConnectError {
    Unauthorized { challenge: Option<String> },
    Failed(String),
}

pub(crate) enum Client {
    Stdio(Stdio),
    Http(Http),
}

impl Client {
    /// The process group a stdio server runs in, so quitting can kill it.
    pub(crate) fn group(&self) -> Option<i32> {
        match self {
            Client::Stdio(stdio) => stdio.group,
            Client::Http(_) => None,
        }
    }

    pub(crate) fn close(&mut self) {
        match self {
            Client::Stdio(stdio) => stdio.close(),
            Client::Http(_) => {}
        }
    }

    fn request(&self, method: &str, params: &Value) -> Result<Value, CallError> {
        match self {
            Client::Stdio(stdio) => stdio.request(method, params),
            Client::Http(http) => http.request(method, params),
        }
    }

    fn notify(&self, method: &str, params: &Value) -> Result<(), CallError> {
        match self {
            Client::Stdio(stdio) => stdio.notify(method, params),
            Client::Http(http) => http.notify(method, params),
        }
    }

    /// The version the server negotiated, echoed on every later HTTP request.
    fn set_protocol(&self, version: &str) {
        if let Client::Http(http) = self {
            http.set_protocol(version);
        }
    }

    /// Calls one of the server's tools and renders its result as the text the model reads.
    pub(crate) fn call(&mut self, tool: &str, arguments: &str) -> Result<String, CallError> {
        let parsed: Value =
            serde_json::from_str(arguments).map_err(|e| CallError::Failed(format!("invalid arguments: {e}")))?;
        if !parsed.is_object() {
            return Err(CallError::Failed("invalid arguments: expected a JSON object".into()));
        }
        let result = self.request("tools/call", &json!({"name": tool, "arguments": parsed}))?;
        Ok(render(&result))
    }

    /// `resources/list`, following `nextCursor` until the server stops offering one.
    pub(crate) fn resources(&mut self) -> Result<Vec<Resource>, CallError> {
        let mut resources = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen: HashSet<String> = HashSet::new();
        loop {
            let params = cursor.as_ref().map_or_else(|| json!({}), |cursor| json!({"cursor": cursor}));
            let page = self.request("resources/list", &params)?;
            for item in page.get("resources").and_then(Value::as_array).into_iter().flatten() {
                let Some(uri) = item.get("uri").and_then(Value::as_str) else { continue };
                resources.push(Resource {
                    uri: uri.to_string(),
                    name: item.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                    mime: item.get("mimeType").and_then(Value::as_str).map(ToString::to_string),
                });
            }
            cursor = page.get("nextCursor").and_then(Value::as_str).map(ToString::to_string);
            match cursor {
                None => return Ok(resources),
                Some(next) if !seen.insert(next.clone()) => {
                    return Err(CallError::Failed(format!("resources/list repeated cursor {next}")));
                }
                Some(_) if seen.len() > MAX_RESOURCE_PAGES => {
                    return Err(CallError::Failed(format!("resources/list went past {MAX_RESOURCE_PAGES} pages")));
                }
                Some(_) => {}
            }
        }
    }

    /// `resources/read`: its `contents[]` as text, a blob named rather than inlined.
    pub(crate) fn read_resource(&mut self, uri: &str) -> Result<String, CallError> {
        let result = self.request("resources/read", &json!({"uri": uri}))?;
        let mut parts: Vec<String> = Vec::new();
        for item in result.get("contents").and_then(Value::as_array).into_iter().flatten() {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(text.to_string());
            } else if let Some(blob) = item.get("blob").and_then(Value::as_str) {
                let mime = item.get("mimeType").and_then(Value::as_str).unwrap_or("unknown");
                parts.push(format!("[binary: {mime}, {} base64 bytes]", blob.len()));
            } else {
                parts.push(item.to_string());
            }
        }
        Ok(parts.join("\n"))
    }
}

/// `content[]` flattened into text: an image or a resource is named, not inlined.
fn render(result: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in result.get("content").and_then(Value::as_array).into_iter().flatten() {
        let rendered = match item.get("type").and_then(Value::as_str) {
            Some("text") => item.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
            Some("image") => {
                format!("[image: {}]", item.get("mimeType").and_then(Value::as_str).unwrap_or("unknown"))
            }
            Some("resource") => format!(
                "[resource: {}]",
                item.get("resource").and_then(|r| r.get("uri")).and_then(Value::as_str).unwrap_or("unknown")
            ),
            _ => item.to_string(),
        };
        parts.push(rendered);
    }
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        parts.push(structured.to_string());
    }
    let text = parts.join("\n");
    if result.get("isError").and_then(Value::as_bool).unwrap_or(false) { format!("error: {text}") } else { text }
}

/// Connects, handshakes and lists the server's tools.
///
/// `token` is the stored OAuth access token, when the server has one; a server that wants a
/// credential answers `Unauthorized`, which is what sends the user to `/login`.
pub(crate) fn connect(config: &ServerConfig, token: Option<String>) -> Result<Connected, ConnectError> {
    let client = match &config.transport {
        Transport::Stdio { command, args, env, cwd } => {
            Client::Stdio(Stdio::spawn(&config.name, command, args, env, cwd.as_deref(), config.timeout)?)
        }
        Transport::Http { url, headers, legacy_sse } => {
            if *legacy_sse {
                return Err(ConnectError::Failed("legacy sse transport is not supported; use \"http\"".into()));
            }
            Client::Http(Http::new(&config.name, url, headers, token, config.timeout)?)
        }
    };
    let init = client.request("initialize", &initialize_params()).map_err(as_connect_error)?;
    if let Some(version) = init.get("protocolVersion").and_then(Value::as_str) {
        client.set_protocol(version);
    }
    client.notify("notifications/initialized", &json!({})).map_err(as_connect_error)?;
    let tools = list_tools(&client)?;
    let instructions = init.get("instructions").and_then(Value::as_str).map(ToString::to_string);
    Ok(Connected { client, tools, instructions })
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"roots": {"listChanged": false}},
        "clientInfo": {"name": "overseer", "version": crate::version::VERSION},
    })
}

fn as_connect_error(error: CallError) -> ConnectError {
    match error {
        CallError::Unauthorized => ConnectError::Unauthorized { challenge: None },
        CallError::Failed(why) => ConnectError::Failed(why),
    }
}

/// `tools/list`, following `nextCursor` until the server stops offering one.
fn list_tools(client: &Client) -> Result<Vec<ToolDef>, ConnectError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen: HashSet<String> = HashSet::new();
    loop {
        let params = match &cursor {
            Some(cursor) => json!({"cursor": cursor}),
            None => json!({}),
        };
        let page = client.request("tools/list", &params).map_err(as_connect_error)?;
        for tool in page.get("tools").and_then(Value::as_array).into_iter().flatten() {
            let Some(name) = tool.get("name").and_then(Value::as_str) else { continue };
            tools.push(ToolDef {
                name: name.to_string(),
                description: tool.get("description").and_then(Value::as_str).unwrap_or_default().to_string(),
                input_schema: tool.get("inputSchema").cloned().unwrap_or_else(|| json!({"type": "object"})),
            });
        }
        cursor = page.get("nextCursor").and_then(Value::as_str).map(ToString::to_string);
        match cursor {
            None => return Ok(tools),
            Some(next) if !seen.insert(next.clone()) => {
                return Err(ConnectError::Failed(format!("tools/list repeated cursor {next}")));
            }
            Some(_) if seen.len() > MAX_TOOL_PAGES => {
                return Err(ConnectError::Failed(format!("tools/list went past {MAX_TOOL_PAGES} pages")));
            }
            Some(_) => {}
        }
    }
}

/// A JSON-RPC result, or the error it carried.
fn unwrap_result(frame: &Value) -> Result<Value, CallError> {
    if let Some(error) = frame.get("error") {
        let message = error.get("message").and_then(Value::as_str).unwrap_or("no message");
        return Err(CallError::Failed(message.to_string()));
    }
    frame.get("result").cloned().ok_or_else(|| CallError::Failed("no result in the response".into()))
}

/// The first `n` bytes of a body, on a character boundary, for an error line.
fn excerpt(body: &str) -> String {
    if body.len() <= MAX_ERROR_BODY {
        return body.trim().to_string();
    }
    let mut end = MAX_ERROR_BODY;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", body.get(..end).unwrap_or_default().trim())
}

fn lock<T>(cell: &Mutex<T>) -> MutexGuard<'_, T> {
    cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn debug(on: bool, line: &str) {
    if on {
        eprintln!("{line}");
    }
}

/// A server launched as a child process, one JSON object per line on its stdout.
pub(crate) struct Stdio {
    name: String,
    /// Taken by whichever closer runs first: once the child is reaped, its id may name another group.
    group: Option<i32>,
    writer: Option<mpsc::Sender<Line>>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>,
    next_id: AtomicU64,
    timeout: Option<Duration>,
    child: Mutex<Child>,
    reader: Option<JoinHandle<()>>,
}

/// One line for the writer thread; `done` carries the write's result when its sender waits for it.
struct Line {
    text: String,
    done: Option<mpsc::Sender<std::io::Result<()>>>,
}

impl Stdio {
    fn spawn(
        name: &str,
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&Path>,
        timeout: Option<Duration>,
    ) -> Result<Self, ConnectError> {
        let mut built = Command::new(command);
        built.args(args).envs(config::launch_env(env)).stdin(process::Stdio::piped()).stdout(process::Stdio::piped());
        built.stderr(process::Stdio::piped()).process_group(0);
        if let Some(cwd) = config::launch_cwd(cwd) {
            built.current_dir(cwd);
        }
        let mut child = built
            .spawn()
            .map_err(|e| ConnectError::Failed(format!("{command}: {}", crate::config::io_error(command, &e))))?;
        let group = i32::try_from(child.id()).map_err(|_| {
            end(&mut child);
            ConnectError::Failed(format!("{command}: process id does not fit a process group"))
        })?;
        let Some(stdin) = child.stdin.take() else {
            end(&mut child);
            return Err(ConnectError::Failed(format!("{command}: no stdin pipe")));
        };
        let (writer, lines) = mpsc::channel();
        thread::spawn(move || write_lines(stdin, &lines));
        let pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
        let stdout = child.stdout.take();
        let reader = stdout.map(|stdout| {
            let writer = writer.clone();
            let pending = Arc::clone(&pending);
            let name = name.to_string();
            thread::spawn(move || read_frames(&name, stdout, &writer, &pending))
        });
        if let Some(stderr) = child.stderr.take() {
            let debug_on = std::env::var_os(crate::config::DEBUG).is_some();
            let name = name.to_string();
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    debug(debug_on, &format!("[{name}] {line}"));
                }
            });
        }
        Ok(Self {
            name: name.to_string(),
            group: Some(group),
            writer: Some(writer),
            pending,
            next_id: AtomicU64::new(1),
            timeout,
            child: Mutex::new(child),
            reader,
        })
    }

    /// Writes one frame; a server whose stdin closed is a server that died.
    fn send(&self, frame: &Value) -> Result<(), CallError> {
        let mut text = frame.to_string();
        text.push('\n');
        let (done, written) = mpsc::channel();
        let Some(writer) = self.writer.as_ref() else {
            return Err(CallError::Failed(format!("mcp server {} exited", self.name)));
        };
        writer
            .send(Line { text, done: Some(done) })
            .map_err(|_| CallError::Failed(format!("mcp server {} exited", self.name)))?;
        match written.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(CallError::Failed(format!("mcp server {}: {e}", self.name))),
            Err(_) => Err(CallError::Failed(format!("mcp server {} exited", self.name))),
        }
    }

    fn request(&self, method: &str, params: &Value) -> Result<Value, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        lock(&self.pending).insert(id, tx);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.send(&frame) {
            lock(&self.pending).remove(&id);
            return Err(e);
        }
        let answer = self.wait(&rx, id)?;
        unwrap_result(&answer)
    }

    /// Waits for this request's answer, or fails the request once its deadline passes.
    fn wait(&self, rx: &Receiver<Value>, id: u64) -> Result<Value, CallError> {
        let answer = match self.timeout {
            Some(timeout) => rx.recv_timeout(timeout).map_err(|_| {
                CallError::Failed(format!("mcp server {}: timeout after {}s", self.name, timeout.as_secs()))
            }),
            None => rx.recv().map_err(|_| CallError::Failed(format!("mcp server {} exited", self.name))),
        };
        lock(&self.pending).remove(&id);
        answer
    }

    fn notify(&self, method: &str, params: &Value) -> Result<(), CallError> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn close(&mut self) {
        if let Some(group) = self.group.take() {
            let _ = Command::new("kill").arg("-9").arg(format!("-{group}")).status();
        }
        // Dropping the last sender ends the writer thread, which closes the child's stdin.
        drop(self.writer.take());
        let mut child = lock(&self.child);
        let _ = child.wait();
        drop(child);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for Stdio {
    fn drop(&mut self) {
        // Same path as `close`: a dropped client must not leave a zombie or a reader behind.
        self.close();
    }
}

/// Ends a server that was spawned but cannot be kept: its group first, then the child itself.
fn end(child: &mut Child) {
    let _ = Command::new("kill").arg("-9").arg(format!("-{}", child.id())).status();
    let _ = child.kill();
    let _ = child.wait();
}

/// Writes every queued line, so neither the reader thread nor a caller waits on a full pipe.
fn write_lines(mut stdin: ChildStdin, lines: &Receiver<Line>) {
    for line in lines {
        let written = stdin.write_all(line.text.as_bytes()).and_then(|()| stdin.flush());
        let failed = written.is_err();
        if let Some(done) = line.done {
            let _ = done.send(written);
        }
        if failed {
            return;
        }
    }
}

/// Reads one JSON object per line: an answer, a server request to answer, or a notification.
fn read_frames<R: std::io::Read>(
    name: &str,
    stdout: R,
    writer: &mpsc::Sender<Line>,
    pending: &Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>,
) {
    let debug_on = std::env::var_os(crate::config::DEBUG).is_some();
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        debug(debug_on, &format!("[{name}] <- {line}"));
        let Ok(frame) = serde_json::from_str::<Value>(&line) else { continue };
        let id = frame.get("id").and_then(Value::as_u64);
        match (frame.get("method").and_then(Value::as_str), id) {
            // A server-to-client request: `roots` was declared, so it must be answerable.
            (Some(method), Some(id)) => {
                let answer = match method {
                    "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                    "roots/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"roots": []}}),
                    other => json!({"jsonrpc": "2.0", "id": id, "error": {
                        "code": -32601,
                        "message": format!("method not found: {other}"),
                    }}),
                };
                // Queued, never written here: waiting on a full stdin would stop stdout draining.
                let _ = writer.send(Line { text: format!("{answer}\n"), done: None });
            }
            // A notification: logged under `OVERSEER_DEBUG`, otherwise nothing to do.
            (Some(method), None) => debug(debug_on, &format!("[{name}] notification {method}")),
            (None, Some(id)) => {
                if let Some(sender) = lock(pending).remove(&id) {
                    let _ = sender.send(frame);
                }
            }
            (None, None) => {}
        }
    }
    // EOF: every request still waiting will never be answered.
    let waiting: Vec<mpsc::Sender<Value>> = lock(pending).drain().map(|(_, sender)| sender).collect();
    for sender in waiting {
        let _ = sender.send(json!({"error": {"message": format!("mcp server {name} exited")}}));
    }
}

/// A server reached over Streamable HTTP: one POST per frame, answers in JSON or an event stream.
pub(crate) struct Http {
    name: String,
    url: String,
    headers: BTreeMap<String, String>,
    client: reqwest::blocking::Client,
    session: Mutex<Option<String>>,
    protocol: Mutex<Option<String>>,
    token: Option<String>,
    next_id: AtomicU64,
}

impl Http {
    fn new(
        name: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        token: Option<String>,
        timeout: Option<Duration>,
    ) -> Result<Self, ConnectError> {
        let mut builder = reqwest::blocking::Client::builder();
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let client = builder.build().map_err(|e| ConnectError::Failed(format!("{url}: {e}")))?;
        Ok(Self {
            name: name.to_string(),
            url: url.to_string(),
            headers: config::launch_headers(headers),
            client,
            session: Mutex::new(None),
            protocol: Mutex::new(None),
            token,
            next_id: AtomicU64::new(1),
        })
    }

    fn set_protocol(&self, version: &str) {
        *lock(&self.protocol) = Some(version.to_string());
    }

    /// Posts one frame and returns the response, headers read and session captured.
    fn post(&self, frame: &Value) -> Result<reqwest::blocking::Response, CallError> {
        let mut request = self
            .client
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
            .body(frame.to_string());
        if let Some(session) = lock(&self.session).clone() {
            request = request.header("Mcp-Session-Id", session);
        }
        if let Some(version) = lock(&self.protocol).clone() {
            request = request.header("MCP-Protocol-Version", version);
        }
        for (key, value) in &self.headers {
            request = request.header(key.as_str(), value.as_str());
        }
        if let Some(token) = &self.token
            && !self.headers.keys().any(|key| key.eq_ignore_ascii_case("authorization"))
        {
            request = request.bearer_auth(token);
        }
        let response = request.send().map_err(|e| CallError::Failed(format!("{}: {e}", self.url)))?;
        if let Some(session) = response.headers().get("mcp-session-id").and_then(|value| value.to_str().ok()) {
            *lock(&self.session) = Some(session.to_string());
        }
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().unwrap_or_default();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(CallError::Unauthorized);
        }
        Err(CallError::Failed(format!("HTTP {}: {}", status.as_u16(), excerpt(&body))))
    }

    fn request(&self, method: &str, params: &Value) -> Result<Value, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let response = self.post(&frame)?;
        let streamed = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("text/event-stream"));
        if streamed {
            let frame = read_event_stream(response, id)
                .ok_or_else(|| CallError::Failed(format!("{}: no answer on the event stream", self.name)))?;
            return unwrap_result(&frame);
        }
        let body = response.text().map_err(|e| CallError::Failed(format!("{}: {e}", self.name)))?;
        let frame: Value = serde_json::from_str(&body)
            .map_err(|e| CallError::Failed(format!("{}: unreadable response: {e}", self.name)))?;
        // The same contract as `Stdio::request`: callers get the `result`, not the envelope.
        unwrap_result(&frame)
    }

    fn notify(&self, method: &str, params: &Value) -> Result<(), CallError> {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let response = self.post(&frame)?;
        // The body of a notification is not read: a 202 carries nothing, and a server that answers
        // anyway must not be left with an undrained connection.
        let _ = response.bytes();
        Ok(())
    }
}

/// Reads `data:` lines until the frame carrying `id` arrives.
fn read_event_stream<R: std::io::Read>(response: R, id: u64) -> Option<Value> {
    for line in BufReader::new(response).lines().map_while(Result::ok) {
        let Some(payload) = line.strip_prefix("data:") else { continue };
        let Ok(frame) = serde_json::from_str::<Value>(payload.trim()) else { continue };
        if frame.get("id").and_then(Value::as_u64) == Some(id) {
            return Some(frame);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_result_renders_its_content_and_flags_an_error() {
        let result = json!({"content": [
            {"type": "text", "text": "hello"},
            {"type": "image", "mimeType": "image/png", "data": "…"},
            {"type": "resource", "resource": {"uri": "file:///tmp/x"}},
        ]});
        assert_eq!(render(&result), "hello\n[image: image/png]\n[resource: file:///tmp/x]");
        let structured = json!({"content": [], "structuredContent": {"a": 1}});
        assert_eq!(render(&structured), "{\"a\":1}");
        let failed = json!({"content": [{"type": "text", "text": "boom"}], "isError": true});
        assert_eq!(render(&failed), "error: boom");
    }

    #[test]
    fn an_error_frame_becomes_a_failed_call() {
        let frame = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "nope"}});
        assert!(matches!(unwrap_result(&frame), Err(CallError::Failed(why)) if why == "nope"));
        let ok = json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}});
        assert!(unwrap_result(&ok).is_ok());
    }

    #[test]
    fn the_handshake_announces_the_version_and_no_roots() {
        let params = initialize_params();
        assert_eq!(params.get("protocolVersion").and_then(Value::as_str), Some(PROTOCOL_VERSION));
        assert_eq!(params.get("capabilities"), Some(&json!({"roots": {"listChanged": false}})));
    }
}
