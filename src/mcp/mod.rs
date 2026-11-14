//! MCP servers, loaded the way Oh my Pi loads them and reachable from a worker as tools.

pub(crate) const PREFIX: &str = "mcp__";

mod auth;
mod client;
mod config;
mod oauth;
mod registry;

pub(crate) use registry::Registry;
