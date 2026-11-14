//! The OAuth dance an MCP server asks for: discover, authorize in the browser, exchange, refresh.

use super::auth::{self, Credential};
use super::config::ServerConfig;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

/// How long the browser has to come back with a code.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest OAuth error body echoed into a failure line.
const MAX_ERROR_BODY: usize = 300;

/// What the server or its authorization server advertises.
#[derive(Debug, Default)]
struct Metadata {
    authorization_url: Option<String>,
    token_url: Option<String>,
    registration_url: Option<String>,
    scopes: Option<String>,
    resource: Option<String>,
}

/// Everything a login needs, once discovery and the config have filled the gaps.
struct Plan {
    authorization_url: String,
    token_url: String,
    registration_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    scopes: Option<String>,
    resource: Option<String>,
    redirect_uri: String,
    callback_path: String,
    prompt: Option<String>,
}

/// Signs in to one server and returns what the token store keeps.
pub(crate) fn login(
    client: &reqwest::blocking::Client,
    config: &ServerConfig,
    challenge: Option<&str>,
    notify: &dyn Fn(&str),
) -> Result<Credential, String> {
    let url = config.url().ok_or_else(|| "this server has no URL to authorize against".to_string())?;
    let mut plan = plan(client, config, url, challenge)?;
    let listener = bind(&plan.redirect_uri)?;
    let port = listener.local_addr().map_err(|e| format!("127.0.0.1: {e}"))?.port();
    plan.redirect_uri = format!("http://127.0.0.1:{port}{}", plan.callback_path);
    if plan.client_id.is_none() {
        let (id, secret) = register(client, &plan)?;
        plan.client_id = Some(id);
        plan.client_secret = plan.client_secret.or(secret);
    }
    let verifier = base64url(random_bytes(32)?);
    let state = hex(&random_bytes(16)?);
    let authorize = authorize_url(&plan, &verifier, &state);
    notify(&authorize);
    // Best effort: the URL is in the notice, so it can be opened by hand anywhere.
    let _ = Command::new("open").arg(&authorize).spawn();
    let code = wait_for_callback(&listener, &plan.callback_path, &state)?;
    exchange(client, url, &plan, &code, &verifier)
}

/// Discovery plus the config's overrides; fails before anything is opened when a piece is missing.
fn plan(
    client: &reqwest::blocking::Client,
    config: &ServerConfig,
    url: &str,
    challenge: Option<&str>,
) -> Result<Plan, String> {
    let mut meta = discover(client, url);
    let oauth = config.oauth.clone().unwrap_or_default();
    let auth = config.auth.clone().unwrap_or_default();
    if meta.token_url.is_none() {
        meta.token_url.clone_from(&auth.token_url);
    }
    if meta.resource.is_none() {
        meta.resource.clone_from(&auth.resource);
    }
    let authorization_url =
        meta.authorization_url.clone().ok_or_else(|| format!("no OAuth metadata advertised at {url}"))?;
    let token_url = meta.token_url.clone().ok_or_else(|| format!("no token endpoint advertised at {url}"))?;
    let (uri_port, uri_path) = port_and_path(oauth.redirect_uri.as_deref().unwrap_or_default());
    let callback_path = uri_path
        .or_else(|| oauth.callback_path.clone())
        .filter(|path| path.starts_with('/'))
        .unwrap_or_else(|| "/callback".to_string());
    let port = uri_port.or(oauth.callback_port).unwrap_or(0);
    Ok(Plan {
        authorization_url,
        token_url,
        registration_url: meta.registration_url.clone(),
        // A `scope=` on the challenge outranks everything discovered: the resource asked for it.
        scopes: challenge.and_then(|value| challenge_parameter(value, "scope")).or(meta.scopes).or(oauth.scope),
        resource: meta.resource.or_else(|| Some(url.to_string())),
        client_id: oauth.client_id.or(auth.client_id),
        client_secret: oauth.client_secret.or(auth.client_secret),
        redirect_uri: format!("http://127.0.0.1:{port}{callback_path}"),
        callback_path,
        prompt: oauth.prompt,
    })
}

/// RFC 9728 for the resource, then RFC 8414 / OIDC for the authorization server; a miss is not fatal.
fn discover(client: &reqwest::blocking::Client, url: &str) -> Metadata {
    let (origin, pathname) = split_url(url);
    let mut meta = Metadata::default();
    let protected = get_json(client, &format!("{origin}/.well-known/oauth-protected-resource{pathname}"));
    let issuer = protected.as_ref().and_then(|doc| {
        doc.get("authorization_servers")
            .and_then(Value::as_array)
            .and_then(|servers| servers.first())
            .and_then(Value::as_str)
            .map(|issuer| split_url(issuer).0)
    });
    if let Some(doc) = &protected {
        meta.scopes = doc.get("scopes_supported").and_then(join_scopes);
        meta.resource = doc.get("resource").and_then(Value::as_str).map(ToString::to_string);
    }
    let base = issuer.unwrap_or_else(|| origin.clone());
    for candidate in
        [format!("{base}/.well-known/oauth-authorization-server"), format!("{base}/.well-known/openid-configuration")]
    {
        let Some(doc) = get_json(client, &candidate) else { continue };
        let endpoint = doc.get("authorization_endpoint").and_then(Value::as_str);
        if endpoint.is_none() {
            continue;
        }
        meta.authorization_url = endpoint.map(ToString::to_string);
        meta.token_url = doc.get("token_endpoint").and_then(Value::as_str).map(ToString::to_string);
        meta.registration_url = doc.get("registration_endpoint").and_then(Value::as_str).map(ToString::to_string);
        meta.scopes = meta.scopes.or_else(|| doc.get("scopes_supported").and_then(join_scopes));
        break;
    }
    meta
}

/// A metadata document, or `None` for a 404 or a body that is not a JSON object.
fn get_json(client: &reqwest::blocking::Client, url: &str) -> Option<Value> {
    let response = client.get(url).header(reqwest::header::ACCEPT, "application/json").send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Value>().ok().filter(Value::is_object)
}

/// `scopes_supported` as one space-separated string.
fn join_scopes(value: &Value) -> Option<String> {
    let scopes: Vec<&str> = value.as_array()?.iter().filter_map(Value::as_str).collect();
    (!scopes.is_empty()).then(|| scopes.join(" "))
}

/// The `key="value"` parameter of a `WWW-Authenticate` challenge.
fn challenge_parameter(challenge: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = challenge.to_ascii_lowercase().find(&needle)?;
    let rest = challenge.get(start + needle.len()..)?;
    let end = rest.find('"')?;
    let value = rest.get(..end)?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Dynamic client registration, for a server whose authorization server offers it.
fn register(client: &reqwest::blocking::Client, plan: &Plan) -> Result<(String, Option<String>), String> {
    let url = plan
        .registration_url
        .as_deref()
        .ok_or_else(|| "no client id is configured and the server offers no registration endpoint".to_string())?;
    let body = json!({
        "client_name": "overseer",
        "redirect_uris": [plan.redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let response = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .map_err(|e| format!("{url}: {e}"))?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status.as_u16(), excerpt(&text)));
    }
    let doc: Value = serde_json::from_str(&text).map_err(|e| format!("{url}: {e}"))?;
    let id = doc
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{url}: no client_id in the registration answer"))?
        .to_string();
    Ok((id, doc.get("client_secret").and_then(Value::as_str).map(ToString::to_string)))
}

/// The URL the browser opens, with the endpoint's own query parameters kept.
fn authorize_url(plan: &Plan, verifier: &str, state: &str) -> String {
    let mut params: Vec<(&str, String)> = vec![
        ("response_type", "code".to_string()),
        ("client_id", plan.client_id.clone().unwrap_or_default()),
        ("redirect_uri", plan.redirect_uri.clone()),
        ("state", state.to_string()),
        ("code_challenge", pkce_challenge(verifier)),
        ("code_challenge_method", "S256".to_string()),
    ];
    if let Some(scopes) = &plan.scopes {
        params.push(("scope", scopes.clone()));
    }
    if let Some(resource) = &plan.resource {
        params.push(("resource", resource.clone()));
    }
    let prompt = plan.prompt.clone().or_else(|| {
        plan.scopes
            .as_deref()
            .filter(|scopes| scopes.split_whitespace().any(|scope| scope == "offline_access"))
            .map(|_| "consent".to_string())
    });
    if let Some(prompt) = prompt {
        params.push(("prompt", prompt));
    }
    let encoded =
        params.iter().map(|(key, value)| format!("{key}={}", percent_encode(value))).collect::<Vec<String>>().join("&");
    let separator = if plan.authorization_url.contains('?') { '&' } else { '?' };
    format!("{}{separator}{encoded}", plan.authorization_url)
}

/// PKCE S256: the base64url SHA-256 of the verifier (RFC 7636 §4.2).
fn pkce_challenge(verifier: &str) -> String {
    base64url(Sha256::digest(verifier.as_bytes()).to_vec())
}

fn base64url(bytes: Vec<u8>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// `len` bytes from the kernel's entropy pool; a `rand` dependency buys nothing more here.
fn random_bytes(len: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0_u8; len];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(bytes)
}

/// Listens on the callback address; a pinned port that is busy fails rather than silently moving,
/// because the provider's registered redirect URI names that port.
fn bind(redirect_uri: &str) -> Result<TcpListener, String> {
    let (port, _) = port_and_path(redirect_uri);
    let port = port.unwrap_or(0);
    let listener =
        TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    listener.set_nonblocking(true).map_err(|e| format!("127.0.0.1:{port}: {e}"))?;
    Ok(listener)
}

/// Waits for the browser's callback, skipping the requests a browser makes on its own.
fn wait_for_callback(listener: &TcpListener, path: &str, state: &str) -> Result<String, String> {
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err(format!("no OAuth callback within {}s", CALLBACK_TIMEOUT.as_secs()));
        }
        let accepted = listener.accept();
        let (mut stream, _) = match accepted {
            Ok(pair) => pair,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(format!("127.0.0.1: {e}")),
        };
        if let Some(outcome) = handle_callback(&mut stream, path, state) {
            return outcome;
        }
    }
}

/// One request on the callback listener: `None` when it is not the callback, so the wait goes on.
fn handle_callback(stream: &mut TcpStream, path: &str, state: &str) -> Option<Result<String, String>> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    BufReader::new(stream.try_clone().ok()?).read_line(&mut line).ok()?;
    let target = line.split_whitespace().nth(1)?.to_string();
    let (request_path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    if request_path != path {
        respond(stream, "404 Not Found", "not the OAuth callback");
        return None;
    }
    let params = parse_query(query);
    if let Some(error) = params.get("error") {
        let detail = params.get("error_description").map_or_else(|| error.clone(), |why| format!("{error}: {why}"));
        respond(stream, "200 OK", "overseer could not sign in; see the terminal");
        return Some(Err(detail));
    }
    if params.get("state").map(String::as_str) != Some(state) {
        respond(stream, "400 Bad Request", "state mismatch");
        return Some(Err("the OAuth callback did not carry the state this login sent".into()));
    }
    if let Some(code) = params.get("code") {
        respond(stream, "200 OK", "overseer is signed in; close this tab");
        Some(Ok(code.clone()))
    } else {
        respond(stream, "400 Bad Request", "no code");
        Some(Err("the OAuth callback carried no code".into()))
    }
}

/// Answers the browser so its tab does not hang; the body is one line of text.
fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let page = format!("<!doctype html><title>overseer</title><p>{body}\n");
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        page.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(page.as_bytes());
    let _ = stream.flush();
}

/// The authorization code becomes an access token, and a refresh token when the server offers one.
fn exchange(
    client: &reqwest::blocking::Client,
    server_url: &str,
    plan: &Plan,
    code: &str,
    verifier: &str,
) -> Result<Credential, String> {
    let mut form: Vec<(&str, String)> = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", plan.redirect_uri.clone()),
        ("client_id", plan.client_id.clone().unwrap_or_default()),
        ("code_verifier", verifier.to_string()),
    ];
    if let Some(secret) = &plan.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    if let Some(resource) = &plan.resource {
        form.push(("resource", resource.clone()));
    }
    let doc = token_request(client, &plan.token_url, &form)?;
    credential_from(&doc, server_url, plan, None)
}

/// Renews an expired or rejected access token.
pub(crate) fn refresh(client: &reqwest::blocking::Client, credential: &Credential) -> Result<Credential, String> {
    let refresh_token = credential
        .refresh_token
        .clone()
        .ok_or_else(|| format!("{}: the stored login has no refresh token", credential.server_url))?;
    let mut form: Vec<(&str, String)> = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.clone()),
        ("client_id", credential.client_id.clone()),
    ];
    if let Some(secret) = &credential.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    if let Some(resource) = &credential.resource {
        form.push(("resource", resource.clone()));
    }
    let doc = token_request(client, &credential.token_url, &form)?;
    let access_token = doc
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "the token answer carried no access_token".to_string())?
        .to_string();
    let mut renewed = credential.clone();
    renewed.access_token = access_token;
    renewed.expires_at = expiry(&doc);
    // A server that does not rotate its refresh token keeps the one that worked.
    if let Some(fresh) = doc.get("refresh_token").and_then(Value::as_str) {
        renewed.refresh_token = Some(fresh.to_string());
    }
    Ok(renewed)
}

/// When an access token stops being usable, with a minute of slack so it cannot expire between the
/// check and the request.
fn expiry(doc: &Value) -> i64 {
    let ttl = doc.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
    auth::now() + ttl - 60
}

/// Reads a token response into a credential; a missing `refresh_token` keeps the previous one.
fn credential_from(
    doc: &Value,
    server_url: &str,
    plan: &Plan,
    previous: Option<&Credential>,
) -> Result<Credential, String> {
    let access_token = doc
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "the token answer carried no access_token".to_string())?
        .to_string();
    let refresh_token = doc
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| previous.and_then(|old| old.refresh_token.clone()));
    Ok(Credential {
        server_url: server_url.to_string(),
        authorization_url: plan.authorization_url.clone(),
        token_url: plan.token_url.clone(),
        client_id: plan.client_id.clone().unwrap_or_default(),
        client_secret: plan.client_secret.clone(),
        access_token,
        refresh_token,
        expires_at: expiry(doc),
        scopes: plan.scopes.clone().or_else(|| previous.and_then(|old| old.scopes.clone())),
        resource: plan.resource.clone(),
    })
}

/// One form-encoded token request.
fn token_request(client: &reqwest::blocking::Client, url: &str, form: &[(&str, String)]) -> Result<Value, String> {
    // Encoded by hand: reqwest's `form` support is a feature this build does not enable, and the
    // encoding here is the same one the authorization URL already needs.
    let body =
        form.iter().map(|(key, value)| format!("{key}={}", percent_encode(value))).collect::<Vec<String>>().join("&");
    let response = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .map_err(|e| format!("{url}: {e}"))?;
    let status = response.status();
    let text = response.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status.as_u16(), excerpt(&text)));
    }
    serde_json::from_str(&text).map_err(|e| format!("{url}: unreadable token answer: {e}"))
}

/// The first `n` bytes of a body, on a character boundary.
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

/// `scheme://authority` and the path after it, without a query or a fragment.
fn split_url(url: &str) -> (String, String) {
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let (authority, tail) = rest.find('/').map_or((rest, ""), |index| rest.split_at(index));
    let path = tail.split(['?', '#']).next().unwrap_or_default();
    let pathname = if path.is_empty() || path == "/" { String::new() } else { path.to_string() };
    (format!("{scheme}://{authority}"), pathname)
}

/// The port and path of a redirect URI, either of which may be absent.
fn port_and_path(uri: &str) -> (Option<u16>, Option<String>) {
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let (authority, path) = rest.find('/').map_or((rest, ""), |index| rest.split_at(index));
    let port = authority.rsplit_once(':').and_then(|(_, port)| port.parse::<u16>().ok());
    let path = (!path.is_empty()).then(|| path.to_string());
    (port, path)
}

/// Query parameters, percent-decoded; `+` is a space, as a browser sends it.
fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(key), percent_decode(value));
    }
    params
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(byte) = bytes.get(index) {
        match byte {
            b'+' => out.push(b' '),
            b'%' => {
                let pair = bytes.get(index + 1..index + 3).and_then(|hex| std::str::from_utf8(hex).ok());
                match pair.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(decoded) => {
                        out.push(decoded);
                        index += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            other => out.push(*other),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(byte)),
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pkce_challenge_matches_the_rfc_vector() {
        // RFC 7636 appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(random_bytes(32).unwrap().len(), 32);
        assert_eq!(base64url(random_bytes(32).unwrap()).len(), 43);
    }

    #[test]
    fn a_query_yields_its_code_state_or_error() {
        let params = parse_query("code=abc%2F1&state=deadbeef");
        assert_eq!(params.get("code").map(String::as_str), Some("abc/1"));
        assert_eq!(params.get("state").map(String::as_str), Some("deadbeef"));
        let error = parse_query("error=access_denied&error_description=no+thanks");
        assert_eq!(error.get("error_description").map(String::as_str), Some("no thanks"));
    }

    #[test]
    fn a_challenge_names_its_scope() {
        let challenge = r#"Bearer error="insufficient_scope", scope="mcp.read mcp.write""#;
        assert_eq!(challenge_parameter(challenge, "scope").as_deref(), Some("mcp.read mcp.write"));
        assert_eq!(challenge_parameter("Bearer error=\"invalid_token\"", "scope"), None);
    }

    #[test]
    fn discovery_urls_follow_a_path_scoped_server_url() {
        assert_eq!(
            split_url("https://host.example/mcp/v1?x=1"),
            ("https://host.example".to_string(), "/mcp/v1".to_string())
        );
        assert_eq!(split_url("https://host.example"), ("https://host.example".to_string(), String::new()));
        assert_eq!(split_url("https://host.example/"), ("https://host.example".to_string(), String::new()));
        assert_eq!(port_and_path("http://127.0.0.1:8765/cb"), (Some(8765), Some("/cb".to_string())));
        assert_eq!(port_and_path("http://127.0.0.1"), (None, None));
    }

    #[test]
    fn the_authorization_url_keeps_the_endpoint_query_and_asks_for_consent() {
        let plan = Plan {
            authorization_url: "https://auth.example/authorize?tenant=a".into(),
            token_url: "https://auth.example/token".into(),
            registration_url: None,
            client_id: Some("client".into()),
            client_secret: None,
            scopes: Some("openid offline_access".into()),
            resource: Some("https://host.example/mcp".into()),
            redirect_uri: "http://127.0.0.1:1234/callback".into(),
            callback_path: "/callback".into(),
            prompt: None,
        };
        let url = authorize_url(&plan, "verifier", "state");
        assert!(url.starts_with("https://auth.example/authorize?tenant=a&"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("prompt=consent"), "{url}");
        assert!(url.contains("resource=https%3A%2F%2Fhost.example%2Fmcp"), "{url}");
    }
}
