//! Worker codenames: two word lists mixed from the worker id.
//!
//! The id stays the only key the app looks a worker up by; a codename is only what the user
//! reads. The mix is deterministic, so a rerun gives id 3 the same codename it had before.

/// First half of a codename.
const ADJECTIVES: [&str; 32] = [
    "brave", "bright", "bold", "brisk", "calm", "canny", "clever", "deft", "eager", "fleet", "glad", "hardy", "humble",
    "jolly", "keen", "lofty", "lucid", "mellow", "merry", "nimble", "noble", "plucky", "polite", "proud", "quiet",
    "rapid", "ready", "sharp", "steady", "subtle", "swift", "tidy",
];

/// Second half of a codename.
const NOUNS: [&str; 32] = [
    "otter", "heron", "badger", "falcon", "ferret", "gannet", "gecko", "hare", "hedgehog", "ibis", "jackdaw",
    "kestrel", "lemur", "lynx", "magpie", "marten", "mole", "newt", "osprey", "pelican", "puffin", "quail", "raven",
    "robin", "seal", "sparrow", "stoat", "tern", "thrush", "vole", "walrus", "weasel",
];

/// The codename a worker id always gets, such as `brave-otter`.
pub(crate) fn codename(id: usize) -> String {
    let adjective = ADJECTIVES.get(mix(id) % ADJECTIVES.len()).copied().unwrap_or("quiet");
    let noun = NOUNS.get(mix(mix(id)) % NOUNS.len()).copied().unwrap_or("otter");
    format!("{adjective}-{noun}")
}

/// The worker's name on its own: the one the user chose, else the codename.
pub(crate) fn name(id: usize, chosen: Option<&str>) -> String {
    chosen.map(str::trim).filter(|name| !name.is_empty()).map_or_else(|| codename(id), str::to_string)
}

/// How a worker reads: its name, then the id it answers to.
pub(crate) fn label(id: usize, chosen: Option<&str>) -> String {
    format!("{} ({id})", name(id, chosen))
}

/// Spreads neighbouring ids apart, so ids 1, 2 and 3 never get adjacent words. The constants fit
/// a 32-bit `usize`, so the same id gives the same codename on every target.
fn mix(value: usize) -> usize {
    let mut x = value;
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn words(name: &str) -> (&str, &str) {
        name.split_once('-').unwrap()
    }

    #[test]
    fn a_codename_is_stable_and_built_from_the_lists() {
        assert_eq!(codename(3), codename(3));
        let name = codename(1);
        let (adjective, noun) = words(&name);
        assert!(ADJECTIVES.contains(&adjective) && NOUNS.contains(&noun), "{name}");
    }

    #[test]
    fn neighbouring_ids_get_distinct_codenames() {
        let names: HashSet<String> = (1..=12).map(codename).collect();
        assert_eq!(names.len(), 12, "{names:?}");
    }

    #[test]
    fn a_name_is_the_chosen_one_or_the_codename() {
        assert_eq!(name(3, None), codename(3));
        assert_eq!(name(3, Some("fix-nav")), "fix-nav");
        assert_eq!(name(3, Some("  ")), codename(3));
    }

    #[test]
    fn a_label_carries_the_name_then_the_id() {
        assert_eq!(label(3, None), format!("{} (3)", codename(3)));
        assert_eq!(label(3, Some("fix-nav")), "fix-nav (3)");
        assert_eq!(label(3, Some("  ")), format!("{} (3)", codename(3)), "a blank name falls back to the codename");
        assert_eq!(label(3, Some(" fix-nav ")), "fix-nav (3)");
    }
}
