use std::path::Path;

use super::linkutil::{ensure_symlink, home_dir};
use super::{cap, Step};
use crate::graph::Capability;

/// Top-level repo entries to symlink into `$HOME`, and whether an existing
/// non-symlink entry should be force-replaced (moved aside to `.orig`
/// first) rather than left alone. Grow this list as more of the repo
/// becomes relevant to a fresh beam.
///
/// `.ssh` is workstation-local and never linked: SSH key use goes through
/// the daemon's relay, and the Beam's own SSH configuration stays intact.
///
/// `.claude` is deliberately excluded entirely: `~/.claude` already holds
/// real beam-managed state (session history, credentials, plugins), so it
/// can't be replaced wholesale. See `claude_md` for the one piece of it
/// (the repo's `CLAUDE.md`) that does get merged in. Shell config lives
/// under `shell/` and is linked separately by `shell_config.rs`, since it
/// targets `~/.config/...` rather than `~` directly.
const LINKS: &[(&str, bool)] = &[("bin", false)];

pub struct LinkFiles;

impl Step for LinkFiles {
    fn id(&self) -> &'static str {
        "link-files"
    }

    fn provides(&self) -> Vec<Capability> {
        vec![cap::DOTFILES_LINKED.into()]
    }

    fn run(&self, repo: &Path) -> anyhow::Result<Option<String>> {
        link_files(repo, &home_dir()?)
    }
}

fn link_files(repo: &Path, home: &Path) -> anyhow::Result<Option<String>> {
    let mut created = Vec::new();
    for (name, force) in LINKS {
        if ensure_symlink(&repo.join(name), &home.join(name), *force)? {
            created.push(*name);
        }
    }
    if created.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!("linked {}", created.join(", "))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("beam-links-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("repo/bin")).unwrap();
            fs::create_dir_all(root.join("home")).unwrap();
            Self(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn clean_checkout_links_bin_idempotently_without_touching_beam_ssh() {
        let fixture = Fixture::new();
        let repo = fixture.0.join("repo");
        let home = fixture.0.join("home");
        fs::create_dir(home.join(".ssh")).unwrap();
        fs::write(home.join(".ssh/known_hosts"), "beam-managed hosts\n").unwrap();
        assert!(!repo.join(".ssh").exists());

        assert_eq!(
            link_files(&repo, &home).unwrap().as_deref(),
            Some("linked bin")
        );
        assert_eq!(fs::read_link(home.join("bin")).unwrap(), repo.join("bin"));
        assert_eq!(link_files(&repo, &home).unwrap(), None);
        assert!(!fs::symlink_metadata(home.join(".ssh"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(home.join(".ssh/known_hosts")).unwrap(),
            "beam-managed hosts\n"
        );
        assert!(!home.join(".ssh.orig").exists());
    }

    #[test]
    fn leftover_repo_ssh_does_not_replace_an_existing_beam_ssh_symlink() {
        let fixture = Fixture::new();
        let repo = fixture.0.join("repo");
        let home = fixture.0.join("home");
        let managed = fixture.0.join("managed-ssh");
        fs::create_dir(&managed).unwrap();
        fs::write(managed.join("known_hosts"), "managed\n").unwrap();
        fs::create_dir(repo.join(".ssh")).unwrap();
        symlink(&managed, home.join(".ssh")).unwrap();

        link_files(&repo, &home).unwrap();
        assert_eq!(fs::read_link(home.join(".ssh")).unwrap(), managed);
        assert_eq!(
            fs::read_to_string(home.join(".ssh/known_hosts")).unwrap(),
            "managed\n"
        );
    }
}
