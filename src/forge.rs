//! Issues and pull/merge requests, read through `gh` and `glab` exactly as a user would run them.

use crate::tools;
use std::path::Path;
use std::time::Duration;

/// How long one forge command may run before it is killed.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How many rows a bare listing asks for.
const LIMIT: &str = "20";

/// What a `issue://`, `pr://` or `mr://` path names: nothing (a listing), one number, or a diff.
#[derive(Debug)]
pub(crate) struct Target {
    pub(crate) number: Option<u32>,
    pub(crate) diff: bool,
    /// The `?state=` filter, kept only for a listing: it says nothing about one request.
    pub(crate) state: Option<String>,
}

fn usage(scheme: &str) -> String {
    format!("usage: {scheme}://[<N>][?state=…]")
}

/// Splits one target into what it names, or says how the path should read.
pub(crate) fn parse(target: &str, scheme: &str) -> Result<Target, String> {
    let target = target.strip_prefix('/').unwrap_or(target);
    let (path, query) = target.split_once('?').map_or((target, ""), |(path, query)| (path, query));
    let mut state = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').ok_or_else(|| usage(scheme))?;
        if key != "state" {
            return Err(usage(scheme));
        }
        state = Some(value.to_string());
    }
    let (path, diff) = match path.strip_suffix("/diff") {
        Some(head) => (head, true),
        None => (path, false),
    };
    if path.is_empty() {
        return Ok(Target { number: None, diff: false, state });
    }
    if path.chars().all(|c| c.is_ascii_digit()) {
        let number = path.parse::<u32>().map_err(|_| usage(scheme))?;
        return Ok(Target { number: Some(number), diff, state: None });
    }
    Err(usage(scheme))
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

/// `--state <v>` appended to a listing's arguments, when the target asked for one.
fn with_state(mut argv: Vec<String>, state: Option<String>) -> Vec<String> {
    if let Some(state) = state {
        argv.push("--state".to_string());
        argv.push(state);
    }
    argv
}

/// `issue://`, `issue://<N>`, and a `?state=` filter on the listing.
pub(crate) fn issue(cwd: &Path, target: &str) -> Result<String, String> {
    let Target { number, diff, state } = parse(target, "issue")?;
    if diff {
        return Err(usage("issue"));
    }
    if let Some(number) = number {
        return tools::capture("gh", &args(&["issue", "view", "--", &number.to_string()]), cwd, TIMEOUT);
    }
    let argv = with_state(args(&["issue", "list", "--limit", LIMIT]), state);
    tools::capture("gh", &argv, cwd, TIMEOUT)
}

/// `pr://`, `pr://<N>`, `pr://<N>/diff`, and a `?state=` filter on the listing.
pub(crate) fn pr(cwd: &Path, target: &str) -> Result<String, String> {
    let Target { number, diff, state } = parse(target, "pr")?;
    match (number, diff) {
        (Some(number), true) => tools::capture("gh", &args(&["pr", "diff", "--", &number.to_string()]), cwd, TIMEOUT),
        (Some(number), false) => tools::capture("gh", &args(&["pr", "view", "--", &number.to_string()]), cwd, TIMEOUT),
        (None, _) => {
            let argv = with_state(args(&["pr", "list", "--limit", LIMIT]), state);
            tools::capture("gh", &argv, cwd, TIMEOUT)
        }
    }
}

/// `mr://` on GitLab: the same shapes, through `glab`, which takes no `--` before its id.
pub(crate) fn mr(cwd: &Path, target: &str) -> Result<String, String> {
    let Target { number, diff, state } = parse(target, "mr")?;
    match (number, diff) {
        (Some(number), true) => tools::capture("glab", &args(&["mr", "diff", &number.to_string()]), cwd, TIMEOUT),
        (Some(number), false) => tools::capture("glab", &args(&["mr", "view", &number.to_string()]), cwd, TIMEOUT),
        (None, _) => {
            let argv = with_state(args(&["mr", "list"]), state);
            tools::capture("glab", &argv, cwd, TIMEOUT)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_a_listing_a_number_or_a_diff() {
        let bare = parse("", "pr").unwrap();
        assert_eq!(bare.number, None);
        assert!(!bare.diff);
        assert_eq!(bare.state, None);
        assert_eq!(parse("1", "pr").unwrap().number, Some(1));
        let diff = parse("1/diff", "pr").unwrap();
        assert_eq!((diff.number, diff.diff), (Some(1), true));
        assert_eq!(parse("?state=closed", "pr").unwrap().state.as_deref(), Some("closed"));
        assert_eq!(parse("1?state=closed", "pr").unwrap().state, None, "a number ignores the filter");
        assert_eq!(parse("/7", "pr").unwrap().number, Some(7));
        assert_eq!(parse("x", "pr").unwrap_err(), "usage: pr://[<N>][?state=…]");
        assert_eq!(parse("1?sort=asc", "mr").unwrap_err(), "usage: mr://[<N>][?state=…]");
        assert!(parse("1/diff", "issue").unwrap().diff, "parse keeps the shape; issue refuses it");
    }
}
