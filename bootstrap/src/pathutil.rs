use std::path::PathBuf;

/// Expands a leading `~/` against `$HOME`. Used anywhere a path arrives
/// as user/caller-supplied text - CLI flags, and (once params arrive as
/// JSON over the daemon socket) task params - rather than as a `Path`
/// the compiler already knows is absolute.
pub fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(rest)
        }
        None => PathBuf::from(path),
    }
}
