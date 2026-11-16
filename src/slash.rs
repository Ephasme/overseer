//! The slash commands: the one table the composer's completion popup reads.
//!
//! `swarm::Event::from_line` stays the only parser. The table is what the popup lists, and its test
//! submits every row through that parser: a command added here and not there, or the other way
//! round, fails the build instead of silently becoming a message to the model.

/// `/resume <id>`: retask an existing agent without a model call.
///
/// The completion popup reads this name too, so `/resume` completes agent ids without the spelling
/// living in two places.
pub(crate) const RESUME: &str = "/resume";

/// `/pipeline <name|list|stop>`: run or manage a Rhai pipeline without a model call.
pub(crate) const PIPELINE: &str = "/pipeline";

/// Most rows the popup shows at once.
pub(crate) const LIMIT: usize = 6;

/// One slash command: what it is called, the argument it takes, and what it does.
pub(crate) struct Command {
    pub(crate) name: &'static str,
    /// The argument the command expects; `None` for one that takes none.
    pub(crate) hint: Option<&'static str>,
    /// One line, as the popup shows it.
    pub(crate) about: &'static str,
}

impl Command {
    /// The name and the argument it takes, as the popup lists it.
    pub(crate) fn label(&self) -> String {
        match self.hint {
            Some(hint) => format!("{} {hint}", self.name),
            None => self.name.to_string(),
        }
    }

    /// What accepting the command writes into the composer: the name, and the space its argument
    /// follows it in.
    pub(crate) fn insertion(&self) -> String {
        match self.hint {
            Some(_) => format!("{} ", self.name),
            None => self.name.to_string(),
        }
    }
}

/// Every slash command `Event::from_line` recognises, in the order the popup lists them.
pub(crate) const COMMANDS: [Command; 6] = [
    Command { name: RESUME, hint: Some("<id>"), about: "retask an agent without a model call" },
    Command { name: "/compact", hint: Some("[id]"), about: "force context compaction" },
    Command { name: PIPELINE, hint: Some("<name|list|stop>"), about: "run or manage a Rhai pipeline" },
    Command { name: "/schemes", hint: None, about: "list the internal URI schemes" },
    Command { name: "/mcp", hint: None, about: "show the MCP servers" },
    Command { name: "/login", hint: Some("<server>"), about: "log in to an MCP server" },
];

/// The commands whose name starts with `token`, in table order, at most [`LIMIT`] rows.
pub(crate) fn matching(token: &str) -> Vec<&'static Command> {
    COMMANDS.iter().filter(|command| command.name.starts_with(token)).take(LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm::Event;

    fn names(commands: Vec<&'static Command>) -> Vec<&'static str> {
        commands.into_iter().map(|command| command.name).collect()
    }

    /// The drift guard: every row of the table is a line the one parser reads as a command, never as
    /// a prompt to the model. A row with an argument is submitted with a sample one that parses
    /// without reaching a server.
    #[test]
    fn every_command_in_the_table_is_one_the_parser_recognises() {
        for command in &COMMANDS {
            let line = match command.hint {
                Some(_) => format!("{} 1", command.name),
                None => command.name.to_string(),
            };
            let parsed = Event::from_line(&line);
            assert!(parsed.is_some(), "{line} is in the table but parses to nothing");
            assert!(
                !matches!(parsed, Some(Event::User(_))),
                "{line} is in the table but reaches the model as a prompt"
            );
        }
    }

    #[test]
    fn the_table_filters_by_what_has_been_typed() {
        let table: Vec<&str> = COMMANDS.iter().map(|command| command.name).collect();
        assert_eq!(names(matching("/")), table, "/ offers every command, in order");
        assert_eq!(names(matching("/res")), ["/resume"]);
        assert_eq!(names(matching("/m")), ["/mcp"]);
        assert!(matching("/nope").is_empty(), "a token that matches nothing offers no row");
    }

    #[test]
    fn accepting_writes_the_name_and_the_space_an_argument_follows_it_in() {
        let resume = matching("/resume");
        assert_eq!(resume.first().map(|command| command.insertion()), Some("/resume ".into()));
        assert_eq!(resume.first().map(|command| command.label()), Some("/resume <id>".into()));
        let schemes = matching("/schemes");
        assert_eq!(schemes.first().map(|command| command.insertion()), Some("/schemes".into()));
        assert_eq!(schemes.first().map(|command| command.label()), Some("/schemes".into()));
    }
}
