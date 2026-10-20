use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::bail;

use super::k3s_bot::bot_dir;
use super::linkutil::{ensure_symlink, home_dir};
use super::Step;

const WAIT_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Symlinks tbot's generated kubeconfig into kubectl's default path, so
/// kubectl works against the k3s cluster out of the box with no --kubeconfig
/// flag or KUBECONFIG env var needed. tbot writes the file shortly after
/// `start-tbot` launches it, so this polls briefly rather than assuming it's
/// already there.
pub struct LinkKubeconfig;

impl Step for LinkKubeconfig {
    fn id(&self) -> &'static str {
        "kubeconfig-symlink"
    }

    fn run(&self, _repo: &Path) -> anyhow::Result<Option<String>> {
        let source = bot_dir()?.join("kube/kubeconfig.yaml");
        let target = home_dir()?.join(".kube/config");

        let start = Instant::now();
        while !source.exists() {
            if start.elapsed() > WAIT_TIMEOUT {
                bail!(
                    "{} did not appear within {:?} - is the tbot service healthy? (beamctl logs tbot)",
                    source.display(),
                    WAIT_TIMEOUT
                );
            }
            sleep(POLL_INTERVAL);
        }

        if ensure_symlink(&source, &target, false)? {
            Ok(Some(format!("linked {}", target.display())))
        } else {
            Ok(None)
        }
    }
}
