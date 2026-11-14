//! MCP servers, loaded the way Oh my Pi loads them and reachable from a worker as tools.

pub(crate) const PREFIX: &str = "mcp__";

/// What a secret field prints as in `Debug`, so an incidental log line cannot leak a credential.
pub(crate) const REDACTED: &str = "<redacted>";

mod auth;
mod client;
mod config;
mod oauth;
mod registry;

pub(crate) use registry::Registry;
