use std::path::{Path, PathBuf};

/// Absolute paths are used as given, relative ones join the session directory.
pub(crate) fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}
