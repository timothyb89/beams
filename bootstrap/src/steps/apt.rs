use std::path::Path;

use super::{cap, resource, Step};
use crate::aptlock;
use crate::graph::Capability;

/// Packages to install on every beam, that are available from Debian's
/// default repos as-is. apt-get already no-ops packages that are already
/// installed, so this list doesn't need its own is-it-installed check -
/// just grow it as needed. zellij and kubectl aren't in Debian's repos and
/// are installed by their own dedicated steps instead (see `zellij.rs` and
/// `kubectl.rs`).
const PACKAGES: &[&str] = &["micro", "less", "fish"];

pub struct InstallPackages;

impl Step for InstallPackages {
    fn id(&self) -> &'static str {
        "apt-install"
    }

    fn provides(&self) -> Vec<Capability> {
        vec![cap::APT_PACKAGES_INSTALLED.into()]
    }

    fn exclusive_resources(&self) -> Vec<Capability> {
        vec![resource::APT.into()]
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        // A fresh beam's apt index can be stale/empty - `apt-get install`
        // then fails with "unable to locate package" for everything,
        // indistinguishable from a real typo in PACKAGES. Always update
        // first; it's a no-op cost-wise if the index is already current.
        aptlock::apt_get(&["update"])?;

        let mut install_args = vec!["install", "-y"];
        install_args.extend_from_slice(PACKAGES);
        aptlock::apt_get(&install_args)?;

        Ok(Some(format!("installed {}", PACKAGES.join(", "))))
    }
}
