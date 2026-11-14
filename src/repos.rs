use std::path::{Path, PathBuf};

/// Levels below a root searched for repositories; a repository is never descended into.
const REPO_DEPTH: usize = 3;

/// Every git repository under `roots`, sorted and without duplicates; a repository is never
/// descended into.
pub(crate) fn find(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        collect(root, REPO_DEPTH, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if dir.join(".git").exists() {
        out.push(dir.to_path_buf());
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && !path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.')) {
            collect(&path, depth - 1, out);
        }
    }
}
