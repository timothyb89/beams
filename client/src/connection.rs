//! How the client reaches a Beam: one [`BeamTarget`] per Beam, and a
//! [`BeamConnection`] that turns operations (exec, forward, copy, Mutagen)
//! into commands. Callers never build `tsh ...` argv themselves, so a
//! multiplexed transport can replace [`TshConnection`] in one place.
//!
//! [`TshConnection`] is Tier C: every operation is its own pinned `tsh`
//! process. Tier B (an OpenSSH ControlMaster over a pinned `tsh proxy ssh`,
//! falling back to Tier C per operation) is `crate::master`. See
//! `docs/designs/BEAM_TRANSPORT_DESIGN.md`.

use std::ffi::{OsStr, OsString};
use std::fmt::Debug;
use std::process::Command;
use std::sync::Arc;

use crate::endpoint::{Endpoint, Tsh};

/// A Beam and where it lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeamTarget {
    /// User-facing ID, for messages only.
    pub beam_id: String,
    pub beam_uuid: String,
    /// OS login on the Beam (`beams`).
    pub login: String,
    /// Where the Beam lives; `None` only for records from before pinning.
    pub endpoint: Option<Endpoint>,
}

impl BeamTarget {
    /// SSH destination, `login@uuid`.
    pub fn destination(&self) -> String {
        format!("{}@{}", self.login, self.beam_uuid)
    }

    /// A copy operand for `path` on the Beam.
    pub fn remote_path(&self, path: &str) -> OsString {
        format!("{}:{path}", self.destination()).into()
    }
}

/// How a local forward is held open.
#[derive(Debug)]
pub enum ForwardPlan {
    /// A process that holds the forward until killed, and exits if the
    /// forward can't be established or is lost.
    Process(Command),
    /// A forward registered on a shared master: run `open` (fails if the
    /// port can't be bound), and `cancel` to remove it. It disappears with
    /// the master, which `lease` reports.
    Master { open: Command, cancel: Command, lease: crate::master::Lease },
}

/// Builds the commands that reach one Beam. Each returned [`Command`] has
/// program and arguments set; callers add stdio, parent-death handling
/// and spawn it.
pub trait BeamConnection: Send + Sync + Debug {
    fn target(&self) -> &BeamTarget;

    /// Runs `remote_command` without a PTY. It must be one already-quoted
    /// shell line: ssh joins trailing arguments with spaces and hands the
    /// result to the remote shell (see `tsh::prepare_git_target`).
    fn exec(&self, remote_command: &str) -> Command;

    /// Forwards `127.0.0.1:local_port` to `remote_host:remote_port` on the
    /// Beam's side.
    fn local_forward(&self, local_port: u16, remote_host: &str, remote_port: u16) -> ForwardPlan;

    /// Copies `source` to `destination`; exactly one of them is a
    /// [`BeamTarget::remote_path`].
    fn copy(&self, recursive: bool, source: &OsStr, destination: &OsStr) -> Command;

    /// Pins `git` (a `git` command about to push or fetch) and returns the
    /// `ext::` remote URL that reaches `remote_path` on the Beam. The caller
    /// still sets `GIT_ALLOW_PROTOCOL=ext`.
    fn git_remote(&self, git: &mut Command, remote_path: &str) -> String;

    /// Contents of the per-host file Mutagen's wrapper scripts read before
    /// each connection (see `sync::SSH_WRAPPER`), or `None` to leave the
    /// wrappers unpinned.
    fn mutagen_host_entry(&self) -> Option<String>;
}

pub type Connection = Arc<dyn BeamConnection>;

/// Tier C: every operation is a separate `tsh` invocation, pinned to the
/// Beam's endpoint.
#[derive(Clone, Debug)]
pub struct TshConnection {
    target: BeamTarget,
    tsh: Tsh,
}

impl TshConnection {
    pub fn new(program: impl Into<String>, target: BeamTarget) -> Self {
        let tsh = Tsh::new(program, target.endpoint.clone());
        Self { target, tsh }
    }

    pub fn shared(program: impl Into<String>, target: BeamTarget) -> Connection {
        Arc::new(Self::new(program, target))
    }
}

impl BeamConnection for TshConnection {
    fn target(&self) -> &BeamTarget {
        &self.target
    }

    fn exec(&self, remote_command: &str) -> Command {
        let mut command = self.tsh.command();
        command.args(["ssh", &self.target.destination(), "--", remote_command]);
        command
    }

    fn local_forward(&self, local_port: u16, remote_host: &str, remote_port: u16) -> ForwardPlan {
        let forward = format!("127.0.0.1:{local_port}:{remote_host}:{remote_port}");
        let mut command = self.tsh.command();
        command.args([
            "ssh",
            "-N",
            "-o",
            "ExitOnForwardFailure=yes",
            "-L",
            &forward,
            &self.target.destination(),
        ]);
        ForwardPlan::Process(command)
    }

    fn copy(&self, recursive: bool, source: &OsStr, destination: &OsStr) -> Command {
        let mut command = self.tsh.command();
        command.arg("scp");
        if recursive {
            command.arg("-r");
        }
        command.arg(source).arg(destination);
        command
    }

    fn git_remote(&self, git: &mut Command, remote_path: &str) -> String {
        if let Some(endpoint) = &self.target.endpoint {
            endpoint.apply(git);
        }
        crate::tsh::git_ext_url(&format!("{} ssh", self.tsh.program), &self.target.destination(), remote_path)
    }

    fn mutagen_host_entry(&self) -> Option<String> {
        let endpoint = self.target.endpoint.as_ref()?;
        let home = endpoint
            .teleport_home
            .as_ref()
            .map(|home| home.display().to_string())
            .unwrap_or_default();
        Some(format!("{}\n{}\n{home}\n", endpoint.proxy, endpoint.cluster))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> BeamTarget {
        BeamTarget {
            beam_id: "quiet-otter".into(),
            beam_uuid: "uuid-1".into(),
            login: "beams".into(),
            endpoint: Some(Endpoint {
                proxy: "a.example.com:443".into(),
                cluster: "a.example.com".into(),
                user: String::new(),
                teleport_home: None,
            }),
        }
    }

    fn argv(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn env(command: &Command, key: &str) -> Option<String> {
        command
            .get_envs()
            .find(|(name, _)| *name == key)
            .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
    }

    #[test]
    fn tsh_operations_are_pinned_and_shaped_like_ssh() {
        let connection = TshConnection::new("tsh", target());
        let exec = connection.exec("bash -lc 'true'");
        assert_eq!(argv(&exec), ["tsh", "ssh", "beams@uuid-1", "--", "bash -lc 'true'"]);
        assert_eq!(env(&exec, "TELEPORT_PROXY").as_deref(), Some("a.example.com:443"));

        let ForwardPlan::Process(forward) = connection.local_forward(8080, "localhost", 3000) else {
            panic!("tsh forwards are processes");
        };
        assert_eq!(
            argv(&forward),
            ["tsh", "ssh", "-N", "-o", "ExitOnForwardFailure=yes", "-L", "127.0.0.1:8080:localhost:3000", "beams@uuid-1"]
        );

        let remote = connection.target().remote_path("/tmp/x");
        let copy = connection.copy(true, &remote, OsStr::new("/local"));
        assert_eq!(argv(&copy), ["tsh", "scp", "-r", "beams@uuid-1:/tmp/x", "/local"]);
        assert_eq!(env(&copy, "TELEPORT_CLUSTER").as_deref(), Some("a.example.com"));

        let mut git = Command::new("git");
        let url = connection.git_remote(&mut git, "beams");
        assert_eq!(url, "ext::tsh ssh beams@uuid-1 -- %S 'beams'");
        assert_eq!(env(&git, "TELEPORT_PROXY").as_deref(), Some("a.example.com:443"));
    }

    #[test]
    fn mutagen_entries_carry_the_pin() {
        let connection = TshConnection::new("tsh", target());
        assert_eq!(connection.mutagen_host_entry().as_deref(), Some("a.example.com:443\na.example.com\n\n"));
        let legacy = TshConnection::new("tsh", BeamTarget { endpoint: None, ..target() });
        assert_eq!(legacy.mutagen_host_entry(), None);
    }
}
