//! Tool output too large for one result, spilled whole and read back through `artifact://`.
//!
//! The store is process-wide, like the shell's process groups: every agent of one session sees the
//! same entries, and the oldest go first once the caps are reached.

use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Most artifacts one session keeps; the oldest is evicted past this.
const MAX_ARTIFACTS: usize = 32;

/// Longest text one artifact keeps; the rest of a larger text is dropped.
const MAX_ARTIFACT_BYTES: usize = 4 << 20;

/// Longest total the store keeps; the oldest artifacts go until it holds again.
const MAX_TOTAL_BYTES: usize = 16 << 20;

/// Longest first line an `artifact://` listing shows.
const FIRST_LINE_WIDTH: usize = 80;

struct Artifact {
    id: usize,
    text: String,
    /// The text was larger than `MAX_ARTIFACT_BYTES` and only its head was kept.
    truncated: bool,
}

static ARTIFACTS: Mutex<Vec<Artifact>> = Mutex::new(Vec::new());

fn store() -> MutexGuard<'static, Vec<Artifact>> {
    ARTIFACTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Keeps `text` and returns the id a marker names it by; the oldest entries go once a cap is hit.
pub(crate) fn spill(text: String) -> usize {
    let mut kept = store();
    let id = kept.last().map_or(1, |artifact| artifact.id + 1);
    let truncated = text.len() > MAX_ARTIFACT_BYTES;
    let text = if truncated { head(text) } else { text };
    kept.push(Artifact { id, text, truncated });
    while kept.len() > MAX_ARTIFACTS || total(&kept) > MAX_TOTAL_BYTES {
        kept.remove(0);
    }
    id
}

/// The text of one artifact, saying so when only its head was kept.
pub(crate) fn read(id: usize) -> Option<String> {
    let kept = store();
    let artifact = kept.iter().find(|artifact| artifact.id == id)?;
    let mut text = artifact.text.clone();
    if artifact.truncated {
        let _ = write!(text, "\n[artifact truncated: first {MAX_ARTIFACT_BYTES} bytes kept]");
    }
    Some(text)
}

/// Every artifact as `(id, bytes, first line)`, oldest first.
pub(crate) fn list() -> Vec<(usize, usize, String)> {
    store()
        .iter()
        .map(|artifact| {
            let first: String =
                artifact.text.lines().next().unwrap_or_default().chars().take(FIRST_LINE_WIDTH).collect();
            (artifact.id, artifact.text.len(), first)
        })
        .collect()
}

/// The ids the store holds, for an error that has to name what a stale id could have been.
pub(crate) fn ids() -> Vec<usize> {
    store().iter().map(|artifact| artifact.id).collect()
}

fn total(kept: &[Artifact]) -> usize {
    kept.iter().map(|artifact| artifact.text.len()).sum()
}

/// The longest prefix of `text` that fits the artifact budget, on a character boundary.
fn head(text: String) -> String {
    if text.len() <= MAX_ARTIFACT_BYTES {
        return text;
    }
    let mut end = MAX_ARTIFACT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test for the whole store: it is process-global, so parallel tests would see each other.
    #[test]
    fn an_artifact_is_kept_under_its_id_and_evicted_once_the_store_is_full() {
        let first = spill("one\ntwo".into());
        assert_eq!(read(first).as_deref(), Some("one\ntwo"));
        let listing = list();
        let entry = listing.iter().find(|(id, _, _)| *id == first).expect("the artifact is listed");
        assert_eq!(entry.1, 7);
        assert_eq!(entry.2, "one");
        assert_eq!(read(usize::MAX), None);

        let big = spill("x".repeat(MAX_ARTIFACT_BYTES + 1));
        assert!(read(big).unwrap().ends_with(&format!("[artifact truncated: first {MAX_ARTIFACT_BYTES} bytes kept]")));

        let mut last = big;
        for _ in 0..MAX_ARTIFACTS {
            last = spill("y".into());
        }
        assert!(list().len() <= MAX_ARTIFACTS, "the store grew past its cap");
        assert!(read(first).is_none(), "the oldest artifact survived");
        assert_eq!(read(last).as_deref(), Some("y"));
        assert!(ids().contains(&last));
    }
}
