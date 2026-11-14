//! Stamps the binary with the commit it was built from, so a stale install is visible from the UI.

use std::path::Path;
use std::process::Command;

/// Runs one git command, trimmed stdout, or `None` when git is missing or refuses.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Absolute form of `dir/name`: `--git-common-dir` answers relative to the checkout.
fn join(dir: &str, name: &str) -> String {
    let dir = Path::new(dir);
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    dir.join(name).display().to_string()
}

fn main() {
    // A commit changes no source file, and naming any path here stops cargo rerunning on the
    // whole package: the git metadata has to be listed, or the stamp keeps naming the previous
    // commit. A linked worktree stores `.git` as a file, so git is asked for the real directories.
    for path in ["src", "Cargo.toml"] {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        for name in ["HEAD", "index"] {
            println!("cargo:rerun-if-changed={}", join(&dir, name));
        }
    }
    if let Some(dir) = git(&["rev-parse", "--git-common-dir"]) {
        for name in ["refs", "packed-refs"] {
            println!("cargo:rerun-if-changed={}", join(&dir, name));
        }
    }

    let hash =
        git(&["rev-parse", "--short", "HEAD"]).filter(|hash| !hash.is_empty()).unwrap_or_else(|| "unknown".to_string());
    // A tree with uncommitted changes no longer matches its commit, and the hash alone would lie.
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|changed| !changed.is_empty());
    let stamp = if dirty { format!("{hash}-dirty") } else { hash };
    println!("cargo:rustc-env=OVERSEER_GIT={stamp}");
}
