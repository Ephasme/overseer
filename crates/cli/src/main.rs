mod config;

use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use config::Config;
use overseer_core::agent::{Agent, AgentConfig, AgentEvent, EventSink, default_system_prompt};
use overseer_core::llm::{Client, ClientConfig};
use overseer_core::tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

/// Prints a turn as it happens: text inline, one line per tool call and result.
struct Printer;

impl EventSink for Printer {
    fn emit(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(text) => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolCallDelta {
                name: Some(name),
                arguments,
                ..
            } => println!("\n→ {name} {arguments}"),
            AgentEvent::ToolResult { ok, output, .. } => {
                let status = if ok { "ok" } else { "failed" };
                let first = output.lines().next().unwrap_or_default();
                println!("← {status}: {first}");
            }
            AgentEvent::AssistantMessage { .. } => println!(),
            AgentEvent::Error { message } => eprintln!("error: {message}"),
            AgentEvent::TurnEnd { reason } => println!("[turn end: {reason:?}]"),
            AgentEvent::UserMessage { .. }
            | AgentEvent::ReasoningDelta(_)
            | AgentEvent::ToolCallDelta { name: None, .. }
            | AgentEvent::ToolProgress { .. } => {}
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let path = Config::default_path();
    let config = Config::load(&path)?;
    let cwd = std::env::current_dir().context("cannot read the working directory")?;
    let mut client_config = ClientConfig::new(config.base_url, config.model);
    client_config.api_key = Config::api_key();
    client_config.send_reasoning = config.send_reasoning;
    let client = Client::new(client_config)?;
    let tools = ToolRegistry::new(&cwd);
    let mut agent_config = AgentConfig::new(default_system_prompt(&cwd));
    if let Some(max_steps) = config.max_steps {
        agent_config.max_steps = max_steps.max(1);
    }
    let mut agent = Agent::new(client, tools, agent_config);
    let cancel = CancellationToken::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.context("cannot read stdin")?;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text == "exit" || text == "quit" {
            break;
        }
        agent.run_turn(text, &mut Printer, &cancel).await;
    }
    Ok(())
}
