use std::path::Path;

use anyhow::Context;

use super::{cap, Step};
use crate::graph::Capability;

/// Points ssh's `KnownHostsCommand` at this binary (a drop-in in
/// `/etc/ssh/ssh_config.d/`; see `crate::knownhosts`), so `git@github.com:`
/// clones verify the host against the workstation's known_hosts instead of
/// failing "Host key verification failed". `bin/beam-shell` runs it
/// synchronously, before any startup clone can start; profile launches
/// install the same drop-in from `launch start`.
pub struct InstallKnownHostsCommand;

impl Step for InstallKnownHostsCommand {
    fn id(&self) -> &'static str {
        "known-hosts-command"
    }

    fn provides(&self) -> Vec<Capability> {
        vec![cap::KNOWN_HOSTS_COMMAND.into()]
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        let exe = std::env::current_exe().context("resolving this binary's path")?;
        Ok(crate::knownhosts::install_ssh_config(&exe)?
            .then(|| format!("wrote {}", crate::knownhosts::SSH_CONFIG_DROP_IN)))
    }
}
