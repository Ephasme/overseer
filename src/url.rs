//! Internal URIs: a tool path of the form `scheme://…` names something other than a file.
//!
//! One table decides what exists. A scheme with handlers is advertised everywhere a model looks —
//! tool descriptions, `/schemes` — and a reserved name is refused with the same words as a name
//! nobody ever wrote, so a model cannot learn about a capability this build does not have.

use crate::conflict;
use crate::skills;
use std::fmt::Write as _;

/// What a handler is given besides its own path: the state of the caller that asked.
pub(crate) struct Ctx<'a> {
    /// The caller's conflict registry; `None` for the orchestrator, which owns no files.
    pub(crate) conflicts: Option<&'a mut conflict::Registry>,
    /// The skills the caller may read; `None` for a caller that has no catalogue, such as a write.
    pub(crate) skills: Option<&'a skills::Catalogue>,
}

/// A handler answers one `scheme://…` path: `(caller, rest of the URI)` or `(caller, rest, content)`.
type Read = fn(&mut Ctx<'_>, &str) -> Result<String, String>;
type Write = fn(&mut Ctx<'_>, &str, &str) -> Result<String, String>;

/// What an active scheme does. `summary` is the one line models and the user read.
struct Active {
    summary: &'static str,
    read: Option<Read>,
    write: Option<Write>,
}

enum Status {
    Active(Active),
    /// A name held for a scheme whose handlers are not written: refused like an unknown name.
    Reserved,
}

struct Scheme {
    name: &'static str,
    status: Status,
}

/// Every scheme of the toolkit this port follows, in its order. `conflict` and `skill` answer.
///
/// The reserved names are the roadmap, not documentation: nothing reads them into a prompt.
const SCHEMES: &[Scheme] = &[
    Scheme { name: "agent", status: Status::Reserved },
    Scheme { name: "artifact", status: Status::Reserved },
    Scheme { name: "attachment", status: Status::Reserved },
    Scheme { name: "cfg", status: Status::Reserved },
    Scheme {
        name: "conflict",
        status: Status::Active(Active {
            summary: "a merge-conflict block by id: read `conflict://<N>` (`/ours`, `/theirs`, `/base` for one side), write it to resolve that block, `conflict://*` to resolve several at once",
            read: Some(conflict_read),
            write: Some(conflict_write),
        }),
    },
    Scheme { name: "history", status: Status::Reserved },
    Scheme { name: "issue", status: Status::Reserved },
    Scheme { name: "local", status: Status::Reserved },
    Scheme { name: "mcp", status: Status::Reserved },
    Scheme { name: "memory", status: Status::Reserved },
    Scheme { name: "mr", status: Status::Reserved },
    Scheme { name: "omp", status: Status::Reserved },
    Scheme { name: "out", status: Status::Reserved },
    Scheme { name: "pr", status: Status::Reserved },
    Scheme { name: "proc", status: Status::Reserved },
    Scheme { name: "rule", status: Status::Reserved },
    Scheme { name: "security", status: Status::Reserved },
    Scheme {
        name: "skill",
        status: Status::Active(Active {
            summary: "a skill by name: read `skill://<name>` for its `SKILL.md`, `skill://<name>/<path>` for another file of it",
            read: Some(skill_read),
            write: None,
        }),
    },
    Scheme { name: "ssh", status: Status::Reserved },
    Scheme { name: "vault", status: Status::Reserved },
    Scheme { name: "xd", status: Status::Reserved },
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
    let (name, active, rest) = resolve(href)?;
    let handler = active.read.ok_or_else(|| format!("`{name}://` cannot be read"))?;
    handler(ctx, rest)
}

/// Writes `content` to `href` through the scheme that owns it.
pub(crate) fn write(ctx: &mut Ctx, href: &str, content: &str) -> Result<String, String> {
    let (name, active, rest) = resolve(href)?;
    let handler = active.write.ok_or_else(|| format!("`{name}://` is read-only"))?;
    handler(ctx, rest, content)
}

/// The active schemes, as tool descriptions state them; `None` when this build resolves nothing.
pub(crate) fn summary() -> Option<String> {
    let lines: Vec<String> = active().map(|(name, active)| format!("- `{name}://` — {}", active.summary)).collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Every scheme and its state: what `/schemes` shows a human, and what models never see.
pub(crate) fn report() -> String {
    let active: Vec<String> = active().map(|(name, active)| format!("  {name}://  {}", active.summary)).collect();
    let reserved: Vec<String> = SCHEMES
        .iter()
        .filter(|scheme| matches!(scheme.status, Status::Reserved))
        .map(|scheme| format!("{}://", scheme.name))
        .collect();
    let mut out = format!("internal URIs: {} active, {} reserved\n", active.len(), reserved.len());
    if !active.is_empty() {
        out.push_str(&active.join("\n"));
        out.push('\n');
    }
    if !reserved.is_empty() {
        let _ = writeln!(out, "reserved, not resolvable: {}", reserved.join(" "));
    }
    out
}

fn active() -> impl Iterator<Item = (&'static str, &'static Active)> {
    SCHEMES.iter().filter_map(|scheme| match &scheme.status {
        Status::Active(active) => Some((scheme.name, active)),
        Status::Reserved => None,
    })
}

/// Splits `href` into its scheme and the remainder, or says why no scheme answers to it.
fn resolve(href: &str) -> Result<(&'static str, &'static Active, &str), String> {
    let Some((name, rest)) = href.split_once("://") else {
        return Err(format!("`{}` is not an internal URI: it has no `scheme://` prefix", shown(href)));
    };
    let found = SCHEMES
        .iter()
        .find(|scheme| scheme.name.eq_ignore_ascii_case(name))
        .map(|scheme| (scheme.name, &scheme.status));
    match found {
        Some((name, Status::Active(active))) => Ok((name, active, rest)),
        // A reserved name gets the same answer as a name nobody ever wrote, and neither answer
        // lists what else exists: only the active ones are advertised.
        _ => Err(unavailable(name)),
    }
}

/// `text` with control characters escaped, so a path cannot forge lines in a diagnostic.
fn shown(text: &str) -> String {
    text.chars().flat_map(char::escape_debug).collect()
}

fn unavailable(name: &str) -> String {
    let known: Vec<String> = active().map(|(name, _)| format!("`{name}://`")).collect();
    if known.is_empty() {
        return format!("`{}://` is not an available scheme: this build resolves none", shown(name));
    }
    format!("`{}://` is not an available scheme: this build resolves {}", shown(name), known.join(", "))
}

fn conflict_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(conflicts) = ctx.conflicts.as_mut() else {
        return Err("conflict ids belong to the worker that read the file".to_string());
    };
    conflicts.read(rest)
}

fn conflict_write(ctx: &mut Ctx, rest: &str, content: &str) -> Result<String, String> {
    let Some(conflicts) = ctx.conflicts.as_mut() else {
        return Err("conflict ids belong to the worker that read the file".to_string());
    };
    conflicts.write(rest, content)
}

fn skill_read(ctx: &mut Ctx, rest: &str) -> Result<String, String> {
    let Some(skills) = ctx.skills else {
        return Err("skills are not available to this caller: it has no skill catalogue".to_string());
    };
    skills.read(rest)
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
        "omp",
        "out",
        "pr",
        "proc",
        "rule",
        "security",
        "skill",
        "ssh",
        "vault",
        "xd",
    ];

    fn context(registry: &mut conflict::Registry) -> Ctx<'_> {
        Ctx { conflicts: Some(registry), skills: None }
    }

    #[test]
    fn the_table_holds_every_scheme_of_the_toolkit() {
        let names: Vec<&str> = SCHEMES.iter().map(|scheme| scheme.name).collect();
        assert_eq!(names, PARITY);
        assert_eq!(format!("{}://", SCHEMES[4].name), conflict::SCHEME, "the registry name and the scheme must agree");
    }

    #[test]
    fn only_the_schemes_with_handlers_are_advertised() {
        let summary = summary().unwrap();
        assert!(summary.contains("`conflict://`"), "{summary}");
        assert!(summary.contains("`skill://`"), "{summary}");
        assert!(!summary.contains("mcp"), "{summary}");
        assert!(!summary.contains("ssh"), "{summary}");
    }

    #[test]
    fn a_reserved_name_is_refused_without_being_named_as_reserved() {
        for scheme in ["mcp", "pr", "ssh", "nobody-wrote-this"] {
            let mut registry = conflict::Registry::default();
            let error = read(&mut context(&mut registry), &format!("{scheme}://x")).unwrap_err();
            assert_eq!(
                error,
                format!("`{scheme}://` is not an available scheme: this build resolves `conflict://`, `skill://`")
            );
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
        let mut ctx = Ctx { conflicts: None, skills: None };
        assert_eq!(read(&mut ctx, "conflict://1").unwrap_err(), "conflict ids belong to the worker that read the file");
        assert!(write(&mut ctx, "conflict://1", "@ours").is_err());
    }
}
