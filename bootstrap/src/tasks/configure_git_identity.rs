use std::process::Command;

use anyhow::Context;

use super::{Task, Template};

/// Sets `git config --global user.name`/`user.email` on the Beam.
///
/// Both fields are optional: the client fills them from the
/// workstation's own `git config` at compile time (see
/// `client/src/config/plan.rs`'s `default_git_identity`), overridable
/// per profile, but a workstation with no git identity of its own
/// leaves both unset here rather than failing the launch over it. A
/// field left `None` is simply not touched - whatever's already on the
/// Beam (from an earlier run, or nothing) stands. `git config --global`
/// is idempotent and only ever touches `user.name`/`user.email`, so
/// re-running this leaves every other Beam-side git setting alone.
pub struct ConfigureGitIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl Task for ConfigureGitIdentity {
    fn id(&self) -> String {
        Self::NAME.to_string()
    }

    fn run(&self) -> anyhow::Result<Option<String>> {
        let mut set = Vec::new();
        if let Some(name) = &self.name {
            set_global(&["user.name", name])?;
            set.push(format!("user.name={name}"));
        }
        if let Some(email) = &self.email {
            set_global(&["user.email", email])?;
            set.push(format!("user.email={email}"));
        }
        if set.is_empty() {
            return Ok(None);
        }
        Ok(Some(set.join(", ")))
    }
}

impl Template for ConfigureGitIdentity {
    const NAME: &'static str = "configure-git-identity";
}

fn set_global(args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("git")
        .arg("config")
        .arg("--global")
        .args(args)
        .status()
        .context("running git config --global")?;
    if !status.success() {
        anyhow::bail!("git config --global {} exited with {status}", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_guard() -> (std::path::PathBuf, Option<String>) {
        let dir = std::env::temp_dir().join(format!("configure-git-identity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), None)
    }

    #[test]
    fn sets_only_the_fields_given() {
        let (dir, _) = home_guard();
        let prior = std::env::var("HOME").ok();
        std::env::set_var("HOME", &dir);

        let task = ConfigureGitIdentity { name: Some("Ada".into()), email: None };
        task.run().unwrap();

        let out = Command::new("git").args(["config", "--global", "user.name"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "Ada");
        let out = Command::new("git").args(["config", "--global", "user.email"]).output().unwrap();
        assert!(!out.status.success(), "email was never set");

        match prior {
            Some(prior) => std::env::set_var("HOME", prior),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_with_spaces_is_set_whole_not_split_on_whitespace() {
        let (dir, _) = home_guard();
        let prior = std::env::var("HOME").ok();
        std::env::set_var("HOME", &dir);

        let task = ConfigureGitIdentity { name: Some("Ada Lovelace".into()), email: None };
        task.run().unwrap();

        let out = Command::new("git").args(["config", "--global", "user.name"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "Ada Lovelace");

        match prior {
            Some(prior) => std::env::set_var("HOME", prior),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_fields_is_a_no_op() {
        let task = ConfigureGitIdentity { name: None, email: None };
        assert_eq!(task.run().unwrap(), None);
    }
}
