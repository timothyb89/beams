use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};

use super::{cap, resource, Step};
use crate::graph::Capability;

/// kubectl isn't in Debian's default repos, so we add the official
/// Kubernetes apt repo first. Bump this when a newer minor is wanted -
/// see https://kubernetes.io/releases/ for the current stable minor.
const K8S_MINOR: &str = "v1.37";
const KEYRING: &str = "/etc/apt/keyrings/kubernetes-apt-keyring.gpg";
const SOURCES_LIST: &str = "/etc/apt/sources.list.d/kubernetes.list";

pub struct InstallKubectl;

impl Step for InstallKubectl {
    fn id(&self) -> &'static str {
        "install-kubectl"
    }

    fn provides(&self) -> Vec<Capability> {
        vec![cap::KUBECTL_INSTALLED.into()]
    }

    fn exclusive_resources(&self) -> Vec<Capability> {
        vec![resource::APT.into()]
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        if Command::new("which")
            .arg("kubectl")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Ok(None);
        }

        let repo_line = format!(
            "deb [signed-by={KEYRING}] https://pkgs.k8s.io/core:/stable:/{K8S_MINOR}/deb/ /"
        );
        let already_configured = std::fs::read_to_string(SOURCES_LIST)
            .map(|contents| contents.contains(&repo_line))
            .unwrap_or(false);

        if !already_configured {
            run(Command::new("sudo").args(["mkdir", "-p", "/etc/apt/keyrings"]))?;

            // sh -c pipeline: `curl ... | sudo gpg --dearmor ... -o keyring`
            let pipeline = format!(
                "curl -fsSL https://pkgs.k8s.io/core:/stable:/{K8S_MINOR}/deb/Release.key \
                 | sudo gpg --batch --yes --dearmor -o {KEYRING}"
            );
            run(Command::new("sh").args(["-c", &pipeline]))?;

            let tee = format!("echo '{repo_line}' | sudo tee {SOURCES_LIST} >/dev/null");
            run(Command::new("sh").args(["-c", &tee]))?;

            run(Command::new("sudo").args(["apt-get", "update"]))?;
        }

        run(Command::new("sudo").args(["apt-get", "install", "-y", "kubectl"]))?;

        Ok(Some(format!("installed kubectl ({K8S_MINOR} repo)")))
    }
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let output = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    if !output.status.success() {
        bail!(
            "{:?} failed ({}): {}",
            cmd,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
